//! Online updates, taking the same route to the network as the rest of the app.
//!
//! The manifest is a GitHub Release asset (`…/releases/latest/download/latest.json`) and every
//! package is minisign-signed with the key whose public half sits in `tauri.conf.json`, so
//! rewriting the CDN is not enough to push a build. Routing mirrors `sevqo_engine::net`: try the
//! local proxy only when something is listening on it, then fall back to direct — GitHub is not
//! reliably reachable directly from this machine, and a stale proxy must not be a hard failure.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::commands::Res;
use crate::state::AppState;

pub const EV_UPDATE: &str = "update://state";

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// A proxy that answers TCP but is being throttled should not hold the whole check hostage.
const PROXY_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
const DIRECT_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Covers the manifest and the archive; a release package is tens of megabytes.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub notes: Option<String>,
    pub published_at: Option<String>,
}

/// Pushed on `update://state`; the UI renders straight from this.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEvent {
    /// `checking` | `upToDate` | `available` | `downloading` | `installing` | `installed` | `failed`
    pub state: &'static str,
    pub info: Option<UpdateInfo>,
    pub percent: Option<u8>,
    pub downloaded: Option<u64>,
    pub total: Option<u64>,
    pub error: Option<String>,
}

impl UpdateEvent {
    fn new(state: &'static str) -> Self {
        UpdateEvent {
            state,
            info: None,
            percent: None,
            downloaded: None,
            total: None,
            error: None,
        }
    }

    fn available(info: UpdateInfo) -> Self {
        UpdateEvent {
            info: Some(info),
            ..Self::new("available")
        }
    }

    fn failed(error: String) -> Self {
        UpdateEvent {
            error: Some(error),
            ..Self::new("failed")
        }
    }
}

fn info_of(u: &Update) -> UpdateInfo {
    UpdateInfo {
        version: u.version.clone(),
        current_version: u.current_version.clone(),
        notes: u.body.clone(),
        published_at: u.date.map(|d| d.to_string()),
    }
}

fn state_of(app: &AppHandle) -> Arc<AppState> {
    app.state::<Arc<AppState>>().inner().clone()
}

fn updater(app: &AppHandle, proxy: Option<&str>) -> Res<tauri_plugin_updater::Updater> {
    let url = match proxy.map(url::Url::parse).transpose() {
        Ok(u) => u,
        Err(e) => return Err(format!("代理地址无效：{e}")),
    };
    let builder = app
        .updater_builder()
        .timeout(REQUEST_TIMEOUT)
        .configure_client(|c| c.connect_timeout(CONNECT_TIMEOUT))
        // macOS swaps the bundle and needs a relaunch, which we drive ourselves.
        .restart_after_install(false);
    let builder = match url {
        Some(u) => builder.proxy(u),
        None => builder.no_proxy(),
    };
    builder.build().map_err(|e| e.to_string())
}

/// Proxy first, direct second. `None` means we are already on the newest version.
async fn check_with_fallback(app: &AppHandle, proxy: Option<&str>) -> Res<Option<Update>> {
    if let Some(p) = sevqo_engine::net::pick_proxy(proxy) {
        let via_proxy = updater(app, Some(&p))?;
        match tokio::time::timeout(PROXY_CHECK_TIMEOUT, via_proxy.check()).await {
            Ok(result) => return result.map_err(|e| e.to_string()),
            Err(_) => tracing::warn!("经 {p} 检查更新超时，改走直连"),
        }
    }
    let direct = updater(app, None)?;
    tokio::time::timeout(DIRECT_CHECK_TIMEOUT, direct.check())
        .await
        .map_err(|_| "检查更新超时：代理与直连都不可用".to_string())?
        .map_err(|e| e.to_string())
}

/// Fetch the manifest and park the result, so the user decides when to swap the bundle.
pub async fn check(app: AppHandle) -> Res<Option<UpdateInfo>> {
    let state = state_of(&app);
    let proxy = state.settings().net_proxy;
    let _ = app.emit(EV_UPDATE, UpdateEvent::new("checking"));
    match check_with_fallback(&app, proxy.as_deref()).await {
        Ok(Some(update)) => {
            let info = info_of(&update);
            state.set_pending_update(Some(update));
            let _ = app.emit(EV_UPDATE, UpdateEvent::available(info.clone()));
            Ok(Some(info))
        }
        Ok(None) => {
            state.set_pending_update(None);
            let _ = app.emit(EV_UPDATE, UpdateEvent::new("upToDate"));
            Ok(None)
        }
        Err(e) => {
            let _ = app.emit(EV_UPDATE, UpdateEvent::failed(e.clone()));
            Err(e)
        }
    }
}

