//! Exact vector store: one flat little-endian f32 file plus a memory-mapped reader.
//!
//! At 512 dimensions a row is 2 KB, so 100k rows is ~200 MB and a full scan is a few
//! milliseconds with rayon. That beats an approximate index here: no recall loss, no
//! extra dependency, and deletion is a flag in SQLite rather than a graph repair.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{Error, Result, VecRow};

const HEADER: &[u8; 8] = b"SEVQOVC1";

#[derive(Debug, Clone, Copy)]
pub struct Scored {
    pub row: i64,
    pub score: f32,
}

fn row_bytes(dim: usize) -> usize {
    dim * 4
}

/// `head` is the first 16 bytes, `total_len` the whole file: the row count lives in the
/// header, and a file shorter than that count claims is truncated. Overflow in the
/// header's own arithmetic means the file claims more address space than exists — reject
/// rather than carry a suspect header into pointer arithmetic (P0-05).
fn parse_header(head: &[u8], total_len: usize, path: &Path) -> Result<(usize, usize)> {
    if head.len() < 16 {
        return Err(Error::Config(format!("向量文件已损坏: {}", path.display())));
    }
    if &head[0..8] != HEADER {
        return Err(Error::Config(format!("向量文件格式错误: {}", path.display())));
    }
    let dim = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
    let rows = u32::from_le_bytes([head[12], head[13], head[14], head[15]]) as usize;
    if dim == 0 {
        return Err(Error::Config(format!("向量文件维度为 0: {}", path.display())));
    }
    let need = rows
        .checked_mul(row_bytes(dim))
        .and_then(|body| body.checked_add(16))
        .ok_or_else(|| Error::Config(format!("向量文件头超出合法范围: {}", path.display())))?;
    if total_len < need {
        return Err(Error::Config(format!("向量文件长度不合法: {}", path.display())));
    }
    Ok((dim, rows))
}

/// Offset of a row's first byte, with the same overflow discipline as `parse_header`.
fn row_offset(row: usize, dim: usize) -> Option<usize> {
    row.checked_mul(row_bytes(dim))?.checked_add(16)
}

/// Write side. Rows are addressed by `vec_map.row_id`, so sparse ids are normal: a
/// deleted file leaves a hole rather than shifting every later row.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    dim: usize,
    rows: usize,
    file: File,
}

