//! The indexer: scan → extract → embed → persist, with pause/cancel and progress.
//!
//! Work is batched per pass because the sidecar is cheapest when it gets a group of
//! like inputs (one torch call for N texts). Row ids are reserved before embedding so
//! the response key `target:row` maps straight back to a slot in the vector file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use crate::config::Settings;
use crate::db::Db;
use crate::extract;
use crate::infer::Client;
use crate::paths::Paths;
use crate::scan::{self, ScanOptions, ScanStats};
use crate::vectors::{normalize, Store};
use crate::video::plan_clips;
use crate::{EmbedItem, EmbedRowKind, Error, FileRecord, FileState, Kind, Phase, Progress, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Filename + full text only. Works before the model is downloaded, so search is
    /// useful immediately on first launch.
    TextOnly,
    /// Everything, including image/video/text vectors.
    Full,
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub done: u64,
    pub failed: u64,
    pub skipped: u64,
    pub vectors: u64,
    pub ms: f64,
}

#[derive(Clone, Default)]
pub struct Control {
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
}

impl Control {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn pause(&self) {
        self.pause.store(true, Ordering::SeqCst);
    }
    pub fn resume(&self) {
        self.pause.store(false, Ordering::SeqCst);
    }
    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }
    pub fn reset(&self) {
        self.cancel.store(false, Ordering::SeqCst);
        self.pause.store(false, Ordering::SeqCst);
    }
}

enum Prepared {
    Embed { items: Vec<EmbedItem>, keys: Vec<String> },
    /// Nothing left to do: text is stored, or the file has no indexable content.
    Indexed,
    Skip(String),
}

#[derive(Debug, Default)]
struct Out {
    done: u64,
    failed: u64,
    skipped: u64,
    vectors: u64,
}

struct BatchRecovery<'a> {
    db: &'a Db,
    ids: Vec<i64>,
}

impl Drop for BatchRecovery<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.db.recover_indexing_files(&self.ids) {
            tracing::warn!("无法恢复中断的索引批次: {e}");
        }
    }
}

pub struct Indexer {
    db: Arc<Db>,
    paths: Paths,
    settings: Settings,
    client: Option<Arc<Client>>,
    store: Arc<Mutex<Option<Store>>>,
    control: Control,
    emit: Sender<Progress>,
}

/// Keys are `target:row`, which is all the response needs to place a vector.
/// `kind:vec_row:ref_id`. The sidecar echoes keys back untouched, and a response has to
/// address two different tables: vectors go to the vector file by `vec_row`, thumbnails
/// belong to the clip or file row, which is *not* the same id.
fn key(row: i64, target: EmbedRowKind, ref_id: i64) -> String {
    format!("{}:{}:{}", target.as_str(), row, ref_id)
}

fn parse_key(k: &str) -> Option<(EmbedRowKind, i64, i64)> {
    let (t, rest) = k.split_once(':')?;
    let (row, ref_id) = rest.split_once(':')?;
    Some((EmbedRowKind::parse(t), row.parse().ok()?, ref_id.parse().ok()?))
}

impl Indexer {
    pub fn new(
        db: Arc<Db>,
        paths: Paths,
        settings: Settings,
        client: Option<Arc<Client>>,
        emit: Sender<Progress>,
    ) -> Indexer {
        Indexer {
            db,
            paths,
            settings,
            client,
            store: Arc::new(Mutex::new(None)),
            control: Control::default(),
            emit,
        }
    }

    pub fn control(&self) -> Control {
        self.control.clone()
    }

    /// Share the caller's cancel/pause flags instead of minting new ones, so a long
    /// running pass can be steered from outside the thread that started it.
    pub fn with_control(mut self, control: Control) -> Self {
        self.control = control;
        self
    }

    pub fn store_handle(&self) -> Arc<Mutex<Option<Store>>> {
        self.store.clone()
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// Open (or create) the vector file on first use.
    fn store(&self) -> Result<std::sync::MutexGuard<'_, Option<Store>>> {
        let mut g = self
            .store
            .lock()
            .map_err(|_| Error::Infer("向量存储锁中毒".into()))?;
        if g.is_none() {
            *g = Some(Store::open(&self.paths.vector_path, self.settings.vector_dim)?);
        }
        Ok(g)
    }

