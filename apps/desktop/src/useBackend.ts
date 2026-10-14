// Shared backend state for the main window: everything the sidebar needs,
// kept fresh from events plus a slow poll (events can be missed while hidden).
import { useCallback, useEffect, useRef, useState } from 'react';

import {
  api,
  errorText,
  onIndexProgress,
  onModelState,
  onModelProgress,
  onUpdateState,
  onWatchChange,
} from './api';
import { labels } from './i18n';
import type {
  DownloadProgress,
  IndexMode,
  ModelInfo,
  ModelStatePayload,
  Overview,
  Progress,
  Root,
  Settings,
  UpdateEvent,
  UpdateInfo,
  WatchChange,
} from './types';
import { invalidateThumbs, useInterval } from './hooks';

const idleProgress: Progress = {
  phase: 'idle',
  discovered: 0,
  queued: 0,
  done: 0,
  failed: 0,
  skipped: 0,
  totalEstimate: 0,
  current: null,
  imagesPerMin: 0,
  elapsedS: 0,
  message: null,
};

export interface Flash {
  id: number;
  text: string;
  kind: 'info' | 'ok' | 'error';
}

export interface Backend {
  settings: Settings | null;
  overview: Overview | null;
  model: ModelInfo | null;
  roots: Root[];
  progress: Progress;
  download: DownloadProgress | null;
  modelState: ModelStatePayload | null;
  update: UpdateEvent | null;
  version: string | null;
  changes: WatchChange[];
  bootError: string | null;
  busy: boolean;
  updateBusy: boolean;
  flash: Flash | null;
  dismissFlash: () => void;
  refreshAll: () => Promise<void>;
  saveSettings: (next: Settings) => Promise<boolean>;
  addRoot: (path: string, label?: string) => Promise<void>;
  removeRoot: (root: Root) => Promise<void>;
  toggleRoot: (root: Root, enabled: boolean) => Promise<void>;
  rescan: (rootId?: number | null) => Promise<void>;
  startIndex: (opts: { mode: IndexMode; maxFiles?: number | null }) => Promise<void>;
  pause: () => Promise<void>;
  resume: () => Promise<void>;
  cancel: () => Promise<void>;
  rebuild: (clearVectors: boolean) => Promise<void>;
  backfill: () => Promise<void>;
  compact: () => Promise<void>;
  restartWatch: () => Promise<void>;
  downloadModel: () => Promise<void>;
  cancelDownload: () => Promise<void>;
  verifyModel: () => Promise<void>;
  checkUpdate: () => Promise<void>;
  installUpdate: () => Promise<void>;
  cancelUpdate: () => Promise<void>;
}

