#![allow(deprecated)]

#[cfg(all(test))]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::Arc;

    use collection::config::VectorParams;
    use collection::optimizers_builder::OptimizersConfig;
    use segment::types::Distance;
    use storage::content_manager::collection_meta_ops::{
        ChangeAliasesOperation, CollectionMetaOperations, CreateAlias, CreateCollection,
        CreateCollectionOperation, DeleteAlias, RenameAlias,
    };
    use storage::content_manager::consensus::operation_sender::OperationSender;
    use storage::content_manager::toc::TableOfContent;
    use storage::dispatcher::Dispatcher;
    use storage::types::{PerformanceConfig, StorageConfig};
    use tempfile::Builder;
    use tokio::runtime::Runtime;

    #[test]
    fn test_alias_operation() {
        let storage_dir = Builder::new().prefix("storage").tempdir().unwrap();

        let config = StorageConfig {
            storage_path: storage_dir.path().to_str().unwrap().to_string(),
            snapshots_path: storage_dir
                .path()
                .join("snapshots")
                .to_str()
                .unwrap()
                .to_string(),
            on_disk_payload: false,
            optimizers: OptimizersConfig {
                deleted_threshold: 0.5,
                vacuum_min_vector_number: 100,
                default_segment_number: 2,
                max_segment_size: None,
                memmap_threshold: Some(100),
                indexing_threshold: 100,
                flush_interval_sec: 2,
                max_optimization_threads: 2,
            },
            wal: Default::default(),
            performance: PerformanceConfig {
                max_search_threads: 1,
            },
            hnsw_index: Default::default(),
        };

        let runtime = Runtime::new().unwrap();
        let handle = runtime.handle().clone();

        let (propose_sender, _propose_receiver) = std::sync::mpsc::channel();
        let propose_operation_sender = OperationSender::new(propose_sender);

        let toc = Arc::new(TableOfContent::new(
            &config,
            runtime,
            Default::default(),
            0,
            Some(propose_operation_sender),
        ));
        let dispatcher = Dispatcher::new(toc);

        handle
            .block_on(
                dispatcher.submit_collection_meta_op(
                    CollectionMetaOperations::CreateCollection(CreateCollectionOperation::new(
                        "test".to_string(),
                        CreateCollection {
                            vectors: VectorParams {
                                size: NonZeroU64::new(10).unwrap(),
                                distance: Distance::Cosine,
                            }
                            .into(),
                            hnsw_config: None,
                            wal_config: None,
                            optimizers_config: None,
                            shard_number: Some(1),
                            on_disk_payload: None,
                            replication_factor: None,
                            write_consistency_factor: None,
                        },
                    )),
                    None,
                ),
            )
            .unwrap();

        handle
            .block_on(dispatcher.submit_collection_meta_op(
                CollectionMetaOperations::ChangeAliases(ChangeAliasesOperation {
                    actions: vec![CreateAlias {
                            collection_name: "test".to_string(),
                            alias_name: "test_alias".to_string(),
                        }
                        .into()],
                }),
                None,
            ))
            .unwrap();

        handle
            .block_on(dispatcher.submit_collection_meta_op(
                CollectionMetaOperations::ChangeAliases(ChangeAliasesOperation {
                    actions: vec![
                            CreateAlias {
                                collection_name: "test".to_string(),
                                alias_name: "test_alias2".to_string(),
                            }
                            .into(),
                            DeleteAlias {
                                alias_name: "test_alias".to_string(),
                            }
                            .into(),
                            RenameAlias {
                                old_alias_name: "test_alias2".to_string(),
                                new_alias_name: "test_alias3".to_string(),
                            }
                            .into(),
                        ],
                }),
                None,
            ))
            .unwrap();

        let _ = handle
            .block_on(dispatcher.get_collection("test_alias3"))
            .unwrap();
    }
}
