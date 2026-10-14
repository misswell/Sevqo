// One bundle serves both Tauri windows; the window label decides the mode.
import { getCurrentWindow } from '@tauri-apps/api/window';

export type WindowScope = 'spotlight' | 'main';

function fromQuery(): WindowScope | null {
  const v = new URLSearchParams(window.location.search).get('window');
  return v === 'spotlight' || v === 'main' ? v : null;
}

export function detectScope(): WindowScope {
  try {
    return getCurrentWindow().label === 'spotlight' ? 'spotlight' : 'main';
  } catch {
    // Plain browser preview (`vite` without Tauri) — fall back to the query string.
    return fromQuery() ?? 'main';
  }
}

export function applyScopeToDocument(scope: WindowScope): void {
  document.documentElement.dataset.window = scope;
  document.documentElement.lang = 'zh-CN';
  document.title = scope === 'spotlight' ? 'Sevqo 搜索' : 'Sevqo';
}

export function isMac(): boolean {
  const p = navigator.userAgent;
  return /Mac|iPhone|iPad/i.test(p);
}

/** ⌘ on macOS, Ctrl elsewhere — used for the shortcut hints and key handling. */
export function hasMeta(e: { metaKey: boolean; ctrlKey: boolean }): boolean {
  return isMac() ? e.metaKey : e.ctrlKey;
}
