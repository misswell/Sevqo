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
    /// Load + validate (第十一章)：启动时也执行统一校验，而不是只在用户保存时。
    /// 解析失败回退默认值，但把坏文件留档为 `settings.bad.json` 供诊断——
    /// 直接覆盖会让用户失去找回旧配置的机会。
    pub fn load(path: &std::path::Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<Settings>(&s) {
                Ok(mut v) => {
                    v.validate();
                    v
                }
                Err(e) => {
                    tracing::warn!(
                        "设置文件解析失败（{}），使用默认值；原文件保留为 settings.bad.json",
                        e
                    );
                    let _ = std::fs::copy(path, path.with_extension("bad.json"));
                    let mut v = Settings::default();
                    v.validate();
                    v
                }
            },
            Err(_) => {
                let mut v = Settings::default();
                v.validate();
                v
            }
        }
    }

    /// 原子保存（第十一章）：临时文件写入 + sync + 原子替换；替换前保留上一份
    /// `settings.bak.json`，写坏时用户有一步回退。
    pub fn save(&self, path: &std::path::Path) -> crate::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            std::io::Write::write_all(&mut f, serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
        }
        if path.exists() {
            let _ = std::fs::copy(path, path.with_extension("bak.json"));
        }
        std::fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
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
        self.max_file_bytes = self
            .max_file_bytes
            .clamp(1024 * 1024, 16 * 1024 * 1024 * 1024);
        if (self.video_window_s * self.video_fps) as usize > 32 {
            // never ask the encoder for more than max_frames
            self.video_fps = 32.0 / self.video_window_s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 原子保存与加载校验（十一章）：保存走临时文件+替换并留 .bak；
    /// 损坏的设置文件回退默认但留档 .bad.json；加载时也执行 validate。
    #[test]
    fn settings_save_load_roundtrip_with_backup_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        let cfg = Settings {
            max_file_bytes: 0,
            ..Settings::default()
        }; // 非法值：validate 会收到 1MB 下限
        cfg.save(&path).unwrap();
        cfg.save(&path).unwrap(); // 第二次保存时才存在可备份的上一份
        assert!(
            path.with_extension("bak.json").exists(),
            "保存应保留上一份备份"
        );
        let loaded = Settings::load(&path);
        assert_eq!(loaded.max_file_bytes, 1024 * 1024, "加载时也要 validate");

        // 坏文件：解析失败回退默认，且原文件留档。
        std::fs::write(&path, "{not json").unwrap();
        let fallback = Settings::load(&path);
        assert_eq!(fallback.vector_dim, 512);
        assert!(path.with_extension("bad.json").exists(), "坏文件要留档诊断");
        // 原样保留，没有被覆盖。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }

    #[test]
    fn file_size_limit_stays_usable() {
        let mut s = Settings {
            max_file_bytes: 0,
            ..Settings::default()
        };
        s.validate();
        assert_eq!(s.max_file_bytes, 1024 * 1024);

        s.max_file_bytes = u64::MAX;
        s.validate();
        assert_eq!(s.max_file_bytes, 16 * 1024 * 1024 * 1024);
    }
}
