// Number / time / path formatting for the Chinese UI.

import type { Kind } from './types';

const KB = 1024;
const MB = KB * 1024;
const GB = MB * 1024;
const TB = GB * 1024;

export function formatBytes(n: number | null | undefined, digits = 1): string {
  if (n == null || !Number.isFinite(n) || n < 0) return '—';
  if (n < KB) return `${Math.round(n)} B`;
  if (n < MB) return `${(n / KB).toFixed(n / KB < 10 ? 1 : 0)} KB`;
  if (n < GB) return `${(n / MB).toFixed(digits)} MB`;
  if (n < TB) return `${(n / GB).toFixed(digits)} GB`;
  return `${(n / TB).toFixed(digits)} TB`;
}

export function formatNumber(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return '—';
  return Math.round(n).toLocaleString('zh-CN');
}

export function formatFixed(n: number | null | undefined, digits = 1): string {
  if (n == null || !Number.isFinite(n)) return '—';
  return n.toFixed(digits);
}

export function formatPercent(done: number, total: number): string {
  if (!Number.isFinite(total) || total <= 0) return '—';
  const p = Math.min(100, Math.max(0, (done / total) * 100));
  return `${p < 10 ? p.toFixed(1) : Math.round(p)}%`;
}

export function formatSpeed(bytesPerSecond: number): string {
  if (!Number.isFinite(bytesPerSecond) || bytesPerSecond <= 0) return '—';
  return `${formatBytes(bytesPerSecond, 1)}/秒`;
}

