//! Supervisor for the Python inference sidecar.
//!
//! Model loading, media decoding and PDF text live in a child process so the app never
//! links torch. The protocol is JSON lines over stdin/stdout: one request, one response,
//! correlated by `id`. Requests are serialised under one lock, which also keeps torch
//! from running two encodes at once and blowing the RAM budget.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::Receiver;
use serde_json::{json, Value};

use crate::config::Settings;
use crate::paths::Paths;
use crate::{EmbedItem, EmbedResponse, Error, Result};

#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub duration_s: f64,
    pub width: i32,
    pub height: i32,
    pub has_audio: bool,
    pub fps: f64,
    pub has_video: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimeInfo {
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

struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
}

/// `shutdown` is the polite path. This is the one that still fires when a panic or a
/// settings change simply releases the session — an orphaned sidecar parks ~1 GB of
/// weights and holds the model directory open.
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Client {
    inner: Arc<Mutex<Option<Session>>>,
    next_id: AtomicU64,
    log_path: PathBuf,
    program: Vec<String>,
    env: HashMap<String, String>,
    dim: usize,
    needs_vision: AtomicBool,
}

/// Locate the sidecar's interpreter and package root.
///
/// Order: explicit setting, `SEVQO_PYTHON`, a `.venv` next to the executable or in the
/// working directory, then the system `python3`.
pub fn python_binary(settings: &Settings, preferred: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = settings.infer_python.as_ref().filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = preferred {
        return Some(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("SEVQO_PYTHON") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    for root in candidate_roots() {
        for rel in ["../inference", "inference"] {
            let manifest = root.join(rel);
            if manifest.join("sevqo_embed").is_dir() {
                for py in [".venv/bin/python", ".venv/Scripts/python.exe", "venv/bin/python"] {
                    let c = root.join(py);
                    if c.exists() {
                        return Some(c);
                    }
                }
            }
        }
    }
    None
}

fn candidate_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        let mut d = exe.parent().map(|p| p.to_path_buf());
        while let Some(dir) = d {
            out.push(dir.clone());
            if dir.parent().is_some() {
                d = dir.parent().map(|p| p.to_path_buf());
            } else {
                break;
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd);
    }
    out
}

/// Where `python -m sevqo_embed` finds the package: the checkout layout while developing,
/// otherwise the copy materialised from the binary — a bundle has no `inference/` next to it.
fn package_root(paths: &Paths) -> Option<PathBuf> {
    package_root_in(candidate_roots(), paths)
}

fn package_root_in(
    roots: impl IntoIterator<Item = PathBuf>,
    paths: &Paths,
) -> Option<PathBuf> {
    for root in roots {
        for rel in ["../inference", "inference", "../../inference"] {
            let dir = root.join(rel);
            if dir.join("sevqo_embed").is_dir() {
                return Some(dir);
            }
        }
    }
    crate::sidecar_assets::stage(&paths.cache_dir.join("sidecar")).ok()
}

impl Client {
    pub fn new(paths: &Paths, settings: &Settings) -> Client {
        let py = python_binary(settings, None)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "python3".to_string());
        let mut env = HashMap::new();
        env.insert("PYTHONUNBUFFERED".into(), "1".into());
        if let Some(root) = package_root(paths) {
            env.insert("PYTHONPATH".into(), root.to_string_lossy().to_string());
        }
        env.insert("SEVQO_MODEL_HOME".into(), paths.model_dir.to_string_lossy().to_string());
        env.insert("OMP_NUM_THREADS".into(), settings.infer_threads.max(1).to_string());
        Client {
            inner: Arc::new(Mutex::new(None)),
            next_id: AtomicU64::new(1),
            log_path: paths.log_dir.join("sidecar.log"),
            program: vec![
                py,
                "-m".into(),
                "sevqo_embed".into(),
                "serve".into(),
                "--dim".into(),
                settings.vector_dim.to_string(),
                "--threads".into(),
                settings.infer_threads.to_string(),
            ],
            env,
            dim: settings.vector_dim,
            needs_vision: AtomicBool::new(settings.index_images || settings.index_video),
        }
    }

    /// `--text-only` is what keeps the vision tower out of RAM, so changing media
    /// settings recycles the sidecar instead of leaving it on the wrong feature set.
    pub fn set_needs_vision(&self, vision: bool) {
        if self.needs_vision.swap(vision, Ordering::Relaxed) != vision {
            self.restart();
        }
    }

    fn args(&self) -> Vec<String> {
        let mut a = self.program.clone();
        if !self.needs_vision.load(Ordering::Relaxed) {
            a.push("--text-only".into());
        }
        a
    }

    fn start(&self) -> Result<Session> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let args = self.args();
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..])
            .envs(&self.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log));
        let mut child = cmd.spawn().map_err(|e| {
            Error::Infer(format!(
                "无法启动推理进程（{}）：{e}。可在设置中指定 Python 路径。",
                args[0]
            ))
        })?;
        let stdin = child.stdin.take().ok_or_else(|| Error::Infer("缺少 stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Infer("缺少 stdout".into()))?;
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                            let _ = tx.send(v);
                        }
                    }
                }
            }
        });
        Ok(Session { child, stdin, rx })
    }

    fn session(&self) -> Result<std::sync::MutexGuard<'_, Option<Session>>> {
        let mut guard = self.inner.lock().map_err(|_| Error::Infer("锁中毒".into()))?;
        if guard.is_none() {
            *guard = Some(self.start()?);
        }
        Ok(guard)
    }

    fn restart(&self) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(mut s) = guard.take() {
            let _ = s.child.kill();
            let _ = s.child.wait();
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    fn request(&self, payload: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut req = serde_json::Map::new();
        if let Some(obj) = payload.as_object() {
            for (k, v) in obj {
                req.insert(k.clone(), v.clone());
            }
        }
        req.insert("id".into(), json!(id));
        let line = serde_json::to_string(&Value::Object(req))?;

        let attempt = |guard: &mut Option<Session>| -> Result<Value> {
            let s = guard.as_mut().ok_or_else(|| Error::Infer("进程未启动".into()))?;
            s.stdin.write_all(line.as_bytes())?;
            s.stdin.write_all(b"\n")?;
            s.stdin.flush()?;
            loop {
                let resp = match s.rx.recv_timeout(timeout) {
                    Ok(v) => v,
                    Err(_) => {
                        let alive = matches!(s.child.try_wait(), Ok(None));
                        if alive {
                            return Err(Error::Infer(format!("推理进程 {timeout:?} 无响应")));
                        }
                        return Err(Error::Infer("推理进程已退出".into()));
                    }
                };
                if resp.get("id").and_then(Value::as_u64) == Some(id) {
                    return Ok(resp);
                }
                // stale response from an earlier timed-out request: drop it
            }
        };

        let mut guard = self.session()?;
        match attempt(&mut guard) {
            Err(first) => {
                // one restart: a killed or OOM'd sidecar should not fail the whole batch
                drop(guard);
                self.restart();
                let mut guard = self.session()?;
                attempt(&mut guard).map_err(|_| Error::Infer(format!("{first}；重启后仍失败")))
            }
            Ok(v) => Ok(v),
        }
    }

    pub fn load(&self) -> Result<RuntimeInfo> {
        let v = self.request(json!({"cmd": "info"}), Duration::from_secs(600))?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Infer(
                v.get("error").and_then(Value::as_str).unwrap_or("模型加载失败").to_string(),
            ));
        }
        Ok(RuntimeInfo {
            device: v.get("device").and_then(Value::as_str).unwrap_or("?").to_string(),
            dtype: v.get("dtype").and_then(Value::as_str).unwrap_or("?").to_string(),
            dim: v.get("dim").and_then(Value::as_u64).unwrap_or(0) as usize,
            params_millions: v.get("params_millions").and_then(Value::as_f64).unwrap_or(0.0),
            load_seconds: v.get("load_seconds").and_then(Value::as_f64).unwrap_or(0.0),
            vision: v.get("vision").and_then(Value::as_bool).unwrap_or(false),
            audio: v.get("audio").and_then(Value::as_bool).unwrap_or(false),
            threads: v.get("threads").and_then(Value::as_u64).unwrap_or(0) as usize,
            rss_mb: v.get("rss_mb").and_then(Value::as_f64).unwrap_or(-1.0),
        })
    }

    /// Embed one batch. Query-role text takes the search-instruction prefix, documents
    /// take the `title: … | text: …` form; media items take no prefix.
    pub fn embed(&self, items: &[EmbedItem], batch: usize, timeout: Duration) -> Result<EmbedResponse> {
        if items.is_empty() {
            return Ok(EmbedResponse::default());
        }
        let payload = json!({
            "cmd": "embed",
            "items": items,
            "batch_size": batch.max(1),
        });
        let v = self.request(payload, timeout)?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) && v.get("vectors").is_none() {
            return Err(Error::Infer(
                v.get("error").and_then(Value::as_str).unwrap_or("嵌入失败").to_string(),
            ));
        }
        let mut resp: EmbedResponse = serde_json::from_value(v.clone()).unwrap_or_default();
        resp.vectors.retain(|_, v| {
            if v.len() != self.dim {
                return false;
            }
            v.iter().all(|x| x.is_finite())
        });
        Ok(resp)
    }

    pub fn probe(&self, path: &Path, timeout: Duration) -> Result<ProbeInfo> {
        let v = self.request(json!({"cmd": "probe", "path": path.to_string_lossy()}), timeout)?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Infer(
                v.get("error").and_then(Value::as_str).unwrap_or("无法解析媒体").to_string(),
            ));
        }
        Ok(ProbeInfo {
            duration_s: v.get("durationS").and_then(Value::as_f64).unwrap_or(0.0),
            width: v.get("width").and_then(Value::as_i64).unwrap_or(0) as i32,
            height: v.get("height").and_then(Value::as_i64).unwrap_or(0) as i32,
            has_audio: v.get("hasAudio").and_then(Value::as_bool).unwrap_or(false),
            fps: v.get("fps").and_then(Value::as_f64).unwrap_or(0.0),
            has_video: v.get("hasVideo").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    pub fn extract_pdf(&self, path: &Path, max_chars: usize) -> Result<(Option<String>, String, bool)> {
        let v = self.request(
            json!({"cmd": "text", "path": path.to_string_lossy(), "maxChars": max_chars}),
            Duration::from_secs(120),
        )?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Unsupported(
                v.get("error").and_then(Value::as_str).unwrap_or("PDF 解析失败").to_string(),
            ));
        }
        Ok((
            v.get("title").and_then(Value::as_str).map(|s| s.to_string()),
            v.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            v.get("truncated").and_then(Value::as_bool).unwrap_or(false),
        ))
    }

    pub fn shutdown(&self) {
        let guard = self.inner.lock();
        if let Ok(mut g) = guard {
            if let Some(mut s) = g.take() {
                let _ = s.stdin.write_all(b"{\"cmd\":\"shutdown\"}\n");
                let _ = s.stdin.flush();
                let _ = s.child.wait_timeout(Duration::from_secs(3));
                let _ = s.child.kill();
            }
        }
    }

    /// Ask the OS to stop the reader thread when the app quits.
    pub fn pid(&self) -> Option<u32> {
        self.inner.lock().ok().and_then(|g| g.as_ref().map(|s| s.child.id()))
    }
}

