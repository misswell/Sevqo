import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type RefObject } from 'react';

import { getCurrentWindow } from '@tauri-apps/api/window';

import { api } from './api';
import type { Moment } from './types';

/** Stable callback identity so event subscriptions are not re-created per render. */
export function useEventCallback<A>(fn: (arg: A) => void): (arg?: A) => void {
  const ref = useRef(fn);
  useEffect(() => {
    ref.current = fn;
  }, [fn]);
  return useCallback((arg?: A) => {
    ref.current(arg as A);
  }, []);
}

export function useInterval(action: () => void, ms: number | null): void {
  const ref = useEventCallback(action);
  useEffect(() => {
    if (ms == null) return;
    const id = window.setInterval(() => ref(), ms);
    return () => window.clearInterval(id);
  }, [ms, ref]);
}

export interface Debounced<A> {
  (arg: A): void;
  /** Drop the pending call. Clearing the box must not let an older keystroke land after it. */
  cancel: () => void;
}

/** Fires `action` `wait` ms after the last call; `reset` restarts the timer. */
export function useDebounced<A>(action: (arg: A) => void, wait: number): Debounced<A> {
  const ref = useEventCallback(action);
  const timer = useRef<number | null>(null);
  const cancel = useCallback(() => {
    if (timer.current != null) window.clearTimeout(timer.current);
    timer.current = null;
  }, []);
  useEffect(
    () => () => {
      if (timer.current != null) window.clearTimeout(timer.current);
    },
    [],
  );
  const run = useCallback(
    (arg: A) => {
      if (timer.current != null) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => {
        timer.current = null;
        ref(arg);
      }, wait);
    },
    [ref, wait],
  );
  return useMemo(() => Object.assign(run, { cancel }), [run, cancel]);
}

// ---------------------------------------------------------------- thumbnails

/**
 * Bounded LRU (P2-03): browsing a large library used to grow these Maps without limit —
 * data-URL thumbnails are megabytes of base64 each. The cache is now capped by *bytes*
 * (UTF-16 code units, which is what a retained string costs) instead of entry count alone,
 * and null results expire so a temporary failure does not stick forever.
 */
type CacheEntry<V> = { v: V; bytes: number; at: number };

class Lru<V> {
  private map = new Map<string, CacheEntry<V>>();
  private total = 0;
  constructor(
    private maxBytes: number,
    private maxEntries: number,
  ) {}

  get(key: string): CacheEntry<V> | undefined {
    const e = this.map.get(key);
    if (!e) return undefined;
    // re-insert to mark as most recently used
    this.map.delete(key);
    this.map.set(key, e);
    return e;
  }

  set(key: string, v: V, bytes: number): void {
    const prev = this.map.get(key);
    if (prev) {
      this.total -= prev.bytes;
      this.map.delete(key);
    }
    this.map.set(key, { v, bytes, at: Date.now() });
    this.total += bytes;
    while ((this.total > this.maxBytes || this.map.size > this.maxEntries) && this.map.size > 1) {
      const oldest = this.map.keys().next().value as string;
      const e = this.map.get(oldest);
      if (e) this.total -= e.bytes;
      this.map.delete(oldest);
    }
  }

  delete(key: string): void {
    const e = this.map.get(key);
    if (e) this.total -= e.bytes;
    this.map.delete(key);
  }

  clear(): void {
    this.map.clear();
    this.total = 0;
  }

  keys(): string[] {
    return [...this.map.keys()];
  }
}

/** A failed thumbnail is retried after this long instead of being cached forever. */
const NEGATIVE_TTL_MS = 30_000;
/** ~48 MiB of data URLs, or 400 entries, whichever comes first. */
const thumbCache = new Lru<string | null>(48 * 1024 * 1024, 400);
const inflight = new Map<string, Promise<string | null>>();

/**
 * Bumped whenever the caches are invalidated. Mounted rows subscribe to it, otherwise a
 * re-indexed file would keep showing its old data URL until it scrolls out of view —
 * clearing the Map alone only helps rows that fetch afterwards.
 */
let cacheVersion = 0;
const cacheListeners = new Set<() => void>();

function bumpCacheVersion(): void {
  cacheVersion += 1;
  for (const l of cacheListeners) l();
}

function subscribeCache(listener: () => void): () => void {
  cacheListeners.add(listener);
  return () => cacheListeners.delete(listener);
}

function useCacheVersion(): number {
  return useSyncExternalStore(subscribeCache, () => cacheVersion);
}

function cached(key: string): string | null | undefined {
  const e = thumbCache.get(key);
  if (!e) return undefined;
  if (e.v === null) {
    if (Date.now() - e.at > NEGATIVE_TTL_MS) {
      thumbCache.delete(key);
      return undefined;
    }
    return null;
  }
  return e.v;
}

function loadFileThumb(fileId: number): Promise<string | null> {
  const key = `f:${fileId}`;
  const running = inflight.get(key);
  if (running) return running;
  const p = api
    .thumb(fileId)
    .catch(() => null)
    .then((v) => {
      thumbCache.set(key, v, v ? v.length * 2 : 0);
      inflight.delete(key);
      return v;
    });
  inflight.set(key, p);
  return p;
}

function loadClipThumb(fileId: number, clipId: number): Promise<string | null> {
  const key = `c:${fileId}:${clipId}`;
  const running = inflight.get(key);
  if (running) return running;
  const p = api
    .clipThumb(fileId, clipId)
    .catch(() => null)
    .then((v) => {
      thumbCache.set(key, v, v ? v.length * 2 : 0);
      inflight.delete(key);
      return v;
    });
  inflight.set(key, p);
  return p;
}

