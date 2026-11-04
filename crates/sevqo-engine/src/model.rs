use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const MODEL_REPO: &str = "google/embeddinggemma-2";
pub const MODEL_ID: &str = "embeddinggemma-2";

/// 不可变模型版本（P0-04）：下载一律走 `/resolve/<revision>/`，上游不再能用 main
/// 悄悄替换内容。固定前经 HF API 核对过该 commit 的文件树。
pub const MODEL_REVISION: &str = "f9972ac22e696a105c482202646979e33e4b5a1e";

#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub path: &'static str,
    pub size: u64,
    /// 来自上游的可信 SHA256（P0-04）：三个 LFS 大文件取自 HF tree API 在该 revision
    /// 下登记的 LFS oid；其余小文件在 2026-11-05 从 `/resolve/<revision>/` 全量下载后
    /// 计算并逐一复核。本地首次下载生成的哈希只作变化检测，从不当作信任依据。
    pub sha256: &'static str,
}

/// Files required to load `SentenceTransformer(dir)` locally. The model is Apache-2.0
/// and ungated, so no token is ever needed.
pub const FILES: &[ModelFile] = &[
    ModelFile { path: "1_Pooling/config.json", size: 90, sha256: "8759bdf7c77efc7df7723f64856a593c8943b71ee38baf2a88771fbaf78438f9" },
    ModelFile { path: "2_Normalize/config.json", size: 97, sha256: "cdb09dfca347a56aa2d691744e38d5ad3c7cbc2834e7181272b9a15328b82524" },
    ModelFile { path: "README.md", size: 22_234, sha256: "2c058c304c68221f1de80979594036eff3972b2bce686bb7b0f43621a6b0b011" },
    ModelFile { path: "chat_template.jinja", size: 1016, sha256: "4b852efc0b9960283e735363331e6f325b33bc74bdbaa076f595bc4e9b94d85e" },
    ModelFile { path: "config.json", size: 4455, sha256: "b8f1e9931b57fbc054acdb445c41765d55b0074c58d145fa82839941ad1b5bb3" },
    ModelFile { path: "config_sentence_transformers.json", size: 1565, sha256: "031e56a498d33c349ab489a21885bcfe25b4fcba841149dc99e1e90d4a7c28f5" },
    ModelFile { path: "modules.json", size: 413, sha256: "3d02572a0455b832de67fb8e63a54981bc7e8b46e337c95e917bd8122a533bfd" },
    ModelFile { path: "preprocessor_config.json", size: 511, sha256: "ea2ae257e901064abdd98dceb19f2b0da06af600bed15e0f99f5c85c37ee9d78" },
    ModelFile { path: "processor_config.json", size: 1788, sha256: "168f6a08522f3ce5dea596d94d003af2fd691742d4f41fe1f9d8cce76bfbf69c" },
    ModelFile { path: "sentence_bert_config.json", size: 747, sha256: "b1bcd9f2dce3ae863b359e87d0710b5dbc3314a59ecb4e2f97c7778fc8e4b228" },
    ModelFile { path: "tokenizer_config.json", size: 1599, sha256: "17bd5d6e9364ca49a534e1502076593317c298d4a663623091ed45388f004874" },
    ModelFile { path: "tokenizer.model", size: 4_689_013, sha256: "e594c8a90eb08d8bda498ff4747977dc827ae0c3c56b5c0d41a605a22d02ef03" },
    ModelFile { path: "tokenizer.json", size: 32_170_510, sha256: "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4" },
    ModelFile { path: "model.safetensors", size: 1_488_915_288, sha256: "197a32965d4b1105faf060417baa899e193fb73cd401f42ec9295234d5553d79" },
];

