use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use collection::collection_state;
use collection::shards::shard::PeerId;
use collection::shards::CollectionId;
use futures::future::join_all;
use parking_lot::{Mutex, RwLock};
use raft::eraftpb::{ConfChangeType, ConfChangeV2, Entry as RaftEntry};
use raft::{GetEntriesContext, RaftState, RawNode, SoftState, Storage};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::sync::oneshot::Receiver;
use tokio::time::error::Elapsed;
use tonic::transport::Uri;

use super::alias_mapping::AliasMapping;
use super::consensus_ops::ConsensusOperations;
use super::errors::StorageError;
use super::CollectionContainer;
use crate::content_manager::consensus::consensus_wal::ConsensusOpWal;
use crate::content_manager::consensus::entry_queue::EntryId;
use crate::content_manager::consensus::is_ready::IsReady;
use crate::content_manager::consensus::operation_sender::OperationSender;
use crate::content_manager::consensus::persistent::Persistent;
use crate::types::{
    ClusterInfo, ClusterStatus, ConsensusThreadStatus, MessageSendErrors, PeerAddressById,
    PeerInfo, RaftInfo,
};
use crate::CollectionMetaOperations;

pub const DEFAULT_META_OP_WAIT: Duration = Duration::from_secs(10);

pub mod prelude {
    use crate::content_manager::toc::TableOfContent;

