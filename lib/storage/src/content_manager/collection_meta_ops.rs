use collection::config::VectorsConfig;
use collection::operations::config_diff::{
    CollectionParamsDiff, HnswConfigDiff, OptimizersConfigDiff, WalConfigDiff,
};
use collection::shards::replica_set::ReplicaState;
use collection::shards::shard::{PeerId, ShardId};
use collection::shards::transfer::shard_transfer::{ShardTransfer, ShardTransferKey};
use collection::shards::{replica_set, CollectionId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::content_manager::shard_distribution::ShardDistributionProposal;

// *Operation wrapper structure is only required for better OpenAPI generation

/// Create alternative name for a collection.
/// Collection will be available under both names for search, retrieve,
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct CreateAlias {
    pub collection_name: String,
    pub alias_name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct CreateAliasOperation {
    pub create_alias: CreateAlias,
}

/// Delete alias if exists
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct DeleteAlias {
    pub alias_name: String,
}

/// Delete alias if exists
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct DeleteAliasOperation {
    pub delete_alias: DeleteAlias,
}

/// Change alias to a new one
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct RenameAlias {
    pub old_alias_name: String,
    pub new_alias_name: String,
}

/// Change alias to a new one
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct RenameAliasOperation {
    pub rename_alias: RenameAlias,
}

/// Group of all the possible operations related to collection aliases
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
#[serde(untagged)]
pub enum AliasOperations {
    CreateAlias(CreateAliasOperation),
    DeleteAlias(DeleteAliasOperation),
    RenameAlias(RenameAliasOperation),
}

impl From<CreateAlias> for AliasOperations {
    fn from(create_alias: CreateAlias) -> Self {
        AliasOperations::CreateAlias(CreateAliasOperation { create_alias })
    }
}

impl From<DeleteAlias> for AliasOperations {
    fn from(delete_alias: DeleteAlias) -> Self {
        AliasOperations::DeleteAlias(DeleteAliasOperation { delete_alias })
    }
}

impl From<RenameAlias> for AliasOperations {
    fn from(rename_alias: RenameAlias) -> Self {
        AliasOperations::RenameAlias(RenameAliasOperation { rename_alias })
    }
}

/// Operation for creating new collection and (optionally) specify index params
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct CreateCollection {
    /// Vector data config.
    /// It is possible to provide one config for single vector mode and list of configs for multiple vectors mode.
    pub vectors: VectorsConfig,
    /// Number of shards in collection.
    /// Default is 1 for standalone, otherwise equal to the number of nodes
    /// Minimum is 1
    #[serde(default)]
    pub shard_number: Option<u32>,
    /// Number of shards replicas.
    /// Default is 1
    /// Minimum is 1
    #[serde(default)]
    pub replication_factor: Option<u32>,
    /// Defines how many replicas should apply the operation for us to consider it successful.
    /// Increasing this number will make the collection more resilient to inconsistencies, but will
    /// also make it fail if not enough replicas are available.
    /// Does not have any performance impact.
    #[serde(default)]
    pub write_consistency_factor: Option<u32>,
    /// If true - point's payload will not be stored in memory.
    /// It will be read from the disk every time it is requested.
    /// This setting saves RAM by (slightly) increasing the response time.
    /// Note: those payload values that are involved in filtering and are indexed - remain in RAM.
    #[serde(default)]
    pub on_disk_payload: Option<bool>,
    /// Custom params for HNSW index. If none - values from service configuration file are used.
    pub hnsw_config: Option<HnswConfigDiff>,
    /// Custom params for WAL. If none - values from service configuration file are used.
    pub wal_config: Option<WalConfigDiff>,
    /// Custom params for Optimizers.  If none - values from service configuration file are used.
    pub optimizers_config: Option<OptimizersConfigDiff>,
}

/// Operation for creating new collection and (optionally) specify index params
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct CreateCollectionOperation {
    pub collection_name: String,
    pub create_collection: CreateCollection,
    distribution: Option<ShardDistributionProposal>,
}

impl CreateCollectionOperation {
    pub fn new(collection_name: String, create_collection: CreateCollection) -> Self {
        Self {
            collection_name,
            create_collection,
            distribution: None,
        }
    }

    pub fn is_distribution_set(&self) -> bool {
        self.distribution.is_some()
    }

    pub fn take_distribution(&mut self) -> Option<ShardDistributionProposal> {
        self.distribution.take()
    }

    pub fn set_distribution(&mut self, distribution: ShardDistributionProposal) {
        self.distribution = Some(distribution);
    }
}

/// Operation for updating parameters of the existing collection
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct UpdateCollection {
    /// Custom params for Optimizers.  If none - values from service configuration file are used.
    /// This operation is blocking, it will only proceed ones all current optimizations are complete
    pub optimizers_config: Option<OptimizersConfigDiff>, // ToDo: Allow updates for other configuration params as well
    /// Collection base params.  If none - values from service configuration file are used.
    pub params: Option<CollectionParamsDiff>,
}

/// Operation for updating parameters of the existing collection
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct UpdateCollectionOperation {
    pub collection_name: String,
    pub update_collection: UpdateCollection,
    shard_replica_changes: Option<Vec<replica_set::Change>>,
}

impl UpdateCollectionOperation {
    pub fn new_empty(collection_name: String) -> Self {
        Self {
            collection_name,
            update_collection: UpdateCollection {
                optimizers_config: None,
                params: None,
            },
            shard_replica_changes: None,
        }
    }

    pub fn new(collection_name: String, update_collection: UpdateCollection) -> Self {
        Self {
            collection_name,
            update_collection,
            shard_replica_changes: None,
        }
    }

    pub fn take_shard_replica_changes(&mut self) -> Option<Vec<replica_set::Change>> {
        self.shard_replica_changes.take()
    }

    pub fn set_shard_replica_changes(&mut self, changes: Vec<replica_set::Change>) {
        if changes.is_empty() {
            self.shard_replica_changes = None;
        } else {
            self.shard_replica_changes = Some(changes);
        }
    }
}

/// Operation for performing changes of collection aliases.
/// Alias changes are atomic, meaning that no collection modifications can happen between
/// alias operations.
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct ChangeAliasesOperation {
    pub actions: Vec<AliasOperations>,
}

/// Operation for deleting collection with given name
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub struct DeleteCollectionOperation(pub String);

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Clone)]
pub enum ShardTransferOperations {
    Start(ShardTransfer),
    Finish(ShardTransfer),
    Abort {
        transfer: ShardTransferKey,
        reason: String,
    },
}

/// Sets the state of shard replica
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Clone)]
pub struct SetShardReplicaState {
    pub collection_name: String,
    pub shard_id: ShardId,
    pub peer_id: PeerId,
    /// If `Active` then the replica is up to date and can receive updates and answer requests
    pub state: ReplicaState,
}

/// Enumeration of all possible collection update operations
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub enum CollectionMetaOperations {
    CreateCollection(CreateCollectionOperation),
    UpdateCollection(UpdateCollectionOperation),
    DeleteCollection(DeleteCollectionOperation),
    ChangeAliases(ChangeAliasesOperation),
    TransferShard(CollectionId, ShardTransferOperations),
    SetShardReplicaState(SetShardReplicaState),
    Nop { token: usize }, // Empty operation
}
