use std::collections::HashMap;
use std::io::Error as IoError;
use std::path::{Path, PathBuf};
use std::result;

use atomicwrites::Error as AtomicIoError;
use rayon::ThreadPoolBuildError;
use thiserror::Error;

use crate::common::file_operations::FileStorageError;
use crate::data_types::named_vectors::NamedVectors;
use crate::data_types::vectors::VectorElementType;
use crate::index::field_index::CardinalityEstimation;
use crate::telemetry::SegmentTelemetry;
use crate::types::{
    Filter, Payload, PayloadFieldSchema, PayloadKeyType, PayloadKeyTypeRef, PointIdType,
    ScoredPoint, SearchParams, SegmentConfig, SegmentInfo, SegmentType, SeqNumberType, WithPayload,
    WithVector,
};

#[derive(Error, Debug, Clone)]
#[error("{0}")]
pub enum OperationError {
    #[error("Vector inserting error: expected dim: {expected_dim}, got {received_dim}")]
    WrongVector {
        expected_dim: usize,
        received_dim: usize,
    },
    #[error("Not existing vector name error: {received_name}")]
    VectorNameNotExists { received_name: String },
    #[error("Missed vector name error: {received_name}")]
    MissedVectorName { received_name: String },
    #[error("No point with id {missed_point_id} found")]
    PointIdError { missed_point_id: PointIdType },
    #[error("Payload type does not match with previously given for field {field_name}. Expected: {expected_type}")]
    TypeError {
        field_name: PayloadKeyType,
        expected_type: String,
    },
    #[error("Unable to infer type for the field '{field_name}'. Please specify `field_type`")]
    TypeInferenceError { field_name: PayloadKeyType },
    /// Service Error prevents further update of the collection until it is fixed.
    /// Should only be used for hardware, data corruption, IO, or other unexpected internal errors.
    #[error("Service runtime error: {description}")]
    ServiceError { description: String },
    #[error("Operation cancelled: {description}")]
    Cancelled { description: String },
}

impl OperationError {
    pub fn service_error(description: &str) -> OperationError {
        OperationError::ServiceError {
            description: description.to_string(),
        }
    }
}

/// Contains information regarding last operation error, which should be fixed before next operation could be processed
#[derive(Debug, Clone)]
pub struct SegmentFailedState {
    pub version: SeqNumberType,
    pub point_id: Option<PointIdType>,
    pub error: OperationError,
}

impl From<semver::Error> for OperationError {
    fn from(error: semver::Error) -> Self {
        OperationError::ServiceError {
            description: error.to_string(),
        }
    }
}

impl From<ThreadPoolBuildError> for OperationError {
    fn from(error: ThreadPoolBuildError) -> Self {
        OperationError::ServiceError {
            description: format!("{}", error),
        }
    }
}

impl From<FileStorageError> for OperationError {
    fn from(err: FileStorageError) -> Self {
        match err {
            FileStorageError::IoError { description } => {
                OperationError::service_error(&format!("IO Error: {}", description))
            }
            FileStorageError::UserAtomicIoError => {
                OperationError::service_error("Unknown atomic write error")
            }
            FileStorageError::GenericError { description } => {
                OperationError::service_error(&description)
            }
        }
    }
}

impl From<serde_cbor::Error> for OperationError {
    fn from(err: serde_cbor::Error) -> Self {
        OperationError::service_error(&format!("Failed to parse data: {}", err))
    }
}

impl<E> From<AtomicIoError<E>> for OperationError {
    fn from(err: AtomicIoError<E>) -> Self {
        match err {
            AtomicIoError::Internal(io_err) => OperationError::from(io_err),
            AtomicIoError::User(_user_err) => {
                OperationError::service_error("Unknown atomic write error")
            }
        }
    }
}

impl From<IoError> for OperationError {
    fn from(err: IoError) -> Self {
        OperationError::service_error(&format!("IO Error: {}", err))
    }
}

impl From<serde_json::Error> for OperationError {
    fn from(err: serde_json::Error) -> Self {
        OperationError::service_error(&format!("Json error: {}", err))
    }
}

impl From<fs_extra::error::Error> for OperationError {
    fn from(err: fs_extra::error::Error) -> Self {
        OperationError::service_error(&format!("File system error: {}", err))
    }
}

pub type OperationResult<T> = result::Result<T, OperationError>;

pub fn get_service_error<T>(err: &OperationResult<T>) -> Option<OperationError> {
    match err {
        Ok(_) => None,
        Err(error) => match error {
            OperationError::ServiceError { .. } => Some(error.clone()),
            _ => None,
        },
    }
}

/// Define all operations which can be performed with Segment or Segment-like entity.
/// Assume, that all operations are idempotent - which means that
///     no matter how much time they will consequently executed - storage state will be the same.
pub trait SegmentEntry {
    /// Get current update version of the segment
    fn version(&self) -> SeqNumberType;

    /// Get version of specified point
    fn point_version(&self, point_id: PointIdType) -> Option<SeqNumberType>;

    #[allow(clippy::too_many_arguments)]
    fn search(
        &self,
        vector_name: &str,
        vector: &[VectorElementType],
        with_payload: &WithPayload,
        with_vector: &WithVector,
        filter: Option<&Filter>,
        top: usize,
        params: Option<&SearchParams>,
    ) -> OperationResult<Vec<ScoredPoint>>;