/** Files are re-rendered on disk after a rebuild or a watcher-driven re-index; drop what we cached for them. */
export function invalidateFileCache(fileId?: number): void {
  if (fileId == null) {
    thumbCache.clear();
    momentCache.clear();
    bumpCacheVersion();
    return;
  }
  for (const key of thumbCache.keys()) {
    if (key === `f:${fileId}` || key.startsWith(`c:${fileId}:`)) thumbCache.delete(key);
  }
  momentCache.delete(momentsKey(fileId));
  bumpCacheVersion();
}

/** Lazy data-URL thumbnail; only fetched once `enabled` (row in view). */
export function useThumb(fileId: number | null, enabled: boolean): string | null {
  const [url, setUrl] = useState<string | null>(null);
  const version = useCacheVersion();
  useEffect(() => {
    if (fileId == null || !enabled) {
      setUrl(null);
      return;
    }
    const hit = cached(`f:${fileId}`);
    if (hit !== undefined) {
      setUrl(hit);
      return;
    }
    let alive = true;
    setUrl(null);
    void loadFileThumb(fileId).then((v) => {
      if (alive) setUrl(v);
    });
    return () => {
      alive = false;
    };
  }, [fileId, enabled, version]);
  return url;
}

export function useClipThumb(fileId: number | null, clipId: number | null): string | null {
  const [url, setUrl] = useState<string | null>(null);
  const version = useCacheVersion();
  useEffect(() => {
    if (fileId == null || clipId == null) {
      setUrl(null);
      return;
    }
    const hit = cached(`c:${fileId}:${clipId}`);
    if (hit !== undefined) {
      setUrl(hit);
      return;
    }
    let alive = true;
    setUrl(null);
    void loadClipThumb(fileId, clipId).then((v) => {
      if (alive) setUrl(v);
    });
    return () => {
      alive = false;
    };
  }, [fileId, clipId, version]);
  return url;
}

/** True once the element has been visible; keeps sticky rows from re-fetching. */
export function useInView<T extends HTMLElement>(): [RefObject<T>, boolean] {
  const ref = useRef<T>(null);
  const [seen, setSeen] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el || seen) return;
    if (typeof IntersectionObserver === 'undefined') {
      setSeen(true);
      return;
    }
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          setSeen(true);
          io.disconnect();
        }
      },
      { rootMargin: '160px' },
    );
    io.observe(el);
    return () => io.disconnect();
  }, [seen]);
  return [ref, seen];
}

/** Moments for one file, fetched on demand and memoised per file id. */
/** Moments are small but there can be many files; cap by entry count and rough bytes. */
const momentCache = new Lru<Moment[]>(8 * 1024 * 1024, 200);
const momentsKey = (fileId: number): string => `m:${fileId}`;

export function useMoments(fileId: number | null): Moment[] | null {
  const [moments, setMoments] = useState<Moment[] | null>(
    fileId != null ? (momentCache.get(momentsKey(fileId))?.v ?? null) : null,
  );
  const version = useCacheVersion();
  useEffect(() => {
    if (fileId == null) {
      setMoments(null);
      return;
    }
    const cachedForFile = momentCache.get(momentsKey(fileId))?.v;
    if (cachedForFile) {
      setMoments(cachedForFile);
      return;
    }
    let alive = true;
    setMoments(null);
    void api
      .moments(fileId)
      .then((m) => {
        momentCache.set(momentsKey(fileId), m, JSON.stringify(m).length * 2);
        if (alive) setMoments(m);
      })
      .catch(() => {
        if (alive) setMoments([]);
      });
    return () => {
      alive = false;
    };
  }, [fileId, version]);
  return moments;
}

/** Debounced auto-focus helper for the two search inputs. */
export function useFocusOnMount(): RefObject<HTMLInputElement> {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const id = window.setTimeout(() => ref.current?.focus(), 60);
    return () => window.clearTimeout(id);
  }, []);
  return ref;
}

/**
 * Hidden webviews miss events and get their timers throttled, so coming back on screen
 * is exactly when stale state shows up. Re-ask for the truth at that moment.
 */
export function useOnVisible(action: () => void): void {
  const ref = useEventCallback(action);
  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState === 'visible') ref();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => document.removeEventListener('visibilitychange', onVisible);
  }, [ref]);
}

/**
 * Files dropped on the window. Tauri hands over absolute paths, which is what makes
 * "drag a screenshot onto the box" work without a file-reading permission. The
 * callback goes through a ref, so an inline closure never re-registers the listener.
 */
export function useFileDrop(onDrop: (paths: string[]) => void): boolean {
  const ref = useEventCallback(onDrop);
  const [hovering, setHovering] = useState(false);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let dead = false;
    try {
      void getCurrentWindow()
        .onDragDropEvent((e) => {
          const drop = e.payload;
          if (drop.type === 'enter' || drop.type === 'over') setHovering(true);
          else if (drop.type === 'leave') setHovering(false);
          else {
            setHovering(false);
            if (drop.paths.length > 0) ref(drop.paths);
          }
        })
        .then((fn) => {
          if (dead) fn();
          else unlisten = fn;
        });
    } catch {
      // Plain `vite` preview has no Tauri window — drop support simply stays off.
    }
    return () => {
      dead = true;
      unlisten?.();
    };
  }, [ref]);

  return hovering;
}
