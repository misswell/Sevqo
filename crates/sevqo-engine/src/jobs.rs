//! The indexer: scan → extract → embed → persist, with pause/cancel and progress.
//!
//! Work is batched per pass because the sidecar is cheapest when it gets a group of
//! like inputs (one torch call for N texts). Row ids are reserved before embedding so
//! the response key `target:row` maps straight back to a slot in the vector file.

use std::path::Path;
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
use crate::vectors::{active_generation, active_vector_path, gen_tmp_path, normalize, Store};
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
    Embed {
        items: Vec<EmbedItem>,
        keys: Vec<String>,
    },
    /// Nothing left to do: text is stored, or the file has no indexable content.
    Indexed,
    /// 关键词索引完成但语义单元未全部持久化（模型不可用期间的 TextOnly 产物）。
    /// 标记为 keyword_only：不进入待处理队列，模型就绪后由 mark_missing_vectors 补齐。
    KeywordOnly,
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
    Some((
        EmbedRowKind::parse(t),
        row.parse().ok()?,
        ref_id.parse().ok()?,
    ))
}

/// `(row, bytes)` pairs destined for the vector file, in response order.
type VectorWrites = Vec<(usize, Vec<f32>)>;

/// One file's share of an embed response, resolved to concrete units: which units
/// produced a usable vector (`ok_units`: target, ref_id, row) and which did not
/// (`failed_units`: target, ref_id), plus the first failure reason.
#[derive(Debug, PartialEq)]
struct FileOutcome {
    file_id: i64,
    ok_units: Vec<(EmbedRowKind, i64, i64)>,
    failed_units: Vec<(EmbedRowKind, i64)>,
    total: usize,
    err: Option<String>,
}

/// Validate an embed response against the reserved keys and split it into rows to
/// persist and units to publish. Pure, so the whole failure matrix — malformed key,
/// duplicate row, wrong dimension, NaN/Inf, missing key — is testable without a sidecar
/// (P0-02). A rejected unit keeps only its staged row: the mapping is never activated,
/// so search can never see a mapping that points at bytes nobody wrote.
fn place_vectors(
    vectors: &std::collections::HashMap<String, Vec<f32>>,
    errors: &std::collections::HashMap<String, String>,
    per_file: &[(i64, Vec<String>)],
    dim: usize,
) -> (VectorWrites, Vec<i64>, Vec<FileOutcome>) {
    let mut writes: VectorWrites = Vec::new();
    let mut rows: Vec<i64> = Vec::new();
    let mut outcomes = Vec::with_capacity(per_file.len());
    let mut claimed: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for (file_id, keys) in per_file {
        let mut ok_units: Vec<(EmbedRowKind, i64, i64)> = Vec::new();
        let mut failed_units: Vec<(EmbedRowKind, i64)> = Vec::new();
        let mut err: Option<String> = None;
        for k in keys {
            let Some((target, row, ref_id)) = parse_key(k) else {
                err = err.or_else(|| Some(format!("向量键格式错误 {k}")));
                failed_units.push((target_of(k), ref_of(k)));
                continue;
            };
            if row < 0 {
                err = err.or_else(|| Some(format!("向量行号 {row} 非法")));
                failed_units.push((target, ref_id));
                continue;
            }
            // Two keys addressing one row is a sidecar protocol bug: the later one would
            // silently overwrite the earlier unit's vector. Claim the row up front, even
            // before looking up the vector, so a duplicate is caught regardless of
            // whether the first claim carried usable bytes.
            if !claimed.insert(row) {
                err = err.or_else(|| Some(format!("向量行 {row} 被多个键占用")));
                failed_units.push((target, ref_id));
                continue;
            }
            let Some(v) = vectors.get(k) else {
                err = err.or_else(|| {
                    Some(
                        errors
                            .get(k)
                            .cloned()
                            .unwrap_or_else(|| "模型未返回向量".into()),
                    )
                });
                failed_units.push((target, ref_id));
                continue;
            };
            let mut v = v.clone();
            if v.len() != dim {
                err = err.or_else(|| Some(format!("向量维度 {} 不符", v.len())));
                failed_units.push((target, ref_id));
                continue;
            }
            if !v.iter().all(|x| x.is_finite()) {
                err = err.or_else(|| Some("向量包含 NaN/Inf".into()));
                failed_units.push((target, ref_id));
                continue;
            }
            normalize(&mut v);
            writes.push((row as usize, v));
            rows.push(row);
            ok_units.push((target, ref_id, row));
        }
        outcomes.push(FileOutcome {
            file_id: *file_id,
            ok_units,
            failed_units,
            total: keys.len(),
            err,
        });
    }
    (writes, rows, outcomes)
}

/// Re-group one bounded request's items by file, preserving response order. Pure so the
/// splitting behavior is testable without a sidecar.
fn regroup_keys(
    group: &[EmbedItem],
    file_of_key: &std::collections::HashMap<&str, i64>,
) -> Vec<(i64, Vec<String>)> {
    let mut out: Vec<(i64, Vec<String>)> = Vec::new();
    for it in group {
        let key = match it {
            EmbedItem::Text { key, .. }
            | EmbedItem::Image { key, .. }
            | EmbedItem::Video { key, .. } => key,
        };
        let fid = file_of_key.get(key.as_str()).copied().unwrap_or(-1);
        match out.last_mut() {
            Some((last_fid, keys)) if *last_fid == fid => keys.push(key.clone()),
            _ => out.push((fid, vec![key.clone()])),
        }
    }
    out
}

/// Best-effort (target, ref) recovery from a malformed key, so the failure still lands
/// on some unit instead of being silently dropped.
fn target_of(key: &str) -> EmbedRowKind {
    key.split(':')
        .next()
        .map(EmbedRowKind::parse)
        .unwrap_or(EmbedRowKind::Image)
}

