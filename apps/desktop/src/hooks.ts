import { useCallback, useEffect, useMemo, useRef, useState, type RefObject } from 'react';

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

const thumbCache = new Map<string, string | null>();
const inflight = new Map<string, Promise<string | null>>();

function cached(key: string): string | null | undefined {
  return thumbCache.has(key) ? (thumbCache.get(key) ?? null) : undefined;
}

function loadFileThumb(fileId: number): Promise<string | null> {
  const key = `f:${fileId}`;
  const running = inflight.get(key);
  if (running) return running;
  const p = api
    .thumb(fileId)
    .catch(() => null)
    .then((v) => {
      thumbCache.set(key, v);
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
      thumbCache.set(key, v);
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
    return;
  }
  for (const key of [...thumbCache.keys()]) {
    if (key === `f:${fileId}` || key.startsWith(`c:${fileId}:`)) thumbCache.delete(key);
  }
  momentCache.delete(fileId);
}

/** Lazy data-URL thumbnail; only fetched once `enabled` (row in view). */
export function useThumb(fileId: number | null, enabled: boolean): string | null {
  const [url, setUrl] = useState<string | null>(null);
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
  }, [fileId, enabled]);
  return url;
}

export function useClipThumb(fileId: number | null, clipId: number | null): string | null {
  const [url, setUrl] = useState<string | null>(null);
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
  }, [fileId, clipId]);
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
const momentCache = new Map<number, Moment[]>();

export function useMoments(fileId: number | null): Moment[] | null {
  const [moments, setMoments] = useState<Moment[] | null>(
    fileId != null ? (momentCache.get(fileId) ?? null) : null,
  );
  useEffect(() => {
    if (fileId == null) {
      setMoments(null);
      return;
    }
    const cachedForFile = momentCache.get(fileId);
    if (cachedForFile) {
      setMoments(cachedForFile);
      return;
    }
    let alive = true;
    setMoments(null);
    void api
      .moments(fileId)
      .then((m) => {
        momentCache.set(fileId, m);
        if (alive) setMoments(m);
      })
      .catch(() => {
        if (alive) setMoments([]);
      });
    return () => {
      alive = false;
    };
  }, [fileId]);
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
