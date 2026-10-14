//! The IPC surface. Every handler is thin: argument plumbing, then a call into
//! `sevqo_engine`. Heavy work (`search`, `index_*`, `model_*`) runs through
//! `spawn_blocking` so the webview thread never waits on SQLite, torch or a download.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use base64::Engine as _;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use sevqo_engine::db::{self, Db};
use sevqo_engine::infer::python_binary;
use sevqo_engine::jobs::{compact, Indexer, Mode};
use sevqo_engine::model::{self, ModelStatus};
use sevqo_engine::scan::dir_label;
use sevqo_engine::search::Searcher;
use sevqo_engine::vectors::Store;
use sevqo_engine::watch::{apply_change_with_options, Applied, Change, WatcherHandle};
use sevqo_engine::{
    FileRecord, Filters, Hit, Kind, Moment, Paths, Phase, Progress, Root, SearchRequest, SearchResponse, Settings,
};

use crate::state::AppState;

pub type Res<T> = Result<T, String>;

pub const EV_INDEX: &str = "index://progress";
pub const EV_MODEL: &str = "model://progress";
pub const EV_MODEL_STATE: &str = "model://state";
pub const EV_WATCH: &str = "watch://change";

fn es<X: std::fmt::Display>(v: X) -> String {
    let s = v.to_string();
    if s.trim().is_empty() {
        "未知错误".to_string()
    } else {
        s
    }
}

/// Run work off the webview thread. Every `async` command goes through this.
async fn block<T, F>(f: F) -> Res<T>
where
    T: Send + 'static,
    F: FnOnce() -> Res<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f).await.map_err(es)?
}

fn app_state(app: &AppHandle) -> Arc<AppState> {
    app.state::<Arc<AppState>>().inner().clone()
}

/// Which pass makes sense now: vectors only when some media kind is on *and* the model
/// is present, otherwise the text/filename pass that works with no weights at all.
fn auto_mode(st: &AppState) -> Mode {
    if st.can_infer() {
        Mode::Full
    } else {
        Mode::TextOnly
    }
}

fn expand(raw: &str) -> PathBuf {
    sevqo_engine::scan::expand_home(raw.trim())
}

