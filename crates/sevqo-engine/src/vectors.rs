//! Exact vector store: one flat little-endian f32 file plus a memory-mapped reader.
//!
//! At 512 dimensions a row is 2 KB, so 100k rows is ~200 MB and a full scan is a few
//! milliseconds with rayon. That beats an approximate index here: no recall loss, no
//! extra dependency, and deletion is a flag in SQLite rather than a graph repair.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::{Error, Result, VecRow};

const HEADER: &[u8; 8] = b"SEVQOVC1";

// ------------------------------------------------------------------ generations
//
// The vector file and SQLite are two separate persistent objects, so no single
// transaction can commit both. Compaction therefore never overwrites the live file: it
// streams a new generation file, fsyncs it, atomically renames it, and only then flips
// the `vec_generation` meta row inside one SQLite transaction together with the row-id
// remap (P0-01). That commit is the switch point:
//
//   * crash before the commit  → old generation is still authoritative, the new file is
//     an orphan a startup cleanup deletes;
//   * crash after the commit   → the new file is authoritative and complete, older
//     generations are cleaned up at startup.

/// SQLite meta key holding the active generation number.
pub const GEN_META: &str = "vec_generation";

/// Path of generation `gen` for a store based at `base` (`…/vectors.f32` →
/// `…/vectors.g000001.f32`). The legacy name is only used before adoption.
pub fn gen_path(base: &Path, gen: u64) -> PathBuf {
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("vectors");
    base.with_file_name(format!("{stem}.g{gen:06}.f32"))
}