    pub fn kinds_for(&self, mode: Mode) -> Vec<Kind> {
        let mut v = Vec::new();
        if mode == Mode::Full {
            if self.settings.index_images {
                v.push(Kind::Image);
            }
            if self.settings.index_video {
                v.push(Kind::Video);
            }
        }
        if self.settings.index_documents {
            v.push(Kind::Document);
        }
        if self.settings.index_code {
            v.push(Kind::Code);
        }
        v
    }

    // ------------------------------------------------------------------ scan

    pub fn scan_root(&self, root_id: i64, mode: Mode) -> Result<ScanStats> {
        let roots = self.db.list_roots()?;
        let root = roots
            .iter()
            .find(|r| r.id == root_id)
            .ok_or_else(|| Error::Config(format!("找不到索引目录 {root_id}")))?;
        if !root.enabled {
            return Ok(ScanStats::default());
        }
        let path = scan::expand_home(&root.path);
        let seen = crate::db::now_ns();
        self.db.begin_scan(root_id)?;
        // A text-only pass must still mark media as present. Otherwise its sweep
        // deletes previously indexed images/videos when weights are unavailable.
        let scan_kinds = self.kinds_for(Mode::Full);
        if scan_kinds.is_empty() {
            return Err(Error::Config("没有勾选任何要索引的文件类型".into()));
        }
        let opts = ScanOptions {
            exclude_dirs: &self.settings.exclude_dirs,
            exclude_prefixes: &self.settings.exclude_path_prefixes,
            max_file_bytes: self.settings.max_file_bytes,
            kinds: &scan_kinds,
        };
        let kinds = self.kinds_for(mode);
        let mut candidates = 0u64;
        let mut write_errors = 0u64;
        let mut queued = 0u64;
        let mut progress = Progress {
            phase: Phase::Scanning,
            current: Some(root.label.clone()),
            total_estimate: 0,
            ..Default::default()
        };
        let db = self.db.clone();
        let cancel = &self.control.cancel;
        let emit = self.emit.clone();
        let mut stats = scan::scan(root_id, &path, &opts, cancel, |f| {
            candidates += 1;
            match db.upsert_file(&f, seen) {
                Ok((_, needs)) => {
                    if needs {
                        queued += 1;
                    }
                }
                Err(_) => write_errors += 1,
            }
            if candidates % 512 == 0 {
                progress.discovered = candidates;
                progress.queued = queued;
                let _ = emit.try_send(progress.clone());
            }
        })?;
        stats.errors += write_errors;
        // An unreadable subtree is not evidence that its files were deleted.
        let removed = if stats.errors == 0 && stats.unreadable == 0 {
            self.db.sweep(root_id, seen)?
        } else {
            0
        };
        let queued_now = self.db.pending_count(&kinds)?;
        progress.discovered = stats.walked;
        progress.queued = queued_now;
        progress.message = Some(format!("扫描完成：新增 {queued} 个，清理 {removed} 个已删除"));
        let _ = self.emit.try_send(progress);
        Ok(stats)
    }

    // ------------------------------------------------------------------- run