fn ref_of(key: &str) -> i64 {
    key.split(':')
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1)
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

    /// Open (or create) the vector file on first use. Appends always target the *active*
    /// generation (P0-01); compaction holds the same run slot, so the generation cannot
    /// switch underneath an open handle.
    fn store(&self) -> Result<std::sync::MutexGuard<'_, Option<Store>>> {
        let mut g = self
            .store
            .lock()
            .map_err(|_| Error::Infer("向量存储锁中毒".into()))?;
        if g.is_none() {
            let path = active_vector_path(&self.paths.vector_path, &self.db)?;
            *g = Some(Store::open(&path, self.settings.vector_dim)?);
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
        // 6.2 扫描履历：取消/失败也落一行，且绝不触发 sweep——错误在 `?` 处直接
        // 传播，删除只可能发生在完整成功（无错误、无不可读子树）的扫描之后。
        let scan_result = scan::scan(root_id, &path, &opts, cancel, |f| {
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
        });
        let mut stats = match scan_result {
            Ok(v) => v,
            Err(Error::Cancelled) => {
                let _ = self.db.record_scan_run(
                    root_id,
                    "cancelled",
                    seen,
                    candidates as i64,
                    queued as i64,
                    0,
                );
                return Err(Error::Cancelled);
            }
            Err(e) => {
                let _ = self.db.record_scan_run(
                    root_id,
                    "failed",
                    seen,
                    candidates as i64,
                    queued as i64,
                    0,
                );
                return Err(e);
            }
        };
        stats.errors += write_errors;
        // An unreadable subtree is not evidence that its files were deleted.
        let removed = if stats.errors == 0 && stats.unreadable == 0 {
            self.db.sweep(root_id, seen)?
        } else {
            0
        };
        let queued_now = self.db.pending_count(&kinds)?;
        let _ = self.db.record_scan_run(
            root_id,
            "completed",
            seen,
            stats.walked as i64,
            queued_now as i64,
            removed as i64,
        );
        progress.discovered = stats.walked;
        progress.queued = queued_now;
        progress.message = Some(format!(
            "扫描完成：新增 {queued} 个，清理 {removed} 个已删除"
        ));
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
            discovered: self
                .db
                .counts()
                .map(|c| c.values().sum::<i64>().max(0) as u64)
                .unwrap_or(0),
            queued: self.db.pending_count(&kinds).unwrap_or(0),
            ..Default::default()
        };
        let _ = self.emit.try_send(live.clone());
        loop {
            if self.control.is_cancelled() {
                break;
            }
            while self.control.is_paused() {
                let _ = self.emit.try_send(Progress {
                    phase: Phase::Paused,
                    ..live.clone()
                });
                std::thread::sleep(Duration::from_millis(300));
                if self.control.is_cancelled() {
                    sum.ms = started.elapsed().as_secs_f64() * 1000.0;
                    return Ok(sum);
                }
            }
            let processed = sum.done + sum.failed + sum.skipped;
            let want = max_files
                .map(|m| m.saturating_sub(processed as usize))
                .unwrap_or(usize::MAX);
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
                    live.images_per_min = if ms > 1.0 {
                        sum.done as f64 / (ms / 60.0)
                    } else {
                        0.0
                    };
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
        let per_min = if sum.ms > 1.0 {
            sum.done as f64 / (sum.ms / 60_000.0)
        } else {
            0.0
        };
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
        let _recovery = BatchRecovery {
            db: &self.db,
            ids: files.iter().map(|f| f.id).collect(),
        };
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
                Ok(Prepared::KeywordOnly) => {
                    self.db.set_file_state(
                        f.id,
                        FileState::KeywordOnly,
                        Some("已建立关键词索引；语义索引待模型可用后补齐"),
                    )?;
                    out.done += 1;
                }
                Ok(Prepared::Skip(reason)) => {
                    self.db
                        .set_file_state(f.id, FileState::Skipped, Some(&reason))?;
                    out.skipped += 1;
                }
                Err(e) => {
                    let msg = e.to_string();
                    let state = if matches!(e, Error::Unsupported(_)) {
                        FileState::Skipped
                    } else {
                        FileState::Failed
                    };
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

        // 单个请求有界（P1-06 的第一步）：长文档/长视频不再把几百个单元塞进一次
        // 请求——JSON、Python 内存和取消等待都随之有界。每个子批独立落盘、独立
        // 激活、独立记账单元状态：中途崩溃只损失未完成子批，已持久化的单元保留。
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| Error::Infer("推理进程未启动".into()))?;
        let batch = self.settings.infer_batch.max(1);
        let group_size = MAX_EMBED_ITEMS_PER_REQUEST.max(batch);
        // key -> file：place_vectors 需要按文件归组，而分组会被子批切开。
        let file_of_key: std::collections::HashMap<&str, i64> = per_file
            .iter()
            .flat_map(|(fid, keys)| keys.iter().map(move |k| (k.as_str(), *fid)))
            .collect();
        // 每个文件还有多少 key 未结算：最后一个子批结算完才聚合文件状态。
        let mut remaining: std::collections::HashMap<i64, usize> = per_file
            .iter()
            .map(|(fid, keys)| (*fid, keys.len()))
            .collect();
        let mut first_error: std::collections::HashMap<i64, String> =
            std::collections::HashMap::new();

        for group in items.chunks(group_size) {
            if self.control.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let timeout = Duration::from_secs(180 + 60 * (group.len() / batch) as u64);
            let resp = client.embed(group, batch, timeout)?;

            let group_per_file = regroup_keys(group, &file_of_key);
            let (writes, rows, outcomes) = place_vectors(
                &resp.vectors,
                &resp.errors,
                &group_per_file,
                self.settings.vector_dim,
            );
            for fo in &outcomes {
                out.vectors += fo.ok_units.len() as u64;
                if let Some(err) = &fo.err {
                    first_error.entry(fo.file_id).or_insert_with(|| err.clone());
                }
            }

            if !writes.is_empty() {
                let mut g = self.store()?;
                if let Some(store) = g.as_mut() {
                    store.put_many(&writes)?;
                    // fsync before publishing: a mapping must never commit ahead of its bytes.
                    store.flush()?;
                }
            }
            // Bytes are durable — publish rows, then unit states. All sit behind the
            // search-visibility gate (alive=1 AND state='done'), so a crash between
            // these steps can expose neither a staged row nor a vector-less done file.
            // Failed units keep their state and get retried individually (P0-03).
            self.db.activate_vec_rows(&rows)?;
            for fo in &outcomes {
                let ok_units: Vec<(EmbedRowKind, i64)> =
                    fo.ok_units.iter().map(|(t, r, _)| (*t, *r)).collect();
                self.db.mark_units_persisted(&ok_units)?;
                if !fo.failed_units.is_empty() {
                    let msg = fo.err.clone().unwrap_or_else(|| "模型未返回向量".into());
                    self.db.mark_units_failed(&fo.failed_units, &msg)?;
                }
            }
            // 本文件的全部子批都结算完，才用单元终态聚合文件状态并计数。
            for fo in &outcomes {
                let entry = remaining.entry(fo.file_id).or_insert(0);
                *entry = entry.saturating_sub(fo.total);
                if *entry > 0 {
                    continue;
                }
                let synthetic = FileOutcome {
                    file_id: fo.file_id,
                    ok_units: vec![],
                    failed_units: vec![],
                    total: 0,
                    err: first_error.get(&fo.file_id).cloned(),
                };
                let (state, note) = aggregate_file_state(&self.db, fo.file_id, &synthetic)?;
                match state {
                    FileState::Done => out.done += 1,
                    FileState::Failed => out.failed += 1,
                    _ => {}
                }
                self.db.set_file_state(fo.file_id, state, note.as_deref())?;
            }
            for (k, thumb) in &resp.thumbs {
                let Some((target, _row, ref_id)) = parse_key(k) else {
                    continue;
                };
                match target {
                    EmbedRowKind::Clip => self.db.set_clip_thumb(ref_id, thumb)?,
                    EmbedRowKind::Image => {
                        self.db
                            .set_file_meta(ref_id, None, None, None, Some(thumb))?
                    }
                    EmbedRowKind::Chunk => {}
                }
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
                self.db
                    .set_file_meta(f.id, None, Some(w as i32), Some(h as i32), None)?;
            }
        }
        if !want_vectors {
            // an image without a model carries only metadata, so nothing is searchable
            return Err(Error::Unsupported("未启用图片语义索引".into()));
        }
        // The image's single unit is the file itself: an activated vector row means this
        // unit is already persisted — do not re-encode an unchanged picture (P0-03).
        if self.db.has_active_vector(EmbedRowKind::Image, f.id)? {
            return Ok(Prepared::Indexed);
        }
        let row = self.db.reserve_vec_row(EmbedRowKind::Image, f.id, f.id)?;
        let k = key(row, EmbedRowKind::Image, f.id);
        let thumb = self.paths.thumb_path(&f.path);
        Ok(Prepared::Embed {
            items: vec![EmbedItem::Image {
                key: k.clone(),
                path: f.path.clone(),
                thumb: Some(path_str(&thumb)),
            }],
            keys: vec![k],
        })
    }

    fn prepare_video(&self, f: &FileRecord, want_vectors: bool) -> Result<Prepared> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| Error::Infer("推理进程未启动".into()))?;
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
            return Ok(if self.db.has_incomplete_units(f.id)? {
                Prepared::KeywordOnly
            } else {
                Prepared::Indexed
            });
        }
        let clips = plan_clips(probe.duration_s, &self.settings);
        if clips.is_empty() {
            return Ok(Prepared::Skip("时长过短".into()));
        }
        self.db.sync_clips(f.id, &clips)?;
        // Per-unit retry: only clips without a persisted vector are embedded this pass.
        let todo = self.db.clips_needing_embed(f.id)?;
        if todo.is_empty() {
            return Ok(Prepared::Indexed);
        }
        let mut items = Vec::with_capacity(todo.len());
        let mut keys = Vec::with_capacity(todo.len());
        for (clip_id, start, end) in todo {
            let row = self.db.reserve_vec_row(EmbedRowKind::Clip, clip_id, f.id)?;
            let k = key(row, EmbedRowKind::Clip, clip_id);
            let thumb = self.paths.clip_thumb_path(f.id, clip_id);
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
            let (t, body, cut) = client.extract_pdf(
                path,
                (self.settings.max_extract_bytes as usize).max(1 << 20),
            )?;
            (t, body, cut)
        } else if extract::native(&ext) {
            let got = extract::extract(
                path,
                &ext,
                self.settings.max_extract_bytes,
                self.settings.max_text_store_kb,
            )?;
            (got.title, got.text, got.truncated)
        } else {
            return Err(Error::Unsupported(format!(".{ext} 暂不支持提取正文")));
        };
        self.db
            .store_text(f.id, title.as_deref(), &text, truncated)?;
        if !want_vectors || text.trim().is_empty() {
            // TextOnly 不得把「部分语义完成」伪装成完整 Done（P1 修复）：
            // 之前成功过的单元保持原状，缺失的等模型就绪后由 mark_missing_vectors 补。
            if text.trim().is_empty() {
                return Ok(Prepared::Indexed);
            }
            return Ok(if self.db.has_incomplete_units(f.id)? {
                Prepared::KeywordOnly
            } else {
                Prepared::Indexed
            });
        }
        let chunks = extract::chunk_text(&text, 900, MAX_CHUNKS_PER_FILE);
        if chunks.is_empty() {
            return Ok(Prepared::Indexed);
        }
        self.db.sync_chunks(f.id, &chunks)?;
        // Per-unit retry: chunks already persisted are not re-embedded (P0-03).
        let todo = self.db.chunks_needing_embed(f.id)?;
        if todo.is_empty() {
            return Ok(Prepared::Indexed);
        }
        let mut items = Vec::with_capacity(todo.len());
        let mut keys = Vec::with_capacity(todo.len());
        for (chunk_id, body) in todo {
            let row = self
                .db
                .reserve_vec_row(EmbedRowKind::Chunk, chunk_id, f.id)?;
            let k = key(row, EmbedRowKind::Chunk, chunk_id);
            items.push(EmbedItem::Text {
                key: k.clone(),
                text: body,
                title: title.clone().or_else(|| Some(f.name.clone())),
                role: "document".into(),
            });
            keys.push(k);
        }
        Ok(Prepared::Embed { items, keys })
    }
}

