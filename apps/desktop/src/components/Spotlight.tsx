// Spotlight window: one input, one compact dropdown. Keystroke feedback comes
// from `search_keyword`; the fused `search` result replaces it ~350 ms later.
import { useCallback, useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactElement } from 'react';

import { api, errorText } from '../api';
import { emptyFilters, type Hit } from '../types';
import { formatDurationMs, fileName, firstMediaPath, parentDir, relativeFromNs, shortenPath, type MediaQuery } from '../format';
import { labels } from '../i18n';
import { useDebounced, useFileDrop, useFocusOnMount } from '../hooks';
import { hasMeta } from '../scope';
import { setWindowSize } from '../window';
import { CloseIcon, KindIcon, SearchIcon, WindowIcon } from './Icons';
import { Note } from './ui';

const MAX_ROWS = 8;
const BASE_HEIGHT = 60;
const MAX_HEIGHT = 560;
/** `.spot` vertical padding (6 px top + 8 px bottom) added to the card height. */
const CARD_PADDING = 14;

export function Spotlight(): ReactElement {
  const [query, setQuery] = useState('');
  const [hits, setHits] = useState<Hit[]>([]);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [active, setActive] = useState(0);
  /** Set when a picture or clip was dropped: the list below is similar media, not a text match. */
  const [mediaOf, setMediaOf] = useState<MediaQuery | null>(null);

  const seq = useRef(0);
  const cardRef = useRef<HTMLDivElement | null>(null);
  const listRef = useRef<HTMLUListElement | null>(null);
  const inputRef = useFocusOnMount();

  const run = useCallback(async (raw: string, hybrid: boolean) => {
    const id = ++seq.current;
    const q = raw.trim();
    setLoading(true);
    setError(null);
    try {
      if (!q) {
        const list = await api.recent(MAX_ROWS);
        if (id !== seq.current) return;
        setHits(list);
        setNote(null);
        return;
      }
      const req = { query: q, limit: MAX_ROWS, filters: emptyFilters(), semantic: hybrid };
      const resp = hybrid ? await api.search(req) : await api.searchKeyword(req);
      if (id !== seq.current) return;
      setHits(resp.hits);
      setNote(resp.note);
    } catch (e) {
      if (id !== seq.current) return;
      setError(errorText(e));
      setHits([]);
    } finally {
      if (id === seq.current) setLoading(false);
    }
  }, []);

  const runMedia = useCallback(async (media: MediaQuery) => {
    const id = ++seq.current;
    setLoading(true);
    setError(null);
    try {
      const resp = await api.searchMedia(media.path, MAX_ROWS);
      if (id !== seq.current) return;
      setMediaOf(media);
      setQuery('');
      setActive(0);
      setHits(resp.hits);
      setNote(resp.note);
    } catch (e) {
      if (id !== seq.current) return;
      setError(errorText(e));
      setHits([]);
    } finally {
      if (id === seq.current) setLoading(false);
    }
  }, []);

  const dropping = useFileDrop((paths) => {
    const m = firstMediaPath(paths);
    if (!m) {
      setError(labels.search.mediaDropIgnored);
      return;
    }
    void runMedia(m);
  });

  const debouncedHybrid = useDebounced<string>((q) => {
    void run(q, true);
  }, 350);

  const onChange = useCallback(
    (value: string) => {
      setQuery(value);
      setActive(0);
      setMediaOf(null);
      // instant channel first, then the fused / hybrid set
      void run(value, false);
      debouncedHybrid(value);
    },
    [debouncedHybrid, run],
  );

  // First paint shows the recent files, so the empty box is never dead space.
  useEffect(() => {
    void run('', true);
  }, [run]);

  // The frameless window is 60 px tall: grow it whenever there is more to show.
  // Only the card is measured (content-driven height); the outer wrapper would
  // track the window itself and the size could never shrink again.
  const measure = useCallback(() => {
    const card = cardRef.current;
    if (!card) return;
    const h = Math.min(MAX_HEIGHT, Math.max(BASE_HEIGHT, card.scrollHeight + CARD_PADDING));
    void setWindowSize(760, h);
  }, []);

  useEffect(() => {
    const id = window.setTimeout(measure, 40);
    return () => window.clearTimeout(id);
  }, [measure, hits, note, error, query, loading, mediaOf]);

  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState === 'visible') measure();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => document.removeEventListener('visibilitychange', onVisible);
  }, [measure]);

  const scrollActiveIntoView = useCallback((index: number) => {
    const el = listRef.current?.querySelector<HTMLElement>(`[data-index="${index}"]`);
    el?.scrollIntoView({ block: 'nearest' });
  }, []);

  const open = useCallback((hit: Hit, reveal: boolean) => {
    const fn = reveal ? api.revealInFolder : api.openFile;
    fn(hit.path).catch((e) => setError(errorText(e)));
  }, []);

  const onKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLInputElement>) => {
      switch (e.key) {
        case 'ArrowDown': {
          e.preventDefault();
          const next = Math.min(hits.length - 1, active + 1);
          setActive(next);
          scrollActiveIntoView(next);
          break;
        }
        case 'ArrowUp': {
          e.preventDefault();
          const next = Math.max(0, active - 1);
          setActive(next);
          scrollActiveIntoView(next);
          break;
        }
        case 'Enter': {
          const hit = hits[active];
          if (!hit) {
            if (hasMeta(e)) void api.showMain();
            return;
          }
          e.preventDefault();
          open(hit, hasMeta(e));
          break;
        }
        case 'Escape':
          e.preventDefault();
          if (query) {
            setQuery('');
            setActive(0);
            void run('', true);
          } else if (mediaOf) {
            setMediaOf(null);
            void run('', true);
          } else {
            void api.spotlightHide();
          }
          break;
        case 'k':
        case 'K':
          if (hasMeta(e)) {
            e.preventDefault();
            void api.showMain();
          }
          break;
        default:
          break;
      }
    },
    [active, hits, mediaOf, open, query, run, scrollActiveIntoView],
  );

  // Hide when the panel loses focus, but only after it genuinely had focus once
  // (the window is shown unfocused at launch; a stray blur must not close it).
  useEffect(() => {
    let hadFocus = document.hasFocus();
    const onFocus = () => {
      hadFocus = true;
    };
    const onBlur = () => {
      if (hadFocus) void api.spotlightHide();
    };
    window.addEventListener('focus', onFocus);
    window.addEventListener('blur', onBlur);
    return () => {
      window.removeEventListener('focus', onFocus);
      window.removeEventListener('blur', onBlur);
    };
  }, []);

  const showList = hits.length > 0;

  return (
    <div className="spot">
      {dropping ? <div className="drop-hint">{labels.search.mediaDropHint}</div> : null}
      <div className="spot-card" ref={cardRef}>
        <div className="spot-input-row">
          <span className="spot-glyph">
            <SearchIcon size={17} />
          </span>
          <input
            ref={inputRef}
            className="spot-input"
            type="text"
            value={query}
            placeholder={labels.spotlight.placeholder}
            spellCheck={false}
            autoComplete="off"
            aria-label={labels.spotlight.placeholder}
            onChange={(e) => onChange(e.target.value)}
            onKeyDown={onKeyDown}
          />
          {loading ? <span className="spot-spinner" aria-hidden="true" /> : null}
        </div>

        {mediaOf ? (
          <div className="spot-image-chip">
            <span className="spot-image-tag">
              {mediaOf.kind === 'video' ? labels.search.videoModeTitle : labels.search.imageModeTitle}
            </span>
            <span className="spot-image-name" title={mediaOf.path}>
              {fileName(mediaOf.path)}
            </span>
            <button
              type="button"
              className="spot-image-clear"
              aria-label={labels.common.clear}
              onClick={() => {
                setMediaOf(null);
                void run(query, true);
                inputRef.current?.focus();
              }}
            >
              <CloseIcon size={11} />
            </button>
          </div>
        ) : null}

        {note ? <Note>{note}</Note> : null}
        {error ? <Note tone="error">{error}</Note> : null}

        {showList ? (
          <ul className="spot-list" ref={listRef}>
            {hits.map((hit, i) => (
              <li key={`${hit.fileId}-${i}`}>
                <button
                  type="button"
                  data-index={i}
                  className={`spot-row${i === active ? ' spot-row-active' : ''}`}
                  onMouseMove={() => setActive(i)}
                  onClick={() => open(hit, false)}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    open(hit, true);
                  }}
                >
                  <span className={`spot-row-icon kind-${hit.kind}`}>
                    <KindIcon kind={hit.kind} size={15} />
                  </span>
                  <span className="spot-row-main">
                    <span className="spot-row-name">{hit.name}</span>
                    <span className="spot-row-dir">{shortenPath(parentDir(hit.path), 8, 40)}</span>
                  </span>
                  <span className="spot-row-side">
                    {hit.durationMs ? <span className="mono">{formatDurationMs(hit.durationMs)}</span> : null}
                    <span className="spot-row-time">{relativeFromNs(hit.mtimeNs)}</span>
                  </span>
                </button>
              </li>
            ))}
          </ul>
        ) : (
          <p className="spot-empty">
            {loading
              ? labels.spotlight.searching
              : mediaOf || query.trim()
                ? labels.spotlight.noResult
                : labels.spotlight.emptyQuery}
          </p>
        )}

        <div className="spot-foot">
          <span className="spot-hints">
            <span>{labels.spotlight.hintOpen}</span>
            <span>{labels.spotlight.hintReveal}</span>
            <span>{labels.spotlight.hintMain}</span>
            <span>{labels.spotlight.hintEsc}</span>
          </span>
          <button type="button" className="spot-main-btn" onClick={() => void api.showMain()}>
            <WindowIcon size={13} />
            <span>{labels.spotlight.showMain}</span>
          </button>
        </div>
      </div>
    </div>
  );
}