export function useBackend(): Backend {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [overview, setOverview] = useState<Overview | null>(null);
  const [model, setModel] = useState<ModelInfo | null>(null);
  const [roots, setRoots] = useState<Root[]>([]);
  const [progress, setProgress] = useState<Progress>(idleProgress);
  const [download, setDownload] = useState<DownloadProgress | null>(null);
  const [modelState, setModelState] = useState<ModelStatePayload | null>(null);
  const [update, setUpdate] = useState<UpdateEvent | null>(null);
  const [version, setVersion] = useState<string | null>(null);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [changes, setChanges] = useState<WatchChange[]>([]);
  const [bootError, setBootError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [flash, setFlash] = useState<Flash | null>(null);

  const flashSeq = useRef(0);
  const alive = useRef(true);
  useEffect(
    () => () => {
      alive.current = false;
    },
    [],
  );

  const say = useCallback((text: string, kind: Flash['kind'] = 'info') => {
    flashSeq.current += 1;
    setFlash({ id: flashSeq.current, text, kind });
  }, []);

  const dismissFlash = useCallback(() => setFlash(null), []);

  const refreshAll = useCallback(async () => {
    const [stats, info, rs, st] = await Promise.allSettled([
      api.stats(),
      api.modelStatus(),
      api.listRoots(),
      api.getSettings(),
    ]);
    if (!alive.current) return;
    const failures: string[] = [];
    if (stats.status === 'fulfilled') setOverview(stats.value);
    else failures.push(errorText(stats.reason));
    if (info.status === 'fulfilled') setModel(info.value);
    else failures.push(errorText(info.reason));
    if (rs.status === 'fulfilled') setRoots(rs.value);
    else failures.push(errorText(rs.reason));
    if (st.status === 'fulfilled') setSettings(st.value);
    else failures.push(errorText(st.reason));
    setBootError(failures.length > 0 ? failures[0] : null);
  }, []);

  // ------------------------------------------------------------------ events
  const buffered = useRef<Progress | null>(null);
  const flushTimer = useRef<number | null>(null);
  const onProgressEvent = useCallback((p: Progress) => {
    buffered.current = p;
    if (flushTimer.current != null) return;
    flushTimer.current = window.setTimeout(() => {
      flushTimer.current = null;
      if (buffered.current && alive.current) setProgress(buffered.current);
    }, 180);
  }, []);

  useEffect(() => {
    const unsubs: Array<Promise<() => void>> = [
      onModelProgress((p) => {
        if (alive.current) setDownload(p);
      }),
      onModelState((s) => {
        if (!alive.current) return;
        setModelState(s);
        setDownload(null);
        if (s.note) say(s.note, s.ready ? 'ok' : 'info');
        void refreshAll();
      }),
      onWatchChange((c) => {
        if (!alive.current) return;
        setChanges((prev) => [c, ...prev].slice(0, 8));
      }),
      onUpdateState((e) => {
        if (alive.current) setUpdate(e);
      }),
    ];
    const off = onIndexProgress(onProgressEvent);
    return () => {
      void off.then((f) => f());
      for (const p of unsubs) void p.then((f) => f());
    };
  }, [onProgressEvent, refreshAll, say]);

  useEffect(() => {
    void refreshAll();
    return () => {
      if (flushTimer.current != null) window.clearTimeout(flushTimer.current);
    };
  }, [refreshAll]);

  // 版本号和"启动后台检查已经跑完"的结果各问一次：放进轮询里会把下载进度条冲回零。
  useEffect(() => {
    void (async () => {
      const [v, pending] = await Promise.allSettled([api.appVersion(), api.pendingUpdate()]);
      if (!alive.current) return;
      if (v.status === 'fulfilled') setVersion(v.value);
      if (pending.status === 'fulfilled' && pending.value) {
        const info: UpdateInfo | null = pending.value;
        setUpdate((prev) => prev ?? { state: 'available', info, percent: null, downloaded: null, total: null, error: null });
      }
    })();
  }, []);

  const active = progress.phase === 'indexing' || progress.phase === 'scanning' || !!model?.downloading;
  useInterval(
    () => {
      void refreshAll();
    },
    active ? 4000 : 15000,
  );

  // ------------------------------------------------------------------ actions
  const run = useCallback(
    async (fn: () => Promise<unknown>, ok?: (v: unknown) => string | null): Promise<boolean> => {
      setBusy(true);
      try {
        const v = await fn();
        const msg = ok ? ok(v) : null;
        if (msg) say(msg, 'ok');
        await refreshAll();
        return true;
      } catch (e) {
        say(errorText(e), 'error');
        return false;
      } finally {
        if (alive.current) setBusy(false);
      }
    },
    [refreshAll, say],
  );

  const saveSettings = useCallback(
    async (next: Settings): Promise<boolean> => {
      const ok = await run(
        () =>
          api.setSettings(next).then((s) => {
            setSettings(s);
            return s;
          }),
        () => labels.settings.saved,
      );
      return ok;
    },
    [run],
  );

  const addRoot = useCallback(
    async (path: string, label?: string) => {
      await run(() => api.addRoot(path, label), (r) => {
        const root = r as Root;
        return labels.flash.added(root.label || root.path);
      });
    },
    [run],
  );

  const removeRoot = useCallback(
    async (root: Root) => {
      if (!window.confirm(labels.roots.confirmRemove(root.label || root.path))) return;
      await run(() => api.removeRoot(root.id), (n) => labels.flash.removed(n as number));
    },
    [run],
  );

  const toggleRoot = useCallback(
    async (root: Root, enabled: boolean) => {
      await run(() => api.setRootEnabled(root.id, enabled));
    },
    [run],
  );

  const rescan = useCallback(
    async (rootId?: number | null) => {
      await run(() => api.rescan(rootId), (r) => {
        const rep = r as { queued: number };
        return labels.flash.scan(rep.queued);
      });
    },
    [run],
  );

  const startIndex = useCallback(
    async (opts: { mode: IndexMode; maxFiles?: number | null }) => {
      const ok = await run(() => api.indexStart(opts), () => labels.flash.indexStart);
      if (ok) setProgress((p) => ({ ...p, phase: 'indexing', message: null }));
    },
    [run],
  );

  const pause = useCallback(async () => {
    await run(() => api.indexPause(), () => labels.flash.paused);
  }, [run]);

  const resume = useCallback(async () => {
    await run(() => api.indexResume(), () => labels.flash.resumed);
  }, [run]);

  const cancel = useCallback(async () => {
    await run(() => api.indexCancel(), () => labels.flash.cancelled);
  }, [run]);

  const rebuild = useCallback(
    async (clearVectors: boolean) => {
      if (clearVectors && !window.confirm(labels.index.confirmClearVectors)) return;
      // Thumbnails get regenerated by the rebuild: drop the cached data URLs.
      invalidateThumbs();
      await run(() => api.rebuildIndex(clearVectors), (n) => labels.flash.rebuilt(n as number));
    },
    [run],
  );

  const backfill = useCallback(async () => {
    await run(() => api.backfillVectors(), (n) => labels.flash.backfill(n as number));
  }, [run]);

  const compact = useCallback(async () => {
    await run(() => api.compactVectors(), (n) => labels.flash.compact(n as number));
  }, [run]);

  const restartWatch = useCallback(async () => {
    await run(() => api.watchRestart(), (n) => labels.flash.watchRestart(n as number));
  }, [run]);

  const downloadModel = useCallback(async () => {
    await run(
      () => {
        setDownload(null);
        return api.modelDownloadStart();
      },
      () => labels.model.downloading,
    );
  }, [run]);

  const cancelDownload = useCallback(async () => {
    await run(() => api.modelDownloadCancel(), () => labels.model.paused);
  }, [run]);

  const verifyModel = useCallback(async () => {
    await run(() => api.modelVerify(), (v) => `${labels.flash.verifyingDone}：${String(v ?? '')}`);
  }, [run]);

  // 更新有自己的 busy：一个安装包要下几分钟，不该把整条侧栏冻住。
  const checkUpdate = useCallback(async () => {
    setUpdateBusy(true);
    try {
      const info = await api.checkForUpdate();
      say(info ? labels.flash.updateAvailable(info.version) : labels.flash.updateUpToDate, 'ok');
    } catch (e) {
      say(errorText(e), 'error');
    } finally {
      if (alive.current) setUpdateBusy(false);
    }
  }, [say]);

  const installUpdate = useCallback(async () => {
    if (!window.confirm(labels.update.restartHint)) return;
    // 安装成功会直接重启，这个 promise 可能永远不会落地，所以不占 busy。
    await api.installUpdate().catch((e) => say(errorText(e), 'error'));
  }, [say]);

  const cancelUpdate = useCallback(async () => {
    try {
      await api.cancelUpdate();
      say(labels.flash.updateCancelled, 'info');
    } catch (e) {
      say(errorText(e), 'error');
    }
  }, [say]);

  return {
    settings,
    overview,
    model,
    roots,
    progress,
    download,
    modelState,
    update,
    version,
    changes,
    bootError,
    busy,
    updateBusy,
    flash,
    dismissFlash,
    refreshAll,
    saveSettings,
    addRoot,
    removeRoot,
    toggleRoot,
    rescan,
    startIndex,
    pause,
    resume,
    cancel,
    rebuild,
    backfill,
    compact,
    restartWatch,
    downloadModel,
    cancelDownload,
    verifyModel,
    checkUpdate,
    installUpdate,
    cancelUpdate,
  };
}