pub fn total_bytes() -> u64 {
    FILES.iter().map(|f| f.size).sum()
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub dir: String,
    /// 本构建信任的 manifest 版本；status 里带出来供界面/诊断对照。
    pub revision: &'static str,
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
        revision: MODEL_REVISION,
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
    format!(
        "{}/{}/resolve/{}/{}?download=true",
        endpoint.trim_end_matches('/'),
        MODEL_REPO,
        MODEL_REVISION,
        path
    )
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
    /// Range-resume one file. Returns Err only after exhausting `attempts`. The download
    /// only replaces the target file after its SHA256 matches the trusted manifest
    /// (P0-04): a mismatch deletes the `.part` and retries from scratch, and any existing
    /// file is left untouched until a verified replacement exists.
    fn one(
        &self,
        rel: &str,
        size: u64,
        expected_sha: &str,
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
                Ok(digest) => match commit_verified(&part_path, &final_path, &digest, expected_sha) {
                    Ok(()) => {
                        remember_checksum(self.dir, rel, &digest);
                        return Ok(());
                    }
                    Err(e) => {
                        last_err = e.to_string();
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
                },
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

    let mut overall = 0u64;
    for (i, f) in FILES.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        // 尺寸达标还不够：必须与 manifest 里的可信 SHA256 一致才算「已有」。
        // 同尺寸损坏文件在这里被识别并触发重新下载——这就是一键修复的入口。
        let p = dir.join(f.path);
        let cached = std::fs::metadata(&p)
            .map(|m| m.len() == f.size)
            .unwrap_or(false)
            && sha256_of(&p).map(|got| got == f.sha256).unwrap_or(false);
        if cached {
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
        fetch.one(f.path, f.size, f.sha256, 24, cancel, &mut |mut pr| {
            pr.overall_bytes = base + pr.done_bytes;
            on_progress(pr)
        }, i, base)?;
        overall += f.size;
    }
    // 每个文件都已单独通过 manifest 校验（缓存哈希比对、下载提交前比对），
    // 不再对整个目录重复整轮哈希；需要全量复验时走 [`verify`]。
    Ok(())
}

/// A download becomes the real file only when its digest matches the trusted manifest.
/// On mismatch the `.part` is discarded (a resumed prefix can be corrupt) and the
/// existing file — if any — is never touched (P0-04).
fn commit_verified(part: &Path, final_path: &Path, digest: &str, expected_sha: &str) -> Result<()> {
    if !digest.eq_ignore_ascii_case(expected_sha) {
        let _ = std::fs::remove_file(part);
        return Err(Error::Download(format!("SHA256 校验失败：期望 {expected_sha}，实际 {digest}")));
    }
    std::fs::rename(part, final_path)?;
    Ok(())
}

/// Deep check against the trusted manifest: sizes *and* SHA256 for every file. This is
/// what the manual "校验/修复模型" action and the startup background check run.
pub fn verify(dir: &Path) -> Result<()> {
    for f in FILES {
        let p = dir.join(f.path);
        let m = std::fs::metadata(&p).map_err(|_| Error::Download(format!("缺少 {}", f.path)))?;
        if m.len() != f.size {
            return Err(Error::Download(format!("{} 大小不符（期望 {} 字节，实际 {}）", f.path, f.size, m.len())));
        }
        let got = sha256_of(&p)?;
        if got != f.sha256 {
            return Err(Error::Download(format!("{} 校验失败，内容与可信清单不符，请修复模型", f.path)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同尺寸损坏必须被识破：本地没有、也不需要任何「首次下载记录」——manifest 里的
    /// 可信 SHA256 就是唯一依据；校验失败绝不删除用户文件。
    #[test]
    fn verification_detects_same_size_corruption_and_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = &FILES[0];
        let path = dir.path().join(first.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = vec![b'x'; first.size as usize];
        std::fs::write(&path, &bytes).unwrap();
        assert!(verify(dir.path()).unwrap_err().to_string().contains("校验失败"));
        assert_eq!(std::fs::read(path).unwrap(), bytes, "校验失败不得删文件");
    }

    /// manifest 本身的健全性：每个文件都有 64 位十六进制哈希，revision 是 40 位
    /// 十六进制。写错一格就会让所有下载永远失败，所以钉在这里。
    #[test]
    fn manifest_entries_are_well_formed() {
        assert_eq!(MODEL_REVISION.len(), 40);
        assert!(MODEL_REVISION.bytes().all(|b| b.is_ascii_hexdigit()));
        for f in FILES {
            assert!(f.size > 0, "{}", f.path);
            assert_eq!(f.sha256.len(), 64, "{}", f.path);
            assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()), "{}", f.path);
        }
    }

    /// 下载只在哈希与可信清单一致时才替换目标文件；不一致时丢弃 .part、
    /// 保留原文件。
    #[test]
    fn a_download_commits_only_after_the_digest_matches() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("f.part");
        let final_path = dir.path().join("f.bin");
        std::fs::write(&part, b"corrupt bytes").unwrap();
        std::fs::write(&final_path, b"original").unwrap();

        let err = commit_verified(&part, &final_path, "deadbeef", "cafebabe").unwrap_err();
        assert!(err.to_string().contains("校验失败"), "{err}");
        assert!(!part.exists(), "校验失败的 .part 必须丢弃");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"original", "原文件不得被动过");

        std::fs::write(&part, b"good bytes").unwrap();
        commit_verified(&part, &final_path, "cafebabe", "CAFEBABE").unwrap();
        assert!(!part.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"good bytes");
    }

    fn placeholder(dir: &Path, rel: &str, len: u64) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    }

    #[test]
    fn url_pins_the_revision_not_main() {
        assert_eq!(
            url("https://hf-mirror.com/", "1_Pooling/config.json"),
            format!("https://hf-mirror.com/google/embeddinggemma-2/resolve/{MODEL_REVISION}/1_Pooling/config.json?download=true")
        );
        assert!(!url("https://huggingface.co", "x").contains("/resolve/main/"), "绝不追着 main 下载");
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