// ------------------------------------------------------------------ payloads

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub files: i64,
    pub bytes: i64,
    pub vectors: i64,
    pub clips: i64,
    pub chunks: i64,
    pub roots: i64,
    pub pending: i64,
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
    pub dead_rows: i64,
    pub by_kind: HashMap<String, i64>,
    pub indexing: bool,
    pub semantic_ready: bool,
    pub has_python: bool,
    pub sidecar_running: bool,
    pub vector_dim: usize,
    pub watched_roots: usize,
    pub vector_bytes: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub status: ModelStatus,
    pub downloading: bool,
    pub ready: bool,
    pub has_python: bool,
    pub python: Option<String>,
    pub endpoints: Vec<String>,
    pub model_dir: String,
    pub db_path: String,
    pub cache_dir: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Runtime {
    pub device: String,
    pub dtype: String,
    pub dim: usize,
    pub params_millions: f64,
    pub load_seconds: f64,
    pub vision: bool,
    pub audio: bool,
    pub threads: usize,
    pub rss_mb: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
    pub roots: usize,
    pub walked: u64,
    pub candidates: u64,
    pub skipped_hidden: u64,
    pub skipped_too_big: u64,
    pub skipped_excluded: u64,
    pub unreadable: u64,
    pub errors: u64,
    pub queued: i64,
    pub elapsed_ms: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexReport {
    pub done: u64,
    pub failed: u64,
    pub skipped: u64,
    pub vectors: u64,
    pub ms: f64,
    pub mode: String,
}

fn overview(st: &AppState) -> Res<Overview> {
    let s = db::stats(&st.db).map_err(es)?;
    let t = db::totals(&st.db).map_err(es)?;
    let mut by_kind = HashMap::new();
    for (k, v) in s.iter() {
        if let Some(rest) = k.strip_prefix("kind_") {
            by_kind.insert(rest.to_string(), *v);
        }
    }
    let g = |key: &str| s.get(key).copied().unwrap_or(0);
    Ok(Overview {
        files: g("files"),
        bytes: g("bytes"),
        vectors: g("vectors"),
        clips: g("clips"),
        chunks: g("chunks"),
        roots: g("roots"),
        pending: t.pending,
        done: t.done,
        failed: t.failed,
        skipped: t.skipped,
        dead_rows: st.db.dead_rows().map_err(es)? as i64,
        by_kind,
        indexing: st.indexing(),
        semantic_ready: st.can_infer(),
        has_python: st.has_python(),
        sidecar_running: st.client().is_running(),
        vector_dim: st.vector_dim(),
        watched_roots: st.watched_roots(),
        vector_bytes: std::fs::metadata(&st.vector_path).map(|m| m.len() as i64).unwrap_or(0),
    })
}

fn model_info(st: &AppState) -> ModelInfo {
    let s = st.settings();
    ModelInfo {
        status: model::status(&st.paths.model_dir),
        downloading: st.downloading.load(Ordering::Relaxed),
        ready: st.can_infer(),
        has_python: st.has_python(),
        python: python_binary(&s, None).map(|p| p.display().to_string()),
        endpoints: s.model_endpoints.clone(),
        model_dir: st.paths.model_dir.display().to_string(),
        db_path: st.paths.db_path.display().to_string(),
        cache_dir: st.paths.cache_dir.display().to_string(),
    }
}

// ------------------------------------------------------------------ settings

#[tauri::command]
pub async fn get_settings(app: AppHandle) -> Res<Settings> {
    let st = app_state(&app);
    block(move || Ok(st.settings())).await
}

/// Persist settings and rewire what depends on them: the sidecar's vision flag and the
/// watcher. A dimension change with vectors already on disk is refused, because mixing
/// two embedding spaces silently ranks garbage.
#[tauri::command]
pub async fn set_settings(app: AppHandle, next: Settings) -> Res<Settings> {
    let st = app_state(&app);
    block(move || {
        let _permit = st.try_begin_index().ok_or("请先停止正在运行的索引或扫描")?;
        let _vectors = st.vector_access.write().map_err(es)?;
        let mut cfg = next.clone();
        cfg.validate();
        let old = st.settings();
        if cfg.model_repo != model::MODEL_REPO {
            return Err(format!("当前版本仅支持模型 {}", model::MODEL_REPO));
        }
        let dim_changed = old.vector_dim != cfg.vector_dim;
        let runtime_changed = dim_changed || old.infer_python != cfg.infer_python
            || old.infer_threads != cfg.infer_threads
            || (old.index_images || old.index_video) != (cfg.index_images || cfg.index_video);
        if dim_changed && st.db.alive_vec_rows().map_err(es)? > 0 {
            return Err("修改向量维度需要重建索引：确认后会清空现有向量并重新编码全部文件。".to_string());
        }
        if dim_changed {
            Store::delete_all(&st.vector_path).map_err(es)?;
            st.db.reset_vectors().map_err(es)?;
        }
        let shortcut_changed = old.global_shortcut != cfg.global_shortcut;
        if shortcut_changed {
            app.global_shortcut().register(cfg.global_shortcut.as_str()).map_err(es)?;
        }
        if let Err(e) = cfg.save(&st.paths.settings_path) {
            if shortcut_changed {
                let _ = app.global_shortcut().unregister(cfg.global_shortcut.as_str());
            }
            return Err(es(e));
        }
        *st.settings.lock().map_err(|_| "设置锁已中毒".to_string())? = cfg.clone();
        if runtime_changed {
            st.configure_client(&cfg);
        }
        if shortcut_changed {
            let _ = app.global_shortcut().unregister(old.global_shortcut.as_str());
        }
        let _ = st.db.set_meta("vector_dim", &cfg.vector_dim.to_string());
        Ok(cfg)
    })
    .await
}

#[tauri::command]
pub async fn stats(app: AppHandle) -> Res<Overview> {
    let st = app_state(&app);
    block(move || overview(&st)).await
}

/// Read from the bundle so the About panel can't drift from the released version.
#[tauri::command]
pub fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
pub async fn model_status(app: AppHandle) -> Res<ModelInfo> {
    let st = app_state(&app);
    block(move || Ok(model_info(&st))).await
}

// ------------------------------------------------------------------ roots

#[tauri::command]
pub async fn list_roots(app: AppHandle) -> Res<Vec<Root>> {
    let st = app_state(&app);
    block(move || st.db.list_roots().map_err(es)).await
}

#[tauri::command]
pub async fn add_root(app: AppHandle, path: String, label: Option<String>) -> Res<Root> {
    let st = app_state(&app);
    let handle = app.clone();
    block(move || {
        let _permit = st.try_begin_index().ok_or("请先停止正在运行的索引或扫描")?;
        let expanded = expand(&path);
        if !expanded.exists() {
            return Err(format!("目录不存在：{}", expanded.display()));
        }
        if !expanded.is_dir() {
            return Err("请选择目录，而不是文件".to_string());
        }
        let s = expanded.to_string_lossy().to_string();
        if st.db.list_roots().map_err(es)?.iter().any(|r| r.path == s) {
            return Err("该目录已经在索引范围内".to_string());
        }
        let name = label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| dir_label(&expanded));
        let id = st.db.add_root(&s, &name).map_err(es)?;
        let root = st.db.list_roots().map_err(es)?.into_iter().find(|r| r.id == id).ok_or("无法读取新建目录")?;
        let _ = restart_watcher(&st, &handle);
        Ok(root)
    })
    .await
}