#[cfg(test)]
fn chunk_id_of(db: &Db, file_id: i64, ord: usize) -> i64 {
    db.chunk_rows(file_id).unwrap()[ord].0
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// How many attempts a unit gets before its file stops re-queueing and reports the
/// failure with the recorded reason. A manual re-index resets the counters.
const UNIT_RETRY_MAX: i64 = 3;

/// Upper bound of one inference request: keeps the JSON payload, the sidecar's memory
/// and the cancel-wait bounded no matter how many chunks a long document produced.
const MAX_EMBED_ITEMS_PER_REQUEST: usize = 256;

/// 语义索引每份文档的最大片段数。超过后正文仍完整进入 FTS，但语义向量只覆盖前
/// N 片——文件状态会带说明，绝不静默（审查 P1：原 24 片硬截断导致「关键词能搜到、
/// 语义搜不到」且无人知晓）。
const MAX_CHUNKS_PER_FILE: usize = 2048;

/// Fold unit states into the file-level state (P0-03). A file is `done` only when every
/// unit is persisted; retryable failures re-queue it as `stale` with an explanatory
/// note, exhausted failures surface as a diagnosable `failed`. Image files have no unit
/// rows — their single unit is the file itself, decided by whether this pass produced
/// its vector.
fn aggregate_file_state(
    db: &Db,
    file_id: i64,
    fo: &FileOutcome,
) -> Result<(FileState, Option<String>)> {
    let sum = db.unit_summary(file_id, UNIT_RETRY_MAX)?;
    let total = sum.total;
    let persisted = sum.persisted;
    let exhausted = sum.exhausted;
    let unit_err = sum.error;
    if total == 0 {
        return Ok(if fo.ok_units.is_empty() {
            (
                FileState::Failed,
                Some(fo.err.clone().unwrap_or_else(|| "模型未返回向量".into())),
            )
        } else {
            (FileState::Done, None)
        });
    }
    if exhausted > 0 {
        let detail = unit_err.unwrap_or_else(|| "嵌入失败".into());
        return Ok((
            FileState::Failed,
            Some(format!(
                "{exhausted}/{total} 个片段重复失败（已重试 {UNIT_RETRY_MAX} 次）：{detail}"
            )),
        ));
    }
    if persisted == total {
        // 语义覆盖范围要诚实：正文超过片段预算时说明只覆盖了前 N 片，
        // 全文仍可按关键词检索。doc.chars 是提取时的真实字符数，跨轮次稳定可推导。
        let note = db
            .doc_char_count(file_id)
            .ok()
            .filter(|chars| *chars > (MAX_CHUNKS_PER_FILE as i64).saturating_mul(900))
            .map(|_| {
                format!(
                    "正文过长，语义索引覆盖前 {MAX_CHUNKS_PER_FILE} 个片段；全文仍可按关键词检索"
                )
            });
        return Ok((FileState::Done, note));
    }
    let waiting = total - persisted;
    let detail = fo
        .err
        .clone()
        .or(unit_err)
        .unwrap_or_else(|| "嵌入失败".into());
    Ok((
        FileState::Stale,
        Some(format!(
            "{persisted}/{total} 个片段已索引，{waiting} 个待重试：{detail}"
        )),
    ))
}

/// Re-pack the vector file, dropping rows of deleted or re-indexed files (P0-01).
///
/// Compaction never touches the live file: it streams a new generation from the old one,
/// fsyncs it, then commits the row remap and the generation flip in one SQLite
/// transaction — that commit is the switch point. Old generations are unlinked after the
/// commit (compaction holds the run slot and the vector write lock, so nothing is
/// reading or appending).
pub fn compact(db: &Db, store_path: &Path, dim: usize) -> Result<usize> {
    // A failed or interrupted embed reserves rows that never received bytes. Left alive,
    // they are copied as zero rows forever, and a reservation past the end of the vector
    // file makes compaction abort with 越界. Tombstone them first — nothing is mid-embed
    // here because compaction holds the run slot.
    let freed = db.invalidate_stale_rows()?;
    if freed > 0 {
        tracing::info!("压缩前回收 {freed} 行失效向量");
    }
    let dead = db.dead_rows()?;
    let mapping = db.live_rows_for_compaction()?;
    if dead == 0 {
        // 没有可回收的行：不制造新 generation，避免反复点击压缩徒增文件翻转。
        return Ok(0);
    }
    let active = active_vector_path(store_path, db)?;
    let next = active_generation(db) + 1;
    let tmp = gen_tmp_path(store_path, next);
    let kept = mapping.len() as i64;
    let sha = crate::vectors::write_generation(&active, &tmp, dim, &mapping)?;
    let dropped = db.commit_compaction(&mapping, next, mapping.len(), dim, &sha)?;
    // 旧 Generation 不在此处删除（方案 2.2 延迟清理）：保留到下一次启动校验通过之后，
    // 给「新代损坏时人工/工具恢复」留一个窗口。mmap 语义下保留旧文件也没有读者风险。
    tracing::info!("向量压缩：generation {next}，保留 {kept} 行，回收 {dropped} 行；旧代文件延迟到下次启动清理");
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
        Settings {
            index_images: false,
            index_video: false,
            ..Settings::default()
        }
    }

    fn add_doc(h: &Harness, name: &str, body: &str) -> FileRecord {
        let root =
            h.db.list_roots()
                .unwrap()
                .first()
                .map(|r| r.id)
                .unwrap_or(1);
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
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        add_doc(&h, "note.md", "海边的日落记录，天空很红");
        let (idx, _rx) = indexer(&h, text_settings());
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.done, 1, "{sum:?}");
        let rec = h.db.pending(10, &[Kind::Document]).unwrap();
        assert!(rec.is_empty());
        let stored =
            h.db.file_by_path(&h.dir.path().join("note.md").to_string_lossy())
                .unwrap();
        assert_eq!(stored.state, FileState::Done);
        assert!(h.db.stored_text(stored.id).unwrap().contains("海边的日落"));
        assert_eq!(
            h.db.alive_vec_rows().unwrap(),
            0,
            "text-only must not allocate vectors"
        );
        assert!(!h.paths.vector_path.exists());
    }

    #[test]
    fn chunking_creates_one_item_per_chunk() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let body = "海边的日落。".repeat(200);
        let rec = add_doc(&h, "long.md", &body);
        let mut settings = text_settings();
        settings.max_text_store_kb = 1;
        let (idx, _rx) = indexer(&h, settings);
        idx.run(Mode::TextOnly, None).unwrap();
        let rows = h.db.chunk_rows(rec.id).unwrap();
        assert!(rows.is_empty(), "text-only does not chunk");
        let stored = h.db.stored_text(rec.id).unwrap();
        assert!(
            stored.len() < body.len(),
            "{} >= {}",
            stored.len(),
            body.len()
        );
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
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
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
        let running: Vec<&Progress> = frames
            .iter()
            .filter(|f| f.phase == Phase::Indexing)
            .collect();
        assert!(!running.is_empty(), "索引过程中必须发帧: {frames:?}");
        for f in running {
            assert!(f.queued > 0, "还在跑就不能把队列报成空: {f:?}");
        }
        let last = frames.last().expect("一轮索引至少以收尾帧结束");
        assert_eq!(last.phase, Phase::Idle);
        assert_eq!(
            (last.done, last.skipped, last.queued),
            (sum.done, sum.skipped, 0)
        );
        assert_eq!(
            h.db.pending_count(&kinds).unwrap(),
            h.db.pending(usize::MAX, &kinds).unwrap().len() as u64
        );
        assert_eq!(
            h.db.pending_count(&[]).unwrap(),
            h.db.pending(usize::MAX, &[]).unwrap().len() as u64
        );
    }

    #[test]
    fn unsupported_ext_is_skipped_not_failed() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let rec = add_doc(&h, "x.pdf", "not a pdf");
        let (idx, _rx) = indexer(&h, text_settings());
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.skipped, 1, "{sum:?}");
        assert_eq!(h.db.file(rec.id).unwrap().state, FileState::Skipped);
    }

    #[test]
    fn binary_document_is_skipped() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
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
        let st = Settings {
            index_code: false,
            ..Settings::default()
        };
        let (idx, _rx) = indexer(&h, st);
        let kinds = idx.kinds_for(Mode::Full);
        assert!(kinds.contains(&Kind::Image));
        assert!(!kinds.contains(&Kind::Code));
        assert_eq!(idx.kinds_for(Mode::TextOnly), vec![Kind::Document]);
    }

    #[test]
    fn empty_queue_returns_immediately() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let (idx, _rx) = indexer(&h, text_settings());
        let sum = idx.run(Mode::TextOnly, None).unwrap();
        assert_eq!(sum.done, 0);
    }

    #[test]
    fn cancel_stops_the_loop() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
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
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
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
        let req = crate::SearchRequest {
            query: "雾".into(),
            ..Default::default()
        };
        let expr = crate::segment::match_expression(&req.query).unwrap();
        let hits = h.db.keyword_search(&expr, &req, 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn text_only_scan_preserves_existing_media_and_vectors() {
        let h = harness();
        let root =
            h.db.add_root(&h.dir.path().to_string_lossy(), "test")
                .unwrap();
        let path = h.dir.path().join("photo.jpg");
        std::fs::write(&path, "placeholder").unwrap();
        let (idx, _) = indexer(&h, Settings::default());
        idx.scan_root(root, Mode::Full).unwrap();
        let photo = h.db.file_by_path(&path.to_string_lossy()).unwrap();
        h.db.set_file_state(photo.id, FileState::Done, None)
            .unwrap();
        let row =
            h.db.reserve_vec_row(EmbedRowKind::Image, photo.id, photo.id)
                .unwrap();
        h.db.activate_vec_rows(&[row]).unwrap();
        idx.scan_root(root, Mode::TextOnly).unwrap();
        assert!(
            h.db.file(photo.id).is_some(),
            "a text-only pass must not sweep media"
        );
        assert_eq!(h.db.alive_vec_rows().unwrap(), 1);
    }

    #[test]
    fn embed_failure_requeues_the_batch_without_reopening_the_database() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
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
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let video = add_doc(&h, "clip.mp4", "placeholder");
        let clips = h.db.sync_clips(video.id, &[(0.0, 1.0)]).unwrap();
        let old =
            h.db.reserve_vec_row(EmbedRowKind::Clip, clips[0].0, video.id)
                .unwrap();
        let replacement = h.db.sync_clips(video.id, &[(0.0, 2.0)]).unwrap();
        let next =
            h.db.reserve_vec_row(EmbedRowKind::Clip, replacement[0].0, video.id)
                .unwrap();
        assert!(next > old);
        // 预留即 staging（alive=0）：旧行作废 + 新行未激活，两行都还不可搜索。
        assert_eq!(h.db.alive_vec_rows().unwrap(), 0);
        assert_eq!(h.db.dead_rows().unwrap(), 2);
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
        assert_eq!(
            h.db.file_by_path(&p.to_string_lossy()).unwrap().state,
            FileState::Done
        );
        std::fs::remove_file(&p).unwrap();
        let stats = idx.scan_root(root, Mode::TextOnly).unwrap();
        assert_eq!(stats.indexed_candidates, 0);
        assert!(h.db.file_by_path(&p.to_string_lossy()).is_none());
    }

    #[test]
    fn compact_reclaims_dead_rows() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
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
        h.db.activate_vec_rows(&[r0, r1]).unwrap();
        h.db.delete_by_path("/tmp/one.jpg").unwrap();
        let n = compact(&h.db, &h.paths.vector_path, 2).unwrap();
        assert_eq!(n, 1, "one dead row reclaimed");
        // 压缩产出新 generation 文件；通过解析出的活动路径重开，而不是旧 base 名。
        let active = crate::vectors::active_vector_path(&h.paths.vector_path, &h.db).unwrap();
        let store = Store::open(&active, 2).unwrap();
        assert_eq!(store.rows(), 1);
        let rows = h.db.vec_candidates(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_id, b);
        let view = crate::vectors::MmapView::open(&active).unwrap().unwrap();
        let hits = view.search(&[0.0, 1.0], &rows, 4);
        assert!(
            (hits[0].1 - 1.0).abs() < 1e-6,
            "row pointers must follow compaction"
        );
    }

    /// 重建（不清空向量文件）不得让旧行字节冒充新向量：reindex_all 清掉映射后，
    /// 新一轮预留同一行号，只有这一轮的字节落盘并激活后该行才可见；
    /// 推理失败时旧行内容必须保持不可见（P0 数据污染回归）。
    #[test]
    fn rebuilding_without_wiping_never_serves_stale_row_bytes() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
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
        let a = h.db.upsert_file(&mk("/tmp/a.jpg"), seen).unwrap().0;
        h.db.set_file_state(a, FileState::Done, None).unwrap();
        let old_row = h.db.reserve_vec_row(EmbedRowKind::Image, a, a).unwrap();
        let mut store = Store::open(&h.paths.vector_path, 2).unwrap();
        store.put(old_row as usize, &[1.0, 0.0]).unwrap();
        store.flush().unwrap();
        drop(store);
        h.db.activate_vec_rows(&[old_row]).unwrap();
        assert_eq!(h.db.vec_candidates(&[]).unwrap().len(), 1);

        // 保留向量文件重建：映射清空，文件回到队列，重新预留同一行号。
        h.db.reindex_all().unwrap();
        assert_eq!(h.db.vec_rows().unwrap(), 0, "映射已清空");
        let row = h.db.reserve_vec_row(EmbedRowKind::Image, a, a).unwrap();
        assert_eq!(row, old_row, "新一轮从同一行号重新分配");
        // 这轮推理失败：字节没写、映射没激活。
        assert!(
            h.db.vec_candidates(&[]).unwrap().is_empty(),
            "旧行字节不得冒充本轮向量"
        );
        assert_eq!(h.db.alive_vec_rows().unwrap(), 0);

        // 推理成功：新字节落盘并激活后才可见。
        let mut store = Store::open(&h.paths.vector_path, 2).unwrap();
        store.put(row as usize, &[0.0, 1.0]).unwrap();
        store.flush().unwrap();
        drop(store);
        h.db.activate_vec_rows(&[row]).unwrap();
        h.db.set_file_state(a, FileState::Done, None).unwrap();
        let rows = h.db.vec_candidates(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        let view = crate::vectors::MmapView::open(&h.paths.vector_path)
            .unwrap()
            .unwrap();
        let hits = view.search(&[0.0, 1.0], &rows, 4);
        assert!(
            (hits[0].1 - 1.0).abs() < 1e-6,
            "可见的必须是本轮新向量，不是旧字节"
        );
    }

    /// 嵌入失败只预留了行、字节没落盘：这些行必须被压缩回收。修复前它们被当作
    /// 活行拷贝成零向量；若向量文件还没建立，`Store::compact` 更会因行越界整体失败。
    #[test]
    fn compact_reclaims_rows_reserved_by_failed_embeds() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
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
        let ok = h.db.upsert_file(&mk("/tmp/ok.jpg"), seen).unwrap().0;
        let bad = h.db.upsert_file(&mk("/tmp/bad.jpg"), seen).unwrap().0;
        h.db.set_file_state(ok, FileState::Done, None).unwrap();
        h.db.set_file_state(bad, FileState::Done, None).unwrap();
        let r_ok = h.db.reserve_vec_row(EmbedRowKind::Image, ok, ok).unwrap();
        let r_bad = h.db.reserve_vec_row(EmbedRowKind::Image, bad, bad).unwrap();
        assert!(r_bad > r_ok);
        // ok 的行落盘并激活；bad 的行只预留不落盘，也没有激活——这正是嵌入半途失败
        // 留下的现场。
        let mut store = Store::open(&h.paths.vector_path, 2).unwrap();
        store.put(r_ok as usize, &[1.0, 0.0]).unwrap();
        store.flush().unwrap();
        drop(store);
        h.db.activate_vec_rows(&[r_ok]).unwrap();
        h.db.set_file_state(bad, FileState::Failed, Some("嵌入失败"))
            .unwrap();

        let dropped = compact(&h.db, &h.paths.vector_path, 2).unwrap();

        assert_eq!(dropped, 1, "失败文件占的行要被回收");
        assert_eq!(h.db.alive_vec_rows().unwrap(), 1);
        let rows = h.db.vec_candidates(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_id, ok);
    }

    /// 只有失败预留、向量文件一个字节都还没有：压缩必须成功而不是报「越界」。
    #[test]
    fn compact_survives_reservations_beyond_the_vector_file() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let seen = crate::db::now_ns();
        let s = Scanned {
            root_id: 1,
            path: "/tmp/only.jpg".into(),
            name: "only.jpg".into(),
            ext: "jpg".into(),
            kind: Kind::Image,
            size: 1,
            mtime_ns: seen,
        };
        let id = h.db.upsert_file(&s, seen).unwrap().0;
        h.db.set_file_state(id, FileState::Done, None).unwrap();
        h.db.reserve_vec_row(EmbedRowKind::Image, id, id).unwrap();
        h.db.set_file_state(id, FileState::Failed, Some("嵌入失败"))
            .unwrap();
        assert!(!h.paths.vector_path.exists(), "该场景下向量文件尚未建立");

        let dropped = compact(&h.db, &h.paths.vector_path, 2).unwrap();

        assert_eq!(dropped, 1);
        assert_eq!(h.db.vec_rows().unwrap(), 0, "没有任何活行");
        assert_eq!(h.db.vec_candidates(&[]).unwrap().len(), 0);
    }

    // ------------------------------------------------ P0-02: 持久化后才提交映射

    /// 预留（staging）与激活是两步：字节没落盘激活前，映射对搜索不可见；
    /// 落盘并激活后才可见。这就是「写入中断不产生可见的无效映射」的保证。
    #[test]
    fn staged_rows_stay_invisible_until_their_bytes_are_persisted() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let seen = crate::db::now_ns();
        let s = Scanned {
            root_id: 1,
            path: "/tmp/a.jpg".into(),
            name: "a.jpg".into(),
            ext: "jpg".into(),
            kind: Kind::Image,
            size: 1,
            mtime_ns: seen,
        };
        let id = h.db.upsert_file(&s, seen).unwrap().0;
        h.db.set_file_state(id, FileState::Done, None).unwrap();
        let row = h.db.reserve_vec_row(EmbedRowKind::Image, id, id).unwrap();
        let mut store = Store::open(&h.paths.vector_path, 2).unwrap();
        store.put(row as usize, &[0.0, 1.0]).unwrap();
        store.flush().unwrap();
        drop(store);

        // 只写盘、没提交映射：搜索必须看不到它。
        assert_eq!(h.db.alive_vec_rows().unwrap(), 0);
        assert!(h.db.vec_candidates(&[]).unwrap().is_empty());

        h.db.activate_vec_rows(&[row]).unwrap();
        assert_eq!(h.db.alive_vec_rows().unwrap(), 1);
        let rows = h.db.vec_candidates(&[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_id, id);
        // 激活是幂等的：重复提交不得产生第二条有效映射。
        h.db.activate_vec_rows(&[row]).unwrap();
        assert_eq!(h.db.alive_vec_rows().unwrap(), 1);
    }

    fn kf(row: i64, target: EmbedRowKind, ref_id: i64) -> String {
        format!("{}:{}:{}", target.as_str(), row, ref_id)
    }

    #[test]
    fn place_vectors_rejects_missing_keys_and_relayed_errors() {
        let mut vectors = std::collections::HashMap::new();
        vectors.insert(kf(0, EmbedRowKind::Image, 1), vec![1.0, 0.0]);
        let mut errors = std::collections::HashMap::new();
        errors.insert(kf(1, EmbedRowKind::Image, 2), "解码失败".to_string());
        let per_file = vec![
            (
                11,
                vec![kf(0, EmbedRowKind::Image, 1), kf(1, EmbedRowKind::Image, 2)],
            ),
            (12, vec![kf(2, EmbedRowKind::Image, 3)]),
        ];

        let (writes, rows, outcomes) = place_vectors(&vectors, &errors, &per_file, 2);

        assert_eq!(writes, vec![(0, vec![1.0, 0.0])]);
        assert_eq!(rows, vec![0]);
        assert_eq!(
            outcomes[0],
            FileOutcome {
                file_id: 11,
                ok_units: vec![(EmbedRowKind::Image, 1, 0)],
                failed_units: vec![(EmbedRowKind::Image, 2)],
                total: 2,
                err: Some("解码失败".into())
            }
        );
        assert_eq!(
            outcomes[1],
            FileOutcome {
                file_id: 12,
                ok_units: vec![],
                failed_units: vec![(EmbedRowKind::Image, 3)],
                total: 1,
                err: Some("模型未返回向量".into())
            }
        );
    }

    #[test]
    fn place_vectors_rejects_wrong_dimension_nan_and_duplicate_rows() {
        let mut vectors = std::collections::HashMap::new();
        vectors.insert(kf(0, EmbedRowKind::Image, 1), vec![1.0, 0.0, 9.0]); // 维度不符
        vectors.insert(kf(1, EmbedRowKind::Image, 2), vec![0.0, f32::NAN]); // NaN
        vectors.insert(kf(2, EmbedRowKind::Image, 3), vec![0.0, 1.0]);
        vectors.insert(kf(3, EmbedRowKind::Image, 4), vec![1.0, 0.0]);
        vectors.insert(kf(4, EmbedRowKind::Image, 5), vec![0.5, 0.5]);
        let per_file = vec![
            (
                21,
                vec![kf(0, EmbedRowKind::Image, 1), kf(1, EmbedRowKind::Image, 2)],
            ),
            (22, vec![kf(2, EmbedRowKind::Image, 3)]),
            (
                23,
                vec![kf(3, EmbedRowKind::Image, 4), kf(4, EmbedRowKind::Image, 5)],
            ),
            (24, vec![kf(2, EmbedRowKind::Chunk, 3)]), // 与文件 22 抢同一行
        ];

        let (writes, rows, outcomes) = place_vectors(&vectors, &errors_empty(), &per_file, 2);

        // 22 与 23 的正常向量被接受；21 维度/NaN 被拒；24 抢行失败。
        assert_eq!(writes.len(), 3);
        assert_eq!(rows, vec![2, 3, 4]);
        assert!(outcomes[0].ok_units.is_empty());
        assert_eq!(outcomes[0].err, Some("向量维度 3 不符".into()));
        assert_eq!(outcomes[1].ok_units.len(), 1);
        assert_eq!(outcomes[2].ok_units.len(), 2, "正常文件不受别的键影响");
        assert_eq!(outcomes[3].err.as_deref(), Some("向量行 2 被多个键占用"));
    }

    #[test]
    fn place_vectors_rejects_malformed_and_negative_keys() {
        let mut vectors = std::collections::HashMap::new();
        vectors.insert("junk".to_string(), vec![1.0, 0.0]);
        vectors.insert(kf(-7, EmbedRowKind::Image, 1), vec![1.0, 0.0]);
        let per_file = vec![
            (31, vec!["junk".to_string()]),
            (32, vec![kf(-7, EmbedRowKind::Image, 1)]),
        ];

        let (writes, rows, outcomes) = place_vectors(&vectors, &errors_empty(), &per_file, 2);

        assert!(writes.is_empty());
        assert!(rows.is_empty());
        assert_eq!(outcomes[0].err, Some("向量键格式错误 junk".into()));
        assert_eq!(outcomes[1].err, Some("向量行号 -7 非法".into()));
    }

    fn errors_empty() -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }

    // ------------------------------------------ P0-03: 单元级状态与文件级聚合

    /// prepare 只把「还没有持久化向量」的片段交给推理：已持久化的不重复编码，
    /// 失败过的独立重试。
    #[test]
    fn prepare_text_queues_only_unpersisted_chunks() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let doc = add_doc(&h, "big.md", &"海边的日落。".repeat(300));
        let (idx, _rx) = indexer(&h, text_settings());
        let rec = h.db.file(doc.id).unwrap();

        let Prepared::Embed { items, .. } = idx.prepare_text(&rec, true).unwrap() else {
            panic!("首次应该产出嵌入任务");
        };
        let first_count = items.len();
        assert!(first_count > 1, "{first_count} 个片段");

        // 全部持久化后再 prepare：无事可做。
        let units: Vec<(EmbedRowKind, i64)> =
            h.db.chunk_rows(doc.id)
                .unwrap()
                .into_iter()
                .map(|(id, _)| (EmbedRowKind::Chunk, id))
                .collect();
        h.db.mark_units_persisted(&units).unwrap();
        assert!(matches!(
            idx.prepare_text(&rec, true).unwrap(),
            Prepared::Indexed
        ));

        // 只让一个片段失败：下一次 prepare 只包含它。
        h.db.mark_units_failed(&[(EmbedRowKind::Chunk, units[0].1)], "超时")
            .unwrap();
        let Prepared::Embed { items, keys } = idx.prepare_text(&rec, true).unwrap() else {
            panic!("有失败片段时应该产出嵌入任务");
        };
        assert_eq!(items.len(), 1, "只重试失败的片段");
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn regroup_keys_keeps_files_together_in_response_order() {
        let item = |key: &str| EmbedItem::Text {
            key: key.into(),
            text: "x".into(),
            title: None,
            role: "document".into(),
        };
        let mut map = std::collections::HashMap::new();
        map.insert("chunk:1:11", 100i64);
        map.insert("chunk:2:12", 100i64);
        map.insert("chunk:3:13", 200i64);
        map.insert("chunk:4:14", 200i64);
        let group = vec![
            item("chunk:1:11"),
            item("chunk:2:12"),
            item("chunk:3:13"),
            item("chunk:4:14"),
        ];

        let grouped = regroup_keys(&group, &map);

        // 相邻同文件合并，跨文件边界不合并；未知 key 落到 -1 兜底。
        assert_eq!(
            grouped,
            vec![
                (100, vec!["chunk:1:11".into(), "chunk:2:12".into()]),
                (200, vec!["chunk:3:13".into(), "chunk:4:14".into()])
            ]
        );
        let unknown = vec![item("bogus")];
        assert_eq!(
            regroup_keys(&unknown, &map),
            vec![(-1, vec!["bogus".into()])]
        );
    }

    /// 长文档不再被 24 片静默截断：片段按上限同步并全部进入嵌入队列；
    /// 全部持久化后文件为 done，但状态注明语义只覆盖前 N 片。
    #[test]
    fn long_document_gets_full_chunks_and_an_honest_truncation_note() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let body = "x".repeat(MAX_CHUNKS_PER_FILE * 900 + 4096); // 超出片段预算
        let doc = add_doc(&h, "huge.txt", &body);
        let mut st = text_settings();
        st.max_text_store_kb = 4096; // 放开 64KB 文本存储，才能看到截断
        let (idx, _rx) = indexer(&h, st);
        let rec = h.db.file(doc.id).unwrap();

        let Prepared::Embed { items, keys } = idx.prepare_text(&rec, true).unwrap() else {
            panic!("长文档应当产出嵌入任务");
        };
        assert_eq!(
            items.len(),
            MAX_CHUNKS_PER_FILE,
            "按片段预算截断，而不是 24"
        );
        assert_eq!(keys.len(), MAX_CHUNKS_PER_FILE);

        // 全部持久化 → done + 截断说明
        let rows: Vec<i64> = keys
            .iter()
            .filter_map(|k| parse_key(k).map(|(_, r, _)| r))
            .collect();
        h.db.activate_vec_rows(&rows).unwrap();
        let units: Vec<(EmbedRowKind, i64)> =
            h.db.chunk_rows(doc.id)
                .unwrap()
                .into_iter()
                .map(|(id, _)| (EmbedRowKind::Chunk, id))
                .collect();
        h.db.mark_units_persisted(&units).unwrap();
        let outcome = FileOutcome {
            file_id: doc.id,
            ok_units: units
                .iter()
                .map(|(_, id)| (EmbedRowKind::Chunk, *id, *id))
                .collect(),
            failed_units: vec![],
            total: units.len(),
            err: None,
        };
        let (state, note) = aggregate_file_state(&h.db, doc.id, &outcome).unwrap();
        assert_eq!(state, FileState::Done);
        let note = note.unwrap_or_else(|| panic!("超长正文必须带语义覆盖说明"));
        assert!(note.contains("2048") && note.contains("关键词"), "{note}");

        // 正常长度文档：单片段持久化后 done 且无说明。
        let small = add_doc(&h, "small.txt", "很短的内容");
        let small_rec = h.db.file(small.id).unwrap();
        let Prepared::Embed {
            keys: small_keys, ..
        } = idx.prepare_text(&small_rec, true).unwrap()
        else {
            panic!("小文档也应产出嵌入任务");
        };
        let small_rows: Vec<i64> = small_keys
            .iter()
            .filter_map(|k| parse_key(k).map(|(_, r, _)| r))
            .collect();
        h.db.activate_vec_rows(&small_rows).unwrap();
        let small_units: Vec<(EmbedRowKind, i64)> =
            h.db.chunk_rows(small.id)
                .unwrap()
                .into_iter()
                .map(|(id, _)| (EmbedRowKind::Chunk, id))
                .collect();
        h.db.mark_units_persisted(&small_units).unwrap();
        let (state, note) = aggregate_file_state(
            &h.db,
            small.id,
            &FileOutcome {
                file_id: small.id,
                ok_units: small_units
                    .iter()
                    .map(|(_, id)| (EmbedRowKind::Chunk, *id, *id))
                    .collect(),
                failed_units: vec![],
                total: small_units.len(),
                err: None,
            },
        )
        .unwrap();
        assert_eq!(state, FileState::Done);
        assert!(note.is_none(), "{note:?}");
    }

    /// 文件级状态由单元终态聚合：全持久化=done，可重试失败=stale（带解释），
    /// 重试耗尽=failed（带原因），图片（无单元行）按本轮结果判定。
    /// 压缩不得回收「进行中文件」里已持久化的向量（P1 修复回归）：
    /// 3 个片段 2 成 1 败，文件 stale；压缩后 2 条有效映射必须原样保留，
    /// 第 3 条重试成功后整份文件可见、可搜。
    #[test]
    fn compaction_keeps_persisted_units_of_an_incomplete_file() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let doc = add_doc(&h, "part.md", &"海边的日落。".repeat(450));
        let rec = h.db.file(doc.id).unwrap();
        let (idx, _rx) = indexer(&h, text_settings());
        let Prepared::Embed { items, keys } = idx.prepare_text(&rec, true).unwrap() else {
            panic!("应产出嵌入任务");
        };
        assert_eq!(items.len(), 3, "3 个片段");
        // 模拟：前 2 个成功（落盘+激活+persisted），第 3 个失败（只预留 staging 行）
        let rows: Vec<i64> = keys
            .iter()
            .filter_map(|k| parse_key(k).map(|(_, r, _)| r))
            .collect();
        let mut store = Store::open(&h.paths.vector_path, 512).unwrap();
        for (i, r) in rows[..2].iter().enumerate() {
            let mut v = vec![0.0f32; 512];
            v[i] = 1.0;
            store.put(*r as usize, &v).unwrap();
        }
        store.flush().unwrap();
        drop(store);
        h.db.activate_vec_rows(&rows[..2]).unwrap();
        h.db.mark_units_persisted(&[
            (EmbedRowKind::Chunk, chunk_id_of(&h.db, doc.id, 0)),
            (EmbedRowKind::Chunk, chunk_id_of(&h.db, doc.id, 1)),
        ])
        .unwrap();
        h.db.mark_units_failed(
            &[(EmbedRowKind::Chunk, chunk_id_of(&h.db, doc.id, 2))],
            "超时",
        )
        .unwrap();
        h.db.set_file_state(doc.id, FileState::Stale, Some("1 个片段待重试"))
            .unwrap();
        assert_eq!(h.db.alive_vec_rows().unwrap(), 2);

        // 压缩：绝不能把前 2 条有效映射一起回收
        let dropped = compact(&h.db, &h.paths.vector_path, 512).unwrap();
        assert_eq!(dropped, 1, "只回收失败片段的 staging 行");
        assert_eq!(h.db.alive_vec_rows().unwrap(), 2, "已持久化的向量必须保留");
        let sum = h.db.unit_summary(doc.id, UNIT_RETRY_MAX).unwrap();
        assert_eq!((sum.total, sum.persisted), (3, 2), "单元状态不受压缩影响");

        // 第 3 个片段重试成功 → 整份文件完成、三个向量全部可搜
        let row3 =
            h.db.reserve_vec_row(EmbedRowKind::Chunk, chunk_id_of(&h.db, doc.id, 2), doc.id)
                .unwrap();
        let active = crate::vectors::active_vector_path(&h.paths.vector_path, &h.db).unwrap();
        let mut store = Store::open(&active, 512).unwrap();
        let mut v = vec![0.0f32; 512];
        v[2] = 1.0;
        store.put(row3 as usize, &v).unwrap();
        store.flush().unwrap();
        drop(store);
        h.db.activate_vec_rows(&[row3]).unwrap();
        h.db.mark_units_persisted(&[(EmbedRowKind::Chunk, chunk_id_of(&h.db, doc.id, 2))])
            .unwrap();
        h.db.set_file_state(doc.id, FileState::Done, None).unwrap();

        let candidates = h.db.vec_candidates(&[]).unwrap();
        assert_eq!(candidates.len(), 3, "三个片段全部可搜");
        let view = crate::vectors::MmapView::open(&active).unwrap().unwrap();
        let mut q = vec![0.0f32; 512];
        q[1] = 1.0;
        let hits = view.search(&q, &candidates, 5);
        assert!(
            (hits[0].1 - 1.0).abs() < 1e-6,
            "压缩后旧向量内容必须原样可读"
        );
        let _ = idx;
    }

    /// TextOnly 不得把「部分语义完成」伪装成 Done（P1 修复回归）：
    /// 部分失败的文件在 TextOnly 下变 keyword_only；模型就绪后
    /// mark_missing_vectors 按单元完整性把它重新排队。
    #[test]
    fn text_only_marks_partial_semantics_keyword_only_and_backfills_later() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let doc = add_doc(&h, "half.md", &"山间的雾。".repeat(450));
        let rec = h.db.file(doc.id).unwrap();
        let (idx, _rx) = indexer(&h, text_settings());
        let Prepared::Embed { keys, .. } = idx.prepare_text(&rec, true).unwrap() else {
            panic!("应产出嵌入任务");
        };
        let chunk_ids: Vec<i64> =
            h.db.chunk_rows(doc.id)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
        // 前 2 个成功、第 3 个失败 → 文件 stale
        h.db.activate_vec_rows(
            &keys
                .iter()
                .filter_map(|k| parse_key(k).map(|(_, r, _)| r))
                .take(2)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        h.db.mark_units_persisted(&[
            (EmbedRowKind::Chunk, chunk_ids[0]),
            (EmbedRowKind::Chunk, chunk_ids[1]),
        ])
        .unwrap();
        h.db.mark_units_failed(&[(EmbedRowKind::Chunk, chunk_ids[2])], "超时")
            .unwrap();
        h.db.set_file_state(doc.id, FileState::Stale, Some("1 个片段待重试"))
            .unwrap();

        // 模型暂时不可用：TextOnly 一轮
        let rec = h.db.file(doc.id).unwrap();
        assert!(matches!(
            idx.prepare_text(&rec, false).unwrap(),
            Prepared::KeywordOnly
        ));
        h.db.set_file_state(doc.id, FileState::KeywordOnly, None)
            .unwrap();

        // 旧逻辑此时会把文件留在 Done、永不补齐；新逻辑：模型就绪后按单元完整性回队列
        assert_eq!(
            h.db.mark_missing_vectors().unwrap(),
            1,
            "部分失败的文件必须回队列"
        );
        assert_eq!(h.db.file(doc.id).unwrap().state, FileState::Stale);
        assert!(h.db.has_incomplete_units(doc.id).unwrap());
        // 语义完整的文件不被误伤：所有片段 persisted 且映射有效
        let done_full = add_doc(&h, "full.md", "短的正文");
        let rec2 = h.db.file(done_full.id).unwrap();
        let Prepared::Embed {
            keys: full_keys, ..
        } = idx.prepare_text(&rec2, true).unwrap()
        else {
            panic!("应产出嵌入任务");
        };
        h.db.activate_vec_rows(
            &full_keys
                .iter()
                .filter_map(|k| parse_key(k).map(|(_, r, _)| r))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let full_chunks: Vec<(EmbedRowKind, i64)> =
            h.db.chunk_rows(done_full.id)
                .unwrap()
                .into_iter()
                .map(|(id, _)| (EmbedRowKind::Chunk, id))
                .collect();
        h.db.mark_units_persisted(&full_chunks).unwrap();
        h.db.set_file_state(done_full.id, FileState::Done, None)
            .unwrap();
        assert_eq!(
            h.db.mark_missing_vectors().unwrap(),
            0,
            "语义完整的文件不需要回队列"
        );
    }

    #[test]
    fn file_state_aggregates_from_units() {
        let h = harness();
        h.db.add_root(&h.dir.path().to_string_lossy(), "test")
            .unwrap();
        let doc = add_doc(&h, "agg.md", &"山间的雾。".repeat(80));
        let chunks =
            h.db.sync_chunks(doc.id, &["一".into(), "二".into(), "三".into()])
                .unwrap();
        let ids: Vec<i64> = chunks.iter().map(|(id, _)| *id).collect();
        let outcome = |ok: usize| FileOutcome {
            file_id: doc.id,
            ok_units: ids[..ok]
                .iter()
                .map(|id| (EmbedRowKind::Chunk, *id, *id))
                .collect(),
            failed_units: ids[ok..]
                .iter()
                .map(|id| (EmbedRowKind::Chunk, *id))
                .collect(),
            total: ids.len(),
            err: Some("解码失败".into()),
        };

        // 0 成功 + 全部可重试 → stale，note 说明进度与原因
        h.db.mark_units_failed(
            &[
                (EmbedRowKind::Chunk, ids[0]),
                (EmbedRowKind::Chunk, ids[1]),
                (EmbedRowKind::Chunk, ids[2]),
            ],
            "解码失败",
        )
        .unwrap();
        let (state, note) = aggregate_file_state(&h.db, doc.id, &outcome(0)).unwrap();
        assert_eq!(state, FileState::Stale);
        assert!(
            note.as_deref().unwrap_or_default().contains("0/3"),
            "{note:?}"
        );

        // 全部持久化 → done
        h.db.mark_units_persisted(&[
            (EmbedRowKind::Chunk, ids[0]),
            (EmbedRowKind::Chunk, ids[1]),
            (EmbedRowKind::Chunk, ids[2]),
        ])
        .unwrap();
        let (state, note) = aggregate_file_state(&h.db, doc.id, &outcome(3)).unwrap();
        assert_eq!(state, FileState::Done);
        assert!(note.is_none());

        // 重试耗尽 → failed，带稳定原因
        for _ in 0..UNIT_RETRY_MAX {
            h.db.mark_units_failed(&[(EmbedRowKind::Chunk, ids[0])], "持续超时")
                .unwrap();
        }
        let (state, note) = aggregate_file_state(&h.db, doc.id, &outcome(2)).unwrap();
        assert_eq!(state, FileState::Failed);
        let note = note.unwrap_or_default();
        assert!(
            note.contains("重复失败") && note.contains("持续超时"),
            "{note}"
        );

        // 图片：没有单元行，本轮产出向量即 done，否则 failed。
        let img = add_doc(&h, "p.jpg", "placeholder");
        let ok_img = FileOutcome {
            file_id: img.id,
            ok_units: vec![(EmbedRowKind::Image, img.id, 99)],
            failed_units: vec![],
            total: 1,
            err: None,
        };
        assert_eq!(
            aggregate_file_state(&h.db, img.id, &ok_img).unwrap().0,
            FileState::Done
        );
        let bad_img = FileOutcome {
            file_id: img.id,
            ok_units: vec![],
            failed_units: vec![(EmbedRowKind::Image, img.id)],
            total: 1,
            err: Some("解码失败".into()),
        };
        let (state, note) = aggregate_file_state(&h.db, img.id, &bad_img).unwrap();
        assert_eq!(state, FileState::Failed);
        assert_eq!(note.as_deref(), Some("解码失败"));
    }
}