    #[allow(clippy::too_many_arguments)]
    fn search_batch(
        &self,
        vector_name: &str,
        vectors: &[&[VectorElementType]],
        with_payload: &WithPayload,
        with_vector: &WithVector,
        filter: Option<&Filter>,
        top: usize,
        params: Option<&SearchParams>,
    ) -> OperationResult<Vec<Vec<ScoredPoint>>>;

    fn upsert_vector(
        &mut self,
        op_num: SeqNumberType,
        point_id: PointIdType,
        vectors: &NamedVectors,
    ) -> OperationResult<bool>;

    fn delete_point(
        &mut self,
        op_num: SeqNumberType,
        point_id: PointIdType,
    ) -> OperationResult<bool>;

    fn set_payload(
        &mut self,
        op_num: SeqNumberType,
        point_id: PointIdType,
        payload: &Payload,
    ) -> OperationResult<bool>;

    fn set_full_payload(
        &mut self,
        op_num: SeqNumberType,
        point_id: PointIdType,
        full_payload: &Payload,
    ) -> OperationResult<bool>;

    fn delete_payload(
        &mut self,
        op_num: SeqNumberType,
        point_id: PointIdType,
        key: PayloadKeyTypeRef,
    ) -> OperationResult<bool>;

    fn clear_payload(
        &mut self,
        op_num: SeqNumberType,
        point_id: PointIdType,
    ) -> OperationResult<bool>;

    fn vector(
        &self,
        vector_name: &str,
        point_id: PointIdType,
    ) -> OperationResult<Vec<VectorElementType>>;

    fn all_vectors(&self, point_id: PointIdType) -> OperationResult<NamedVectors>;

    fn payload(&self, point_id: PointIdType) -> OperationResult<Payload>;

    fn iter_points(&self) -> Box<dyn Iterator<Item = PointIdType> + '_>;

    /// Paginate over points which satisfies filtering condition starting with `offset` id including.
    fn read_filtered<'a>(
        &'a self,
        offset: Option<PointIdType>,
        limit: Option<usize>,
        filter: Option<&'a Filter>,
    ) -> Vec<PointIdType>;

    /// Read points in [from; to) range
    fn read_range(&self, from: Option<PointIdType>, to: Option<PointIdType>) -> Vec<PointIdType>;

    /// Check if there is point with `point_id` in this segment.
    fn has_point(&self, point_id: PointIdType) -> bool;

    /// Return number of vectors in this segment
    fn points_count(&self) -> usize;

    /// Estimate points count in this segment for given filter.
    fn estimate_points_count<'a>(&'a self, filter: Option<&'a Filter>) -> CardinalityEstimation;

    fn vector_dim(&self, vector_name: &str) -> OperationResult<usize>;

    fn vector_dims(&self) -> HashMap<String, usize>;

    /// Number of vectors, marked as deleted
    fn deleted_count(&self) -> usize;

    /// Get segment type
    fn segment_type(&self) -> SegmentType;

    /// Get current stats of the segment
    fn info(&self) -> SegmentInfo;

    /// Get segment configuration
    fn config(&self) -> SegmentConfig;

    /// Get current stats of the segment
    fn is_appendable(&self) -> bool;

    /// Flushes current segment state into a persistent storage, if possible
    /// if sync == true, block current thread while flushing
    ///
    /// Returns maximum version number which is guaranteed to be persisted.
    fn flush(&self, sync: bool) -> OperationResult<SeqNumberType>;

    /// Removes all persisted data and forces to destroy segment
    fn drop_data(self) -> OperationResult<()>;

    /// Path to data, owned by segment
    fn data_path(&self) -> PathBuf;

    /// Delete field index, if exists
    fn delete_field_index(
        &mut self,
        op_num: SeqNumberType,
        key: PayloadKeyTypeRef,
    ) -> OperationResult<bool>;

    /// Create index for a payload field, if not exists
    fn create_field_index(
        &mut self,
        op_num: SeqNumberType,
        key: PayloadKeyTypeRef,
        field_schema: Option<&PayloadFieldSchema>,
    ) -> OperationResult<bool>;

    /// Get indexed fields
    fn get_indexed_fields(&self) -> HashMap<PayloadKeyType, PayloadFieldSchema>;

    /// Checks if segment errored during last operations
    fn check_error(&self) -> Option<SegmentFailedState>;

    /// Delete points by the given filter
    fn delete_filtered<'a>(
        &'a mut self,
        op_num: SeqNumberType,
        filter: &'a Filter,
    ) -> OperationResult<usize>;

    /// Take a snapshot of the segment.
    ///
    /// Creates a tar archive of the segment directory into `snapshot_dir_path`.
    fn take_snapshot(&self, snapshot_dir_path: &Path) -> OperationResult<()>;

    /// Copy the segment directory structure into `target_dir_path`
    ///
    /// Return the `Path` of the copy
    fn copy_segment_directory(&self, target_dir_path: &Path) -> OperationResult<PathBuf>;

    // Get collected telemetry data of segment
    fn get_telemetry_data(&self) -> SegmentTelemetry;
}
