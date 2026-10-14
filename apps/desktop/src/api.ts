// The only place that talks to Rust. Every command from the IPC contract is
// wrapped here; every failure is normalised to a human-readable Chinese string.
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import type {
  DownloadProgress,
  FileRecord,
  Filters,
  Hit,
  IndexMode,
  Kind,
  ModelInfo,
  ModelStatePayload,
  Moment,
  Overview,
  Progress,
  Root,
  Runtime,
  ScanReport,
  SearchRequest,
  SearchResponse,
  Settings,
  UpdateEvent,
  UpdateInfo,
  WatchChange,
} from './types';

export const events = {
  indexProgress: 'index://progress',
  modelProgress: 'model://progress',
  modelState: 'model://state',
  updateState: 'update://state',
  watchChange: 'watch://change',
} as const;

/** Tauri rejects with the raw Rust error string; anything else gets flattened. */
export function errorText(e: unknown): string {
  if (typeof e === 'string') return e.trim() || '未知错误';
  if (e instanceof Error) return e.message || '未知错误';
  if (e && typeof e === 'object') {
    const rec = e as Record<string, unknown>;
    for (const key of ['message', 'payload', 'error']) {
      const v = rec[key];
      if (typeof v === 'string' && v.trim()) return v.trim();
    }
  }
  const s = String(e);
  return s.trim() ? s : '未知错误';
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw new Error(errorText(e), { cause: e });
  }
}

export interface StartIndexOptions {
  mode?: IndexMode;
  maxFiles?: number | null;
  scan?: boolean;
}

export const api = {
  // settings & stats
  getSettings: () => call<Settings>('get_settings'),
  setSettings: (next: Settings) => call<Settings>('set_settings', { next }),
  stats: () => call<Overview>('stats'),
  appVersion: () => call<string>('app_version'),

  // online updates
  checkForUpdate: () => call<UpdateInfo | null>('check_for_update'),
  installUpdate: () => call<null>('install_update'),
  cancelUpdate: () => call<null>('cancel_update'),
  pendingUpdate: () => call<UpdateInfo | null>('pending_update'),

  // model
  modelStatus: () => call<ModelInfo>('model_status'),
  modelDownloadStart: () => call<null>('model_download_start'),
  modelDownloadCancel: () => call<null>('model_download_cancel'),
  modelRuntime: () => call<Runtime>('model_runtime'),
  modelVerify: () => call<string>('model_verify'),

  // roots
  listRoots: () => call<Root[]>('list_roots'),
  addRoot: (path: string, label?: string) =>
    call<Root>('add_root', { path, label: label && label.trim() ? label.trim() : null }),
  removeRoot: (id: number) => call<number>('remove_root', { id }),
  setRootEnabled: (id: number, enabled: boolean) =>
    call<null>('set_root_enabled', { id, enabled }),

  // indexing
  rescan: (rootId?: number | null) => call<ScanReport>('rescan', { rootId: rootId ?? null }),
  indexStart: (opts: StartIndexOptions = {}) =>
    call<null>('index_start', {
      mode: opts.mode ?? null,
      maxFiles: opts.maxFiles && opts.maxFiles > 0 ? opts.maxFiles : null,
      scan: opts.scan ?? null,
    }),
  indexProgress: () => call<Progress>('index_progress'),
  indexPause: () => call<null>('index_pause'),
  indexResume: () => call<null>('index_resume'),
  indexCancel: () => call<null>('index_cancel'),
  rebuildIndex: (clearVectors: boolean) => call<number>('rebuild_index', { clearVectors }),
  backfillVectors: () => call<number>('backfill_vectors'),
  compactVectors: () => call<number>('compact_vectors'),
  watchRestart: () => call<number>('watch_restart'),

  // search
  search: (req: SearchRequest) => call<SearchResponse>('search', { req }),
  searchKeyword: (req: SearchRequest) => call<SearchResponse>('search_keyword', { req }),
  searchMedia: (path: string, limit?: number, kinds?: Kind[], filters?: Filters) =>
    call<SearchResponse>('search_media', {
      path,
      limit: limit && limit > 0 ? limit : null,
      kinds: kinds && kinds.length > 0 ? kinds : null,
      filters: filters ?? null,
    }),
  recent: (limit?: number) => call<Hit[]>('recent', { limit: limit && limit > 0 ? limit : null }),
  moments: (fileId: number) => call<Moment[]>('moments', { fileId }),

  // media & detail
  thumb: (fileId: number) => call<string | null>('thumb', { fileId }),
  clipThumb: (fileId: number, clipId: number) =>
    call<string | null>('clip_thumb', { fileId, clipId }),
  fileInfo: (fileId: number) => call<FileRecord | null>('file_info', { fileId }),
  openFile: (path: string) => call<null>('open_file', { path }),
  revealInFolder: (path: string) => call<null>('reveal_in_folder', { path }),

  // spotlight window control
  spotlightToggle: () => call<null>('spotlight_toggle'),
  spotlightHide: () => call<null>('spotlight_hide'),
  showMain: () => call<null>('show_main'),
};

/**
 * Stream an indexed video from the Rust side and start it at `atMs`. macOS and Linux
 * resolve a custom scheme as `scheme://localhost/…`; WebView2 needs the http form.
 */
export function mediaUrl(fileId: number, atMs = 0): string {
  const origin = navigator.userAgent.includes('Windows') ? 'http://sevqo-media.localhost' : 'sevqo-media://localhost';
  const t = atMs > 0 ? `#t=${(atMs / 1000).toFixed(3)}` : '';
  return `${origin}/video/${fileId}${t}`;
}

export function subscribe<T>(name: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(name, (e) => handler(e.payload)).catch(() => () => {
    // Listener registration must never break the UI; the poll fallback covers it.
  });
}

export const onIndexProgress = (h: (p: Progress) => void) => subscribe<Progress>(events.indexProgress, h);
export const onModelProgress = (h: (p: DownloadProgress) => void) =>
  subscribe<DownloadProgress>(events.modelProgress, h);
export const onModelState = (h: (p: ModelStatePayload) => void) => subscribe<ModelStatePayload>(events.modelState, h);
export const onUpdateState = (h: (p: UpdateEvent) => void) => subscribe<UpdateEvent>(events.updateState, h);
export const onWatchChange = (h: (p: WatchChange) => void) => subscribe<WatchChange>(events.watchChange, h);
