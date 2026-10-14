use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::sync::Arc;

use sevqo_engine::db::Db;
use sevqo_engine::infer::Client;
use sevqo_engine::jobs::Control;
use sevqo_engine::watch::WatcherHandle;
use sevqo_engine::{Phase, Progress, Paths, Settings};

/// Everything the commands share. Kept behind `Arc`/`Mutex` because Tauri hands state
/// to a worker thread on every command call.
pub struct AppState {
    pub paths: Paths,
    pub settings: Mutex<Settings>,
    pub db: Arc<Db>,
    client: Mutex<Arc<Client>>,
    pub has_python: AtomicBool,
    /// Model files are complete and verified, so the semantic channel may run.
    pub model_ready: AtomicBool,
    pub vision_ready: AtomicBool,
    pub control: Control,
    pub indexing: AtomicBool,
    /// 索引事件的最后一帧。事件只在被收到的那一刻有用：窗口藏着、监听器还没挂上，整条就没了，
    /// 界面会永远停在「索引中」。所以存一份，前端随时能问回来。
    last_progress: Mutex<Progress>,
    pub download_cancel: Arc<AtomicBool>,
    pub downloading: AtomicBool,
    pub watcher: Mutex<Option<WatcherHandle>>,
    pub vector_path: PathBuf,
    /// A search holds its file mapping and DB row mapping together. Maintenance
    /// must not replace one while a query is still using the other.
    pub vector_access: RwLock<()>,
    /// The update we fetched but have not applied. Kept so installing needs no second
    /// manifest round-trip and so 取消 can drop it.
    pub pending_update: Mutex<Option<tauri_plugin_updater::Update>>,
    pub updating: AtomicBool,
    /// Bumped by 取消; a running install re-reads it before it touches the bundle.
    pub update_generation: AtomicU64,
}

impl AppState {
    pub fn new(paths: Paths, settings: Settings, db: Arc<Db>) -> AppState {
        let has_python = sevqo_engine::infer::python_binary(&settings, None).is_some();
        let need_vision = settings.index_images || settings.index_video;
        let client = Client::new(&paths, &settings);
        let status = sevqo_engine::model::status(&paths.model_dir);
        // 按字段注释的含义来：model_ready 只说「权重齐了」，能不能推理由 can_infer 加上 Python 判断。
        // 之前在这里混入 has_python，会和 reseed() 的口径不一致，导致没配 Python 时误判成「权重还缺」。
        let ready = status.complete;
        let vector_path = paths.vector_path.clone();
        AppState {
            client: Mutex::new(Arc::new(client)),
            has_python: AtomicBool::new(has_python),
            model_ready: AtomicBool::new(ready),
            vision_ready: AtomicBool::new(ready && need_vision),
            paths,
            settings: Mutex::new(settings),
            db,
            control: Control::default(),
            indexing: AtomicBool::new(false),
            last_progress: Mutex::new(Progress::default()),
            download_cancel: Arc::new(AtomicBool::new(false)),
            downloading: AtomicBool::new(false),
            watcher: Mutex::new(None),
            vector_path,
            vector_access: RwLock::new(()),
            pending_update: Mutex::new(None),
            updating: AtomicBool::new(false),
            update_generation: AtomicU64::new(0),
        }
    }

    pub fn model_ready(&self) -> bool {
        self.model_ready.load(Ordering::Relaxed)
    }

    pub fn client(&self) -> Arc<Client> {
        self.client.lock().expect("推理配置锁已中毒").clone()
    }

    pub fn has_python(&self) -> bool {
        self.has_python.load(Ordering::Relaxed)
    }

    pub fn configure_client(&self, cfg: &Settings) {
        let next = Arc::new(Client::new(&self.paths, cfg));
        let old = std::mem::replace(&mut *self.client.lock().expect("推理配置锁已中毒"), next);
        old.shutdown();
        self.has_python.store(sevqo_engine::infer::python_binary(cfg, None).is_some(), Ordering::Relaxed);
        self.set_model_ready(sevqo_engine::model::status(&self.paths.model_dir).complete);
    }

    pub fn set_model_ready(&self, v: bool) {
        let st = self.settings();
        self.model_ready.store(v, Ordering::Relaxed);
        self.vision_ready.store(v && (st.index_images || st.index_video), Ordering::Relaxed);
    }

    pub fn indexing(&self) -> bool {
        self.indexing.load(Ordering::Relaxed)
    }

    pub fn publish_progress(&self, p: Progress) {
        if let Ok(mut g) = self.last_progress.lock() {
            *g = p;
        }
    }

    /// 没在跑却停在运行中的相位，说明那是上一轮的残留事件：降级成空闲再交给界面。
    pub fn progress_snapshot(&self) -> Progress {
        let p = self.last_progress.lock().map(|g| g.clone()).unwrap_or_default();
        if self.indexing() {
            return p;
        }
        match p.phase {
            Phase::Scanning | Phase::Indexing | Phase::Paused => Progress { phase: Phase::Idle, current: None, ..p },
            _ => p,
        }
    }

