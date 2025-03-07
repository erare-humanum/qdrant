use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem::{size_of, transmute};
use std::path::Path;
use std::sync::Arc;

use memmap::{Mmap, MmapMut, MmapOptions};
use parking_lot::{RwLock, RwLockReadGuard};

use crate::common::error_logging::LogError;
use crate::common::Flusher;
use crate::data_types::vectors::VectorElementType;
use crate::entry::entry_point::OperationResult;
use crate::types::PointOffsetType;

const HEADER_SIZE: usize = 4;
const DELETED_HEADER: &[u8; 4] = b"drop";
const VECTORS_HEADER: &[u8; 4] = b"data";

/// Mem-mapped file with vectors and soft-delete flags
pub struct MmapVectors {
    pub dim: usize,
    pub num_vectors: usize,
    mmap: Mmap,
    deleted_mmap: Arc<RwLock<MmapMut>>,
    pub deleted_count: usize,
}

fn open_read(path: &Path) -> OperationResult<Mmap> {
    let file = OpenOptions::new()
        .read(true)
        .write(false)
        .append(true)
        .create(true)
        .open(path)?;

    Ok(unsafe { MmapOptions::new().map(&file)? })
}

fn open_write(path: &Path) -> OperationResult<MmapMut> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(path)?;

    Ok(unsafe { MmapMut::map_mut(&file)? })
}

fn ensure_mmap_file_exists(path: &Path, header: &[u8]) -> OperationResult<()> {
    if path.exists() {
        return Ok(());
    }
    let mut file = File::create(path)?;
    file.write_all(header)?;
    Ok(())
}

impl MmapVectors {
    pub fn open(vectors_path: &Path, deleted_path: &Path, dim: usize) -> OperationResult<Self> {
        ensure_mmap_file_exists(vectors_path, VECTORS_HEADER).describe("Create mmap data file")?;
        ensure_mmap_file_exists(deleted_path, DELETED_HEADER)
            .describe("Create mmap deleted flags file")?;

        let mmap = open_read(vectors_path).describe("Open mmap for reading")?;
        let num_vectors = (mmap.len() - HEADER_SIZE) / dim / size_of::<VectorElementType>();

        let deleted_mmap = open_write(deleted_path).describe("Open mmap for writing")?;

        let deleted_count = (HEADER_SIZE..deleted_mmap.len())
            .map(|idx| *deleted_mmap.get(idx).unwrap() as usize)
            .sum();

        Ok(MmapVectors {
            dim,
            num_vectors,
            mmap,
            deleted_mmap: Arc::new(RwLock::new(deleted_mmap)),
            deleted_count,
        })
    }

    pub fn data_offset(&self, key: PointOffsetType) -> Option<usize> {
        let vector_data_length = self.dim * size_of::<VectorElementType>();
        let offset = (key as usize) * vector_data_length + HEADER_SIZE;
        if key >= (self.num_vectors as PointOffsetType) {
            return None;
        }
        Some(offset)
    }

    pub fn raw_size(&self) -> usize {
        self.dim * size_of::<VectorElementType>()
    }

    pub fn raw_vector_offset(&self, offset: usize) -> &[VectorElementType] {
        let byte_slice = &self.mmap[offset..(offset + self.raw_size())];
        let arr: &[VectorElementType] = unsafe { transmute(byte_slice) };
        &arr[0..self.dim]
    }

    pub fn raw_vector(&self, key: PointOffsetType) -> Option<&[VectorElementType]> {
        self.data_offset(key)
            .map(|offset| self.raw_vector_offset(offset))
    }

    pub fn check_deleted(mmap: &MmapMut, key: PointOffsetType) -> Option<bool> {
        mmap.get(HEADER_SIZE + (key as usize)).map(|x| *x > 0)
    }

    pub fn read_deleted_map(&self) -> RwLockReadGuard<MmapMut> {
        self.deleted_mmap.read()
    }

    pub fn deleted(&self, key: PointOffsetType) -> Option<bool> {
        Self::check_deleted(&self.deleted_mmap.read(), key)
    }

    /// Creates returns owned vector (copy of internal vector)
    pub fn get_vector(&self, key: PointOffsetType) -> Option<Vec<VectorElementType>> {
        match self.deleted(key) {
            None | Some(true) => None,
            Some(false) => self
                .data_offset(key)
                .map(|offset| self.raw_vector_offset(offset).to_vec()),
        }
    }

    pub fn delete(&mut self, key: PointOffsetType) -> OperationResult<()> {
        if key < (self.num_vectors as PointOffsetType) {
            let mut deleted_mmap = self.deleted_mmap.write();
            let flag = deleted_mmap.get_mut((key as usize) + HEADER_SIZE).unwrap();

            if *flag == 0 {
                *flag = 1;
                self.deleted_count += 1;
            }
        }
        Ok(())
    }

    pub fn flusher(&self) -> Flusher {
        let deleted_mmap = self.deleted_mmap.clone();
        Box::new(move || {
            deleted_mmap.read().flush()?;
            Ok(())
        })
    }
}