    /// Drain the queue. Returns when nothing is pending, the model stops responding, or
    /// cancellation is requested.
    pub fn run(&self, mode: Mode, max_files: Option<usize>) -> Result<Summary> {
        let started = Instant::now();
        let mut sum = Summary::default();
        let kinds = self.kinds_for(mode);
        if kinds.is_empty() {
            return Err(Error::Config("没有勾选任何要索引的文件类型".into()));
        }
        let file_batch = (self.settings.infer_batch * 2).max(8);
        let mut consecutive_errors = 0usize;
        // 每条中间事件都带累计值。以前只发 phase+current，界面上的计数会被 `..Default::default()`
        // 冲回 0，跑一整轮都显示「0 / 0 队列为空」。
        let mut live = Progress {
            phase: Phase::Indexing,
            discovered: self.db.counts().map(|c| c.values().sum::<i64>().max(0) as u64).unwrap_or(0),
            queued: self.db.pending_count(&kinds).unwrap_or(0),
            ..Default::default()
        };
        let _ = self.emit.try_send(live.clone());
        loop {
            if self.control.is_cancelled() {
                break;
            }
            while self.control.is_paused() {
                let _ = self.emit.try_send(Progress { phase: Phase::Paused, ..live.clone() });
                std::thread::sleep(Duration::from_millis(300));
                if self.control.is_cancelled() {
                    sum.ms = started.elapsed().as_secs_f64() * 1000.0;
                    return Ok(sum);
                }
            }
            let processed = sum.done + sum.failed + sum.skipped;
            let want = max_files.map(|m| (m as u64).saturating_sub(processed)).unwrap_or(usize::MAX as u64) as usize;
            if want == 0 {
                break;
            }
            let mut batch = self.db.pending(file_batch.min(want), &kinds)?;
            if batch.is_empty() {
                break;
            }
            let result = self.process_batch(&mut batch, mode, &live);
            match result {
                Ok(out) => {
                    consecutive_errors = 0;
                    sum.done += out.done;
                    sum.failed += out.failed;
                    sum.skipped += out.skipped;
                    sum.vectors += out.vectors;
                    if let Ok(mut g) = self.store.lock() {
                        if let Some(store) = g.as_mut() {
                            store.flush()?;
                        }
                    }
                    let ms = started.elapsed().as_secs_f64();
                    live.done = sum.done;
                    live.failed = sum.failed;
                    live.skipped = sum.skipped;
                    live.queued = self.db.pending_count(&kinds).unwrap_or(live.queued);
                    live.elapsed_s = ms;
                    live.images_per_min = if ms > 1.0 { sum.done as f64 / (ms / 60.0) } else { 0.0 };
                }
                Err(e) => {
                    if matches!(e, Error::Cancelled) {
                        break;
                    }
                    tracing::warn!("索引批次失败: {e}");
                    consecutive_errors += 1;
                    let _ = self.emit.try_send(Progress {
                        phase: Phase::Failed,
                        message: Some(e.to_string()),
                        ..live.clone()
                    });
                    // Without a model there is no point retrying the same batch forever.
                    if self.client.is_none() && mode == Mode::Full {
                        break;
                    }
                    if consecutive_errors >= 3 {
                        return Err(e);
                    }
                }
            }
        }
        sum.ms = started.elapsed().as_secs_f64() * 1000.0;
        let per_min = if sum.ms > 1.0 { sum.done as f64 / (sum.ms / 60_000.0) } else { 0.0 };
        let _ = self.emit.try_send(Progress {
            phase: Phase::Idle,
            discovered: live.discovered,
            queued: live.queued,
            done: sum.done,
            failed: sum.failed,
            skipped: sum.skipped,
            images_per_min: per_min,
            elapsed_s: sum.ms / 1000.0,
            message: Some(format!("本轮处理 {} 个文件", sum.done)),
            ..Default::default()
        });
        Ok(sum)
    }