#[tauri::command]
pub async fn remove_root(app: AppHandle, id: i64) -> Res<usize> {
    let st = app_state(&app);
    let handle = app.clone();
    block(move || {
        let _permit = st.try_begin_index().ok_or("请先停止正在运行的索引或扫描")?;
        let n = st.db.remove_root(id).map_err(es)?;
        let _ = restart_watcher(&st, &handle);
        Ok(n)
    })
    .await
}

#[tauri::command]
pub async fn set_root_enabled(app: AppHandle, id: i64, enabled: bool) -> Res<()> {
    let st = app_state(&app);
    let handle = app.clone();
    block(move || {
        let _permit = st.try_begin_index().ok_or("请先停止正在运行的索引或扫描")?;
        st.db.set_root_enabled(id, enabled).map_err(es)?;
        let _ = restart_watcher(&st, &handle);
        Ok(())
    })
    .await
}

// ------------------------------------------------------------------ scanning

#[tauri::command]
pub async fn rescan(app: AppHandle, root_id: Option<i64>) -> Res<ScanReport> {
    let st = app_state(&app);
    block(move || {
        let _permit = st.try_begin_index().ok_or("请先停止正在运行的索引或扫描")?;
        st.control.reset();
        scan_all(&st, root_id)
    }).await
}

fn scan_all(st: &Arc<AppState>, root_id: Option<i64>) -> Res<ScanReport> {
    let started = std::time::Instant::now();
    let mode = auto_mode(st);
    let (tx, _rx) = crossbeam_channel::unbounded::<Progress>();
    // a scan-only indexer: no client, so nothing is embedded here
    let idx = Indexer::new(st.db.clone(), st.paths.clone(), st.settings(), None, tx).with_control(st.control.clone());
    let roots: Vec<Root> = st
        .db
        .list_roots()
        .map_err(es)?
        .into_iter()
        .filter(|r| r.enabled && root_id.map(|i| i == r.id).unwrap_or(true))
        .collect();
    let mut rep = ScanReport {
        roots: roots.len(),
        walked: 0,
        candidates: 0,
        skipped_hidden: 0,
        skipped_too_big: 0,
        skipped_excluded: 0,
        unreadable: 0,
        errors: 0,
        queued: 0,
        elapsed_ms: 0.0,
    };
    for r in &roots {
        let s = idx.scan_root(r.id, mode).map_err(es)?;
        rep.walked += s.walked;
        rep.candidates += s.indexed_candidates;
        rep.skipped_hidden += s.skipped_hidden;
        rep.skipped_too_big += s.skipped_too_big;
        rep.skipped_excluded += s.skipped_excluded;
        rep.unreadable += s.unreadable;
        rep.errors += s.errors;
    }
    rep.queued = st.db.pending(usize::MAX, &idx.kinds_for(mode)).map_err(es)?.len() as i64;
    rep.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(rep)
}

// ------------------------------------------------------------------ indexing

#[tauri::command]
pub fn index_start(app: AppHandle, mode: Option<String>, max_files: Option<usize>, scan: Option<bool>) -> Res<()> {
    let st = app_state(&app);
    let permit = st.try_begin_index().ok_or("索引正在运行中")?;
    st.control.reset();
    let m = match mode.as_deref() {
        Some("text") | Some("textOnly") | Some("text_only") => Mode::TextOnly,
        _ => auto_mode(&st),
    };
    let scan_first = scan.unwrap_or(true);
    let app2 = app.clone();
    std::thread::Builder::new()
        .name("sevqo-index".into())
        .spawn(move || {
            let _permit = permit;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_index(&app2, &st, m, max_files, scan_first)));
            match outcome {
                Err(_) => {
                    tracing::error!("索引线程异常终止");
                    emit_progress(&app2, Progress { phase: Phase::Failed, message: Some("索引线程异常终止".into()), ..Default::default() });
                }
                Ok(Err(msg)) => emit_progress(&app2, Progress { phase: Phase::Failed, message: Some(msg), ..Default::default() }),
                Ok(Ok(())) => {}
            }
        })
        .map_err(es)?;
    Ok(())
}

fn emit_progress(app: &AppHandle, p: Progress) {
    let _ = app.emit(EV_INDEX, &p);
}