/// Download the signed archive, verify it against the configured pubkey, install, relaunch.
pub async fn install(app: AppHandle) -> Res<()> {
    let state = state_of(&app);
    let Some(update) = state.pending_update() else {
        return Err("还没有可安装的更新，请先检查更新".to_string());
    };
    if !state.try_begin_update() {
        return Err("更新正在下载中".to_string());
    }
    let generation = state.update_generation();
    let downloaded = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let _ = app.emit(
        EV_UPDATE,
        UpdateEvent {
            info: Some(info_of(&update)),
            ..UpdateEvent::new("downloading")
        },
    );

    // `download` verifies the minisign signature against the configured pubkey before it
    // returns bytes; `install` is what swaps the bundle, so the cancel check belongs between them.
    let result = update
        .download(
            {
                let app = app.clone();
                let downloaded = downloaded.clone();
                let total = total.clone();
                // The plugin hands us each chunk length, so the running total is ours to keep.
                move |chunk, size| {
                    if let Some(size) = size {
                        total.store(size, Ordering::Relaxed);
                    }
                    let done = downloaded.fetch_add(chunk as u64, Ordering::Relaxed) + chunk as u64;
                    let sum = total.load(Ordering::Relaxed);
                    let _ = app.emit(
                        EV_UPDATE,
                        UpdateEvent {
                            downloaded: Some(done),
                            total: (sum > 0).then_some(sum),
                            percent: (sum > 0).then_some(percent_of(done, sum)),
                            ..UpdateEvent::new("downloading")
                        },
                    );
                }
            },
            {
                let app = app.clone();
                move || {
                    let _ = app.emit(EV_UPDATE, UpdateEvent::new("installing"));
                }
            },
        )
        .await;

    let bytes = match result {
        Ok(bytes) => bytes,
        Err(e) => {
            state.end_update();
            let e = e.to_string();
            let _ = app.emit(EV_UPDATE, UpdateEvent::failed(e.clone()));
            return Err(e);
        }
    };

    state.end_update();
    // The stream itself has no abort handle, so a cancel means "fetch finished, bundle untouched".
    if state.update_generation() != generation {
        let _ = app.emit(EV_UPDATE, UpdateEvent::new("available"));
        return Err("已取消，不会安装".to_string());
    }
    if let Err(e) = update.install(bytes) {
        let e = e.to_string();
        let _ = app.emit(EV_UPDATE, UpdateEvent::failed(e.clone()));
        return Err(e);
    }
    state.set_pending_update(None);
    let _ = app.emit(EV_UPDATE, UpdateEvent::new("installed"));
    tracing::info!("更新已写入，重新启动");
    // Never returns: restart() tears down the process, so we don't resolve the command.
    app.restart()
}

fn percent_of(done: u64, total: u64) -> u8 {
    if total == 0 {
        0
    } else {
        ((done.min(total) as f64 / total as f64) * 100.0).round() as u8
    }
}

/// Launch-time background check: stays silent unless there is something to install.
pub async fn check_quietly(app: AppHandle) {
    let state = state_of(&app);
    let settings = state.settings();
    if !settings.update_check {
        return;
    }
    match check_with_fallback(&app, settings.net_proxy.as_deref()).await {
        Ok(Some(update)) => {
            let info = info_of(&update);
            state.set_pending_update(Some(update));
            let _ = app.emit(EV_UPDATE, UpdateEvent::available(info));
        }
        Ok(None) => tracing::debug!("已是最新版本"),
        Err(e) => tracing::warn!("后台检查更新失败：{e}"),
    }
}

#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Res<Option<UpdateInfo>> {
    check(app).await
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Res<()> {
    install(app).await
}

/// Bumps the generation so a running install stops before swapping the bundle.
#[tauri::command]
pub async fn cancel_update(app: AppHandle) -> Res<()> {
    state_of(&app).cancel_update();
    let _ = app.emit(EV_UPDATE, UpdateEvent::new("available"));
    Ok(())
}

