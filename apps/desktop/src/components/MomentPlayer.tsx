import { useEffect, useRef, useState, type ReactElement } from 'react';

import { api, errorText, mediaUrl } from '../api';
import type { Hit, Moment } from '../types';
import { formatDurationMs, formatScore } from '../format';
import { labels } from '../i18n';
import { CloseIcon } from './Icons';
import { Btn, Note } from './ui';

/**
 * Plays the matched second inside the app: launching an external player would start the
 * film at 00:00 and lose the very moment the search found.
 */
export function MomentPlayer({ hit, moment, onClose }: { hit: Hit; moment: Moment; onClose: () => void }): ReactElement {
  const [error, setError] = useState<string | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);

  useEffect(() => {
    const v = videoRef.current;
    if (!v) return;
    // Autoplay can still be refused, and a missing HEVC decoder surfaces only as an error event.
    const onErr = () => setError(labels.detail.playerUnavailable);
    v.addEventListener('error', onErr);
    v.play().catch(() => setError(labels.detail.playerUnavailable));
    return () => v.removeEventListener('error', onErr);
  }, []);

  return (
    <div className="player-backdrop" onClick={onClose}>
      <section className="player" onClick={(e) => e.stopPropagation()} role="dialog" aria-label={hit.name}>
        <header className="player-head">
          <span className="player-title" title={hit.path}>
            {hit.name}
          </span>
          <span className="player-meta mono">
            {formatDurationMs(moment.startMs)}–{formatDurationMs(moment.endMs)} · {formatScore(moment.score)}
          </span>
          <button type="button" className="player-close" onClick={onClose} aria-label={labels.common.close}>
            <CloseIcon size={13} />
          </button>
        </header>

        <video
          ref={videoRef}
          className="player-video"
          src={mediaUrl(hit.fileId, moment.startMs)}
          controls
          autoPlay
          playsInline
          preload="auto"
        />

        {error ? <Note tone="error">{error}</Note> : null}

        <footer className="player-foot">
          <Btn
            onClick={() => {
              api.openFile(hit.path).catch((e) => setError(errorText(e)));
            }}
          >
            {labels.detail.openFile}
          </Btn>
          <Btn
            onClick={() => {
              api.revealInFolder(hit.path).catch((e) => setError(errorText(e)));
            }}
          >
            {labels.detail.openFolder}
          </Btn>
          <Btn tone="primary" onClick={onClose}>
            {labels.common.close}
          </Btn>
        </footer>
      </section>
    </div>
  );
}
