use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::sync::Arc;

use sevqo_engine::db::Db;
use sevqo_engine::infer::Client;
use sevqo_engine::jobs::Control;
use sevqo_engine::watch::WatcherHandle;
use sevqo_engine::{Paths, Settings};

/// Everything the commands share. Kept behind `Arc`/`Mutex` because Tauri hands state
/// to a worker thread on every command call.
pub struct AppState {
    pub paths: Paths,
    pub settings: Mutex<Settings>,
    pub db: Arc<Db>,
    pub client: Arc<Client>,
    pub has_python: bool,
    /// Model files are complete and verified, so the semantic channel may run.
    pub model_ready: AtomicBool,
    pub vision_ready: AtomicBool,
    pub control: Control,
    pub indexing: AtomicBool,
    pub download_cancel: Arc<AtomicBool>,
    pub downloading: AtomicBool,
    pub watcher: Mutex<Option<WatcherHandle>>,
    pub vector_path: PathBuf,
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
        let ready = status.complete && has_python;
        let vector_path = paths.vector_path.clone();
        AppState {
            client: Arc::new(client),
            has_python,
            model_ready: AtomicBool::new(ready),
            vision_ready: AtomicBool::new(ready && need_vision),
            paths,
            settings: Mutex::new(settings),
            db,
            control: Control::default(),
            indexing: AtomicBool::new(false),
            download_cancel: Arc::new(AtomicBool::new(false)),
            downloading: AtomicBool::new(false),
            watcher: Mutex::new(None),
            vector_path,
            pending_update: Mutex::new(None),
            updating: AtomicBool::new(false),
            update_generation: AtomicU64::new(0),
        }
    }

    pub fn model_ready(&self) -> bool {
        self.model_ready.load(Ordering::Relaxed)
    }

    pub fn set_model_ready(&self, v: bool) {
        let st = self.settings();
        self.model_ready.store(v, Ordering::Relaxed);
        self.vision_ready.store(v && (st.index_images || st.index_video), Ordering::Relaxed);
    }

    pub fn indexing(&self) -> bool {
        self.indexing.load(Ordering::Relaxed)
    }

    /// False when a pass is already running, so two triggers cannot race.
    pub fn try_begin_index(&self) -> bool {
        !self.indexing.swap(true, Ordering::SeqCst)
    }

    pub fn end_index(&self) {
        self.indexing.store(false, Ordering::Relaxed);
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn vector_dim(&self) -> usize {
        self.settings().vector_dim
    }

    /// Semantic work needs both the weights and a usable interpreter.
    pub fn can_infer(&self) -> bool {
        self.has_python && self.model_ready()
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