#[tauri::command]
pub async fn pending_update(app: AppHandle) -> Res<Option<UpdateInfo>> {
    Ok(state_of(&app).pending_update().as_ref().map(info_of))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_clamped_and_never_exceeds_100() {
        assert_eq!(percent_of(0, 100), 0);
        assert_eq!(percent_of(50, 100), 50);
        assert_eq!(percent_of(100, 100), 100);
        assert_eq!(percent_of(150, 100), 100);
        assert_eq!(percent_of(10, 0), 0, "unknown length must not divide by zero");
    }

    /// The event shape is what the webview renders; a typo here silently blanks the UI.
    #[test]
    fn events_serialize_camel_case_for_the_ui() {
        let json = serde_json::to_value(UpdateEvent {
            percent: Some(42),
            downloaded: Some(1),
            total: Some(2),
            info: Some(UpdateInfo {
                version: "0.2.0".into(),
                current_version: "0.1.0".into(),
                notes: Some("n".into()),
                published_at: None,
            }),
            ..UpdateEvent::new("downloading")
        })
        .unwrap();
        assert_eq!(json["state"], "downloading");
        assert_eq!(json["percent"], 42);
        assert_eq!(json["downloaded"], 1);
        assert_eq!(json["info"]["currentVersion"], "0.1.0");
        assert!(json.get("current_version").is_none());
    }

    /// 发布包必须能被 `tauri.conf.json` 里的公钥验通，否则客户端永远装不上这次更新——
    /// 密钥换过、配置粘错、口令打错都会在这里暴露。跑法（先出产物）：
    /// `bash tools/release_macos.sh && cargo test -p sevqo-desktop -- --ignored`
    #[test]
    #[ignore = "需要 tools/release_macos.sh 产出的更新包"]
    fn release_packages_verify_with_the_configured_pubkey() {
        use base64::Engine as _;
        use std::path::{Path, PathBuf};

        fn archives(dir: &Path) -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("找不到更新产物目录 {}：{e}", dir.display()))
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "gz") && p.to_string_lossy().ends_with(".app.tar.gz"))
                .collect();
            v.sort();
            v
        }

        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let conf: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(manifest.join("tauri.conf.json")).unwrap(),
        )
        .unwrap();
        let encoded = conf["plugins"]["updater"]["pubkey"]
            .as_str()
            .expect("tauri.conf.json 里要有 plugins.updater.pubkey");
        let version = conf["version"].as_str().expect("tauri.conf.json 里要有 version");
        let text = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("公钥不是合法 base64"),
        )
        .expect("公钥不是 UTF-8");
        let public = minisign_verify::PublicKey::decode(&text).expect("公钥格式不对");

        let dir = std::env::var("SEVQO_UPDATE_DIR")
            .map(PathBuf::from)
            // src-tauri → desktop → apps → repo root, where `target/` lives
            .unwrap_or_else(|_| manifest.join("../../../target/release/bundle/updater"));
        let list = archives(&dir);
        assert!(!list.is_empty(), "{} 里没有 *.app.tar.gz", dir.display());
        for archive in list {
            let sig_path = PathBuf::from(format!("{}.sig", archive.display()));
            let bytes = std::fs::read(&archive).unwrap();
            // `tauri signer` writes the minisign file base64-packed on one line — the same
            // string that goes into latest.json — so the plugin's format and ours match.
            let packed = std::fs::read_to_string(&sig_path)
                .unwrap_or_else(|e| panic!("{} 读不出来：{e}", sig_path.display()));
            let text = String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(packed.trim())
                    .unwrap_or_else(|e| panic!("{} 不是合法 base64：{e}", sig_path.display())),
            )
            .expect("签名不是 UTF-8");
            let signature = minisign_verify::Signature::decode(&text)
                .unwrap_or_else(|e| panic!("{} 的签名解析失败：{e}", sig_path.display()));
            public
                .verify(&bytes, &signature, false)
                .unwrap_or_else(|e| panic!("{} 与配置里的公钥不匹配：{e}", archive.display()));
            // `requireSignedVersion` 依赖 trusted comment 里的版本；漏掉 --app-version
            // 的包在本地验签会过，装到客户端却被拒。
            let signed = signature
                .trusted_comment()
                .split('\t')
                .find_map(|f| f.strip_prefix("version:"));
            assert_eq!(
                signed,
                Some(version),
                "{} 的 trusted comment 版本与 tauri.conf.json 的 {version} 不一致",
                archive.display()
            );
        }
    }
}
