// 模型下载入口要出现在多个窗口里，而 Spotlight 窗口没有主窗口的 useBackend，
// 所以这块状态自己订阅事件、自己问 model_status，谁用它谁就自带完整能力。
import { useCallback, useEffect, useRef, useState } from 'react';

import { api, errorText, onModelProgress, onModelState } from './api';
import { useOnVisible } from './hooks';
import type { DownloadProgress, ModelInfo } from './types';

export interface ModelGate {
  model: ModelInfo | null;
  download: DownloadProgress | null;
  /** 后端给的一句话（就绪、缺 Python、失败原因）。 */
  note: string | null;
  error: string | null;
  busy: boolean;
  /** 权重还缺东西，值得放一个下载入口。 */
  incomplete: boolean;
  downloading: boolean;
  start: () => Promise<void>;
  cancel: () => Promise<void>;
  refresh: () => Promise<void>;
}

export function useModel(): ModelGate {
  const [model, setModel] = useState<ModelInfo | null>(null);
  const [download, setDownload] = useState<DownloadProgress | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const alive = useRef(true);
  useEffect(
    () => () => {
      alive.current = false;
    },
    [],
  );

  const refresh = useCallback(async () => {
    try {
      const info = await api.modelStatus();
      if (!alive.current) return;
      setModel(info);
      // 收尾那一帧可能整条漏掉（窗口藏着）：以 `downloading` 为准，别让进度条停在半路。
      if (!info.downloading) setDownload(null);
    } catch (e) {
      if (alive.current) setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const unsubs = [
      onModelProgress((p) => {
        if (alive.current) setDownload(p);
      }),
      onModelState((s) => {
        if (!alive.current) return;
        setDownload(null);
        setNote(s.note || null);
        void refresh();
      }),
    ];
    return () => {
      for (const p of unsubs) void p.then((f) => f());
    };
  }, [refresh]);

  // 下载中问得勤一点：进度事件可能因为窗口隐藏漏掉，数字还得对上磁盘上的实际字节。
  const active = !!model?.downloading || !!download;
  useIntervalWhen(active, refresh);
  // 平时不轮询：别的窗口把权重下完了，回到前台就得问一次，否则这条提示一直挂着。
  useOnVisible(() => void refresh());

  const start = useCallback(async () => {
    setBusy(true);
    setError(null);
    setNote(null);
    setDownload(null);
    try {
      await api.modelDownloadStart();
      // 后端已接手：立刻把 downloading 问回来，否则首帧事件漏掉就没人再刷新。
      await refresh();
    } catch (e) {
      if (alive.current) setError(errorText(e));
    } finally {
      if (alive.current) setBusy(false);
    }
  }, [refresh]);

  const cancel = useCallback(async () => {
    setBusy(true);
    try {
      await api.modelDownloadCancel();
      await refresh();
    } catch (e) {
      if (alive.current) setError(errorText(e));
    } finally {
      if (alive.current) setBusy(false);
    }
  }, [refresh]);

  return {
    model,
    download,
    note,
    error,
    busy,
    incomplete: !!model && !model.status.complete,
    downloading: !!model?.downloading || !!download,
    start,
    cancel,
    refresh,
  };
}

function useIntervalWhen(on: boolean, fn: () => Promise<void>) {
  useEffect(() => {
    if (!on) return;
    const id = window.setInterval(() => void fn(), 3000);
    return () => window.clearInterval(id);
  }, [on, fn]);
}
