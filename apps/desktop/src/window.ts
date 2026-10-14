// Window helpers that need the `core:window:*` permissions granted in
// `src-tauri/capabilities/default.json`. All of them are best-effort: outside
// Tauri (plain `vite` preview) they silently no-op.
import { getCurrentWindow } from '@tauri-apps/api/window';
import { LogicalSize } from '@tauri-apps/api/dpi';

export async function setWindowSize(width: number, height: number): Promise<void> {
  try {
    const win = getCurrentWindow();
    await win.setSize(new LogicalSize(width, height));
    await win.center();
  } catch {
    /* not running inside Tauri */
  }
}