fn run_index(app: &AppHandle, st: &Arc<AppState>, mode: Mode, max_files: Option<usize>, scan_first: bool) -> Res<()> {
    let (tx, rx) = crossbeam_channel::unbounded::<Progress>();
    let fwd = {
        let app = app.clone();
        std::thread::spawn(move || {
            while let Ok(p) = rx.recv() {
                let _ = app.emit(EV_INDEX, &p);
            }
        })
    };
    let cfg = st.settings();
    let client = if mode == Mode::Full && !st.can_infer() {
        tracing::warn!("模型未就绪，向量通道跳过");
        None
    } else {
        Some(st.client())
    };
    let idx = Indexer::new(st.db.clone(), st.paths.clone(), cfg, client, tx.clone()).with_control(st.control.clone());
    let result = (|| -> Res<IndexReport> {
        if mode == Mode::Full {
            st.db.mark_missing_vectors().map_err(es)?;
        }
        if scan_first {
            emit_progress(app, Progress { phase: Phase::Scanning, message: Some("正在扫描目录".into()), ..Default::default() });
            let rep = scan_all(st, None)?;
            tracing::info!("扫描完成：{} 个候选，排队 {} 个", rep.candidates, rep.queued);
        }
        emit_progress(app, Progress { phase: Phase::Indexing, ..Default::default() });
        let sum = idx.run(mode, max_files).map_err(es)?;
        Ok(IndexReport {
            done: sum.done,
            failed: sum.failed,
            skipped: sum.skipped,
            vectors: sum.vectors,
            ms: sum.ms,
            mode: if mode == Mode::Full { "full".into() } else { "text".into() },
        })
    })();
    close_progress(idx, tx, fwd);
    match &result {
        Ok(rep) => {
            tracing::info!("索引一轮完成：{} 成功 / {} 失败 / {} 跳过", rep.done, rep.failed, rep.skipped);
            emit_progress(
                app,
                Progress {
                    phase: Phase::Idle,
                    done: rep.done,
                    failed: rep.failed,
                    skipped: rep.skipped,
                    elapsed_s: rep.ms / 1000.0,
                    message: Some(format!("本轮处理 {} 个文件", rep.done)),
                    ..Default::default()
                },
            );
        }
        Err(msg) => emit_progress(app, Progress { phase: Phase::Failed, message: Some(msg.clone()), ..Default::default() }),
    }
    result.map(|_| ())
}

#[tauri::command]
pub fn index_pause(app: AppHandle) {
    app_state(&app).control.pause();
}

#[tauri::command]
pub fn index_resume(app: AppHandle) {
    app_state(&app).control.resume();
}

#[tauri::command]
pub fn index_cancel(app: AppHandle) {
    app_state(&app).control.cancel();
}

/// Hand every file back to the queue. `clear_vectors` is the explicit acknowledgement
/// that the existing embedding space is being thrown away.
#[tauri::command]
pub async fn rebuild_index(app: AppHandle, clear_vectors: bool) -> Res<usize> {
    let st = app_state(&app);
    block(move || {
        let _permit = st.try_begin_index().ok_or("请先停止正在运行的索引")?;
        let _vectors = st.vector_access.write().map_err(es)?;
        let n = st.db.reindex_all().map_err(es)?;
        if clear_vectors {
            Store::delete_all(&st.vector_path).map_err(es)?;
        }
        Ok(n)
    })
    .await
}

/// Queue files whose text is indexed but whose vectors are missing (model arrived late).
#[tauri::command]
pub async fn backfill_vectors(app: AppHandle) -> Res<usize> {
    let st = app_state(&app);
    block(move || st.db.mark_missing_vectors().map_err(es)).await
}

#[tauri::command]
pub async fn compact_vectors(app: AppHandle) -> Res<usize> {
    let st = app_state(&app);
    block(move || {
        let _permit = st.try_begin_index().ok_or("请等待索引完成后再压缩向量")?;
        let _vectors = st.vector_access.write().map_err(es)?;
        Ok(compact(&st.db, &st.vector_path, st.vector_dim()).map_err(es)?)
    })
    .await
}

fn close_progress(idx: Indexer, tx: crossbeam_channel::Sender<Progress>, fwd: std::thread::JoinHandle<()>) {
    // The indexer also owns a sender; dropping only `tx` leaves recv() waiting forever.
    drop(idx);
    drop(tx);
    let _ = fwd.join();
}

// ------------------------------------------------------------------ search

/// The keyword channel answers immediately; the semantic channel is attempted and
/// downgraded with a readable note when the sidecar cannot serve it.
#[tauri::command]
pub async fn search(app: AppHandle, req: SearchRequest) -> Res<SearchResponse> {
    let st = app_state(&app);
    block(move || run_search(&st, req, None)).await
}

#[tauri::command]
pub async fn search_keyword(app: AppHandle, req: SearchRequest) -> Res<SearchResponse> {
    let st = app_state(&app);
    block(move || {
        let mut r = req;
        r.semantic = false;
        run_search(&st, r, None)
    })
    .await
}

