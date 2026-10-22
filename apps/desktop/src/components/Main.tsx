import { useCallback, useEffect, useRef, useState, type ReactElement } from 'react';

import type { Hit, Moment } from '../types';
import { labels } from '../i18n';
import { useBackend } from '../useBackend';
import { isMac } from '../scope';
import { DetailPane } from './DetailPane';
import { MomentPlayer } from './MomentPlayer';
import { SearchArea } from './SearchArea';
import { SettingsSheet } from './SettingsSheet';
import { Sidebar } from './Sidebar';

export function Main(): ReactElement {
  const backend = useBackend();
  const [selected, setSelected] = useState<Hit | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [playing, setPlaying] = useState<{ hit: Hit; moment: Moment } | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const openMoment = useCallback((hit: Hit, moment: Moment) => setPlaying({ hit, moment }), []);

  const { flash, dismissFlash } = backend;
  useEffect(() => {
    if (!flash) return;
    const id = window.setTimeout(() => dismissFlash(), 4200);
    return () => window.clearTimeout(id);
  }, [flash, dismissFlash]);

  // ⌘K / Ctrl+K focuses the search box; Esc closes the topmost overlay.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((isMac() ? e.metaKey : e.ctrlKey) && (e.key === 'k' || e.key === 'K')) {
        e.preventDefault();
        inputRef.current?.focus();
        inputRef.current?.select();
        return;
      }
      if (e.key !== 'Escape') return;
      if (playing) setPlaying(null);
      else if (settingsOpen) setSettingsOpen(false);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [playing, settingsOpen]);

  return (
    <div className="shell">
      <Sidebar backend={backend} onOpenSettings={() => setSettingsOpen(true)} />

      <main className="middle">
        <SearchArea
          roots={backend.roots}
          semanticReady={!!backend.overview?.semanticReady}
          selectedFileId={selected?.fileId ?? null}
          onSelect={setSelected}
          onMoment={openMoment}
          inputRef={inputRef}
        />
      </main>

      <DetailPane hit={selected} roots={backend.roots} onMoment={openMoment} />

      {flash ? (
        <div className={`flash flash-${flash.kind}`} role="status">
          <span>{flash.text}</span>
          <button type="button" className="flash-close" onClick={dismissFlash} aria-label={labels.common.close}>
            ×
          </button>
        </div>
      ) : null}

      {settingsOpen && backend.settings ? (
        <SettingsSheet
          settings={backend.settings}
          busy={backend.busy}
          indexing={backend.indexing}
          onClose={() => setSettingsOpen(false)}
          onSave={backend.saveSettings}
        />
      ) : null}

      {playing ? (
        <MomentPlayer
          key={`${playing.hit.fileId}-${playing.moment.startMs}`}
          hit={playing.hit}
          moment={playing.moment}
          onClose={() => setPlaying(null)}
        />
      ) : null}
    </div>
  );
}
