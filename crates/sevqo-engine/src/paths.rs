use std::path::PathBuf;

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub model_dir: PathBuf,
    pub thumb_dir: PathBuf,
    pub clip_thumb_dir: PathBuf,
    pub db_path: PathBuf,
    pub vector_path: PathBuf,
    pub settings_path: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    pub fn resolve(model_home: Option<&str>) -> Result<Paths> {
        let data_dir = dirs::config_dir()
            .ok_or_else(|| Error::Config("找不到用户配置目录".into()))?
            .join("Sevqo");
        let cache_dir = dirs::cache_dir()
            .ok_or_else(|| Error::Config("找不到用户缓存目录".into()))?
            .join("Sevqo");
        let model_dir = match model_home {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => cache_dir.join("models").join("google").join("embeddinggemma-2"),
        };
        let thumb_dir = cache_dir.join("thumbs");
        Ok(Paths {
            log_dir: data_dir.join("logs"),
            clip_thumb_dir: cache_dir.join("clip-thumbs"),
            db_path: data_dir.join("index.sqlite"),
            vector_path: cache_dir.join("vectors.f32"),
            settings_path: data_dir.join("settings.json"),
            thumb_dir,
            model_dir,
            cache_dir,
            data_dir,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [
            &self.data_dir,
            &self.cache_dir,
            &self.thumb_dir,
            &self.clip_thumb_dir,
            &self.log_dir,
        ] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    /// Thumbnails are content-addressed so a moved file keeps its cached image.
    pub fn thumb_path(&self, key: &str) -> PathBuf {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        self.thumb_dir.join(format!("{:016x}.jpg", h))
    }

    pub fn clip_thumb_path(&self, file_id: i64, clip_id: i64) -> PathBuf {
        self.clip_thumb_dir
            .join(format!("{file_id}"))
            .join(format!("clip-{clip_id}.jpg"))
    }
}