    pub type ConsensusState = super::ConsensusState<TableOfContent>;
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SnapshotData {
    pub collections_data: CollectionsSnapshot,
    #[serde(with = "crate::serialize_peer_addresses")]
    pub address_by_id: PeerAddressById,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct CollectionsSnapshot {
    pub collections: HashMap<CollectionId, collection_state::State>,
    pub aliases: AliasMapping,
}

impl TryFrom<&[u8]> for SnapshotData {
    type Error = serde_cbor::Error;

    fn try_from(bytes: &[u8]) -> Result<SnapshotData, Self::Error> {
        serde_cbor::from_slice(bytes)
    }
}

pub struct ConsensusState<C: CollectionContainer> {
    pub persistent: RwLock<Persistent>,
    pub is_leader_established: Arc<IsReady>,
    wal: Mutex<ConsensusOpWal>,
    soft_state: RwLock<Option<SoftState>>,
    toc: Arc<C>,
    on_consensus_op_apply:
        Mutex<HashMap<ConsensusOperations, oneshot::Sender<Result<bool, StorageError>>>>,
    propose_sender: OperationSender,
    first_voter: RwLock<Option<PeerId>>,
    consensus_thread_status: RwLock<ConsensusThreadStatus>,
    message_send_failures: RwLock<HashMap<String, MessageSendErrors>>,
}

impl<C: CollectionContainer> ConsensusState<C> {
    pub fn new(
        persistent_state: Persistent,
        toc: Arc<C>,
        propose_sender: OperationSender,
        storage_path: &str,
    ) -> Self {
        Self {
            persistent: RwLock::new(persistent_state),
            is_leader_established: Arc::new(IsReady::default()),
            wal: Mutex::new(ConsensusOpWal::new(storage_path)),
            soft_state: RwLock::new(None),
            toc,
            on_consensus_op_apply: Default::default(),
            propose_sender,
            first_voter: Default::default(),
            consensus_thread_status: RwLock::new(ConsensusThreadStatus::Working {
                last_update: Utc::now(),
            }),
            message_send_failures: Default::default(),
        }
    }

    pub fn record_message_send_failure<E: Error>(&self, peer_address: &Uri, error: E) {
        let mut message_send_failures = self.message_send_failures.write();
        let mut entry = message_send_failures
            .entry(peer_address.to_string())
            .or_insert_with(Default::default);
        // Log only first error
        if entry.count == 0 {
            log::warn!("Failed to send message to {peer_address} with error: {error}")
        }
        entry.count += 1;
        entry.latest_error = Some(error.to_string());
    }

    pub fn record_message_send_success(&self, peer_address: &Uri) {
        self.message_send_failures
            .write()
            .remove(&peer_address.to_string());
    }

    pub fn record_consensus_working(&self) {
        *self.consensus_thread_status.write() = ConsensusThreadStatus::Working {
            last_update: Utc::now(),
        }
    }

    pub fn on_consensus_stopped(&self) {
        *self.consensus_thread_status.write() = ConsensusThreadStatus::Stopped
    }

    pub fn on_consensus_thread_err<E: Display>(&self, err: E) {
        *self.consensus_thread_status.write() = ConsensusThreadStatus::StoppedWithErr {
            err: err.to_string(),
        }
    }

    pub fn set_raft_soft_state(&self, state: &SoftState) {
        *self.soft_state.write() = Some(SoftState { ..*state });
    }

    pub fn this_peer_id(&self) -> PeerId {
        self.persistent.read().this_peer_id
    }

    pub fn first_voter(&self) -> PeerId {
        match self.first_voter.read().as_ref() {
            Some(id) => *id,
            None => self.this_peer_id(),
        }
    }

    pub fn set_first_voter(&self, id: PeerId) {
        *self.first_voter.write() = Some(id);
    }

    pub fn cluster_status(&self) -> ClusterStatus {
        let persistent = self.persistent.read();
        let hard_state = &persistent.state.hard_state;
        let peers = persistent
            .peer_address_by_id()
            .into_iter()
            .map(|(peer_id, uri)| {
                (
                    peer_id,
                    PeerInfo {
                        uri: uri.to_string(),
                    },
                )
            })
            .collect();
        let pending_operations = persistent.unapplied_entities_count();
        let soft_state = self.soft_state.read();
        let leader = soft_state.as_ref().map(|state| state.leader_id);
        let role = soft_state.as_ref().map(|state| state.raft_state.into());
        let peer_id = persistent.this_peer_id;
        let is_voter = persistent.state.conf_state.get_voters().contains(&peer_id);
        ClusterStatus::Enabled(ClusterInfo {
            peer_id,
            peers,
            raft_info: RaftInfo {
                term: hard_state.term,
                commit: hard_state.commit,
                pending_operations,
                leader,
                role,
                is_voter,
            },
            consensus_thread_status: self.consensus_thread_status.read().clone(),
            message_send_failures: self.message_send_failures.read().clone(),
        })
    }

    /// Handle peer removal operation.
    ///
    /// 1. Try to remove peer
    /// 2. Handle peer removal error
    /// 3. Report to the listeners
    ///
    /// Return if consensus should be stopped.
    pub fn on_peer_remove(&self, peer_id: PeerId) -> Result<bool, StorageError> {
        let mut stop_consensus: bool = false;

        let report = match self.remove_peer(peer_id) {
            Ok(()) => {
                if self.this_peer_id() == peer_id {
                    stop_consensus = true;
                }
                Ok(true)
            }
            Err(err) => match err {
                err @ StorageError::ServiceError { .. } => {
                    return Err(err);
                }
                _ => Err(err),
            },
        };
        let operation = ConsensusOperations::RemovePeer(peer_id);
        let on_apply = self.on_consensus_op_apply.lock().remove(&operation);
        if let Some(on_apply) = on_apply {
            if on_apply.send(report).is_err() {
                log::warn!("Failed to notify on consensus operation completion: channel receiver is dropped")
            }
        }
        Ok(stop_consensus)
    }

    pub fn apply_conf_change_entry<T: Storage>(
        &self,
        entry: &RaftEntry,
        raw_node: &mut RawNode<T>,
    ) -> Result<bool, StorageError> {
        let change: ConfChangeV2 = prost::Message::decode(entry.get_data())?;

        let conf_state = raw_node.apply_conf_change(&change)?;
        log::debug!("Applied conf state {:?}", conf_state);
        self.persistent
            .write()
            .apply_state_update(|state| state.conf_state = conf_state)?;

        let mut stop_consensus: bool = false;
        for single_change in &change.changes {
            match single_change.change_type() {
                ConfChangeType::AddNode => {
                    debug_assert!(
                        self.peer_address_by_id()
                            .get(&single_change.node_id)
                            .is_some(),
                        "Peer should be already known"
                    )
                }
                ConfChangeType::RemoveNode => {
                    log::debug!("Removing node {}", single_change.node_id);
                    stop_consensus |= self.on_peer_remove(single_change.node_id)?;
                }
                ConfChangeType::AddLearnerNode => {
                    log::debug!("Adding learner node {}", single_change.node_id);
                    if let Ok(peer_uri) = String::from_utf8_lossy(entry.get_context())
                        .deref()
                        .try_into()
                    {
                        let peer_uri: Uri = peer_uri;
                        self.add_peer(single_change.node_id, peer_uri.clone())?;
                        let operation = ConsensusOperations::AddPeer {
                            peer_id: single_change.node_id,
                            uri: peer_uri.to_string(),
                        };
                        let on_apply = self.on_consensus_op_apply.lock().remove(&operation);
                        if let Some(on_apply) = on_apply {
                            if on_apply.send(Ok(true)).is_err() {
                                log::warn!("Failed to notify on consensus operation completion: channel receiver is dropped")
                            }
                        }
                    } else if entry.get_context().is_empty() {
                        // Allow empty context for compatibility
                        log::warn!(
                            "Outdated peer addition entry found with index: {}",
                            entry.get_index()
                        )
                    } else {
                        // Should not be reachable as it is checked in API
                        return Err(StorageError::ServiceError {
                            description: "Failed to parse peer uri".to_string(),
                        });
                    }
                }
            }
        }
        Ok(stop_consensus)
    }

    pub fn set_unapplied_entries(
        &self,
        first_index: EntryId,
        last_index: EntryId,
    ) -> Result<(), raft::Error> {
        self.persistent
            .write()
            .set_unapplied_entries(first_index, last_index)
            .map_err(raft_error_other)
    }

    /// Return `true` if consensus should be stopped.
    pub fn apply_entries<T: Storage>(&self, raw_node: &mut RawNode<T>) -> bool {
        use raft::eraftpb::EntryType;

        if let Err(err) = self.persistent.write().save_if_dirty() {
            log::error!("Failed to save new state of applied entries queue: {err}");
            return true;
        }

        loop {
            let unapplied_index = self.persistent.read().current_unapplied_entry();
            let entry_index = match unapplied_index {
                Some(index) => index,
                None => break,
            };
            log::debug!("Applying committed entry with index {entry_index}");
            let entry = match self.wal.lock().entry(entry_index) {
                Ok(entry) => entry,
                Err(err) => {
                    log::error!("Failed to get entry at index {entry_index}: {err}");
                    break;
                }
            };
            let stop_consensus: bool = if entry.data.is_empty() {
                // Empty entry, when the peer becomes Leader it will send an empty entry.
                false
            } else {
                match entry.get_entry_type() {
                    EntryType::EntryNormal => {
                        let operation_result = self.apply_normal_entry(&entry);
                        match operation_result {
                            Ok(result) => {
                                log::debug!(
                                    "Successfully applied consensus operation entry. Index: {}. Result: {result}",
                                    entry.index);
                                false
                            }
                            Err(err @ StorageError::ServiceError { .. }) => {
                                log::error!("Failed to apply collection meta operation entry with service error: {err}");
                                // This is a service error - stop consensus. Peer can be restarted when the problem is fixed.
                                true
                            }
                            Err(err) => {
                                log::warn!("Failed to apply collection meta operation entry with user error: {err}");
                                // This is a user error so we can safely consider it applied but with error as it was incorrect.
                                false
                            }
                        }
                    }
                    EntryType::EntryConfChangeV2 => {
                        match self.apply_conf_change_entry(&entry, raw_node) {
                            Ok(stop_consensus) => {
                                log::trace!(
                                    "Successfully applied configuration change entry. Index: {}.",
                                    entry.index
                                );
                                stop_consensus
                            }
                            Err(err) => {
                                log::error!(
                                    "Failed to apply configuration change entry with error: {err}"
                                );
                                true
                            }
                        }
                    }
                    ty => {
                        log::error!("Failed to apply entry: unsupported entry type {ty:?}");
                        true
                    }
                }
            };
            if stop_consensus {
                return stop_consensus;
            }
            if let Err(err) = self.persistent.write().entry_applied() {
                log::error!("Failed to save new state of applied entries queue: {err}");
                return true;
            }
        }
        false // do not stop consensus
    }

    pub fn apply_normal_entry(&self, entry: &RaftEntry) -> Result<bool, StorageError> {
        let operation: ConsensusOperations = entry.try_into()?;
        let on_apply = self.on_consensus_op_apply.lock().remove(&operation);
        let result = if let ConsensusOperations::CollectionMeta(operation) = operation {
            self.toc.perform_collection_meta_op(*operation)
        } else {
            // RemovePeer or AddPeer should be converted into native ConfChangeV2 message before sending to the Raft.
            // So we do not expect to receive these operations as a normal entry.
            // This is a debug assert so production migrations should be ok.
            // TODO: parse into CollectionMetaOperation as we will not handle other cases here, but this removes compatibility with previous entry storage
            debug_assert!(
                false,
                "Do not expect RemovePeer or AddPeer to be directly proposed"
            );
            Ok(false)
        };
        if let Some(on_apply) = on_apply {
            if on_apply.send(result.clone()).is_err() {
                log::warn!("Failed to notify on consensus operation completion: channel receiver is dropped")
            }
        }
        result
    }

    pub fn apply_snapshot(&self, snapshot: &raft::eraftpb::Snapshot) -> Result<(), StorageError> {
        let meta = snapshot.get_metadata();
        if raft::Storage::first_index(self)? > meta.index {
            return Err(StorageError::ServiceError {
                description: "Snapshot out of date".to_string(),
            });
        }
        let data: SnapshotData = snapshot.get_data().try_into()?;
        self.toc.apply_collections_snapshot(data.collections_data)?;
        self.wal.lock().clear()?;
        self.persistent
            .write()
            .update_from_snapshot(meta, data.address_by_id)?;
        Ok(())
    }

    pub fn set_hard_state(&self, hard_state: raft::eraftpb::HardState) -> Result<(), StorageError> {
        self.persistent
            .write()
            .apply_state_update(move |state| state.hard_state = hard_state)
    }

    pub fn set_conf_state(&self, conf_state: raft::eraftpb::ConfState) -> Result<(), StorageError> {
        self.persistent
            .write()
            .apply_state_update(move |state| state.conf_state = conf_state)
    }

    /// Check if the consensus have empty operations log
    pub fn is_new_deployment(&self) -> bool {
        self.hard_state().term == 0
    }

    pub fn hard_state(&self) -> raft::eraftpb::HardState {
        self.persistent.read().state().hard_state.clone()
    }

    pub fn conf_state(&self) -> raft::eraftpb::ConfState {
        self.persistent.read().state().conf_state.clone()
    }

    pub fn set_commit_index(&self, index: u64) -> Result<(), StorageError> {
        self.persistent
            .write()
            .apply_state_update(|state| state.hard_state.commit = index)
    }

    pub fn add_peer(&self, peer_id: PeerId, uri: Uri) -> Result<(), StorageError> {
        self.persistent.write().insert_peer(peer_id, uri)
    }

    pub fn remove_peer(&self, peer_id: PeerId) -> Result<(), StorageError> {
        // We sincerely apologize for this piece of code.
        // The `id_to_address` is shared between `channel_pool` and `persistent`,
        // plus we need to make additional removing in the `channel_pool`.
        // So we handle `remove_peer` inside the `toc` and persist changes in the `persistent` after that.
        self.toc.remove_peer(peer_id)?;
        self.persistent.read().save()
    }

    pub async fn propose_consensus_op(
        &self,
        operation: ConsensusOperations,
    ) -> Result<(), StorageError> {
        self.is_leader_established.await_ready();
        self.propose_sender.send(operation)
    }

    async fn await_receiver(
        receiver: Receiver<Result<bool, StorageError>>,
        wait_timeout: Duration,
    ) -> Result<bool, StorageError> {
        tokio::time::timeout(wait_timeout, receiver)
            .await
            .map_err(
                |_: tokio::time::error::Elapsed| StorageError::ServiceError {
                    description: format!(
                        "Waiting for consensus operation commit failed. Timeout set at: {} seconds",
                        wait_timeout.as_secs_f64()
                    ),
                },
                // ??? - forwards 2 possible errors: sender dropped, operation failed
            )??
    }

    pub fn await_for_multiple_operations(
        &self,
        operations: Vec<ConsensusOperations>,
        wait_timeout: Option<Duration>,
    ) -> impl Future<Output = Result<Result<(), StorageError>, Elapsed>> {
        let mut receivers = vec![];
        for operation in operations {
            let (sender, receiver) = oneshot::channel();
            self.on_consensus_op_apply.lock().insert(operation, sender);
            receivers.push(receiver);
        }

        async move {
            let await_for_all = join_all(receivers);
            let results =
                tokio::time::timeout(wait_timeout.unwrap_or(DEFAULT_META_OP_WAIT), await_for_all)
                    .await?;
            for result in results {
                match result {
                    Ok(response_res) => match response_res {
                        Ok(_) => {}
                        Err(err) => return Ok(Err(err)),
                    },
                    Err(recv_error) => return Ok(Err(recv_error.into())),
                }
            }
            Ok(Ok(()))
        }
    }

    pub async fn propose_consensus_op_with_await(
        &self,
        operation: ConsensusOperations,
        wait_timeout: Option<Duration>,
        with_confirmation: bool,
    ) -> Result<bool, StorageError> {
        let wait_timeout = wait_timeout.unwrap_or(DEFAULT_META_OP_WAIT);

        if !self
            .is_leader_established
            .await_ready_for_timeout(wait_timeout)
        {
            return Err(StorageError::service_error(&format!(
                "Failed to propose operation: leader is not established within {} secs",
                wait_timeout.as_secs()
            )));
        }

        let (sender, receiver) = oneshot::channel();
        {
            let mut on_apply_lock = self.on_consensus_op_apply.lock();
            self.propose_sender.send(operation.clone())?;
            on_apply_lock.insert(operation, sender);
        }
        let res = Self::await_receiver(receiver, wait_timeout).await?;

        // Send explicit empty operation to ensure that all previous operations are applied.
        // Assume that peer can not accept new operations until it applies all previous ones.
        if with_confirmation {
            let random_token: usize = rand::random();
            // Send empty operation to make sure that the majority of consensus applied the operation
            let empty_operation =
                ConsensusOperations::CollectionMeta(Box::new(CollectionMetaOperations::Nop {
                    token: random_token,
                }));
            let (sender, receiver) = oneshot::channel();
            {
                let mut on_apply_lock = self.on_consensus_op_apply.lock();
                self.propose_sender.send(empty_operation.clone())?;
                on_apply_lock.insert(empty_operation, sender);
            }
            Self::await_receiver(receiver, wait_timeout).await?;
        }

        Ok(res)
    }

    pub fn peer_address_by_id(&self) -> PeerAddressById {
        self.persistent.read().peer_address_by_id()
    }

    pub fn peer_count(&self) -> usize {
        self.persistent.read().peer_address_by_id.read().len()
    }

    pub fn append_entries(&self, entries: Vec<RaftEntry>) -> Result<(), StorageError> {
        self.wal.lock().append_entries(entries)
    }

    pub fn last_applied_entry(&self) -> Option<u64> {
        self.persistent.read().last_applied_entry()
    }
}

impl<C: CollectionContainer> Storage for ConsensusState<C> {
    fn initial_state(&self) -> raft::Result<RaftState> {
        Ok(self.persistent.read().state.clone())
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: GetEntriesContext,
    ) -> raft::Result<Vec<RaftEntry>> {
        let max_size: Option<_> = max_size.into();
        if low < self.first_index()? {
            return Err(raft::Error::Store(raft::StorageError::Compacted));
        }

        if high > self.last_index()? + 1 {
            panic!(
                "index out of bound (last: {}, high: {})",
                self.last_index()? + 1,
                high
            );
        }
        self.wal.lock().entries(low, high, max_size)
    }

    fn term(&self, idx: u64) -> raft::Result<u64> {
        let persistent = self.persistent.read();
        let snapshot_meta = persistent.latest_snapshot_meta();
        if idx == snapshot_meta.index {
            return Ok(snapshot_meta.term);
        }
        Ok(self.wal.lock().entry(idx)?.term)
    }

    fn first_index(&self) -> raft::Result<u64> {
        let index = match self.wal.lock().first_entry().map_err(raft_error_other)? {
            Some(entry) => entry.index,
            None => self.persistent.read().latest_snapshot_meta().index + 1,
        };
        Ok(index)
    }

    fn last_index(&self) -> raft::Result<u64> {
        let index = match self.wal.lock().last_entry().map_err(raft_error_other)? {
            Some(entry) => entry.index,
            None => self.persistent.read().latest_snapshot_meta().index,
        };
        Ok(index)
    }

    fn snapshot(&self, request_index: u64, _to: u64) -> raft::Result<raft::eraftpb::Snapshot> {
        let collections_data = self.toc.collections_snapshot();
        let persistent = self.persistent.read();
        let raft_state = persistent.state().clone();
        if raft_state.hard_state.commit >= request_index {
            let snapshot = SnapshotData {
                collections_data,
                address_by_id: persistent.peer_address_by_id(),
            };
            Ok(raft::eraftpb::Snapshot {
                data: serde_cbor::to_vec(&snapshot).map_err(raft_error_other)?,
                metadata: Some(raft::eraftpb::SnapshotMetadata {
                    conf_state: Some(raft_state.conf_state),
                    index: raft_state.hard_state.commit,
                    term: raft_state.hard_state.term,
                }),
            })
        } else {
            Err(raft::Error::Store(
                raft::StorageError::SnapshotTemporarilyUnavailable,
            ))
        }
    }
}

#[derive(Clone)]
pub struct ConsensusStateRef(pub Arc<prelude::ConsensusState>);

impl Deref for ConsensusStateRef {
    type Target = prelude::ConsensusState;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

impl From<prelude::ConsensusState> for ConsensusStateRef {
    fn from(state: prelude::ConsensusState) -> Self {
        Self(Arc::new(state))
    }
}

impl Storage for ConsensusStateRef {
    fn initial_state(&self) -> raft::Result<RaftState> {
        self.0.initial_state()
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        context: GetEntriesContext,
    ) -> raft::Result<Vec<RaftEntry>> {
        self.0.entries(low, high, max_size, context)
    }

    fn term(&self, idx: u64) -> raft::Result<u64> {
        self.0.term(idx)
    }

    fn first_index(&self) -> raft::Result<EntryId> {
        self.0.first_index()
    }

    fn last_index(&self) -> raft::Result<EntryId> {
        self.0.last_index()
    }

    fn snapshot(&self, request_index: u64, to: u64) -> raft::Result<raft::eraftpb::Snapshot> {
        self.0.snapshot(request_index, to)
    }
}

pub fn raft_error_other(e: impl std::error::Error) -> raft::Error {
    #[derive(thiserror::Error, Debug)]
    #[error("{0}")]
    struct StrError(String);

    raft::Error::Store(raft::StorageError::Other(Box::new(StrError(e.to_string()))))
}

#[cfg(test)]
mod tests {
    use std::sync::{mpsc, Arc};

    use collection::shards::shard::PeerId;
    use proptest::prelude::*;
    use raft::eraftpb::Entry;
    use raft::storage::{MemStorage, Storage};
    use tempfile::Builder;

    use super::ConsensusState;
    use crate::content_manager::consensus::consensus_wal::ConsensusOpWal;
    use crate::content_manager::consensus::entry_queue::EntryApplyProgressQueue;
    use crate::content_manager::consensus::operation_sender::OperationSender;
    use crate::content_manager::consensus::persistent::Persistent;
    use crate::content_manager::CollectionContainer;

    #[test]
    fn update_is_applied() {
        let dir = Builder::new().prefix("raft_state_test").tempdir().unwrap();
        let mut state = Persistent::load_or_init(dir.path(), false).unwrap();
        assert_eq!(state.state().hard_state.commit, 0);
        state
            .apply_state_update(|state| state.hard_state.commit = 1)
            .unwrap();
        assert_eq!(state.state().hard_state.commit, 1);
    }

    #[test]
    fn save_failure() {
        let mut state = Persistent {
            path: "./unexistent_dir/file".into(),
            ..Default::default()
        };
        assert!(state
            .apply_state_update(|state| { state.hard_state.commit = 1 })
            .is_err());
    }

    #[test]
    fn state_is_loaded() {
        let dir = Builder::new().prefix("raft_state_test").tempdir().unwrap();
        let mut state = Persistent::load_or_init(dir.path(), false).unwrap();
        state
            .apply_state_update(|state| state.hard_state.commit = 1)
            .unwrap();
        assert_eq!(state.state().hard_state.commit, 1);

        let state_loaded = Persistent::load_or_init(dir.path(), false).unwrap();
        assert_eq!(state_loaded.state().hard_state.commit, 1);
    }

    #[test]
    fn unapplied_entries() {
        let mut entries = EntryApplyProgressQueue::new(0, 2);
        assert_eq!(entries.current(), Some(0));
        assert_eq!(entries.len(), 3);
        entries.applied();
        assert_eq!(entries.current(), Some(1));
        assert_eq!(entries.len(), 2);
        entries.applied();
        assert_eq!(entries.current(), Some(2));
        assert_eq!(entries.len(), 1);
        entries.applied();
        assert_eq!(entries.current(), None);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn correct_entry_with_offset() {
        let dir = Builder::new().prefix("raft_state_test").tempdir().unwrap();
        let mut wal = ConsensusOpWal::new(dir.path().to_str().unwrap());
        wal.append_entries(vec![Entry {
            index: 4,
            ..Default::default()
        }])
        .unwrap();
        wal.append_entries(vec![Entry {
            index: 5,
            ..Default::default()
        }])
        .unwrap();
        wal.append_entries(vec![Entry {
            index: 6,
            ..Default::default()
        }])
        .unwrap();
        assert_eq!(wal.entry(5).unwrap().index, 5)
    }

    #[test]
    fn at_least_1_entry() {
        let dir = Builder::new().prefix("raft_state_test").tempdir().unwrap();
        let mut wal = ConsensusOpWal::new(dir.path().to_str().unwrap());
        wal.append_entries(vec![
            Entry {
                index: 4,
                ..Default::default()
            },
            Entry {
                index: 5,
                ..Default::default()
            },
        ])
        .unwrap();
        // Even when `max_size` is `0` this fn should return at least 1 entry
        assert_eq!(wal.entries(4, 5, Some(0)).unwrap().len(), 1)
    }

    struct NoCollections;

    impl CollectionContainer for NoCollections {
        fn perform_collection_meta_op(
            &self,
            _operation: crate::content_manager::collection_meta_ops::CollectionMetaOperations,
        ) -> Result<bool, crate::content_manager::errors::StorageError> {
            Ok(true)
        }

        fn collections_snapshot(&self) -> super::CollectionsSnapshot {
            super::CollectionsSnapshot::default()
        }

        fn apply_collections_snapshot(
            &self,
            _data: super::CollectionsSnapshot,
        ) -> Result<(), crate::content_manager::errors::StorageError> {
            Ok(())
        }

        fn remove_peer(
            &self,
            _peer_id: PeerId,
        ) -> Result<(), crate::content_manager::errors::StorageError> {
            Ok(())
        }
    }

    fn setup_storages(
        entries: Vec<Entry>,
        path: &std::path::Path,
    ) -> (ConsensusState<NoCollections>, MemStorage) {
        let persistent = Persistent::load_or_init(path, true).unwrap();
        let (sender, _) = mpsc::channel();
        let consensus_state = ConsensusState::new(
            persistent,
            Arc::new(NoCollections),
            OperationSender::new(sender),
            path.to_str().unwrap(),
        );
        let mem_storage = MemStorage::new();
        mem_storage.wl().append(entries.as_ref()).unwrap();
        consensus_state.append_entries(entries).unwrap();
        (consensus_state, mem_storage)
    }

    prop_compose! {
        fn gen_entries(min_entries: u64, max_entries: u64)(n in min_entries..max_entries, inc_term_every in 1u64..max_entries) -> Vec<Entry> {
            (1..(n+1)).into_iter().map(|index| Entry {index, term: 1 + index/inc_term_every, ..Default::default()}).collect::<Vec<Entry>>()
        }
    }

    proptest! {
        #[test]
        fn check_first_and_last_indexes(entries in gen_entries(0, 100)) {
            let dir = Builder::new().prefix("raft_state_test").tempdir().unwrap();
            let (consensus_state, mem_storage) = setup_storages(entries, dir.path());
            prop_assert_eq!(mem_storage.last_index(), consensus_state.last_index());
            prop_assert_eq!(mem_storage.first_index(), consensus_state.first_index());
        }

        #[test]
        fn check_term(entries in gen_entries(0, 100), id in 0u64..100) {
            let dir = Builder::new().prefix("raft_state_test").tempdir().unwrap();
            let (consensus_state, mem_storage) = setup_storages(entries, dir.path());
            prop_assert_eq!(mem_storage.term(id), consensus_state.term(id))
        }

        #[test]
        fn check_entries(entries in gen_entries(1, 100),
                low in 0u64..100,
                len in 1u64..100,
                max_size in proptest::option::of(proptest::num::u64::ANY)
            ) {
            let dir = Builder::new().prefix("raft_state_test").tempdir().unwrap();
            let (consensus_state, mem_storage) = setup_storages(entries, dir.path());
            let mut high = low + len;
            let last_index = mem_storage.last_index().unwrap();
            if high > last_index + 1 {
                high = last_index + 1;
            }
            let mut low = low;
            if low > last_index {
                low = last_index;
            }
            let context_1 = raft::storage::GetEntriesContext::empty(false);
            let context_2 = raft::storage::GetEntriesContext::empty(false);
            prop_assert_eq!(mem_storage.entries(low, high, max_size, context_1), consensus_state.entries(low, high, max_size, context_2));
        }
    }
}