    fn process_batch(&self, files: &mut [FileRecord], mode: Mode, live: &Progress) -> Result<Out> {
        // A cancelled or failed embed must be retryable without restarting the app.
        let _recovery = BatchRecovery { db: &self.db, ids: files.iter().map(|f| f.id).collect() };
        let mut out = Out::default();
        let want_vectors = mode == Mode::Full && self.client.is_some();
        let mut items: Vec<EmbedItem> = Vec::new();
        let mut per_file: Vec<(i64, Vec<String>)> = Vec::new();

        for f in files.iter() {
            if self.control.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.db.set_file_state(f.id, FileState::Indexing, None)?;
            let _ = self.emit.try_send(Progress {
                phase: Phase::Indexing,
                current: Some(f.name.clone()),
                ..live.clone()
            });
            let prepared = match f.kind {
                Kind::Image => self.prepare_image(f, want_vectors),
                Kind::Video => self.prepare_video(f, want_vectors),
                Kind::Document | Kind::Code => self.prepare_text(f, want_vectors),
                other => Err(Error::Unsupported(format!("{other} 类型暂不支持索引"))),
            };
            match prepared {
                Ok(Prepared::Embed { items: its, keys }) if want_vectors => {
                    items.extend(its);
                    per_file.push((f.id, keys));
                }
                Ok(Prepared::Embed { .. }) => {
                    // vectors disabled: keep the file done so the queue still drains
                    self.db.set_file_state(f.id, FileState::Done, None)?;
                    out.done += 1;
                }
                Ok(Prepared::Indexed) => {
                    self.db.set_file_state(f.id, FileState::Done, None)?;
                    out.done += 1;
                }
                Ok(Prepared::Skip(reason)) => {
                    self.db.set_file_state(f.id, FileState::Skipped, Some(&reason))?;
                    out.skipped += 1;
                }
                Err(e) => {
                    let msg = e.to_string();
                    let state = if matches!(e, Error::Unsupported(_)) { FileState::Skipped } else { FileState::Failed };
                    self.db.set_file_state(f.id, state, Some(&msg))?;
                    if state == FileState::Skipped {
                        out.skipped += 1;
                    } else {
                        out.failed += 1;
                    }
                }
            }
        }

        if items.is_empty() || !want_vectors {
            return Ok(out);
        }

        let client = self.client.as_ref().ok_or_else(|| Error::Infer("推理进程未启动".into()))?;
        let batch = self.settings.infer_batch.max(1);
        let timeout = Duration::from_secs(180 + 60 * (items.len() / batch) as u64);
        let resp = client.embed(&items, batch, timeout)?;

        let mut writes: Vec<(usize, Vec<f32>)> = Vec::new();
        let mut states = Vec::new();
        for (file_id, keys) in per_file {
            let mut ok = 0usize;
            let mut err: Option<String> = None;
            for k in &keys {
                if let Some(v) = resp.vectors.get(k) {
                    let (_target, row, _) = parse_key(k).ok_or_else(|| Error::Infer(format!("向量键格式错误 {k}")))?;
                    let mut v = v.clone();
                    if v.len() != self.settings.vector_dim {
                        err = Some(format!("向量维度 {} 不符", v.len()));
                        continue;
                    }
                    normalize(&mut v);
                    writes.push((row.max(0) as usize, v));
                    ok += 1;
                } else if let Some(e) = resp.errors.get(k) {
                    err = Some(e.clone());
                }
            }
            if ok > 0 {
                let note = err.filter(|_| ok < keys.len());
                states.push((file_id, FileState::Done, note));
                out.done += 1;
                out.vectors += ok as u64;
            } else {
                let msg = err.unwrap_or_else(|| "模型未返回向量".into());
                states.push((file_id, FileState::Failed, Some(msg)));
                out.failed += 1;
            }
        }

        if !writes.is_empty() {
            let mut g = self.store()?;
            if let Some(store) = g.as_mut() {
                store.put_many(&writes)?;
                store.flush()?;
            }
        }
        // Publish rows to search only after their bytes are durably written.
        for (file_id, state, note) in states {
            self.db.set_file_state(file_id, state, note.as_deref())?;
        }
        for (k, thumb) in &resp.thumbs {
            let Some((target, _row, ref_id)) = parse_key(k) else { continue };
            match target {
                EmbedRowKind::Clip => self.db.set_clip_thumb(ref_id, thumb)?,
                EmbedRowKind::Image => self.db.set_file_meta(ref_id, None, None, None, Some(thumb))?,
                EmbedRowKind::Chunk => {}
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------- per kind

    fn prepare_image(&self, f: &FileRecord, want_vectors: bool) -> Result<Prepared> {
        if !Path::new(&f.path).exists() {
            return Err(Error::Unsupported("文件已不存在".into()));
        }
        if f.width.is_none() {
            if let Some((w, h)) = scan::image_dimensions(Path::new(&f.path)) {
                self.db.set_file_meta(f.id, None, Some(w as i32), Some(h as i32), None)?;
            }
        }
        if !want_vectors {
            // an image without a model carries only metadata, so nothing is searchable
            return Err(Error::Unsupported("未启用图片语义索引".into()));
        }
        let row = self.db.reserve_vec_row(EmbedRowKind::Image, f.id, f.id)?;
        let k = key(row, EmbedRowKind::Image, f.id);
        let thumb = self.paths.thumb_path(&f.path);
        Ok(Prepared::Embed {
            items: vec![EmbedItem::Image { key: k.clone(), path: f.path.clone(), thumb: Some(path_str(&thumb)) }],
            keys: vec![k],
        })
    }

    fn prepare_video(&self, f: &FileRecord, want_vectors: bool) -> Result<Prepared> {
        let client = self.client.as_ref().ok_or_else(|| Error::Infer("推理进程未启动".into()))?;
        if !Path::new(&f.path).exists() {
            return Err(Error::Unsupported("文件已不存在".into()));
        }
        let probe = client.probe(Path::new(&f.path), Duration::from_secs(60))?;
        if !probe.has_video || probe.duration_s <= 0.0 {
            return Err(Error::Unsupported("没有可解析的视频轨".into()));
        }
        self.db.set_file_meta(
            f.id,
            Some((probe.duration_s * 1000.0).round() as i64),
            Some(probe.width),
            Some(probe.height),
            None,
        )?;
        if !want_vectors {
            return Ok(Prepared::Indexed);
        }
        let clips = plan_clips(probe.duration_s, &self.settings);
        if clips.is_empty() {
            return Ok(Prepared::Skip("时长过短".into()));
        }
        let rows = self.db.insert_clips(f.id, &clips)?;
        let mut items = Vec::with_capacity(rows.len());
        let mut keys = Vec::with_capacity(rows.len());
        for (clip_id, ord) in &rows {
            let (start, end) = clips[*ord];
            let row = self.db.reserve_vec_row(EmbedRowKind::Clip, *clip_id, f.id)?;
            let k = key(row, EmbedRowKind::Clip, *clip_id);
            let thumb = self.paths.clip_thumb_path(f.id, *clip_id);
            items.push(EmbedItem::Video {
                key: k.clone(),
                path: f.path.clone(),
                start,
                end,
                fps: Some(self.settings.video_fps),
                thumb: Some(path_str(&thumb)),
            });
            keys.push(k);
        }
        Ok(Prepared::Embed { items, keys })
    }

    fn prepare_text(&self, f: &FileRecord, want_vectors: bool) -> Result<Prepared> {
        let path = Path::new(&f.path);
        if !path.exists() {
            return Err(Error::Unsupported("文件已不存在".into()));
        }
        let ext = f.ext.to_ascii_lowercase();
        let (title, text, truncated) = if ext == "pdf" {
            let client = self
                .client
                .as_ref()
                .ok_or_else(|| Error::Unsupported("PDF 解析需要推理进程".into()))?;
            let (t, body, cut) = client.extract_pdf(path, (self.settings.max_extract_bytes as usize).max(1 << 20))?;
            (t, body, cut)
        } else if extract::native(&ext) {
            let got = extract::extract(path, &ext, self.settings.max_extract_bytes, self.settings.max_text_store_kb)?;
            (got.title, got.text, got.truncated)
        } else {
            return Err(Error::Unsupported(format!(".{ext} 暂不支持提取正文")));
        };
        self.db.store_text(f.id, title.as_deref(), &text, truncated)?;
        if !want_vectors || text.trim().is_empty() {
            return Ok(Prepared::Indexed);
        }
        let chunks = extract::chunk_text(&text, 900, 24);
        if chunks.is_empty() {
            return Ok(Prepared::Indexed);
        }
        let rows = self.db.insert_chunks(f.id, &chunks)?;
        let mut items = Vec::with_capacity(rows.len());
        let mut keys = Vec::with_capacity(rows.len());
        for ((chunk_id, _ord), body) in rows.iter().zip(chunks.iter()) {
            let row = self.db.reserve_vec_row(EmbedRowKind::Chunk, *chunk_id, f.id)?;
            let k = key(row, EmbedRowKind::Chunk, *chunk_id);
            items.push(EmbedItem::Text {
                key: k.clone(),
                text: body.clone(),
                title: title.clone().or_else(|| Some(f.name.clone())),
                role: "document".into(),
            });
            keys.push(k);
        }
        Ok(Prepared::Embed { items, keys })
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// Re-pack the vector file, dropping rows of deleted or re-indexed files.
pub fn compact(db: &Db, store_path: &PathBuf, dim: usize) -> Result<usize> {
    let mapping = db.live_rows_for_compaction()?;
    let mut store = Store::open(store_path, dim)?;
    let kept = mapping.len() as i64;
    store.compact(&mapping)?;
    store.flush()?;
    let dropped = db.apply_compaction(&mapping)?;
    tracing::info!("向量压缩：保留 {kept} 行，回收 {dropped} 行");
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Scanned;
    use crossbeam_channel::Receiver;

    struct Harness {
        dir: tempfile::TempDir,
        db: Arc<Db>,
        paths: Paths,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("i.sqlite")).unwrap());
        let paths = Paths {
            data_dir: dir.path().into(),
            cache_dir: dir.path().into(),
            model_dir: dir.path().into(),
            thumb_dir: dir.path().join("thumbs"),
            clip_thumb_dir: dir.path().join("clip-thumbs"),
            db_path: dir.path().join("i.sqlite"),
            vector_path: dir.path().join("v.f32"),
            settings_path: dir.path().join("s.json"),
            log_dir: dir.path().into(),
        };
        std::fs::create_dir_all(&paths.thumb_dir).unwrap();
        std::fs::create_dir_all(&paths.clip_thumb_dir).unwrap();
        Harness { dir, db, paths }
    }

    fn indexer(h: &Harness, settings: Settings) -> (Indexer, Receiver<Progress>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let idx = Indexer::new(h.db.clone(), h.paths.clone(), settings, None, tx);
        (idx, rx)
    }

    fn text_settings() -> Settings {
        let mut st = Settings::default();
        st.index_images = false;
        st.index_video = false;
        st
    }

    fn add_doc(h: &Harness, name: &str, body: &str) -> FileRecord {
        let root = h.db.list_roots().unwrap().first().map(|r| r.id).unwrap_or(1);
        let seen = crate::db::now_ns();
        let p = h.dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        let s = Scanned {
            root_id: root,
            path: p.to_string_lossy().to_string(),
            name: name.into(),
            ext: name.rsplit('.').next().unwrap_or("").to_lowercase(),
            kind: Kind::from_ext(name.rsplit('.').next().unwrap_or("")),
            size: body.len() as i64,
            mtime_ns: seen,
        };
        let (id, _) = h.db.upsert_file(&s, seen).unwrap();
        h.db.file(id).unwrap()
    }

    #[test]
    fn text_only_indexes_documents_without_vectors() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        add_doc(&h, "note.md", "海边的日落记录，天空很红");
        let (idx, _rx) = indexer(&h, text_settings());
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.done, 1, "{sum:?}");
        let rec = h.db.pending(10, &[Kind::Document]).unwrap();
        assert!(rec.is_empty());
        let stored = h.db.file_by_path(&h.dir.path().join("note.md").to_string_lossy()).unwrap();
        assert_eq!(stored.state, FileState::Done);
        assert!(h.db.stored_text(stored.id).unwrap().contains("海边的日落"));
        assert_eq!(h.db.alive_vec_rows().unwrap(), 0, "text-only must not allocate vectors");
        assert!(!h.paths.vector_path.exists());
    }

    #[test]
    fn chunking_creates_one_item_per_chunk() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        let body = "海边的日落。".repeat(200);
        let rec = add_doc(&h, "long.md", &body);
        let mut settings = text_settings();
        settings.max_text_store_kb = 1;
        let (idx, _rx) = indexer(&h, settings);
        idx.run(Mode::TextOnly, None).unwrap();
        let rows = h.db.chunk_rows(rec.id).unwrap();
        assert!(rows.is_empty(), "text-only does not chunk");
        let stored = h.db.stored_text(rec.id).unwrap();
        assert!(stored.len() < body.len(), "{} >= {}", stored.len(), body.len());
        assert!(
            (1021..=1024).contains(&stored.len()),
            "cap is bytes of UTF-8 cut on a char boundary, got {}",
            stored.len()
        );
    }