impl Store {
    /// Open (or create) the store. A header whose dimension differs from `dim` is a
    /// hard error: vectors of two different dimensions cannot be compared.
    pub fn open(path: &Path, dim: usize) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new().create(true).read(true).write(true).open(path)?;
        let existing = file.metadata()?.len() as usize;
        let (stored_dim, rows) = if existing == 0 {
            file.write_all(HEADER)?;
            file.write_all(&(dim as u32).to_le_bytes())?;
            file.write_all(&(0u32).to_le_bytes())?;
            (dim, 0)
        } else {
            let mut head = vec![0u8; existing.min(16)];
            file.seek(SeekFrom::Start(0))?;
            std::io::Read::read_exact(&mut file, &mut head)?;
            let (d, r) = parse_header(&head, existing, path)?;
            if d != dim {
                return Err(Error::Config(format!(
                    "已索引的向量维度为 {d}，当前设置为 {dim}。修改维度需要重建索引"
                )));
            }
            (d, r)
        };
        file.seek(SeekFrom::End(0))?;
        Ok(Store { path: path.to_path_buf(), dim: stored_dim, rows, file })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }
    pub fn rows(&self) -> usize {
        self.rows
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Length on disk before this row, i.e. whether an append can be used.
    pub fn put(&mut self, row: usize, v: &[f32]) -> Result<()> {
        self.put_many(&[(row, v.to_vec())])
    }

    pub fn put_many(&mut self, items: &[(usize, Vec<f32>)]) -> Result<()> {
        for (_row, v) in items {
            if v.len() != self.dim {
                return Err(Error::Config(format!(
                    "向量维度不符：期望 {}，实际 {}",
                    self.dim,
                    v.len()
                )));
            }
        }
        let max_row = items.iter().map(|(r, _)| *r).max().unwrap_or(0);
        let need = max_row
            .checked_add(1)
            .and_then(|rows| rows.checked_mul(row_bytes(self.dim)))
            .and_then(|body| body.checked_add(16))
            .ok_or_else(|| Error::Config(format!("向量行号 {max_row} 超出合法范围")))?;
        if need > self.file.seek(SeekFrom::End(0))? as usize {
            self.file.set_len(need as u64)?;
        }
        for (row, v) in items {
            self.file.seek(SeekFrom::Start((16 + *row * row_bytes(self.dim)) as u64))?;
            for x in v {
                self.file.write_all(&x.to_le_bytes())?;
            }
            self.rows = self.rows.max(row + 1);
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.seek(SeekFrom::Start(12))?;
        self.file.write_all(&(self.rows as u32).to_le_bytes())?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Read one row through a fresh mapping-free path (used by tests and compaction).
    pub fn row(&mut self, row: usize) -> Result<Vec<f32>> {
        let Some(off) = row_offset(row, self.dim) else {
            return Err(Error::Config(format!("向量行号 {row} 超出合法范围")));
        };
        let need = off
            .checked_add(row_bytes(self.dim))
            .ok_or_else(|| Error::Config(format!("向量行号 {row} 超出合法范围")))?;
        if need > self.file.seek(SeekFrom::End(0))? as usize {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; row_bytes(self.dim)];
        self.file.seek(SeekFrom::Start(off as u64))?;
        std::io::Read::read_exact(&mut self.file, &mut buf)?;
        Ok(buf
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect())
    }

    /// Drop dead rows and pack the survivors from row 0. `live` is (old_row, new_row).
    /// The caller must update `vec_map` with the same mapping.
    pub fn compact(&mut self, live: &[(i64, i64)]) -> Result<()> {
        let capacity = live
            .len()
            .checked_mul(row_bytes(self.dim))
            .and_then(|body| body.checked_add(16))
            .ok_or_else(|| Error::Config("压缩结果超出合法范围".into()))?;
        let mut out: Vec<u8> = Vec::with_capacity(capacity);
        out.extend_from_slice(HEADER);
        out.extend_from_slice(&(self.dim as u32).to_le_bytes());
        out.extend_from_slice(&(live.len() as u32).to_le_bytes());
        for (old, _) in live {
            if *old < 0 {
                continue;
            }
            let v = self.row(*old as usize)?;
            if v.len() != self.dim {
                return Err(Error::Config(format!("向量行 {old} 越界，无法压缩")));
            }
            for x in &v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        // Existing search mappings must retain a valid old file; truncating it in
        // place can cause SIGBUS while a search is reading the mapped pages.
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
        replacement.write_all(&out)?;
        replacement.as_file().sync_all()?;
        self.file = replacement.persist(&self.path).map_err(|e| Error::Io(e.error))?;
        self.rows = live.len();
        self.file.sync_data()?;
        Ok(())
    }

    pub fn delete_all(path: &Path) -> Result<()> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// Read side for search: map the file once, scan without copying or holding RAM.
#[derive(Debug)]
pub struct MmapView {
    inner: memmap2::Mmap,
    dim: usize,
    rows: usize,
}

impl MmapView {
    /// `None` only when there is genuinely nothing to map: no file, or a zero-byte file
    /// that a just-created store has not written yet. A file that *exists* but fails its
    /// header check is corruption — report it, never pretend the store is absent and
    /// silently search with wrong vectors (P0-05).
    pub fn open(path: &Path) -> Result<Option<MmapView>> {
        if !path.exists() {
            return Ok(None);
        }
        let len = path.metadata()?.len() as usize;
        if len == 0 {
            return Ok(None);
        }
        let file = File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let (dim, rows) = parse_header(&mmap[..16.min(mmap.len())], mmap.len(), path)?;
        if rows == 0 {
            return Ok(None);
        }
        Ok(Some(MmapView { inner: mmap, dim, rows }))
    }

    pub fn dim(&self) -> usize {
        self.dim
    }
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// One row as f32s. Every bound is validated before the unsafe dereference: a row id
    /// from a confused database can only degrade into an empty slice (which search scores
    /// below zero), never into an out-of-bounds read. `parse_header` already guarantees
    /// the file holds `rows` complete rows, so a valid id is always fully mapped.
    pub fn row(&self, row: usize) -> &[f32] {
        if row >= self.rows {
            return &[];
        }
        let Some(off) = row_offset(row, self.dim) else {
            return &[];
        };
        let Some(end) = off.checked_add(row_bytes(self.dim)) else {
            return &[];
        };
        if end > self.inner.len() {
            return &[];
        }
        // The file is written as native-aligned f32 at a 4-byte multiple offset.
        unsafe {
            let ptr = self.inner.as_ptr().add(off) as *const f32;
            std::slice::from_raw_parts(ptr, self.dim)
        }
    }

    pub fn search(&self, query: &[f32], candidates: &[VecRow], top: usize) -> Vec<(VecRow, f32)> {
        if query.len() != self.dim || candidates.is_empty() {
            return Vec::new();
        }
        use rayon::prelude::*;
        let mut scored: Vec<(VecRow, f32)> = candidates
            .par_iter()
            .map(|c| {
                let v = self.row(c.row.max(0) as usize);
                let s = if v.len() == self.dim { dot(query, v) } else { -1.0 };
                (clone_row(c), s)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(top);
        scored
    }
}

fn clone_row(r: &VecRow) -> VecRow {
    VecRow { row: r.row, target: r.target, ref_id: r.ref_id, file_id: r.file_id, file_kind: r.file_kind }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut s = 0.0f32;
    for i in 0..a.len() {
        s += a[i] * b[i];
    }
    s
}

/// Rows are stored L2-normalised so the dot product is cosine, but truncation breaks
/// unit length, hence this is applied after slicing a Matryoshka vector.
pub fn normalize(v: &mut [f32]) {
    let mut norm = 0.0f32;
    for x in v.iter() {
        norm += x * x;
    }
    norm = norm.sqrt();
    if norm > 1e-9 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

pub fn truncate_normalize(v: &[f32], dim: usize) -> Vec<f32> {
    let mut out = v[..v.len().min(dim)].to_vec();
    normalize(&mut out);
    out
}

pub fn file_size_bytes(rows: usize, dim: usize) -> u64 {
    16u64 + (rows as u64).saturating_mul(row_bytes(dim) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbedRowKind, Kind};

    fn row(id: i64) -> VecRow {
        VecRow { row: id, target: EmbedRowKind::Image, ref_id: id, file_id: 100 + id, file_kind: Kind::Image }
    }

    #[test]
    fn put_then_mmap_search_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut s = Store::open(&p, 4).unwrap();
        s.put(0, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        s.put(1, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        s.put(2, &[0.6, 0.8, 0.0, 0.0]).unwrap();
        s.flush().unwrap();
        let view = MmapView::open(&p).unwrap().unwrap();
        let hits = view.search(&[1.0, 0.0, 0.0, 0.0], &[row(0), row(1), row(2)], 2);
        assert_eq!(hits[0].0.row, 0);
        assert!((hits[0].1 - 1.0).abs() < 1e-6);
        assert_eq!(hits[1].0.row, 2);
    }

    #[test]
    fn compaction_keeps_existing_search_mappings_valid() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut store = Store::open(&p, 2).unwrap();
        store.put(0, &[1.0, 0.0]).unwrap();
        store.put(1, &[0.0, 1.0]).unwrap();
        store.flush().unwrap();
        let before = MmapView::open(&p).unwrap().unwrap();
        store.compact(&[(1, 0)]).unwrap();
        assert_eq!(before.row(0), &[1.0, 0.0]);
        assert_eq!(before.row(1), &[0.0, 1.0]);
        let after = MmapView::open(&p).unwrap().unwrap();
        assert_eq!(after.rows(), 1);
        assert_eq!(after.row(0), &[0.0, 1.0]);
    }

    #[test]
    fn sparse_ids_leave_zeroed_holes_not_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut s = Store::open(&p, 3).unwrap();
        s.put(0, &[0.0, 1.0, 0.0]).unwrap();
        s.put(9, &[0.0, 0.0, 1.0]).unwrap();
        s.flush().unwrap();
        assert_eq!(s.rows(), 10);
        assert_eq!(s.row(9).unwrap(), vec![0.0, 0.0, 1.0]);
        assert_eq!(s.row(4).unwrap(), vec![0.0, 0.0, 0.0]);
        let view = MmapView::open(&p).unwrap().unwrap();
        assert_eq!(view.rows(), 10);
    }

    #[test]
    fn reopening_keeps_rows() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        {
            let mut s = Store::open(&p, 3).unwrap();
            s.put(0, &[0.0, 1.0, 0.0]).unwrap();
            s.put(3, &[0.0, 0.0, 1.0]).unwrap();
            s.flush().unwrap();
        }
        let mut s = Store::open(&p, 3).unwrap();
        assert_eq!(s.rows(), 4);
        assert_eq!(s.row(3).unwrap(), vec![0.0, 0.0, 1.0]);
    }

    #[test]
    fn dimension_change_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        {
            let mut s = Store::open(&p, 512).unwrap();
            s.put(0, &vec![0.1f32; 512]).unwrap();
            s.flush().unwrap();
        }
        let err = Store::open(&p, 256).unwrap_err();
        assert!(err.to_string().contains("维度"), "{err}");
    }

    #[test]
    fn empty_file_opens_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        std::fs::write(&p, []).unwrap();
        let s = Store::open(&p, 8).unwrap();
        assert_eq!(s.rows(), 0);
        assert!(MmapView::open(&p).unwrap().is_none());
    }

    #[test]
    fn truncate_then_normalize_restores_unit_length() {
        let full = vec![1.0f32; 8];
        let cut = truncate_normalize(&full, 4);
        assert_eq!(cut.len(), 4);
        let norm: f32 = cut.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "{norm}");
    }

    #[test]
    fn compact_drops_dead_rows() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut s = Store::open(&p, 2).unwrap();
        s.put(0, &[1.0, 0.0]).unwrap();
        s.put(1, &[0.0, 1.0]).unwrap();
        s.put(2, &[1.0, 1.0]).unwrap();
        s.flush().unwrap();
        s.compact(&[(2, 0), (0, 1)]).unwrap();
        assert_eq!(s.rows(), 2);
        assert_eq!(s.row(0).unwrap(), vec![1.0, 1.0]);
        assert_eq!(s.row(1).unwrap(), vec![1.0, 0.0]);
        let view = MmapView::open(&p).unwrap().unwrap();
        assert_eq!(view.rows(), 2);
    }

    #[test]
    fn search_ignores_out_of_range_rows() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut s = Store::open(&p, 2).unwrap();
        s.put(0, &[1.0, 0.0]).unwrap();
        s.flush().unwrap();
        let view = MmapView::open(&p).unwrap().unwrap();
        let hits = view.search(&[1.0, 0.0], &[row(0), row(77)], 5);
        assert_eq!(hits[0].0.row, 0);
        assert!(hits.iter().any(|h| h.1 < 0.0), "missing row must score below zero");
    }

    // ------------------------------------------------------ P0-05: 边界与溢出

    fn header_bytes(dim: u32, rows: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(HEADER);
        v.extend_from_slice(&dim.to_le_bytes());
        v.extend_from_slice(&rows.to_le_bytes());
        v
    }

    #[test]
    fn truncated_or_garbage_files_are_errors_not_silence() {
        let dir = tempfile::tempdir().unwrap();
        // 存在但只有半个头：损坏必须报出来，而不是被当成「还没有向量文件」。
        let p = dir.path().join("truncated.f32");
        std::fs::write(&p, [0u8; 10]).unwrap();
        assert!(MmapView::open(&p).is_err(), "半截头必须报错");
        // 头存在但 magic 不对。
        let q = dir.path().join("garbage.f32");
        std::fs::write(&q, [b'X'; 32]).unwrap();
        assert!(MmapView::open(&q).is_err(), "magic 不符必须报错");
        // 0 字节文件是合法的「尚未写入」。
        let e = dir.path().join("empty.f32");
        std::fs::write(&e, []).unwrap();
        assert!(MmapView::open(&e).unwrap().is_none());
    }

    #[test]
    fn header_claiming_more_rows_than_the_file_has_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("overclaim.f32");
        let mut bytes = header_bytes(4, 1000);
        bytes.extend_from_slice(&[0u8; 16]);
        std::fs::write(&p, &bytes).unwrap();
        let err = MmapView::open(&p).unwrap_err();
        assert!(err.to_string().contains("长度不合法"), "{err}");
    }

    #[test]
    fn header_that_overflows_the_address_space_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("overflow.f32");
        let mut bytes = header_bytes(u32::MAX, u32::MAX);
        bytes.extend_from_slice(&[0u8; 16]);
        std::fs::write(&p, &bytes).unwrap();
        let err = MmapView::open(&p).unwrap_err();
        // 两个极端值相乘会溢出 usize：必须在进入指针运算前拒绝。
        assert!(err.to_string().contains("超出合法范围"), "{err}");
    }

    #[test]
    fn mmap_row_bounds_are_checked_before_the_unsafe_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut s = Store::open(&p, 2).unwrap();
        s.put(0, &[1.0, 0.0]).unwrap();
        s.put(1, &[0.0, 1.0]).unwrap();
        s.flush().unwrap();
        let view = MmapView::open(&p).unwrap().unwrap();
        assert_eq!(view.row(0), &[1.0, 0.0]);
        assert!(view.row(2).is_empty(), "恰好等于行数的一行也是越界");
        assert!(view.row(usize::MAX).is_empty(), "异常大行号不得 panic 或越界读");
        assert!(view.row(usize::MAX - 5).is_empty());
    }

    #[test]
    fn store_rejects_impossible_row_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut s = Store::open(&p, 2).unwrap();
        assert!(s.row(usize::MAX).is_err(), "读取端必须拒绝异常行号");
        let err = s.put_many(&[(usize::MAX, vec![0.0, 0.0])]).unwrap_err();
        assert!(err.to_string().contains("超出合法范围"), "{err}");
        // 失败后文件仍然是干净的：没有留下巨大空洞。
        s.put(0, &[1.0, 0.0]).unwrap();
        s.flush().unwrap();
        assert_eq!(s.rows(), 1);
    }
}