/** `ms` here is a *millisecond* duration (not a timestamp). */
export function formatDurationMs(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return '—';
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (v: number) => String(v).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${pad(m)}:${pad(s)}`;
}

export function formatSeconds(seconds: number | null | undefined): string {
  if (seconds == null || !Number.isFinite(seconds) || seconds < 0) return '—';
  if (seconds < 1) return `${Math.round(seconds * 1000)} 毫秒`;
  if (seconds < 60) return `${seconds.toFixed(1)} 秒`;
  const m = Math.floor(seconds / 60);
  const s = Math.round(seconds % 60);
  return `${m} 分 ${String(s).padStart(2, '0')} 秒`;
}

export function formatMillis(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return '—';
  if (ms < 1000) return `${Math.round(ms)} 毫秒`;
  return `${(ms / 1000).toFixed(2)} 秒`;
}

function nsToMillis(ns: number): number {
  return Math.floor(ns / 1_000_000);
}

const dateFmt = new Intl.DateTimeFormat('zh-CN', { month: '2-digit', day: '2-digit' });
const dateTimeFmt = new Intl.DateTimeFormat('zh-CN', {
  year: 'numeric',
  month: '2-digit',
  day: '2-digit',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
});
const yearFmt = new Intl.DateTimeFormat('zh-CN', { year: 'numeric', month: '2-digit', day: '2-digit' });

/** `ns` is nanoseconds since the Unix epoch. */
export function formatNsAbsolute(ns: number | null | undefined): string {
  if (ns == null || !Number.isFinite(ns) || ns <= 0) return '—';
  const d = new Date(nsToMillis(ns));
  return d.getFullYear() === new Date().getFullYear() ? dateTimeFmt.format(d) : yearFmt.format(d);
}

export function relativeFromNs(ns: number | null | undefined): string {
  if (ns == null || !Number.isFinite(ns) || ns <= 0) return '—';
  const then = nsToMillis(ns);
  const diff = Date.now() - then;
  const min = 60_000;
  if (diff < 0) return dateTimeFmt.format(new Date(then));
  if (diff < 45_000) return '刚刚';
  if (diff < 60 * min) return `${Math.floor(diff / min)} 分钟前`;
  if (diff < 24 * 60 * min) return `${Math.floor(diff / (60 * min))} 小时前`;
  const days = Math.floor(diff / (24 * 60 * min));
  if (days === 1) return '昨天';
  if (days < 7) return `${days} 天前`;
  if (days < 30) return `${Math.floor(days / 7)} 周前`;
  if (days < 365) return dateFmt.format(new Date(then));
  return yearFmt.format(new Date(then));
}

/** Directory part of a path, using both separators. */
export function parentDir(path: string): string {
  const idx = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
  if (idx <= 0) return path === '/' ? '/' : '—';
  return path.slice(0, idx);
}

export function fileName(path: string): string {
  const idx = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
  return idx < 0 ? path : path.slice(idx + 1);
}

/** Middle-elides a long path so it fits one compact row. */
export function shortenPath(path: string, head = 10, tail = 26): string {
  if (path.length <= head + tail + 1) return path;
  return `${path.slice(0, head)}…${path.slice(-tail)}`;
}

const IMAGE_EXTS = ['jpg', 'jpeg', 'png', 'webp', 'gif', 'bmp', 'tif', 'tiff', 'heic', 'heif', 'avif', 'dng'];
const VIDEO_EXTS = ['mp4', 'mov', 'm4v', 'mkv', 'avi', 'webm', 'wmv', 'flv', 'mpg', 'mpeg', '3gp'];

export type MediaQuery = { path: string; kind: Extract<Kind, 'image' | 'video'> };

/** Which visual-query mode a path would use — mirrors the backend's `Kind::from_ext`. */
export function mediaKindOf(path: string): MediaQuery | null {
  const dot = path.lastIndexOf('.');
  if (dot <= 0) return null;
  const ext = path.slice(dot + 1).toLowerCase();
  if (IMAGE_EXTS.includes(ext)) return { path, kind: 'image' };
  if (VIDEO_EXTS.includes(ext)) return { path, kind: 'video' };
  return null;
}

/** First dropped path the engine can use as a visual query. */
export function firstMediaPath(paths: string[]): MediaQuery | null {
  for (const p of paths) {
    const m = mediaKindOf(p);
    if (m) return m;
  }
  return null;
}

export function formatScore(score: number): string {
  if (!Number.isFinite(score)) return '—';
  return score >= 1 ? score.toFixed(2) : score.toFixed(3);
}

/** Millisecond epoch -> nanosecond epoch. Precision past ~200 ns is irrelevant here. */
export function msToNs(ms: number): number {
  return Math.round(ms * 1_000_000);
}

export function nsToMs(ns: number): number {
  return Math.floor(ns / 1_000_000);
}

/** Start-of-today / now-7d / now-30d / Jan 1 this year, as nanosecond thresholds. */
export function dateThresholdNs(preset: number): number | null {
  const now = new Date();
  if (preset === 1) {
    const d = new Date(now.getFullYear(), now.getMonth(), now.getDate(), 0, 0, 0, 0);
    return msToNs(d.getTime());
  }
  if (preset === 2) return msToNs(now.getTime() - 7 * 24 * 3600 * 1000);
  if (preset === 3) return msToNs(now.getTime() - 30 * 24 * 3600 * 1000);
  if (preset === 4) return msToNs(new Date(now.getFullYear(), 0, 1, 0, 0, 0, 0).getTime());
  return null;
}

const MB_UNITS = MB;

/** Byte range presets: [min, max] in bytes. */
export function sizeRange(index: number): [number | null, number | null] {
  switch (index) {
    case 1:
      return [null, MB_UNITS];
    case 2:
      return [MB_UNITS, 10 * MB_UNITS];
    case 3:
      return [10 * MB_UNITS, 100 * MB_UNITS];
    case 4:
      return [100 * MB_UNITS, null];
    default:
      return [null, null];
  }
}

/** Parses a byte size typed by the user: "10MB", "1.5 GB", "500". */
export function parseSize(raw: string): number | null {
  const t = raw.trim().toLowerCase().replace(/,/g, '');
  if (!t) return null;
  const m = /^(\d+(?:\.\d+)?)\s*(b|kb|k|mb|m|gb|g|tb|t)?$/u.exec(t);
  if (!m) return null;
  const value = Number.parseFloat(m[1] ?? '0');
  const unit = (m[2] ?? 'b').replace('b', '');
  const mult: Record<string, number> = { '': 1, k: KB, m: MB, g: GB, t: TB };
  const factor = mult[unit] ?? 1;
  return Math.round(value * factor);
}
