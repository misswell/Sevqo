mod commands;
mod media;
mod state;
mod update;

use std::sync::Arc;

use tauri::Manager;
use tauri_plugin_global_shortcut::{Builder as ShortcutBuilder, GlobalShortcutExt};

use crate::state::AppState;

/// `Cmd+Space` belongs to the system Spotlight, so the default is Alt+Space.
const SHORTCUT_FALLBACK: &str = "Alt+Space";

pub fn run() {
    let model_home = std::env::var("SEVQO_MODEL_HOME")
        .ok()
        .filter(|s| !s.is_empty());
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
    // 方案十：文本语义索引同样需要模型权重，不能只看图片/视频开关。
    let wants_media = settings.index_images
        || settings.index_video
        || settings.index_documents
        || settings.index_code;
    let resume = settings.resume_on_launch;
    let update_check = settings.update_check;
    // P0-01：启动时对向量库做一次一致性检查与孤儿 generation 清理。失败只记录并继续：
    // 之后每次搜索都会以同样的错误显式失败并给出原因，不会静默用错向量。
    if let Err(e) = sevqo_engine::vectors::ensure_generation_store(&paths.vector_path, &db) {
        tracing::error!("向量库启动检查失败: {e}");
    }
    // 模型版本与向量库绑定（方案十）：revision 变更即重置语义索引、保留关键词索引。
    if let Err(e) = commands::ensure_model_revision_compat_db(
        &db,
        &paths.vector_path,
        sevqo_engine::model::MODEL_REVISION,
    ) {
        tracing::warn!("模型版本兼容检查失败: {e}");
    }
    // 一致性检查器（方案 2.4）：只自愈可确定的数据关系，其余上报。
    match sevqo_engine::integrity::check_and_repair(&db) {
        Ok(r) if !r.is_clean() => {
            tracing::warn!(
                "索引一致性：自愈 persisted失联 {}、孤儿映射 {}、done缺单元 {}；待人工: {:?}",
                r.fixed_stale_units,
                r.fixed_orphan_rows,
                r.fixed_incomplete_done,
                r.problems
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("索引一致性检查失败: {e}"),
    }

    let state = Arc::new(AppState::new(paths, settings, db));
    // 模型版本与向量库绑定（P1）：revision 变更时重置语义索引、保留关键词索引，
    // 之后的 Full 对账会用新模型重建向量。维度相同也不能混用两个模型的向量空间。
    if let Err(e) =
        commands::ensure_model_revision_compat(&state, sevqo_engine::model::MODEL_REVISION)
    {
        tracing::warn!("模型版本兼容检查失败: {e}");
    }
    let managed = state.clone();

    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("sevqo-media", media::handle)
        .plugin(
            ShortcutBuilder::new()
                .with_handler(|app, _shortcut, event| {
                    if matches!(
                        event.state,
                        tauri_plugin_global_shortcut::ShortcutState::Pressed
                    ) {
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
            commands::index_progress,
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
                if let Err(e) = commands::spawn_model_download(state.clone(), handle.clone(), false)
                {
                    tracing::warn!("模型自动下载未启动：{e}");
                }
            }

            // 启动对账（第六章）：离线期间的新增/修改/删除只有重新扫描才能发现。
            // resume_on_launch 开启时直接跑一轮完整对账（扫描+索引，模式随当前能力）；
            // 关闭时维持旧行为：只提示队列里还有多少，等用户手动开始。
            if resume {
                let counts = state.db.counts().unwrap_or_default();
                let queued = counts.get("pending").copied().unwrap_or(0);
                if queued > 0 {
                    // 走 emit_progress 而不是直接 emit：这一帧多半赶在界面挂上监听器之前，
                    // 只有存进快照，挂载后的首次询问才能把它问回来。
                    commands::emit_progress(
                        &state,
                        &handle,
                        sevqo_engine::Progress {
                            phase: sevqo_engine::Phase::Idle,
                            discovered: counts.values().sum::<i64>().max(0) as u64,
                            queued: queued as u64,
                            message: Some(format!("有 {queued} 个文件等待索引")),
                            ..Default::default()
                        },
                    );
                }
                // 对账扫描顺带处理离线删除（sweep 有完整成功护栏，不会误删）。
                // 推理能力此刻多半还没就绪（模型后台校验中），先按 auto_mode 跑：
                // TextOnly 也能把离线变化收进库；模型就绪后下载线程会再触发一轮补向量。
                if let Ok(n) = state.db.requeue_skipped_existing() {
                    if n > 0 {
                        tracing::info!("启动对账：重排 {n} 个此前被跳过的任务");
                    }
                }
                match commands::spawn_index_thread(
                    &handle,
                    &state,
                    commands::auto_mode(&state),
                    None,
                    true,
                ) {
                    Ok(()) => tracing::info!("启动对账已开始"),
                    // 监听增量正在跑时拿不到许可：队列里的变更不会被丢，下一轮补上。
                    Err(e) => tracing::info!("启动对账未立即开始：{e}"),
                }
            }

            // 外置盘恢复（6.4）：低频健康检查 + 指数退避。根目录从「不存在」变为
            // 「已挂载」时重注册 watcher 并触发一轮扫描；已可用目录不做任何事。
            {
                let disk_state = state.clone();
                let disk_handle = handle.clone();
                std::thread::Builder::new()
                    .name("sevqo-root-health".into())
                    .spawn(move || {
                        // 启动即初始化离线集合（P1 修复：此前从空集合开始，30 秒内
                        // 插回的盘不会触发恢复）。
                        let mut offline: std::collections::HashSet<String> = disk_state
                            .db
                            .list_roots()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|r| r.enabled && !commands::expand(&r.path).exists())
                            .map(|r| r.path)
                            .collect();
                        // 指数退避：30s 起步，最慢 5 分钟；有恢复动作后回到 30s。
                        let mut backoff_secs: u64 = 30;
                        // 对账请求持久化在线程内：许可被占用时不丢，下一轮补跑。
                        let mut pending_rescan = false;
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                            let roots = match disk_state.db.list_roots() {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            // 实际监听中的根目录（watcher 注册成功才会出现在这里）。
                            let watched: std::collections::HashSet<String> = disk_state
                                .watcher
                                .lock()
                                .ok()
                                .map_or_else(Default::default, |g| {
                                    g.as_ref()
                                        .map(|h| {
                                            h.roots
                                                .iter()
                                                .map(|p| p.to_string_lossy().into_owned())
                                                .collect()
                                        })
                                        .unwrap_or_default()
                                });
                            let mut need_restart = false;
                            let mut recovered = Vec::new();
                            for r in roots.iter().filter(|r| r.enabled) {
                                let path = commands::expand(&r.path);
                                let path_str = path.to_string_lossy().into_owned();
                                if !path.exists() {
                                    if offline.insert(r.path.clone()) {
                                        tracing::warn!(
                                            "根目录当前不可用（挂载后自动恢复）：{}",
                                            r.path
                                        );
                                    }
                                    continue;
                                }
                                // 目录在但此前记录为离线：刚重新挂载。
                                if offline.remove(&r.path) {
                                    need_restart = true;
                                    recovered.push(path.clone());
                                }
                                // 目录在但不在监听集合里：启动竞态或注册失败，都需重试。
                                if !watched.contains(&path_str) {
                                    need_restart = true;
                                }
                            }
                            if need_restart {
                                if let Err(e) = commands::restart_watcher(&disk_state, &disk_handle)
                                {
                                    tracing::warn!("重注册监听失败：{e}");
                                }
                                for path in &recovered {
                                    tracing::info!(
                                        "根目录重新可用，开始增量对账：{}",
                                        path.display()
                                    );
                                }
                                // 方案 5.4：磁盘离线期间被跳过的任务，文件已重新出现的重新排队。
                                match disk_state.db.requeue_skipped_existing() {
                                    Ok(n) if n > 0 => tracing::info!("重排 {n} 个此前被跳过的任务"),
                                    Ok(_) => {}
                                    Err(e) => tracing::warn!("重排 skipped 任务失败: {e}"),
                                }
                                pending_rescan = true;
                            }
                            if pending_rescan {
                                match commands::spawn_index_thread(
                                    &disk_handle,
                                    &disk_state,
                                    commands::auto_mode(&disk_state),
                                    None,
                                    true,
                                ) {
                                    Ok(()) => {
                                        pending_rescan = false;
                                        backoff_secs = 30;
                                    }
                                    // 许可被占用：保留待办，下一轮重试。
                                    Err(e) => tracing::info!("对账暂未开始（{e}），稍后重试"),
                                }
                            } else {
                                backoff_secs = (backoff_secs * 2).min(300);
                            }
                        }
                    })
                    .map_err(|e| tracing::warn!("健康检查线程启动失败: {e}"))
                    .ok();
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
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(w)
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}
