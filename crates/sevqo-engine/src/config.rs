use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Shared embedding dimension. Queries and corpus items must use the same value,
    /// so this is global, not per modality. Changing it invalidates every vector.
    pub vector_dim: usize,
    /// Run only the text backbone (saves RAM, disables image/video search).
    pub index_images: bool,
    pub index_video: bool,
    pub index_documents: bool,
    pub index_code: bool,
    /// Model is fetched on first launch from these endpoints, in order.
    pub model_endpoints: Vec<String>,
    pub model_repo: String,
    pub infer_python: Option<String>,
    pub infer_threads: usize,
    pub infer_batch: usize,
    /// Video clip window/step in seconds. The encoder caps one input at 32 sampled
    /// frames, so a window longer than 32 s at 1 fps loses its tail.
    pub video_window_s: f64,
    pub video_step_s: f64,
    pub video_fps: f64,
    pub max_text_store_kb: usize,
    pub max_extract_bytes: u64,
    pub max_file_bytes: u64,
    pub exclude_dirs: Vec<String>,
    pub exclude_path_prefixes: Vec<String>,
    pub global_shortcut: String,
    pub resume_on_launch: bool,
    pub ram_budget_mb: usize,
    /// Proxy for model weights and update packages. From Finder there is no `HTTPS_PROXY` in
    /// the environment, and a direct connection to GitHub or the HF CDN is unreliable here.
    /// Only used when something is actually listening; otherwise we go direct.
    pub net_proxy: Option<String>,
    /// Check for a newer signed release in the background shortly after launch.
    pub update_check: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            vector_dim: 512,
            index_images: true,
            index_video: true,
            index_documents: true,
            index_code: true,
            model_endpoints: vec![
                "https://hf-mirror.com".into(),
                "https://huggingface.co".into(),
            ],
            model_repo: "google/embeddinggemma-2".into(),
            infer_python: None,
            infer_threads: 0,
            infer_batch: 8,
            video_window_s: 16.0,
            video_step_s: 12.0,
            video_fps: 1.0,
            max_text_store_kb: 64,
            max_extract_bytes: 32 * 1024 * 1024,
            max_file_bytes: 256 * 1024 * 1024,
            exclude_dirs: vec![
                "node_modules".into(),
                ".git".into(),
                "target".into(),
                "build".into(),
                "dist".into(),
                ".venv".into(),
                "venv".into(),
                "__pycache__".into(),
                ".cache".into(),
                ".npm".into(),
                ".next".into(),
                ".turbo".into(),
                "Pods".into(),
                ".gradle".into(),
                ".terraform".into(),
                "DerivedData".into(),
            ],
            exclude_path_prefixes: vec![
                "/System".into(),
                "/Library".into(),
                "/usr".into(),
                "/bin".into(),
                "/sbin".into(),
                "/private/var".into(),
                "/proc".into(),
                "/sys".into(),
            ],
            // Alt+Space by default: Cmd+Space is macOS Spotlight and must not be stolen.
            global_shortcut: "Alt+Space".into(),
            resume_on_launch: true,
            ram_budget_mb: 4096,
            net_proxy: Some("http://127.0.0.1:7890".into()),
            update_check: true,
        }
    }
}

impl Settings {
    pub fn load(path: &std::path::Path) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) -> crate::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn validate(&mut self) {
        self.vector_dim = match self.vector_dim {
            768 | 512 | 256 => self.vector_dim,
            _ => 512,
        };
        self.video_window_s = self.video_window_s.clamp(2.0, 32.0);
        self.video_step_s = self.video_step_s.clamp(1.0, self.video_window_s);
        self.video_fps = self.video_fps.clamp(0.1, 4.0);
        self.infer_batch = self.infer_batch.clamp(1, 32);
        // 0 不是「不限制」而是「一个文件都不收」：扫描会静默跳过全部内容，
        // 队列看起来像扫完了。留在 1MB 以上才是个有意义的单文件上限。
        self.max_file_bytes = self.max_file_bytes.clamp(1024 * 1024, 16 * 1024 * 1024 * 1024);
        if (self.video_window_s * self.video_fps) as usize > 32 {
            // never ask the encoder for more than max_frames
            self.video_fps = 32.0 / self.video_window_s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_size_limit_stays_usable() {
        let mut s = Settings::default();
        s.max_file_bytes = 0;
        s.validate();
        assert_eq!(s.max_file_bytes, 1024 * 1024);

        s.max_file_bytes = u64::MAX;
        s.validate();
        assert_eq!(s.max_file_bytes, 16 * 1024 * 1024 * 1024);
    }
}