trait WaitTimeout {
    fn wait_timeout(&mut self, d: Duration) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl WaitTimeout for Child {
    fn wait_timeout(&mut self, d: Duration) -> std::io::Result<Option<std::process::ExitStatus>> {
        let deadline = std::time::Instant::now() + d;
        while std::time::Instant::now() < deadline {
            if let Some(st) = self.try_wait()? {
                return Ok(Some(st));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path in one temp dir: these tests only care about the command line, not storage.
    fn paths_in(dir: &tempfile::TempDir) -> Paths {
        Paths {
            data_dir: dir.path().into(),
            cache_dir: dir.path().into(),
            model_dir: dir.path().into(),
            thumb_dir: dir.path().into(),
            clip_thumb_dir: dir.path().into(),
            db_path: dir.path().join("i.sqlite"),
            vector_path: dir.path().join("v.f32"),
            settings_path: dir.path().join("s.json"),
            log_dir: dir.path().into(),
        }
    }

    #[test]
    fn program_line_targets_the_sidecar_module() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(&dir);
        let mut st = Settings::default();
        st.infer_python = Some("/opt/py/bin/python".into());
        let c = Client::new(&paths, &st);
        assert_eq!(c.program[0], "/opt/py/bin/python");
        assert!(c.program.iter().any(|a| a == "serve"));
        assert!(c.program.iter().any(|a| a == "sevqo_embed"));
        assert_eq!(c.env["PYTHONUNBUFFERED"], "1");
    }

    /// Without a checkout around the executable the package has to come from the embedded copy,
    /// or `python -m sevqo_embed` starts with no PYTHONPATH and dies on ModuleNotFoundError.
    #[test]
    fn falls_back_to_the_embedded_copy() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(&dir);
        let root = package_root_in(Vec::<PathBuf>::new(), &paths).expect("a staged package");
        assert_eq!(root, paths.cache_dir.join("sidecar"));
        assert!(root.join("sevqo_embed/__main__.py").is_file());
        assert!(root.join("sevqo_embed/server.py").is_file());
    }

    #[test]
    fn a_checkout_takes_precedence_over_the_embedded_copy() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(&dir);
        let around_exe = dir.path().join("bundle");
        std::fs::create_dir_all(around_exe.join("inference/sevqo_embed")).unwrap();
        let root = package_root_in(vec![around_exe.clone()], &paths).unwrap();
        assert_eq!(
            root,
            around_exe.join("inference"),
            "editing a module must take effect without a rebuild"
        );
    }

    #[test]
    fn text_only_flag_toggles() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(&dir);
        let st = Settings::default();
        let c = Client::new(&paths, &st);
        assert!(!c.args().iter().any(|a| a == "--text-only"), "media on by default");
        c.set_needs_vision(false);
        assert!(c.args().iter().any(|a| a == "--text-only"));
        c.set_needs_vision(true);
        assert!(!c.args().iter().any(|a| a == "--text-only"));
    }

    #[test]
    fn missing_python_reports_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(&dir);
        let mut st = Settings::default();
        st.infer_python = Some("/nonexistent/python".into());
        let c = Client::new(&paths, &st);
        let err = c.load().unwrap_err().to_string();
        assert!(err.contains("Python") || err.contains("启动"), "{err}");
    }
}
