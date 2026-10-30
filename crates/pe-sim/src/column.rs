//! One component's rows, sorted by object id, in copy-on-write buffers.

use std::sync::Arc;

use pe_core::ObjectId;

/// Rows are stored back to back in a `u64` buffer, so the bytes are 8-byte aligned and typed views
/// (`bytemuck::cast_slice`) work for any component with alignment up to 8. Bytes past the last row are
/// always zero.
#[derive(Clone, Debug)]
pub(crate) struct Column {
    pub size: usize,
    pub ids: Arc<Vec<ObjectId>>,
    pub data: Arc<Vec<u64>>,
}

fn words(bytes: usize) -> usize {
    bytes.div_ceil(8)
}

impl Column {
    pub fn new(size: usize) -> Self {
        Column { size, ids: Arc::new(Vec::new()), data: Arc::new(Vec::new()) }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn find(&self, id: ObjectId) -> Result<usize, usize> {
        self.ids.binary_search(&id)
    }

    /// The used bytes: `len * size`.
    pub fn bytes(&self) -> &[u8] {
        &bytemuck::cast_slice::<u64, u8>(&self.data)[..self.len() * self.size]
    }

    /// The used bytes, writable. Copies the buffer first if a snapshot shares it.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        let n = self.len() * self.size;
        let data: &mut Vec<u64> = Arc::make_mut(&mut self.data);
        &mut bytemuck::cast_slice_mut::<u64, u8>(data.as_mut_slice())[..n]
    }

    pub fn row(&self, i: usize) -> &[u8] {
        &self.bytes()[i * self.size..][..self.size]
    }

    pub fn row_mut(&mut self, i: usize) -> &mut [u8] {
        let s = self.size;
        &mut self.bytes_mut()[i * s..][..s]
    }

    /// Inserts a row at position `at` (from `find`'s `Err`).
    pub fn insert(&mut self, at: usize, id: ObjectId, row: &[u8]) {
        debug_assert_eq!(row.len(), self.size);
        let (n, s) = (self.len(), self.size);
        Arc::make_mut(&mut self.ids).insert(at, id);
        let data = Arc::make_mut(&mut self.data);
        data.resize(words((n + 1) * s), 0);
        let bytes = bytemuck::cast_slice_mut::<u64, u8>(data);
        bytes.copy_within(at * s..n * s, (at + 1) * s);
        bytes[at * s..(at + 1) * s].copy_from_slice(row);
    }

    /// Removes the row at position `at`.
    pub fn remove(&mut self, at: usize) {
        let (n, s) = (self.len(), self.size);
        Arc::make_mut(&mut self.ids).remove(at);
        let data = Arc::make_mut(&mut self.data);
        let bytes = bytemuck::cast_slice_mut::<u64, u8>(data);
        bytes.copy_within((at + 1) * s..n * s, at * s);
        bytes[(n - 1) * s..n * s].fill(0);
        data.truncate(words((n - 1) * s));
    }
}