/// 以图搜图 / 以视频片段搜视频: the query is a media file, so only the semantic channel
/// applies. The file's own kind decides which way it is encoded and what it is matched against.
#[tauri::command]
pub async fn search_media(app: AppHandle, path: String, limit: Option<usize>, kinds: Option<Vec<Kind>>, filters: Option<Filters>) -> Res<SearchResponse> {
    let st = app_state(&app);
    block(move || {
        if !st.can_infer() {
            return Err("媒体检索需要先下载模型权重（约 1.49 GB），点「下载模型」即可开始".to_string());
        }
        let s = st.settings();
        if !(s.index_images || s.index_video) {
            // the sidecar runs with `--text-only` in that case, so there is no vision tower to ask
            return Err("媒体检索需要在设置里开启图片或视频索引".to_string());
        }
        let p = expand(&path);
        if !p.is_file() {
            return Err(format!("文件不存在：{}", p.display()));
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let kind = Kind::from_ext(&ext);
        let v = match kind {
            Kind::Image => sevqo_engine::search::query_vector_from_image(&st.client(), &p, s.infer_batch),
            Kind::Video => sevqo_engine::search::query_vector_from_video(&st.client(), &p, s.video_window_s, s.infer_batch),
            _ => return Err(format!("不支持用 .{ext} 作为查询，支持图片与视频文件")),
        }
        .map_err(es)?;
        let mut filters = filters.unwrap_or_default();
        if let Some(kinds) = kinds {
            filters.kinds = kinds;
        } else if filters.kinds.is_empty() {
            filters.kinds = vec![kind];
        }
        let mut req = SearchRequest {
            limit: limit.unwrap_or(60).clamp(1, 500),
            semantic: true,
            filters,
            ..Default::default()
        };
        req.query = String::new();
        run_search(&st, req, Some(v))
    })
    .await
}

#[tauri::command]
pub async fn recent(app: AppHandle, limit: Option<usize>) -> Res<Vec<Hit>> {
    let st = app_state(&app);
    block(move || {
        let _vectors = st.vector_access.read().map_err(es)?;
        let mut req = SearchRequest {
            limit: limit.unwrap_or(40).clamp(1, 300),
            semantic: false,
            ..Default::default()
        };
        req.query.clear();
        let searcher = Searcher::new(st.db.clone(), &st.vector_path).map_err(es)?;
        Ok(searcher.run(&req, None).map_err(es)?.hits)
    })
    .await
}

#[tauri::command]
pub async fn moments(app: AppHandle, file_id: i64) -> Res<Vec<Moment>> {
    let st = app_state(&app);
    block(move || {
        let map = st.db.moments_for(&[file_id]).map_err(es)?;
        Ok(map
            .get(&file_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(clip_id, start_ms, end_ms, score, thumb)| Moment { clip_id, start_ms, end_ms, score, thumb })
            .collect())
    })
    .await
}

/// Query embedding is best-effort: a dead sidecar must not turn into a failed search.
fn resolve_query_vector(st: &Arc<AppState>, searcher: &Searcher, req: &SearchRequest) -> (Option<Vec<f32>>, Option<String>) {
    if !req.semantic {
        return (None, None);
    }
    let dim = searcher.vector_dim();
    if dim == 0 {
        return (None, Some("还没有语义索引，仅按文件名与正文匹配".to_string()));
    }
    if !st.can_infer() {
        return (None, Some("模型未就绪，仅按文件名与正文匹配".to_string()));
    }
    match sevqo_engine::search::query_vector(&st.client(), req.query.trim(), st.settings().infer_batch) {
        Ok(v) if v.len() == dim => (Some(v), None),
        Ok(v) => (
            None,
            Some(format!("查询向量维度 {} 与索引维度 {dim} 不一致，需要重建索引", v.len())),
        ),
        Err(e) => (None, Some(format!("语义检索暂不可用：{e}"))),
    }
}

fn run_search(st: &Arc<AppState>, mut req: SearchRequest, injected: Option<Vec<f32>>) -> Res<SearchResponse> {
    let _vectors = st.vector_access.read().map_err(es)?;
    req.limit = req.limit.clamp(1, 500);
    let searcher = Searcher::new(st.db.clone(), &st.vector_path).map_err(es)?;
    let (qv, degraded) = match injected {
        Some(v) => {
            let dim = searcher.vector_dim();
            if dim != 0 && v.len() != dim {
                (None, Some(format!("查询向量维度 {} 与索引维度 {dim} 不一致", v.len())))
            } else if dim == 0 {
                (None, Some("还没有语义索引，无法按图片检索".to_string()))
            } else {
                (Some(v), None)
            }
        }
        None if req.query.trim().is_empty() => (None, None),
        None => resolve_query_vector(st, &searcher, &req),
    };
    let mut resp = searcher.run(&req, qv.as_deref()).map_err(es)?;
    if let Some(note) = degraded {
        resp.note = Some(match resp.note.take() {
            Some(old) if !old.is_empty() && old != note => format!("{note}；{old}"),
            _ => note,
        });
    }
    Ok(resp)
}

// ------------------------------------------------------------------ thumbnails

fn data_url(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let mime = match path.extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "webp" => "image/webp",
        "heic" | "heif" => "image/heif",
        _ => "image/jpeg",
    };
    Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[tauri::command]
pub async fn thumb(app: AppHandle, file_id: i64) -> Res<Option<String>> {
    let st = app_state(&app);
    block(move || {
        let thumb = st.db.file(file_id).and_then(|f| f.thumb);
        Ok(thumb.as_deref().map(PathBuf::from).and_then(|p| data_url(&p)))
    })
    .await
}

#[tauri::command]
pub async fn clip_thumb(app: AppHandle, file_id: i64, clip_id: i64) -> Res<Option<String>> {
    let st = app_state(&app);
    block(move || {
        let rows = st.db.clip_rows(file_id).map_err(es)?;
        let path = rows.into_iter().find(|(id, ..)| *id == clip_id).and_then(|(_, _, _, _, thumb)| thumb);
        Ok(path.as_deref().map(PathBuf::from).and_then(|p| data_url(&p)))
    })
    .await
}

#[tauri::command]
pub async fn file_info(app: AppHandle, file_id: i64) -> Res<Option<FileRecord>> {
    let st = app_state(&app);
    block(move || Ok(st.db.file(file_id))).await
}

// ------------------------------------------------------------------ opening files

#[tauri::command]
pub async fn open_file(path: String) -> Res<()> {
    block(move || open_path(&path, false)).await
}

#[tauri::command]
pub async fn reveal_in_folder(path: String) -> Res<()> {
    block(move || open_path(&path, true)).await
}

fn open_path(raw: &str, reveal: bool) -> Res<()> {
    let p = expand(raw);
    if !p.exists() {
        return Err("文件已经不存在，可能被移动或删除".to_string());
    }
    let mut cmd = platform_opener(&p, reveal);
    let status = cmd.spawn().map_err(es)?.wait().map_err(es)?;
    if !status.success() && !cfg!(target_os = "windows") {
        return Err(format!("打开失败（{status}）"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_opener(p: &Path, reveal: bool) -> std::process::Command {
    let mut c = std::process::Command::new("open");
    if reveal {
        c.arg("-R");
        c.arg(p);
    } else {
        c.arg(p);
    }
    c
}

#[cfg(target_os = "windows")]
fn platform_opener(p: &Path, reveal: bool) -> std::process::Command {
    let mut c = std::process::Command::new("explorer");
    if reveal {
        c.arg(format!("/select,{}", p.display()));
    } else {
        c.arg(p);
    }
    c
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_opener(p: &Path, reveal: bool) -> std::process::Command {
    let mut c = std::process::Command::new("xdg-open");
    c.arg(if reveal { p.parent().unwrap_or(p) } else { p });
    c
}

// ------------------------------------------------------------------ model download

#[tauri::command]
pub fn model_download_start(app: AppHandle) -> Res<()> {
    let st = app_state(&app);
    spawn_model_download(st, app, true)
}

/// Fetch whatever the model directory is missing, reporting through `model://progress`.
/// Shared by the command and by launch-time auto-resume; `announce` keeps a manual click from
/// dead-ending silently, while the launch path stays quiet.
pub fn spawn_model_download(st: Arc<AppState>, app: AppHandle, announce: bool) -> Res<()> {
    // 拉权重只是按镜像端点下文件，跟 Python 没关系；只有推理才需要解释器。
    if model::status(&st.paths.model_dir).complete {
        st.set_model_ready(true);
        if announce {
            let note = if st.has_python() {
                "模型已就绪，可以开始语义检索"
            } else {
                "权重已经完整，在设置里指定 Python 后即可语义检索"
            };
            let _ = app.emit(
                EV_MODEL_STATE,
                serde_json::json!({ "ready": st.can_infer(), "note": note, "cancelled": false }),
            );
        }
        return Ok(());
    }
    if !st.try_begin_download() {
        return Err("模型已在下载中".to_string());
    }
    st.download_cancel.store(false, Ordering::SeqCst);
    let cancel = st.download_cancel.clone();
    let endpoints = st.settings().model_endpoints.clone();
    let proxy = st.settings().net_proxy.clone();
    let dir = st.paths.model_dir.clone();
    let spawn_state = st.clone();
    let app2 = app.clone();
    std::thread::Builder::new()
        .name("sevqo-model".into())
        .spawn(move || {
            let on = {
                let app = app2.clone();
                move |p: model::DownloadProgress| {
                    let _ = app.emit(EV_MODEL, &p);
                }
            };
            let result = model::ensure(&dir, &endpoints, proxy.as_deref(), &cancel, on);
            st.end_download();
            let (ready, note, cancelled) = match &result {
                Ok(()) => {
                    st.set_model_ready(true);
                    let _ = app2.emit(
                        EV_MODEL,
                        model::DownloadProgress {
                            file: String::new(),
                            file_index: model::FILES.len(),
                            file_count: model::FILES.len(),
                            done_bytes: model::total_bytes(),
                            total_bytes: model::total_bytes(),
                            overall_bytes: model::total_bytes(),
                            overall_total: model::total_bytes(),
                            bytes_per_second: 0.0,
                            endpoint: String::new(),
                            note: Some("done".into()),
                        },
                    );
                    (true, "模型已就绪，可以开始语义检索".to_string(), false)
                }
                Err(e) => (false, e.to_string(), matches!(e, sevqo_engine::Error::Cancelled)),
            };
            let _ = app2.emit(EV_MODEL_STATE, serde_json::json!({ "ready": ready, "note": note, "cancelled": cancelled }));
        })
        .map_err(|e| {
            spawn_state.end_download();
            es(e)
        })?;
    Ok(())
}

#[tauri::command]
pub fn model_download_cancel(app: AppHandle) {
    app_state(&app).download_cancel.store(true, Ordering::SeqCst);
}

/// Warm the sidecar and report what it actually loaded (device, dtype, RAM).
#[tauri::command]
pub async fn model_runtime(app: AppHandle) -> Res<Runtime> {
    let st = app_state(&app);
    block(move || {
        if !st.can_infer() {
            return Err("模型未就绪".to_string());
        }
        let info = st.client().load().map_err(es)?;
        Ok(Runtime {
            device: info.device,
            dtype: info.dtype,
            dim: info.dim,
            params_millions: info.params_millions,
            load_seconds: info.load_seconds,
            vision: info.vision,
            audio: info.audio,
            threads: info.threads,
            rss_mb: info.rss_mb,
        })
    })
    .await
}

#[tauri::command]
pub async fn model_verify(app: AppHandle) -> Res<String> {
    let st = app_state(&app);
    block(move || {
        if let Err(e) = model::verify(&st.paths.model_dir) {
            st.set_model_ready(false);
            return Err(es(e));
        }
        st.set_model_ready(true);
        Ok("校验通过".to_string())
    })
    .await
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn finishing_a_pass_closes_the_progress_forwarder() {
        let (_dir, state) = crate::state::test_state();
        let (tx, rx) = crossbeam_channel::unbounded::<Progress>();
        let fwd = std::thread::spawn(move || while rx.recv().is_ok() {});
        let idx = Indexer::new(state.db.clone(), state.paths.clone(), state.settings(), None, tx.clone());
        let (done, wait) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            close_progress(idx, tx, fwd);
            done.send(()).unwrap();
        });
        wait.recv_timeout(std::time::Duration::from_secs(2)).expect("progress sender held by Indexer must be dropped before join");
    }

    #[test]
    fn text_only_configuration_uses_full_mode_when_model_is_ready() {
        let (_dir, state) = crate::state::test_state();
        let mut cfg = state.settings();
        cfg.index_images = false;
        cfg.index_video = false;
        *state.settings.lock().unwrap() = cfg;
        state.has_python.store(true, Ordering::Relaxed);
        state.set_model_ready(true);
        assert_eq!(auto_mode(&state), Mode::Full);
    }
}

// ------------------------------------------------------------------ watcher

#[tauri::command]
pub async fn watch_restart(app: AppHandle) -> Res<usize> {
    let st = app_state(&app);
    let handle = app.clone();
    block(move || restart_watcher(&st, &handle)).await
}

/// Rebuild the listener from the enabled roots and start the consumer that turns raw
/// events into index-queue updates (plus a small incremental pass when idle).
pub fn restart_watcher(st: &Arc<AppState>, app: &AppHandle) -> Res<usize> {
    {
        // dropping the old handle closes its watcher and ends its debounce thread
        let mut g = st.watcher.lock().map_err(|_| "监听锁已中毒".to_string())?;
        let _ = g.take();
    }
    let roots: Vec<PathBuf> = st
        .db
        .list_roots()
        .map_err(es)?
        .into_iter()
        .filter(|r| r.enabled)
        .map(|r| expand(&r.path))
        .collect();
    if roots.is_empty() {
        return Ok(0);
    }
    let (tx, rx) = crossbeam_channel::unbounded::<Change>();
    let handle = WatcherHandle::start(&roots, tx).map_err(es)?;
    let n = handle.roots.len();
    *st.watcher.lock().map_err(|_| "监听锁已中毒".to_string())? = Some(handle);

    let st2 = st.clone();
    let app2 = app.clone();
    std::thread::Builder::new()
        .name("sevqo-watch".into())
        .spawn(move || watch_loop(&st2, &app2, rx))
        .map_err(es)?;
    Ok(n)
}

fn watch_loop(st: &Arc<AppState>, app: &AppHandle, rx: crossbeam_channel::Receiver<Change>) {
    let mut dirty = false;
    let mut pending_changes = HashMap::<PathBuf, Change>::new();
    loop {
        let change = match rx.recv_timeout(std::time::Duration::from_millis(800)) {
            Ok(c) => Some(c),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(c) = change {
            pending_changes.insert(c.path().to_path_buf(), c);
            dirty = true;
        } else if dirty {
            // Keep changes queued during a full pass: applying a write while its
            // old contents are being embedded would attach stale vectors to it.
            if let Some(_permit) = st.try_begin_index() {
                let cfg = st.settings();
                let kinds = index_kinds(&cfg);
                let opts = sevqo_engine::scan::ScanOptions {
                    kinds: &kinds,
                    max_file_bytes: cfg.max_file_bytes,
                    exclude_dirs: &cfg.exclude_dirs,
                    exclude_prefixes: &cfg.exclude_path_prefixes,
                };
                let mut needs_rescan = false;
                for (_, c) in pending_changes.drain() {
                    let path = c.path().to_string_lossy().to_string();
                    match apply_change_with_options(&st.db, &c, &opts) {
                        Ok(applied) => {
                            needs_rescan |= matches!(applied, Applied::NeedsRescan);
                            let _ = app.emit(EV_WATCH, serde_json::json!({
                                "path": path, "type": c.kind_name(), "applied": applied_name(applied)
                            }));
                        }
                        Err(err) => {
                            needs_rescan = true;
                            tracing::warn!("监听处理失败 {path}: {err}");
                        }
                    }
                }
                st.control.reset();
                let result = (|| -> Res<bool> {
                    if needs_rescan {
                        scan_all(st, None)?;
                    }
                    incremental_pass(st, app)
                })();
                match result {
                    Ok(more) => dirty = more,
                    Err(err) => {
                        tracing::warn!("增量索引失败: {err}");
                        // Retry the reconciliation rather than losing the events.
                        pending_changes.insert(PathBuf::new(), Change::Rescan(PathBuf::new()));
                        dirty = true;
                    }
                }
            }
        }
    }
}

fn applied_name(a: Applied) -> &'static str {
    match a {
        Applied::Ignored => "ignored",
        Applied::NeedsIndex => "index",
        Applied::Unchanged => "unchanged",
        Applied::Removed => "removed",
        Applied::NeedsRescan => "rescan",
    }
}

fn index_kinds(cfg: &Settings) -> Vec<Kind> {
    let mut v = Vec::new();
    if cfg.index_images {
        v.push(Kind::Image);
    }
    if cfg.index_video {
        v.push(Kind::Video);
    }
    if cfg.index_documents {
        v.push(Kind::Document);
    }
    if cfg.index_code {
        v.push(Kind::Code);
    }
    v
}

/// A bounded pass over what the watcher just marked pending; never fights a full run.
fn incremental_pass(st: &Arc<AppState>, app: &AppHandle) -> Res<bool> {
    let (tx, rx) = crossbeam_channel::unbounded::<Progress>();
    let fwd = {
        let app = app.clone();
        std::thread::spawn(move || {
            while let Ok(p) = rx.recv() {
                let _ = app.emit(EV_INDEX, &p);
            }
        })
    };
    let mode = auto_mode(st);
    let client = if mode == Mode::Full && st.can_infer() { Some(st.client()) } else { None };
    let idx = Indexer::new(st.db.clone(), st.paths.clone(), st.settings(), client, tx.clone()).with_control(st.control.clone());
    let out = idx.run(mode, Some(64));
    let kinds = idx.kinds_for(mode);
    close_progress(idx, tx, fwd);
    out.map_err(es)?;
    Ok(!st.control.is_cancelled() && !st.db.pending(1, &kinds).map_err(es)?.is_empty())
}

// ------------------------------------------------------------------ windows

#[tauri::command]
pub fn spotlight_toggle(app: AppHandle) {
    toggle_window(&app, "spotlight");
}

#[tauri::command]
pub fn spotlight_hide(app: AppHandle) {
    if let Some(w) = app.get_webview_window("spotlight") {
        let _ = w.hide();
    }
}

#[tauri::command]
pub fn show_main(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
    if let Some(w) = app.get_webview_window("spotlight") {
        let _ = w.hide();
    }
}

pub fn toggle_window(app: &AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.center();
            let _ = w.show();
            let _ = w.set_always_on_top(true);
            let _ = w.set_focus();
        }
    }
}

/// One place that turns a data directory into `(paths, settings, db)`.
pub fn bootstrap_paths(model_home: Option<&str>) -> Res<(Paths, Settings, Arc<Db>)> {
    let paths = Paths::resolve(model_home).map_err(es)?;
    paths.ensure().map_err(es)?;
    let settings = Settings::load(&paths.settings_path);
    let db = Arc::new(Db::open(&paths.db_path).map_err(es)?);
    Ok((paths, settings, db))
}