    /// False when a pass is already running, so two triggers cannot race.
    pub fn try_begin_index(self: &Arc<Self>) -> Option<IndexGuard> {
        if self.indexing.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(IndexGuard(self.clone()))
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn vector_dim(&self) -> usize {
        self.settings().vector_dim
    }

    /// Semantic work needs both the weights and a usable interpreter.
    pub fn can_infer(&self) -> bool {
        self.has_python() && self.model_ready()
    }

    pub fn watched_roots(&self) -> usize {
        self.watcher
            .lock()
            .map(|g| g.as_ref().map(|w| w.roots.len()).unwrap_or(0))
            .unwrap_or(0)
    }

    pub fn try_begin_download(&self) -> bool {
        !self.downloading.swap(true, Ordering::SeqCst)
    }

    pub fn end_download(&self) {
        self.downloading.store(false, Ordering::Relaxed);
    }

    pub fn set_pending_update(&self, update: Option<tauri_plugin_updater::Update>) {
        if let Ok(mut g) = self.pending_update.lock() {
            *g = update;
        }
    }

    pub fn pending_update(&self) -> Option<tauri_plugin_updater::Update> {
        self.pending_update.lock().ok().and_then(|g| g.clone())
    }

    /// One install at a time: two would race to replace the same bundle.
    pub fn try_begin_update(&self) -> bool {
        !self.updating.swap(true, Ordering::SeqCst)
    }

    pub fn end_update(&self) {
        self.updating.store(false, Ordering::Relaxed);
    }

    pub fn update_generation(&self) -> u64 {
        self.update_generation.load(Ordering::SeqCst)
    }

    pub fn cancel_update(&self) {
        self.update_generation.fetch_add(1, Ordering::SeqCst);
        self.set_pending_update(None);
    }
}

/// Releases the work slot on errors, panics and thread-spawn failures as well.
pub struct IndexGuard(Arc<AppState>);

impl Drop for IndexGuard {
    fn drop(&mut self) {
        self.0.indexing.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(crate) fn test_state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        data_dir: dir.path().into(), cache_dir: dir.path().into(),
        model_dir: dir.path().join("models"), thumb_dir: dir.path().join("thumbs"),
        clip_thumb_dir: dir.path().join("clip-thumbs"), db_path: dir.path().join("index.sqlite"),
        vector_path: dir.path().join("vectors.f32"), settings_path: dir.path().join("settings.json"),
        log_dir: dir.path().join("logs"),
    };
    paths.ensure().unwrap();
    let db = Arc::new(Db::open(&paths.db_path).unwrap());
    let state = Arc::new(AppState::new(paths, Settings::default(), db));
    (dir, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_slot_is_exclusive_and_released_on_drop() {
        let (_dir, state) = test_state();
        let permit = state.try_begin_index().unwrap();
        assert!(state.indexing());
        assert!(state.try_begin_index().is_none());
        drop(permit);
        assert!(!state.indexing());
        assert!(state.try_begin_index().is_some());
    }

    /// 事件整条漏掉时，界面只能信这一帧：没在跑却报「索引中」就是把上一轮的残留当成现状。
    #[test]
    fn stale_progress_frames_are_downgraded_to_idle() {
        let (_dir, state) = test_state();
        state.publish_progress(Progress {
            phase: Phase::Indexing,
            done: 3,
            queued: 7,
            current: Some("a.md".into()),
            ..Default::default()
        });
        let snap = state.progress_snapshot();
        assert_eq!(snap.phase, Phase::Idle, "空闲时不能把运行中的残留帧交出去");
        assert_eq!((snap.done, snap.queued), (3, 7), "降级相位不该抹掉已经发生的事实");
        assert!(snap.current.is_none(), "没有正在处理的文件却报着文件名");
        let _permit = state.try_begin_index().unwrap();
        assert_eq!(state.progress_snapshot().phase, Phase::Indexing, "真的在跑就原样报出来");
    }

    #[test]
    fn interpreter_changes_replace_the_active_client() {
        let (dir, state) = test_state();
        let previous = state.client();
        let mut cfg = state.settings();
        cfg.infer_python = Some(dir.path().join("missing-python").to_string_lossy().into());
        cfg.vector_dim = 256;
        cfg.infer_threads = 2;
        *state.settings.lock().unwrap() = cfg.clone();
        state.configure_client(&cfg);
        let current = state.client();
        assert!(!Arc::ptr_eq(&previous, &current));
        assert_eq!(state.vector_dim(), 256);
        assert!(current.load().unwrap_err().to_string().contains("missing-python"));
    }
}
