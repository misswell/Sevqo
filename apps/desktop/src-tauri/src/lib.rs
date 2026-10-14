mod commands;
mod media;
mod state;
mod update;

use std::sync::Arc;

use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{Builder as ShortcutBuilder, GlobalShortcutExt};

use crate::state::AppState;

/// `Cmd+Space` belongs to the system Spotlight, so the default is Alt+Space.
const SHORTCUT_FALLBACK: &str = "Alt+Space";

pub fn run() {
    let model_home = std::env::var("SEVQO_MODEL_HOME").ok().filter(|s| !s.is_empty());
    let (paths, settings, db) = match commands::bootstrap_paths(model_home.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Sevqo 无法准备数据目录：{e}");
            std::process::exit(2);
        }
    };
    init_tracing(&paths.log_dir);
    tracing::info!("数据目录 {}", paths.data_dir.display());

    let shortcut = settings.global_shortcut.clone();
    let wants_media = settings.index_images || settings.index_video;
    let resume = settings.resume_on_launch;
    let update_check = settings.update_check;
    let state = Arc::new(AppState::new(paths, settings, db));
    let managed = state.clone();

    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("sevqo-media", media::handle)
        .plugin(
            ShortcutBuilder::new()
                .with_handler(|app, _shortcut, event| {
                    if matches!(event.state, tauri_plugin_global_shortcut::ShortcutState::Pressed) {
                        toggle_spotlight(app);
                    }
                })
                .build(),
        )
        // Commands are ours (`update::*`); the plugin only supplies the manifest fetch,
        // signature check and bundle swap, so the webview never reaches it directly.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(managed)
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::set_settings,
            commands::stats,
            commands::app_version,
            commands::model_status,
            commands::list_roots,
            commands::add_root,
            commands::remove_root,
            commands::set_root_enabled,
            commands::rescan,
            commands::index_start,
            commands::index_pause,
            commands::index_resume,
            commands::index_cancel,
            commands::rebuild_index,
            commands::backfill_vectors,
            commands::compact_vectors,
            commands::search,
            commands::search_keyword,
            commands::search_media,
            commands::recent,
            commands::moments,
            commands::thumb,
            commands::clip_thumb,
            commands::file_info,
            commands::open_file,
            commands::reveal_in_folder,
            commands::model_download_start,
            commands::model_download_cancel,
            commands::model_runtime,
            commands::model_verify,
            commands::watch_restart,
            commands::spotlight_toggle,
            commands::spotlight_hide,
            commands::show_main,
            update::check_for_update,
            update::install_update,
            update::cancel_update,
            update::pending_update,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let state = app.state::<Arc<AppState>>().inner().clone();

            if let Err(e) = handle.global_shortcut().register(shortcut.as_str()) {
                tracing::warn!("快捷键 {shortcut} 注册失败：{e}");
                if shortcut != SHORTCUT_FALLBACK {
                    let _ = handle.global_shortcut().register(SHORTCUT_FALLBACK);
                }
            }

            match commands::restart_watcher(&state, &handle) {
                Ok(n) if n > 0 => tracing::info!("已监听 {n} 个目录"),
                Ok(_) => tracing::info!("还没有添加目录，监听暂不启动"),
                Err(e) => tracing::warn!("监听启动失败：{e}"),
            }

            // 首启按需下载：weights arrive in the background and stream their progress.
            // 判据是「权重齐没齐」而不是「能不能推理」——没配 Python 时也该先把这 1.49 GB 取回来。
            if wants_media && !state.model_ready() {
                if let Err(e) = commands::spawn_model_download(state.clone(), handle.clone(), false) {
                    tracing::warn!("模型自动下载未启动：{e}");
                }
            }

            // Files queued from a previous run: tell the UI it can resume.
            if resume {
                let queued = state.db.counts().map(|c| c.get("pending").copied().unwrap_or(0)).unwrap_or(0);
                if queued > 0 {
                    let _ = handle.emit(
                        commands::EV_INDEX,
                        sevqo_engine::Progress {
                            phase: sevqo_engine::Phase::Idle,
                            queued: queued as u64,
                            message: Some(format!("有 {queued} 个文件等待索引")),
                            ..Default::default()
                        },
                    );
                }
            }

            // 在线更新：晚一点再问，别和首启的模型下载抢带宽；没有新版本就一声不吭。
            if update_check {
                let app_for_update = handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                    update::check_quietly(app_for_update).await;
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Sevqo 启动失败")
        .run(move |_app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                // torch holds gigabytes; kill the child rather than orphan it.
                state.client().shutdown();
            }
        });
}

fn toggle_spotlight(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("spotlight") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.center();
            let _ = w.show();
            let _ = w.set_always_on_top(true);
            let _ = w.set_focus();
        }
    } else {
        commands::toggle_window(app, "main");
    }
}

/// Logs go next to the index so a support request needs no copy-paste from a terminal.
fn init_tracing(log_dir: &std::path::Path) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sevqo_engine=debug"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("sevqo.log"))
        .ok()
        .map(std::sync::Mutex::new);
    match file {
        Some(w) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(w).try_init();
        }
        None => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}