/// Generation number encoded in a sibling file name, if any (`vectors.g000007.f32` → 7).
fn parse_gen_name(base: &Path, name: &str) -> Option<u64> {
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("vectors");
    let rest = name.strip_prefix(stem)?.strip_prefix(".g")?;
    let digits = rest.strip_suffix(".f32")?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn gen_meta(db: &Db) -> Option<u64> {
    db.get_meta(GEN_META).and_then(|v| v.parse().ok())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Resolve the authoritative vector file for reading and appending. The DB meta row
/// decides which generation is active; an index without the meta row (pre-generation
/// build) adopts its legacy file as generation 1 once. The active file's existence,
/// header and recorded rows/dim are validated on every resolve — a corrupt or missing
/// active file with live rows is a hard, diagnosable error, never a silent empty store
/// (P0-01.10). Content hashing is deep-check only (see [`check_active`]).
pub fn active_vector_path(base: &Path, db: &Db) -> Result<PathBuf> {
    let gen = match gen_meta(db) {
        Some(g) => g,
        None => return adopt_legacy(base, db),
    };
    let active = gen_path(base, gen);
    check_active(&active, db, false)?;
    Ok(active)
}

/// Active generation number (1 when the meta row is somehow absent after adoption).
pub fn active_generation(db: &Db) -> u64 {
    gen_meta(db).unwrap_or(1)
}

/// Temp path a compaction writes before its atomic rename: `vectors.g000002.f32.tmp`.
pub fn gen_tmp_path(base: &Path, gen: u64) -> PathBuf {
    let mut p = gen_path(base, gen).into_os_string();
    p.push(".tmp");
    PathBuf::from(p)
}

/// Delete every generation file other than `active`, plus compaction `.tmp` leftovers.
/// Safe while old mmaps are still open (unix unlink keeps mapped pages valid) and while
/// nothing may append to a superseded generation.
pub fn cleanup_generations(base: &Path, active: u64) -> Result<()> {
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("vectors")
        .to_string();
    if let Some(dir) = base.parent() {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(other) = parse_gen_name(base, name) {
                if other != active {
                    let _ = std::fs::remove_file(entry.path());
                }
            } else if name.ends_with(".tmp") && name.starts_with(&stem) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// One-time migration of a pre-generation index: rename the legacy file to generation 1
/// and record the meta row. Idempotent and race-tolerant (rename of a moved source fails
/// harmlessly when a concurrent adoption won): rename first, meta second — if the crash
/// lands between them, the next adopt finds the renamed file and just records the meta.
fn adopt_legacy(base: &Path, db: &Db) -> Result<PathBuf> {
    let gen1 = gen_path(base, 1);
    if base.exists() && !gen1.exists() {
        std::fs::rename(base, &gen1)?;
        if let Some(dir) = gen1.parent() {
            let _ = sync_dir(dir);
        }
    }
    db.set_meta(GEN_META, "1")?;
    check_active(&gen1, db, false)?;
    Ok(gen1)
}

/// Startup consistency pass: adopt a legacy store, deep-validate the active generation
/// (including the recorded SHA256 when nothing appended since compaction), and delete
/// files a crashed compaction left behind — generations other than the active one and
/// `.tmp` write-outs. Call this once at startup before watchers, the indexer and
/// searchers exist; a running compaction must never be cleaned up from underneath.
pub fn ensure_generation_store(base: &Path, db: &Db) -> Result<PathBuf> {
    // 校验先行：活动代通过深度检查之前，绝不能清理任何其他 Generation 文件——
    // 旧代是压缩失败/新代损坏时唯一可能的恢复来源。
    let active = active_vector_path(base, db)?;
    check_active(&active, db, true)?;
    let gen = gen_meta(db).unwrap_or(1);
    // 活动代完整有效，被取代的旧代此时才允许清理（方案 2.2：延迟执行）。
    cleanup_generations(base, gen)?;
    Ok(active)
}

/// Validate the active generation file against its header and the recorded metadata.
/// `deep` adds the SHA256 re-check, which is only meaningful when nothing has appended
/// since the compaction that recorded it (row count unchanged) — hashing a large file on
/// every resolve would be prohibitive.
fn check_active(active: &Path, db: &Db, deep: bool) -> Result<()> {
    let live = db.alive_vec_rows()?;
    if !active.exists() {
        if live > 0 {
            return Err(Error::Config(format!(
                "活动向量文件缺失: {}。索引数据库仍有 {live} 条有效向量映射，文件可能被删除或移动。",
                active.display()
            )));
        }
        return Ok(());
    }
    let len = active.metadata()?.len() as usize;
    if len == 0 {
        // 零字节 ≠ 空索引：磁盘异常/外部截断可能把有数据的文件清成 0 字节。
        // 此时数据库里的映射仍在，放行会让 Store::open 重新初始化文件头、
        // 旧 row_id 全部指向不存在（或被后续写入覆盖）的数据——必须按损坏处理。
        if live > 0 {
            return Err(Error::Config(format!(
                "活动向量文件为 0 字节，但索引数据库仍有 {live} 条有效向量映射，已按损坏处理: {}",
                active.display()
            )));
        }
        return Ok(());
    }
    if len < 16 {
        return Err(Error::Config(format!(
            "向量文件已损坏: {}",
            active.display()
        )));
    }
    let mut head = [0u8; 16];
    {
        let mut f = File::open(active)?;
        std::io::Read::read_exact(&mut f, &mut head)?;
    }
    let (dim, rows) = parse_header(&head, len, active)?;
    // 数据库有效映射必须落在文件声明的行数内；越界即损坏，不能静默当空向量。
    if let Some(max_row) = db.max_alive_row()? {
        if max_row >= rows as i64 {
            return Err(Error::Config(format!(
                "向量映射行号 {max_row} 超出文件行数 {rows}，索引已损坏: {}",
                active.display()
            )));
        }
    }
    let rec_dim = db.get_meta("vec_dim").and_then(|v| v.parse::<usize>().ok());
    if let Some(d) = rec_dim {
        if d != dim {
            return Err(Error::Config(format!(
                "向量文件维度 {dim} 与记录 {d} 不符: {}",
                active.display()
            )));
        }
    }
    let rec_rows = db
        .get_meta("vec_row_count")
        .and_then(|v| v.parse::<usize>().ok());
    if let Some(r) = rec_rows {
        if rows < r {
            return Err(Error::Config(format!(
                "向量文件行数 {rows} 少于记录的 {r}，文件被截断: {}",
                active.display()
            )));
        }
        // 校验值只在压缩时记录；此后追加会改变内容，所以仅在「没有追加过」时复验。
        if deep && rows == r {
            if let Some(sha) = db.get_meta("vec_sha256") {
                if sha256_file(active)? != sha {
                    return Err(Error::Config(format!(
                        "向量文件校验不符，内容已损坏: {}",
                        active.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Throw away every generation file plus the legacy base and the generation meta.
/// Used by the explicit rebuild/dimension-change paths, which already hold the run slot
/// and the vector write lock.
pub fn wipe(base: &Path, db: &Db) -> Result<()> {
    let _ = std::fs::remove_file(base);
    if let Some(dir) = base.parent() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if let Some(n) = name.to_str() {
                    if parse_gen_name(base, n).is_some()
                        || (n.ends_with(".tmp")
                            && n.starts_with(
                                base.file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("vectors"),
                            ))
                    {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
    db.set_meta(GEN_META, "1")?;
    // 维度可能刚刚改变：记录不匹配的活动维度会让下一次启动校验误报损坏。
    db.delete_meta("vec_dim")?;
    db.delete_meta("vec_row_count")?;
    db.delete_meta("vec_sha256")?;
    Ok(())
}

/// Stream a new generation file containing exactly the mapped rows (P0-01): read the old
/// file through an mmap in row order (bounded RAM, sequential pages), write buffered,
/// hash on the fly, fsync the temp file, then atomically rename and fsync the directory.
/// Returns the SHA256 of the complete new file.
pub fn write_generation(src: &Path, dst: &Path, dim: usize, live: &[(i64, i64)]) -> Result<String> {
    if live.len() > u32::MAX as usize {
        return Err(Error::Config(format!(
            "压缩结果 {} 行超出格式上限",
            live.len()
        )));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let view = MmapView::open(src)?;
    if view.is_none() && !live.is_empty() {
        return Err(Error::Config(format!(
            "源向量文件缺失但仍有 {} 条有效向量: {}",
            live.len(),
            src.display()
        )));
    }
    let tmp = File::create(dst)?;
    let mut w = BufWriter::new(tmp);
    let mut hash = Sha256::new();
    fn put(w: &mut BufWriter<File>, hash: &mut Sha256, bytes: &[u8]) -> Result<()> {
        w.write_all(bytes)?;
        hash.update(bytes);
        Ok(())
    }
    put(&mut w, &mut hash, HEADER)?;
    put(&mut w, &mut hash, &(dim as u32).to_le_bytes())?;
    put(&mut w, &mut hash, &(live.len() as u32).to_le_bytes())?;
    for (old, _) in live {
        if *old < 0 {
            return Err(Error::Config(format!(
                "压缩遇到负数向量行号 {old}，索引已损坏"
            )));
        }
        let v = view.as_ref().map(|s| s.row(*old as usize)).unwrap_or(&[]);
        if v.len() != dim {
            return Err(Error::Config(format!("向量行 {old} 越界，无法压缩")));
        }
        for x in v {
            put(&mut w, &mut hash, &x.to_le_bytes())?;
        }
    }
    w.flush()?;
    w.get_ref().sync_all()?;
    drop(w);
    std::fs::rename(dst, gen_final_of(dst))?;
    if let Some(dir) = dst.parent() {
        sync_dir(dir)?;
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `dst` is the `.tmp` path; the final name is the same minus the tmp suffix.
fn gen_final_of(tmp: &Path) -> PathBuf {
    let s = tmp.to_string_lossy();
    PathBuf::from(s.trim_end_matches(".tmp"))
}

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
        return Err(Error::Config(format!(
            "向量文件格式错误: {}",
            path.display()
        )));
    }
    let dim = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
    let rows = u32::from_le_bytes([head[12], head[13], head[14], head[15]]) as usize;
    if dim == 0 {
        return Err(Error::Config(format!(
            "向量文件维度为 0: {}",
            path.display()
        )));
    }
    let need = rows
        .checked_mul(row_bytes(dim))
        .and_then(|body| body.checked_add(16))
        .ok_or_else(|| Error::Config(format!("向量文件头超出合法范围: {}", path.display())))?;
    if total_len < need {
        return Err(Error::Config(format!(
            "向量文件长度不合法: {}",
            path.display()
        )));
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
        // 不加 truncate：已存在的 generation 文件必须原样打开，只有空文件才会被写入头。
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
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
        Ok(Store {
            path: path.to_path_buf(),
            dim: stored_dim,
            rows,
            file,
        })
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
        // 连续批量写入（方案 8.3）：同一行内一次 write_all，相邻行合并成整块，
        // 不再对每个 float 各发一次系统调用（512 维 × 10 万行 = 5120 万次小块写）。
        let mut sorted: Vec<&(usize, Vec<f32>)> = items.iter().collect();
        sorted.sort_by_key(|(row, _)| *row);
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut run_start: usize = 0;
        let mut run_next: usize = 0;
        let flush_run = |buf: &mut Vec<u8>, start: usize, end: usize, f: &mut File| -> Result<()> {
            if buf.is_empty() {
                return Ok(());
            }
            f.seek(SeekFrom::Start((16 + start * row_bytes(self.dim)) as u64))?;
            f.write_all(buf)?;
            buf.clear();
            let _ = end;
            Ok(())
        };
        for &&(row, ref v) in sorted.iter() {
            if run_next != row && !buf.is_empty() {
                flush_run(&mut buf, run_start, run_next, &mut self.file)?;
                run_start = row;
            } else if buf.is_empty() {
                run_start = row;
            }
            for x in v {
                buf.extend_from_slice(&x.to_le_bytes());
            }
            run_next = row + 1;
            self.rows = self.rows.max(row + 1);
        }
        flush_run(&mut buf, run_start, run_next, &mut self.file)?;
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
        self.file = replacement
            .persist(&self.path)
            .map_err(|e| Error::Io(e.error))?;
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
        Ok(Some(MmapView {
            inner: mmap,
            dim,
            rows,
        }))
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
                // 负数行号是数据库损坏的信号，绝不能折叠到第 0 行冒充别人的向量。
                let v = if c.row < 0 {
                    &[][..]
                } else {
                    self.row(c.row as usize)
                };
                let s = if v.len() == self.dim {
                    dot(query, v)
                } else {
                    -1.0
                };
                (clone_row(c), s)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(top);
        scored
    }
}

fn clone_row(r: &VecRow) -> VecRow {
    VecRow {
        row: r.row,
        target: r.target,
        ref_id: r.ref_id,
        file_id: r.file_id,
        file_kind: r.file_kind,
    }
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
        VecRow {
            row: id,
            target: EmbedRowKind::Image,
            ref_id: id,
            file_id: 100 + id,
            file_kind: Kind::Image,
        }
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
    fn put_many_handles_unsorted_and_interleaved_rows() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut s = Store::open(&p, 2).unwrap();
        // 乱序 + 同行重复覆盖 + 非相邻行
        s.put_many(&[
            (5, vec![0.5, 0.5]),
            (0, vec![1.0, 0.0]),
            (2, vec![0.0, 1.0]),
            (5, vec![0.6, 0.8]),
        ])
        .unwrap();
        s.flush().unwrap();
        assert_eq!(s.rows(), 6);
        assert_eq!(s.row(0).unwrap(), vec![1.0, 0.0]);
        assert_eq!(s.row(2).unwrap(), vec![0.0, 1.0]);
        assert_eq!(s.row(5).unwrap(), vec![0.6, 0.8]);
        assert_eq!(s.row(3).unwrap(), vec![0.0, 0.0], "空洞保持零向量");
        let view = MmapView::open(&p).unwrap().unwrap();
        assert_eq!(view.rows(), 6);
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
        assert!(
            hits.iter().any(|h| h.1 < 0.0),
            "missing row must score below zero"
        );
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
        assert!(
            view.row(usize::MAX).is_empty(),
            "异常大行号不得 panic 或越界读"
        );
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

    // -------------------------------------------- P0-01: generation 与恢复协议

    use crate::db::Db;

    /// 在 base 上造一个两行的 store；直接写 legacy 名，走一次 adoption 得到 gen1。
    fn seeded_store(dir: &tempfile::TempDir, name: &str) -> (PathBuf, Db) {
        let base = dir.path().join(name);
        let mut s = Store::open(&base, 2).unwrap();
        s.put(0, &[1.0, 0.0]).unwrap();
        s.put(1, &[0.0, 1.0]).unwrap();
        s.flush().unwrap();
        drop(s);
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let active = active_vector_path(&base, &db).unwrap();
        assert_eq!(
            active,
            gen_path(&base, 1),
            "legacy 文件应被收编为 generation 1"
        );
        (base, db)
    }

    #[test]
    fn legacy_file_is_adopted_as_generation_one() {
        let dir = tempfile::tempdir().unwrap();
        let (base, db) = seeded_store(&dir, "vectors.f32");
        assert!(!base.exists(), "收编后旧名不再存在");
        assert_eq!(db.get_meta(GEN_META).as_deref(), Some("1"));
        // 再解析一次是幂等的：不会再次改名，也不会丢数据。
        let again = active_vector_path(&base, &db).unwrap();
        assert_eq!(again, gen_path(&base, 1));
        let s = Store::open(&again, 2).unwrap();
        assert_eq!(s.rows(), 2);
    }

    /// 压缩在写完新文件、还没提交 SQLite 事务时崩溃：旧 generation 仍是权威，
    /// 新文件是孤儿，启动清理把它删掉。
    #[test]
    fn crash_before_the_commit_leaves_the_old_generation_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let (base, db) = seeded_store(&dir, "vectors.f32");
        let gen1 = gen_path(&base, 1);
        // 模拟：write_generation 已完成（文件已就位），但 commit_compaction 没有执行。
        let gen2_tmp = gen_tmp_path(&base, 2);
        write_generation(&gen1, &gen2_tmp, 2, &[(1, 0)]).unwrap();
        assert!(gen_path(&base, 2).exists());

        let active = ensure_generation_store(&base, &db).unwrap();

        assert_eq!(active, gen1, "数据库仍指向 generation 1");
        assert!(gen1.exists());
        assert!(!gen_path(&base, 2).exists(), "孤儿 generation 必须被清理");
        assert!(!gen2_tmp.exists(), "残留 .tmp 也必须被清理");
    }

    /// 压缩在提交之后、清理旧文件之前崩溃：新 generation 已是权威，旧文件被清掉。
    #[test]
    fn crash_after_the_commit_promotes_the_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let (base, db) = seeded_store(&dir, "vectors.f32");
        let gen1 = gen_path(&base, 1);
        let gen2_tmp = gen_tmp_path(&base, 2);
        write_generation(&gen1, &gen2_tmp, 2, &[(0, 0)]).unwrap();
        // 模拟 commit_compaction 已完成（这里只关心 meta 翻转对路径解析的影响）。
        db.set_meta(GEN_META, "2").unwrap();

        let active = ensure_generation_store(&base, &db).unwrap();

        assert_eq!(active, gen_path(&base, 2), "新 generation 已是权威");
        assert!(gen_path(&base, 2).exists());
        assert!(!gen1.exists(), "被取代的旧 generation 必须被清理");
        // 数据仍在：新文件里是 (0→0) 映射后的内容。
        let mut s = Store::open(&active, 2).unwrap();
        assert_eq!(s.rows(), 1);
        assert_eq!(s.row(0).unwrap(), vec![1.0, 0.0]);
    }

    /// 数据库还有有效向量映射、活动文件却不在了：这是索引损坏，必须显式报错，
    /// 绝不能静默当作空索引继续搜。
    #[test]
    fn missing_active_file_with_live_rows_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("vectors.f32");
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        db.set_meta(GEN_META, "1").unwrap();
        let row = db.reserve_vec_row(EmbedRowKind::Image, 1, 1).unwrap();
        db.activate_vec_rows(&[row]).unwrap();

        let err = active_vector_path(&base, &db).unwrap_err();
        assert!(err.to_string().contains("活动向量文件缺失"), "{err}");
    }

    #[test]
    fn truncated_active_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (base, db) = seeded_store(&dir, "vectors.f32");
        let gen1 = gen_path(&base, 1);
        std::fs::write(&gen1, [0u8; 10]).unwrap();
        let err = active_vector_path(&base, &db).unwrap_err();
        assert!(err.to_string().contains("已损坏"), "{err}");
    }

    /// 没有可回收的行时压缩是空操作：不翻 generation、不造新文件。
    #[test]
    fn compaction_with_nothing_dead_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (base, db) = seeded_store(&dir, "vectors.f32");
        db.set_meta("vec_row_count", "2").unwrap();
        db.set_meta("vec_dim", "2").unwrap();

        let dropped = crate::jobs::compact(&db, &base, 2).unwrap();

        assert_eq!(dropped, 0);
        assert_eq!(active_generation(&db), 1, "generation 不应翻转");
        assert!(
            !gen_path(&base, 2).exists(),
            "没有可回收内容就不该产出新文件"
        );
    }

    /// 零字节活动文件 + 有效映射 = 损坏：必须报错，绝不能放行让 Store 重新初始化
    /// 文件头（P0：意外截断会就此吞掉整套语义索引）。
    #[test]
    fn zero_byte_active_file_with_live_rows_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let (base, db) = seeded_store(&dir, "vectors.f32");
        let active = active_vector_path(&base, &db).unwrap();
        // 模拟磁盘异常截断
        std::fs::write(&active, []).unwrap();
        let row = db.reserve_vec_row(EmbedRowKind::Image, 1, 1).unwrap();
        db.activate_vec_rows(&[row]).unwrap();

        let err = active_vector_path(&base, &db).unwrap_err();
        assert!(err.to_string().contains("0 字节"), "{err}");
        let err = ensure_generation_store(&base, &db).unwrap_err();
        assert!(err.to_string().contains("0 字节"), "{err}");
        assert!(!base.exists(), "文件绝不能被自动重建覆盖");
    }

    /// 零字节 + 零有效映射：合法的「尚未写入」，照常放行。
    #[test]
    fn zero_byte_active_file_without_live_rows_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let (base, db) = seeded_store(&dir, "vectors.f32");
        let active = active_vector_path(&base, &db).unwrap();
        std::fs::write(&active, []).unwrap();
        assert_eq!(active_vector_path(&base, &db).unwrap(), active);
    }

    /// 数据库映射超出文件声明行数：启动即报损坏，不能静默当空向量搜。
    #[test]
    fn live_rows_beyond_header_row_count_are_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let (base, db) = seeded_store(&dir, "vectors.f32"); // 2 行
        let row = db.reserve_vec_row(EmbedRowKind::Image, 1, 1).unwrap(); // 第 2 行
        db.activate_vec_rows(&[row]).unwrap();
        // 把文件截到只剩 1 行：映射行 1 越界
        let active = gen_path(&base, 1);
        let data = std::fs::read(&active).unwrap();
        std::fs::write(&active, &data[..16 + 2 * 4]).unwrap();
        let err = active_vector_path(&base, &db).unwrap_err();
        assert!(
            err.to_string().contains("超出文件行数") || err.to_string().contains("长度不合法"),
            "{err}"
        );
    }

    /// 搜索侧负数行号守卫：绝不能折叠到第 0 行冒充别人的向量。
    #[test]
    fn negative_candidate_rows_never_match_row_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("v.f32");
        let mut s = Store::open(&p, 2).unwrap();
        s.put(0, &[1.0, 0.0]).unwrap();
        s.flush().unwrap();
        let view = MmapView::open(&p).unwrap().unwrap();
        let bad = VecRow {
            row: -5,
            target: EmbedRowKind::Image,
            ref_id: 9,
            file_id: 200,
            file_kind: Kind::Image,
        };
        let good = VecRow {
            row: 0,
            target: EmbedRowKind::Image,
            ref_id: 1,
            file_id: 100,
            file_kind: Kind::Image,
        };
        let hits = view.search(&[1.0, 0.0], &[bad, good], 5);
        let bad_hit = hits.iter().find(|(r, _)| r.file_id == 200).unwrap();
        assert!(
            bad_hit.1 < 0.0,
            "负数行号不得匹配第 0 行的向量: {bad_hit:?}"
        );
        let good_hit = hits.iter().find(|(r, _)| r.file_id == 100).unwrap();
        assert!((good_hit.1 - 1.0).abs() < 1e-6);
    }

    /// write_generation 产出完整可读的新文件：头、行数、内容与哈希一致。
    #[test]
    fn written_generation_roundtrips_and_deletes_no_source() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _db) = seeded_store(&dir, "vectors.f32");
        let gen1 = gen_path(&base, 1);
        let sha = write_generation(&gen1, &gen_tmp_path(&base, 2), 2, &[(1, 0)]).unwrap();
        assert_eq!(sha.len(), 64);
        assert!(gen1.exists(), "源文件在提交前必须原样保留");
        let mut s = Store::open(&gen_path(&base, 2), 2).unwrap();
        assert_eq!(s.rows(), 1);
        assert_eq!(s.row(0).unwrap(), vec![0.0, 1.0]);
        // 记录的哈希能独立复验。
        assert_eq!(sha256_file(&gen_path(&base, 2)).unwrap(), sha);
    }
}