    /// 界面照着这些数字画读数。中间帧只带 phase 时会被 `..Default::default()` 冲成零，
    /// 于是跑一整轮都显示「0 / 0 队列为空」——这条钉住「每一帧都带累计值」的约定。
    #[test]
    fn progress_frames_keep_the_running_totals() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        for i in 0..3 {
            add_doc(&h, &format!("n{i}.md"), &format!("第 {i} 篇海边日记"));
        }
        add_doc(&h, "skip.pdf", "not a pdf");
        let (idx, rx) = indexer(&h, text_settings());
        let kinds = idx.kinds_for(Mode::TextOnly);
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        drop(idx);
        let frames: Vec<Progress> = rx.iter().collect();
        for f in &frames {
            assert_eq!(f.discovered, 4, "每一帧都要带总数: {f:?}");
        }
        let running: Vec<&Progress> = frames.iter().filter(|f| f.phase == Phase::Indexing).collect();
        assert!(!running.is_empty(), "索引过程中必须发帧: {frames:?}");
        for f in running {
            assert!(f.queued > 0, "还在跑就不能把队列报成空: {f:?}");
        }
        let last = frames.last().expect("一轮索引至少以收尾帧结束");
        assert_eq!(last.phase, Phase::Idle);
        assert_eq!((last.done, last.skipped, last.queued), (sum.done, sum.skipped, 0));
        assert_eq!(h.db.pending_count(&kinds).unwrap(), h.db.pending(usize::MAX, &kinds).unwrap().len() as u64);
        assert_eq!(h.db.pending_count(&[]).unwrap(), h.db.pending(usize::MAX, &[]).unwrap().len() as u64);
    }

    #[test]
    fn unsupported_ext_is_skipped_not_failed() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        let rec = add_doc(&h, "x.pdf", "not a pdf");
        let (idx, _rx) = indexer(&h, text_settings());
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.skipped, 1, "{sum:?}");
        assert_eq!(h.db.file(rec.id).unwrap().state, FileState::Skipped);
    }

    #[test]
    fn binary_document_is_skipped() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        let p = h.dir.path().join("blob.txt");
        std::fs::write(&p, [0u8, 1, 2, 3, 4]).unwrap();
        let seen = crate::db::now_ns();
        let s = Scanned {
            root_id: 1,
            path: p.to_string_lossy().to_string(),
            name: "blob.txt".into(),
            ext: "txt".into(),
            kind: Kind::Document,
            size: 5,
            mtime_ns: seen,
        };
        let (id, _) = h.db.upsert_file(&s, seen).unwrap();
        let (idx, _rx) = indexer(&h, text_settings());
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.skipped, 1);
        assert_eq!(h.db.file(id).unwrap().state, FileState::Skipped);
    }

    #[test]
    fn key_roundtrip() {
        // clip 3 can live on vector row 42: the two ids must survive the round trip
        let k = key(42, EmbedRowKind::Clip, 3);
        assert_eq!(k, "clip:42:3");
        let (t, r, reference) = parse_key(&k).unwrap();
        assert_eq!(t, EmbedRowKind::Clip);
        assert_eq!(r, 42);
        assert_eq!(reference, 3);
        assert!(parse_key("bogus").is_none());
        assert!(parse_key("image:1").is_none());
    }

    #[test]
    fn kinds_follow_settings() {
        let h = harness();
        let mut st = Settings::default();
        st.index_code = false;
        let (idx, _rx) = indexer(&h, st);
        let kinds = idx.kinds_for(Mode::Full);
        assert!(kinds.contains(&Kind::Image));
        assert!(!kinds.contains(&Kind::Code));
        assert_eq!(idx.kinds_for(Mode::TextOnly), vec![Kind::Document]);
    }

    #[test]
    fn empty_queue_returns_immediately() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        let (idx, _rx) = indexer(&h, text_settings());
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.done, 0);
    }

    #[test]
    fn cancel_stops_the_loop() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        for i in 0..5 {
            add_doc(&h, &format!("n{i}.md"), "内容");
        }
        let (idx, _rx) = indexer(&h, text_settings());
        idx.control().cancel();
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.done, 0);
        assert_eq!(h.db.pending(10, &[Kind::Document]).unwrap().len(), 5);
    }

    #[test]
    fn max_files_limits_the_pass() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        for i in 0..6 {
            add_doc(&h, &format!("m{i}.md"), "内容内容");
        }
        let (idx, _rx) = indexer(&h, text_settings());
        let sum = idx.run(Mode::TextOnly, Some(2)).unwrap();
        assert_eq!(sum.done, 2);
        assert_eq!(h.db.pending(10, &[Kind::Document]).unwrap().len(), 4);
    }

    #[test]
    fn scan_then_index_end_to_end() {
        let h = harness();
        let root_path = h.dir.path().to_string_lossy().to_string();
        let root = h.db.add_root(&root_path, "test").unwrap();
        std::fs::write(h.dir.path().join("a.md"), "山间的雾").unwrap();
        std::fs::write(h.dir.path().join("b.txt"), "湖边的桥").unwrap();
        let (idx, _rx) = indexer(&h, text_settings());
        let stats = idx.scan_root(root, Mode::TextOnly).unwrap();
        assert_eq!(stats.indexed_candidates, 2, "{stats:?}");
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.done, 2);
        let mut req = crate::SearchRequest::default();
        req.query = "雾".into();
        let expr = crate::segment::match_expression(&req.query).unwrap();
        let hits = h.db.keyword_search(&expr, &req, 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn text_only_scan_preserves_existing_media_and_vectors() {
        let h = harness();
        let root = h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        let path = h.dir.path().join("photo.jpg");
        std::fs::write(&path, "placeholder").unwrap();
        let (idx, _) = indexer(&h, Settings::default());
        idx.scan_root(root, Mode::Full).unwrap();
        let photo = h.db.file_by_path(&path.to_string_lossy()).unwrap();
        h.db.set_file_state(photo.id, FileState::Done, None).unwrap();
        h.db.reserve_vec_row(EmbedRowKind::Image, photo.id, photo.id).unwrap();
        idx.scan_root(root, Mode::TextOnly).unwrap();
        assert!(h.db.file(photo.id).is_some(), "a text-only pass must not sweep media");
        assert_eq!(h.db.alive_vec_rows().unwrap(), 1);
    }

    #[test]
    fn embed_failure_requeues_the_batch_without_reopening_the_database() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        let doc = add_doc(&h, "retry.md", "this document must remain retryable");
        let mut settings = text_settings();
        settings.infer_python = Some(h.dir.path().join("missing-python").to_string_lossy().into());
        let client = Arc::new(Client::new(&h.paths, &settings));
        let (tx, _rx) = crossbeam_channel::unbounded();
        let idx = Indexer::new(h.db.clone(), h.paths.clone(), settings, Some(client), tx);
        assert!(idx.run(Mode::Full, None).is_err());
        assert_eq!(h.db.file(doc.id).unwrap().state, FileState::Pending);
        assert_eq!(h.db.pending(10, &[Kind::Document]).unwrap().len(), 1);
        let (retry, _) = indexer(&h, text_settings());
        assert_eq!(retry.run(Mode::TextOnly, None).unwrap().done, 1);
    }

    #[test]
    fn replacing_video_clips_does_not_reuse_old_vector_rows() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        let video = add_doc(&h, "clip.mp4", "placeholder");
        let clips = h.db.insert_clips(video.id, &[(0.0, 1.0)]).unwrap();
        let old = h.db.reserve_vec_row(EmbedRowKind::Clip, clips[0].0, video.id).unwrap();
        let replacement = h.db.insert_clips(video.id, &[(0.0, 2.0)]).unwrap();
        let next = h.db.reserve_vec_row(EmbedRowKind::Clip, replacement[0].0, video.id).unwrap();
        assert!(next > old);
        assert_eq!(h.db.dead_rows().unwrap(), 1);
    }

    #[test]
    fn deleted_file_is_swept_from_the_index() {
        let h = harness();
        let root_path = h.dir.path().to_string_lossy().to_string();
        let root = h.db.add_root(&root_path, "test").unwrap();
        let p = h.dir.path().join("gone.md");
        std::fs::write(&p, "会消失的内容").unwrap();
        let (idx, _rx) = indexer(&h, text_settings());
        idx.scan_root(root, Mode::TextOnly).unwrap();
        idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(h.db.file_by_path(&p.to_string_lossy()).unwrap().state, FileState::Done);
        std::fs::remove_file(&p).unwrap();
        let stats = idx.scan_root(root, Mode::TextOnly).unwrap();
        assert_eq!(stats.indexed_candidates, 0);
        assert!(h.db.file_by_path(&p.to_string_lossy()).is_none());
    }

    #[test]
    fn compact_reclaims_dead_rows() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test").unwrap();
        let seen = crate::db::now_ns();
        let mk = |path: &str| Scanned {
            root_id: 1,
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            ext: "jpg".into(),
            kind: Kind::Image,
            size: 1,
            mtime_ns: seen,
        };
        let a = h.db.upsert_file(&mk("/tmp/one.jpg"), seen).unwrap().0;
        let b = h.db.upsert_file(&mk("/tmp/two.jpg"), seen).unwrap().0;
        h.db.set_file_state(a, FileState::Done, None).unwrap();
        h.db.set_file_state(b, FileState::Done, None).unwrap();
        let r0 = h.db.reserve_vec_row(EmbedRowKind::Image, a, a).unwrap();
        let r1 = h.db.reserve_vec_row(EmbedRowKind::Image, b, b).unwrap();
        let mut store = Store::open(&h.paths.vector_path, 2).unwrap();
        store.put(r0 as usize, &[1.0, 0.0]).unwrap();
        store.put(r1 as usize, &[0.0, 1.0]).unwrap();
        store.flush().unwrap();
        drop(store);
        h.db.delete_by_path("/tmp/one.jpg").unwrap();
        let n = compact(&h.db, &h.paths.vector_path, 2).unwrap();
        assert_eq!(n, 1, "one dead row reclaimed");
        let store = Store::open(&h.paths.vector_path, 2).unwrap();
        assert_eq!(store.rows(), 1);
        let rows = h.db.vec_candidates(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_id, b);
        let view = crate::vectors::MmapView::open(&h.paths.vector_path).unwrap().unwrap();
        let hits = view.search(&[0.0, 1.0], &rows, 4);
        assert!((hits[0].1 - 1.0).abs() < 1e-6, "row pointers must follow compaction");
    }
}
