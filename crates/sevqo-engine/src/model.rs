use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const MODEL_REPO: &str = "google/embeddinggemma-2";
pub const MODEL_ID: &str = "embeddinggemma-2";

#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub path: &'static str,
    pub size: u64,
}

/// Files required to load `SentenceTransformer(dir)` locally. Sizes come from the
/// repository tree at pin time; the model is Apache-2.0 and ungated, so no token is
/// ever needed.
pub const FILES: &[ModelFile] = &[
    ModelFile { path: "1_Pooling/config.json", size: 90 },
    ModelFile { path: "2_Normalize/config.json", size: 97 },
    ModelFile { path: "README.md", size: 21730 },
    ModelFile { path: "chat_template.jinja", size: 1016 },
    ModelFile { path: "config.json", size: 4455 },
    ModelFile { path: "config_sentence_transformers.json", size: 1565 },
    ModelFile { path: "modules.json", size: 413 },
    ModelFile { path: "preprocessor_config.json", size: 511 },
    ModelFile { path: "processor_config.json", size: 1788 },
    ModelFile { path: "sentence_bert_config.json", size: 747 },
    ModelFile { path: "tokenizer_config.json", size: 1599 },
    ModelFile { path: "tokenizer.model", size: 4_689_013 },
    ModelFile { path: "tokenizer.json", size: 32_170_510 },
    ModelFile { path: "model.safetensors", size: 1_488_915_288 },
];

pub fn total_bytes() -> u64 {
    FILES.iter().map(|f| f.size).sum()
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub dir: String,
    pub present: usize,
    pub total_files: usize,
    pub complete: bool,
    pub bytes_on_disk: u64,
    pub total_bytes: u64,
    pub missing: Vec<String>,
    pub mismatched: Vec<String>,
    pub resume_bytes: u64,
}

