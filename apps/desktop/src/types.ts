// Mirrors the Rust payloads in `src-tauri/src/commands.rs` and
// `crates/sevqo-engine/src/types.rs`. Everything is camelCase on the wire.

export type Kind = 'image' | 'video' | 'document' | 'code' | 'audio' | 'other';

export type Channel = 'keyword' | 'semantic' | 'both';

export interface Root {
  id: number;
  path: string;
  label: string;
  enabled: boolean;
  lastScanNs: number;
}

export interface Moment {
  clipId: number;
  startMs: number;
  endMs: number;
  score: number;
  /** Filesystem path on the Rust side — not usable as an <img> src; use `clip_thumb`. */
  thumb: string | null;
}

export interface Hit {
  fileId: number;
  path: string;
  name: string;
  kind: Kind;
  size: number;
  mtimeNs: number;
  score: number;
  channel: Channel;
  keywordRank: number | null;
  semanticRank: number | null;
  snippet: string | null;
  moments: Moment[];
  thumb: string | null;
  durationMs: number | null;
}

export interface Filters {
  kinds: Kind[];
  rootId: number | null;
  minSize: number | null;
  maxSize: number | null;
  afterNs: number | null;
  beforeNs: number | null;
}

export interface SearchRequest {
  query: string;
  limit: number;
  filters: Filters;
  semantic: boolean;
}

export interface SearchResponse {
  hits: Hit[];
  keywordMs: number;
  semanticMs: number;
  total: number;
  semanticActive: boolean;
  note: string | null;
}

export type FileState = 'pending' | 'indexing' | 'done' | 'failed' | 'skipped' | 'stale';

export interface FileRecord {
  id: number;
  rootId: number | null;
  path: string;
  name: string;
  ext: string;
  kind: Kind;
  size: number;
  mtimeNs: number;
  durationMs: number | null;
  width: number | null;
  height: number | null;
  state: FileState;
  error: string | null;
  indexedNs: number;
  thumb: string | null;
}

export type Phase = 'idle' | 'scanning' | 'indexing' | 'watching' | 'paused' | 'failed';

export interface Progress {
  phase: Phase;
  discovered: number;
  queued: number;
  done: number;
  failed: number;
  skipped: number;
  totalEstimate: number;
  current: string | null;
  imagesPerMin: number;
  elapsedS: number;
  message: string | null;
}

export interface DownloadProgress {
  file: string;
  fileIndex: number;
  fileCount: number;
  doneBytes: number;
  totalBytes: number;
  overallBytes: number;
  overallTotal: number;
  bytesPerSecond: number;
  endpoint: string;
  note: string | null;
}

export interface ModelStatePayload {
  ready: boolean;
  note: string;
  cancelled: boolean;
}

export type Applied = 'ignored' | 'index' | 'unchanged' | 'removed' | 'rescan';

export interface WatchChange {
  path: string;
  type: string;
  applied: Applied;
  /** 行已被删除或还没建出来时为 null；用于丢掉该文件缓存的缩略图。 */
  fileId: number | null;
}

export interface Runtime {
  device: string;
  dtype: string;
  dim: number;
  paramsMillions: number;
  loadSeconds: number;
  vision: boolean;
  audio: boolean;
  threads: number;
  rssMb: number;
}

export interface ModelStatus {
  dir: string;
  present: number;
  totalFiles: number;
  complete: boolean;
  bytesOnDisk: number;
  totalBytes: number;
  missing: string[];
  mismatched: string[];
  resumeBytes: number;
}

export interface ModelInfo {
  status: ModelStatus;
  downloading: boolean;
  ready: boolean;
  hasPython: boolean;
  python: string | null;
  endpoints: string[];
  modelDir: string;
  dbPath: string;
  cacheDir: string;
}

export interface Overview {
  files: number;
  bytes: number;
  vectors: number;
  clips: number;
  chunks: number;
  roots: number;
  pending: number;
  done: number;
  failed: number;
  skipped: number;
  deadRows: number;
  byKind: Record<string, number>;
  indexing: boolean;
  semanticReady: boolean;
  hasPython: boolean;
  sidecarRunning: boolean;
  vectorDim: number;
  watchedRoots: number;
  vectorBytes: number;
}

export interface Settings {
  vectorDim: number;
  indexImages: boolean;
  indexVideo: boolean;
  indexDocuments: boolean;
  indexCode: boolean;
  modelEndpoints: string[];
  modelRepo: string;
  inferPython: string | null;
  inferThreads: number;
  inferBatch: number;
  videoWindowS: number;
  videoStepS: number;
  videoFps: number;
  maxTextStoreKb: number;
  maxExtractBytes: number;
  maxFileBytes: number;
  excludeDirs: string[];
  excludePathPrefixes: string[];
  globalShortcut: string;
  resumeOnLaunch: boolean;
  ramBudgetMb: number;
  netProxy: string | null;
  updateCheck: boolean;
}

export type UpdateState =
  | 'checking'
  | 'upToDate'
  | 'available'
  | 'downloading'
  | 'installing'
  | 'installed'
  | 'failed';

export interface UpdateInfo {
  version: string;
  currentVersion: string;
  notes: string | null;
  publishedAt: string | null;
}

/** Pushed on `update://state` by `src-tauri/src/update.rs`. */
export interface UpdateEvent {
  state: UpdateState;
  info: UpdateInfo | null;
  percent: number | null;
  downloaded: number | null;
  total: number | null;
  error: string | null;
}

export interface ScanReport {
  roots: number;
  walked: number;
  candidates: number;
  skippedHidden: number;
  skippedTooBig: number;
  skippedExcluded: number;
  unreadable: number;
  errors: number;
  queued: number;
  elapsedMs: number;
}

export type IndexMode = 'full' | 'text';

export function emptyFilters(): Filters {
  return {
    kinds: [],
    rootId: null,
    minSize: null,
    maxSize: null,
    afterNs: null,
    beforeNs: null,
  };
}
