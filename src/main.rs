#![allow(deprecated)]

#[cfg(feature = "web")]
mod actix;
pub mod common;
mod consensus;
mod greeting;
mod migrations;
mod settings;
mod snapshots;
mod startup;
mod tonic;

use std::io::Error;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use ::tonic::transport::Uri;
use api::grpc::transport_channel_pool::TransportChannelPool;
use clap::Parser;
use collection::shards::channel_service::ChannelService;
use consensus::Consensus;
use slog::Drain;
use startup::setup_panic_hook;
use storage::content_manager::consensus::operation_sender::OperationSender;
use storage::content_manager::consensus::persistent::Persistent;
use storage::content_manager::consensus_state::{ConsensusState, ConsensusStateRef};
use storage::content_manager::toc::TableOfContent;
use storage::dispatcher::Dispatcher;
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

use crate::common::helpers::create_search_runtime;
use crate::common::telemetry::TelemetryCollector;
use crate::greeting::welcome;
use crate::migrations::single_to_cluster::handle_existing_collections;
use crate::settings::Settings;
use crate::snapshots::{recover_full_snapshot, recover_snapshots};
use crate::startup::setup_logger;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// Qdrant (read: quadrant ) is a vector similarity search engine.
/// It provides a production-ready service with a convenient API to store, search, and manage points - vectors with an additional payload.
///
/// This CLI starts a Qdrant peer/server.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Uri of the peer to bootstrap from in case of multi-peer deployment.
    /// If not specified - this peer will be considered as a first in a new deployment.
    #[arg(long, value_parser, value_name = "URI")]
    bootstrap: Option<Uri>,
    /// Uri of this peer.
    /// Other peers should be able to reach it by this uri.
    ///
    /// This value has to be supplied if this is the first peer in a new deployment.
    ///
    /// In case this is not the first peer and it bootstraps the value is optional.
    /// If not supplied then qdrant will take internal grpc port from config and derive the IP address of this peer on bootstrap peer (receiving side)
    #[arg(long, value_parser, value_name = "URI")]
    uri: Option<Uri>,

    /// Force snapshot re-creation
    /// If provided - existing collections will be replaced with snapshots.
    /// Default is to not recreate from snapshots.
    #[arg(short, long, action, default_value_t = false)]
    force_snapshot: bool,

    /// List of paths to snapshot files.
    /// Format: <snapshot_file_path>:<target_collection_name>
    #[arg(long, value_name = "PATH:NAME", alias = "collection-snapshot")]
    snapshot: Option<Vec<String>>,

    /// Path to snapshot of multiple collections.
    /// Format: <snapshot_file_path>
    #[arg(long, value_name = "PATH")]
    storage_snapshot: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let settings = Settings::new().expect("Can't read config.");

    setup_logger(&settings.log_level);
    setup_panic_hook();
    let args = Args::parse();

    let restored_collections = if let Some(full_snapshot) = args.storage_snapshot {
        recover_full_snapshot(
            &full_snapshot,
            &settings.storage.storage_path,
            args.force_snapshot,
        )
    } else if let Some(snapshots) = args.snapshot {
        // recover from snapshots
        recover_snapshots(
            &snapshots,
            args.force_snapshot,
            &settings.storage.storage_path,
        )
    } else {
        vec![]
    };

    welcome();

    // Create and own search runtime out of the scope of async context to ensure correct
    // destruction of it
    let runtime = create_search_runtime(settings.storage.performance.max_search_threads)
        .expect("Can't create runtime.");
    let runtime_handle = runtime.handle().clone();

    // Create a signal sender and receiver. It is used to communicate with the consensus thread.
    let (propose_sender, propose_receiver) = std::sync::mpsc::channel();

    let propose_operation_sender = if settings.cluster.enabled {
        // High-level channel which could be used to send User-space consensus operations
        Some(OperationSender::new(propose_sender))
    } else {
        // We don't need sender for the single-node mode
        None
    };

    // Saved state of the consensus.
    let persistent_consensus_state =
        Persistent::load_or_init(&settings.storage.storage_path, args.bootstrap.is_none())?;

    // Channel service is used to manage connections between peers.
    // It allocates required number of channels and manages proper reconnection handling
    let mut channel_service = ChannelService::default();

    if settings.cluster.enabled {
        // We only need channel_service in case if cluster is enabled.
        // So we initialize it with real values here
        let p2p_grpc_timeout = Duration::from_millis(settings.cluster.grpc_timeout_ms);
        let connection_timeout = Duration::from_millis(settings.cluster.connection_timeout_ms);
        channel_service.channel_pool = Arc::new(TransportChannelPool::new(
            p2p_grpc_timeout,
            connection_timeout,
            settings.cluster.p2p.connection_pool_size,
        ));
        channel_service.id_to_address = persistent_consensus_state.peer_address_by_id.clone();
    }

    // Table of content manages the list of collections.
    // It is a main entry point for the storage.
    let toc = TableOfContent::new(
        &settings.storage,
        runtime,
        channel_service.clone(),
        persistent_consensus_state.this_peer_id(),
        propose_operation_sender.clone(),
    );

    // Here we load all stored collections.
    runtime_handle.block_on(async {
        for collection in toc.all_collections().await {
            log::debug!("Loaded collection: {}", collection);
        }
    });

    let toc_arc = Arc::new(toc);
    let storage_path = toc_arc.storage_path();

    // Holder for all actively running threads of the service: web, gPRC, consensus, etc.
    let mut handles: Vec<JoinHandle<Result<(), Error>>> = vec![];

    // Router for external queries.
    // It decides if query should go directly to the ToC or through the consensus.
    let mut dispatcher = Dispatcher::new(toc_arc.clone());

    let (telemetry_collector, dispatcher_arc) = if settings.cluster.enabled {
        let consensus_state: ConsensusStateRef = ConsensusState::new(
            persistent_consensus_state,
            toc_arc.clone(),
            propose_operation_sender.unwrap(),
            storage_path,
        )
        .into();
        let is_new_deployment = consensus_state.is_new_deployment();

        dispatcher = dispatcher.with_consensus(consensus_state.clone());

        let dispatcher_arc = Arc::new(dispatcher);

        // Monitoring and telemetry.
        let telemetry_collector = TelemetryCollector::new(settings.clone(), dispatcher_arc.clone());
        let tonic_telemetry_collector = telemetry_collector.tonic_telemetry_collector.clone();

        // `raft` crate uses `slog` crate so it is needed to use `slog_stdlog::StdLog` to forward
        // logs from it to `log` crate
        let slog_logger = slog::Logger::root(slog_stdlog::StdLog.fuse(), slog::o!());

        // Runs raft consensus in a separate thread.
        // Create a pipe `message_sender` to communicate with the consensus
        let p2p_port = settings.cluster.p2p.port.expect("P2P port is not set");

        let handle = Consensus::run(
            &slog_logger,
            consensus_state.clone(),
            args.bootstrap,
            args.uri.map(|uri| uri.to_string()),
            settings.service.host.clone(),
            p2p_port,
            settings.cluster.consensus.clone(),
            channel_service,
            propose_receiver,
            tonic_telemetry_collector,
            toc_arc.clone(),
        )
        .expect("Can't initialize consensus");

        handles.push(handle);

        let toc_arc_clone = toc_arc.clone();
        let consensus_state_clone = consensus_state.clone();
        let _cancel_transfer_handle = runtime_handle.spawn(async move {
            consensus_state_clone.is_leader_established.await_ready();
            match toc_arc_clone
                .cancel_outgoing_all_transfers("Source peer restarted")
                .await
            {
                Ok(_) => {
                    log::debug!("All transfers if any cancelled");
                }
                Err(err) => {
                    log::error!("Can't cancel outgoing transfers: {}", err);
                }
            }
        });

        let collections_to_recover_in_consensus = if is_new_deployment {
            let existing_collections = runtime_handle.block_on(toc_arc.all_collections());
            existing_collections
        } else {
            restored_collections
        };

        if !collections_to_recover_in_consensus.is_empty() {
            runtime_handle.spawn(handle_existing_collections(
                toc_arc.clone(),
                consensus_state.clone(),
                dispatcher_arc.clone(),
                consensus_state.this_peer_id(),
                collections_to_recover_in_consensus,
            ));
        }

        (telemetry_collector, dispatcher_arc)
    } else {
        log::info!("Distributed mode disabled");
        let dispatcher_arc = Arc::new(dispatcher);

        // Monitoring and telemetry.
        let telemetry_collector = TelemetryCollector::new(settings.clone(), dispatcher_arc.clone());
        (telemetry_collector, dispatcher_arc)
    };

    let tonic_telemetry_collector = telemetry_collector.tonic_telemetry_collector.clone();

    #[cfg(feature = "web")]
    {
        let dispatcher_arc = dispatcher_arc.clone();
        let telemetry_collector = Arc::new(tokio::sync::Mutex::new(telemetry_collector));
        let settings = settings.clone();
        let handle = thread::Builder::new()
            .name("web".to_string())
            .spawn(move || actix::init(dispatcher_arc.clone(), telemetry_collector, settings))
            .unwrap();
        handles.push(handle);
    }

    if let Some(grpc_port) = settings.service.grpc_port {
        let settings = settings.clone();
        let handle = thread::Builder::new()
            .name("grpc".to_string())
            .spawn(move || {
                tonic::init(
                    dispatcher_arc,
                    tonic_telemetry_collector,
                    settings.service.host,
                    grpc_port,
                )
            })
            .unwrap();
        handles.push(handle);
    } else {
        log::info!("gRPC endpoint disabled");
    }

    #[cfg(feature = "service_debug")]
    {
        use std::fmt::Write;

        use parking_lot::deadlock;

        const DEADLOCK_CHECK_PERIOD: Duration = Duration::from_secs(10);

        thread::Builder::new()
            .name("deadlock_checker".to_string())
            .spawn(move || loop {
                thread::sleep(DEADLOCK_CHECK_PERIOD);
                let deadlocks = deadlock::check_deadlock();
                if deadlocks.is_empty() {
                    continue;
                }

                let mut error = format!("{} deadlocks detected\n", deadlocks.len());
                for (i, threads) in deadlocks.iter().enumerate() {
                    writeln!(error, "Deadlock #{}", i).expect("fail to writeln!");
                    for t in threads {
                        writeln!(
                            error,
                            "Thread Id {:#?}\n{:#?}",
                            t.thread_id(),
                            t.backtrace()
                        )
                        .expect("fail to writeln!");
                    }
                }
                log::error!("{}", error);
            })
            .unwrap();
    }

    for handle in handles.into_iter() {
        handle.join().expect("Couldn't join on the thread")?;
    }
    drop(toc_arc);
    drop(settings);
    Ok(())
}