pub fn status(dir: &Path) -> ModelStatus {
    let mut present = 0usize;
    let mut bytes_on_disk = 0u64;
    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    let mut resume_bytes = 0u64;
    for f in FILES {
        let p = dir.join(f.path);
        let part = dir.join(format!("{}.part", f.path));
        match std::fs::metadata(&p) {
            Ok(m) if m.len() == f.size => {
                present += 1;
                bytes_on_disk += f.size;
            }
            Ok(m) => {
                // Read-only: this runs on every UI poll, and `SEVQO_MODEL_HOME` may point
                // at a directory we did not create. `ensure()` re-fetches these files.
                missing.push(f.path.to_string());
                mismatched.push(format!("{} ({})", f.path, m.len()));
            }
            Err(_) => {
                if let Ok(pm) = std::fs::metadata(&part) {
                    resume_bytes += pm.len().min(f.size);
                }
                missing.push(f.path.to_string());
            }
        }
    }
    ModelStatus {
        dir: dir.display().to_string(),
        present,
        total_files: FILES.len(),
        complete: missing.is_empty(),
        bytes_on_disk,
        total_bytes: total_bytes(),
        missing,
        mismatched,
        resume_bytes,
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub file: String,
    pub file_index: usize,
    pub file_count: usize,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub overall_bytes: u64,
    pub overall_total: u64,
    pub bytes_per_second: f64,
    pub endpoint: String,
    pub note: Option<String>,
}

fn url(endpoint: &str, path: &str) -> String {
    format!("{}/{}/resolve/main/{}?download=true", endpoint.trim_end_matches('/'), MODEL_REPO, path)
}

fn sha256_of(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Records checksums once a file lands, so later launches can detect corruption.
fn remember_checksum(dir: &Path, rel: &str, digest: &str) {
    let mut all: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let idx = dir.join("checksums.json");
    if let Ok(s) = std::fs::read_to_string(&idx) {
        if let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(&s) {
            all = m;
        }
    }
    all.insert(rel.to_string(), serde_json::Value::String(digest.to_string()));
    let _ = std::fs::write(&idx, serde_json::to_vec_pretty(&all).unwrap_or_default());
}

struct Fetch<'a> {
    client: &'a reqwest::blocking::Client,
    dir: &'a Path,
    endpoints: &'a [String],
}

impl<'a> Fetch<'a> {
    /// Range-resume one file. Returns Err only after exhausting `attempts`.
    fn one(
        &self,
        rel: &str,
        size: u64,
        attempts: usize,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(DownloadProgress),
        file_index: usize,
        overall_base: u64,
    ) -> Result<()> {
        let final_path = self.dir.join(rel);
        let part_path = self.dir.join(format!("{rel}.part"));
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut last_err = String::new();
        for attempt in 0..attempts {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let endpoint = &self.endpoints[attempt % self.endpoints.len().max(1)];
            let mut have = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
            if have > size {
                let _ = std::fs::remove_file(&part_path);
                have = 0;
            }
            match self.stream_to_part(endpoint, rel, size, have, &part_path, cancel, |done, rate| {
                on_progress(DownloadProgress {
                    file: rel.to_string(),
                    file_index,
                    file_count: FILES.len(),
                    done_bytes: done,
                    total_bytes: size,
                    overall_bytes: overall_base + done,
                    overall_total: total_bytes(),
                    bytes_per_second: rate,
                    endpoint: endpoint.clone(),
                    note: None,
                })
            }) {
                Ok(digest) => {
                    std::fs::rename(&part_path, &final_path)?;
                    remember_checksum(self.dir, rel, &digest);
                    return Ok(());
                }
                Err(err) => {
                    last_err = err.to_string();
                    let wait = (3 * (attempt as u64 + 1)).min(45);
                    tracing::warn!("model fetch {rel} attempt {attempt} failed: {last_err}; retrying in {wait}s");
                    let until = Instant::now() + Duration::from_secs(wait);
                    while Instant::now() < until {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        }
        Err(Error::Download(format!("{rel}: {last_err}")))
    }

    fn stream_to_part(
        &self,
        endpoint: &str,
        rel: &str,
        size: u64,
        mut have: u64,
        part: &Path,
        cancel: &AtomicBool,
        mut report: impl FnMut(u64, f64),
    ) -> Result<String> {
        let mut req = self.client.get(url(endpoint, rel));
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut resp = req.send().map_err(|e| Error::Download(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Download(format!("http {status}")));
        }
        if have > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            // endpoint ignored Range: start over rather than appending a corrupt mix
            have = 0;
        }
        let mut digest = Sha256::new();
        if have > 0 {
            let mut f = std::fs::File::open(part)?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                digest.update(&buf[..n]);
            }
        }
        let mut file = if have > 0 {
            std::fs::OpenOptions::new().append(true).open(part)?
        } else {
            std::fs::File::create(part)?
        };
        let started = Instant::now();
        let mut done = have;
        let mut last_emit = Instant::now();
        let mut buf = vec![0u8; 1 << 20];
        // `Response` is a `Read`: pulling the bytes ourselves keeps `cancel` responsive,
        // which `copy_to` would not.
        while done < size {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let n = resp.read(&mut buf).map_err(|e| Error::Download(e.to_string()))?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut file, &buf[..n])?;
            digest.update(&buf[..n]);
            done += n as u64;
            if last_emit.elapsed() > Duration::from_millis(400) {
                let rate = done.saturating_sub(have) as f64 / started.elapsed().as_secs_f64().max(0.001);
                report(done, rate);
                last_emit = Instant::now();
            }
        }
        std::io::Write::flush(&mut file)?;
        drop(file);
        if done != size {
            return Err(Error::Download(format!("连接中断于 {done}/{size} 字节")));
        }
        Ok(digest.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// Ensure the model directory holds every required file, downloading what is missing.
pub fn ensure(
    dir: &Path,
    endpoints: &[String],
    proxy: Option<&str>,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<()> {
    let endpoints = if endpoints.is_empty() {
        vec!["https://hf-mirror.com".to_string(), "https://huggingface.co".to_string()]
    } else {
        endpoints.to_vec()
    };
    std::fs::create_dir_all(dir)?;
    let client = crate::net::download_client(
        "Sevqo/0.1 (local media search)",
        proxy.map(|s| s.trim()).filter(|s| !s.is_empty()),
    )?;
    let fetch = Fetch { client: &client, dir, endpoints: &endpoints };
    let recorded = recorded_checksums(dir);

    let mut overall = 0u64;
    for (i, f) in FILES.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let p = dir.join(f.path);
        let size_ok = std::fs::metadata(&p).map(|m| m.len() == f.size).unwrap_or(false);
        let checksum_ok = if size_ok {
            match recorded.get(f.path).and_then(|v| v.as_str()) {
                Some(want) => sha256_of(&p)? == want,
                None => true,
            }
        } else { false };
        if checksum_ok {
            overall += f.size;
            on_progress(DownloadProgress {
                file: f.path.to_string(),
                file_index: i,
                file_count: FILES.len(),
                done_bytes: f.size,
                total_bytes: f.size,
                overall_bytes: overall,
                overall_total: total_bytes(),
                bytes_per_second: 0.0,
                endpoint: String::new(),
                note: Some("cached".into()),
            });
            continue;
        }
        let base = overall;
        fetch.one(f.path, f.size, 24, cancel, &mut |mut pr| {
            pr.overall_bytes = base + pr.done_bytes;
            on_progress(pr)
        }, i, base)?;
        overall += f.size;
    }
    verify(dir)?;
    Ok(())
}

/// Size check for everything, plus recorded checksums when we have them.
fn recorded_checksums(dir: &Path) -> serde_json::Map<String, serde_json::Value> {
    let idx = dir.join("checksums.json");
    std::fs::read_to_string(&idx)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

pub fn verify(dir: &Path) -> Result<()> {
    let recorded = recorded_checksums(dir);
    for f in FILES {
        let p = dir.join(f.path);
        let m = std::fs::metadata(&p).map_err(|_| Error::Download(format!("缺少 {}", f.path)))?;
        if m.len() != f.size {
            return Err(Error::Download(format!("{} 大小不符", f.path)));
        }
        if let Some(serde_json::Value::String(want)) = recorded.get(f.path) {
            let got = sha256_of(&p)?;
            if &got != want {
                return Err(Error::Download(format!("{} 校验失败，请重新下载模型", f.path)));
            }
        }
    }
    Ok(())
}

pub fn model_dir_marker(dir: &Path) -> PathBuf {
    dir.join("checksums.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_never_deletes_a_corrupt_user_model_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = &FILES[0];
        let path = dir.path().join(first.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = vec![b'x'; first.size as usize];
        std::fs::write(&path, &bytes).unwrap();
        let recorded = std::collections::HashMap::from([(first.path, "incorrect")]);
        std::fs::write(dir.path().join("checksums.json"), serde_json::to_vec(&recorded).unwrap()).unwrap();
        assert!(verify(dir.path()).unwrap_err().to_string().contains("校验失败"));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    fn placeholder(dir: &Path, rel: &str, len: u64) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    }

    #[test]
    fn url_survives_a_trailing_slash_in_the_endpoint() {
        assert_eq!(
            url("https://hf-mirror.com/", "1_Pooling/config.json"),
            "https://hf-mirror.com/google/embeddinggemma-2/resolve/main/1_Pooling/config.json?download=true"
        );
    }

    #[test]
    fn status_counts_present_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let empty = status(dir);
        assert!(!empty.complete);
        assert_eq!(empty.missing.len(), FILES.len());

        placeholder(dir, FILES[0].path, FILES[0].size);
        let one = status(dir);
        assert_eq!(one.present, 1);
        assert_eq!(one.missing.len(), FILES.len() - 1);
        assert!(!one.complete);
    }

    #[test]
    fn status_does_not_delete_files_it_does_not_own() {
        // Polled from the UI, and SEVQO_MODEL_HOME may point at a checkout we didn't make:
        // a same-named file of a different size belongs to the user, so only report it.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let p = dir.join(FILES[0].path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"an lfs pointer, not a model").unwrap();

        let st = status(dir);
        assert!(st.mismatched.iter().any(|m| m.starts_with(FILES[0].path)));
        assert!(p.exists(), "status() must not remove a wrong-sized file");
        assert!(!st.complete);
    }

    #[test]
    fn verify_rejects_a_truncated_weight_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for f in FILES {
            let p = dir.join(f.path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"short").unwrap();
        }
        assert!(verify(dir).is_err());
    }
}
