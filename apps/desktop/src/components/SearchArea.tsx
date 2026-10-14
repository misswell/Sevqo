import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactElement, type RefObject } from 'react';

import { api, errorText } from '../api';
import { type Hit, type Kind, type Moment, type Root, type SearchRequest, type SearchResponse } from '../types';
import { dateThresholdNs, firstMediaPath, formatMillis, mediaKindOf, sizeRange } from '../format';
import { kindOrder, labels } from '../i18n';
import { useDebounced, useFileDrop } from '../hooks';
import { hasMeta, isMac } from '../scope';
import { CloseIcon, SearchIcon } from './Icons';
import { Btn, Chip, EmptyState, Note, Select } from './ui';
import { ResultRow, type RowHandlers } from './ResultRow';

const LIMITS = [20, 40, 100];
type Mode = 'text' | 'image';

export interface SearchAreaProps {
  roots: Root[];
  semanticReady: boolean;
  selectedFileId: number | null;
  onSelect: (hit: Hit | null) => void;
  onMoment: (hit: Hit, moment: Moment) => void;
  inputRef: RefObject<HTMLInputElement>;
}

export function SearchArea({
  roots,
  semanticReady,
  selectedFileId,
  onSelect,
  onMoment,
  inputRef,
}: SearchAreaProps): ReactElement {
  const [query, setQuery] = useState('');
  const [kinds, setKinds] = useState<Kind[]>([]);
  const [rootId, setRootId] = useState<number | null>(null);
  const [sizeIdx, setSizeIdx] = useState(0);
  const [dateIdx, setDateIdx] = useState(0);
  const [limit, setLimit] = useState(40);
  const [semantic, setSemantic] = useState(true);

  const [mode, setMode] = useState<Mode>('text');
  const [imagePath, setImagePath] = useState('');
  const [imageActive, setImageActive] = useState<string | null>(null);

  const [hits, setHits] = useState<Hit[]>([]);
  const [resp, setResp] = useState<SearchResponse | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [activeIndex, setActiveIndex] = useState(-1);

  const seq = useRef(0);

  const filters = useMemo(() => {
    const [minSize, maxSize] = sizeRange(sizeIdx);
    return {
      kinds,
      rootId,
      minSize,
      maxSize,
      afterNs: dateThresholdNs(dateIdx),
      beforeNs: null,
    };
  }, [kinds, rootId, sizeIdx, dateIdx]);

  const filterCount =
    (kinds.length > 0 ? 1 : 0) + (rootId != null ? 1 : 0) + (sizeIdx > 0 ? 1 : 0) + (dateIdx > 0 ? 1 : 0);

  const buildReq = useCallback(
    (q: string, hybrid: boolean): SearchRequest => ({
      query: q,
      limit,
      filters,
      semantic: hybrid && semantic,
    }),
    [filters, limit, semantic],
  );

  const applyResponse = useCallback((r: SearchResponse) => {
    setHits(r.hits);
    setResp(r);
    setNote(r.note);
    setActiveIndex(-1);
  }, []);

  const runText = useCallback(
    async (raw: string, hybrid: boolean) => {
      const id = ++seq.current;
      const q = raw.trim();
      setError(null);
      setImageActive(null);
      setMode('text');
      if (!q) {
        setLoading(true);
        try {
          const list = await api.recent(limit);
          if (id !== seq.current) return;
          setHits(list);
          setResp(null);
          setNote(null);
        } catch (e) {
          if (id !== seq.current) return;
          setError(errorText(e));
          setHits([]);
        } finally {
          if (id === seq.current) setLoading(false);
        }
        return;
      }
      setLoading(true);
      try {
        const req = buildReq(q, hybrid);
        const r = hybrid ? await api.search(req) : await api.searchKeyword(req);
        if (id !== seq.current) return;
        applyResponse(r);
      } catch (e) {
        if (id !== seq.current) return;
        setError(errorText(e));
        setHits([]);
        setResp(null);
      } finally {
        if (id === seq.current) setLoading(false);
      }
    },
    [applyResponse, buildReq, limit],
  );

  const debouncedHybrid = useDebounced<string>((q) => {
    void runText(q, true);
  }, 350);

  const onQueryChange = useCallback(
    (value: string) => {
      setQuery(value);
      void runText(value, false);
      debouncedHybrid(value);
    },
    [debouncedHybrid, runText],
  );

  const runMedia = useCallback(
    async (path: string, withKinds: boolean) => {
      const id = ++seq.current;
      setLoading(true);
      setError(null);
      try {
        const r = await api.searchMedia(path, limit, withKinds ? kinds : undefined);
        if (id !== seq.current) return;
        setMode('image');
        setImageActive(path);
        applyResponse(r);
      } catch (e) {
        if (id !== seq.current) return;
        setError(errorText(e));
        setHits([]);
        setResp(null);
      } finally {
        if (id === seq.current) setLoading(false);
      }
    },
    [applyResponse, kinds, limit],
  );

  const dropping = useFileDrop((paths) => {
    const m = firstMediaPath(paths);
    if (!m) {
      setError(labels.search.mediaDropIgnored);
      return;
    }
    setImagePath(m.path);
    void runMedia(m.path, kinds.length > 0);
  });

  // Re-run the current query whenever filters / limit / semantic change.
  const firstFilterRun = useRef(true);
  useEffect(() => {
    if (firstFilterRun.current) {
      firstFilterRun.current = false;
      return;
    }
    if (mode === 'image' && imageActive) {
      void runMedia(imageActive, kinds.length > 0);
      return;
    }
    void runText(query, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filters, limit, semantic]);

  useEffect(() => {
    void runText('', true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const open = useCallback((hit: Hit, reveal: boolean) => {
    const fn = reveal ? api.revealInFolder : api.openFile;
    fn(hit.path).catch((e) => setError(errorText(e)));
  }, []);

  const handlers = useMemo<RowHandlers>(
    () => ({
      onSelect: (hit) => {
        onSelect(hit);
        setActiveIndex(hits.indexOf(hit));
      },
      onOpen: open,
      onMoment,
    }),
    [hits, onSelect, open, onMoment],
  );

  const moveActive = useCallback(
    (delta: number) => {
      if (hits.length === 0) return;
      const next = Math.min(hits.length - 1, Math.max(0, activeIndex + delta));
      setActiveIndex(next);
      onSelect(hits[next]);
      const el = document.querySelector<HTMLElement>(`.row[data-index="${next}"]`);
      el?.scrollIntoView({ block: 'nearest' });
    },
    [activeIndex, hits, onSelect],
  );

  const onListKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLElement>) => {
      switch (e.key) {
        case 'ArrowDown':
          e.preventDefault();
          moveActive(1);
          break;
        case 'ArrowUp':
          e.preventDefault();
          moveActive(-1);
          break;
        case 'Enter': {
          const hit = hits[activeIndex];
          if (hit) {
            e.preventDefault();
            open(hit, hasMeta(e));
          }
          break;
        }
        case 'Escape':
          if (!query) return;
          e.preventDefault();
          setQuery('');
          void runText('', true);
          break;
        default:
          break;
      }
    },
    [activeIndex, moveActive, open, query, runText, hits],
  );

  const shown = hits.slice(0, limit);
  const metaLine = resp
    ? `${labels.search.resultCount(resp.total)} · ${labels.search.timing(resp.keywordMs, resp.semanticMs, resp.semanticActive)}`
    : null;

  const modelHint = semantic && !semanticReady ? labels.search.semanticUnavailable : null;

  /** Whether the applied media query came from a picture or a clip — decides the wording. */
  const activeMedia = imageActive ? (mediaKindOf(imageActive)?.kind ?? 'image') : null;

  const clearFilters = useCallback(() => {
    setKinds([]);
    setRootId(null);
    setSizeIdx(0);
    setDateIdx(0);
  }, []);

  return (
    <section className="search">
      {dropping ? <div className="drop-hint">{labels.search.mediaDropHint}</div> : null}
      <div className="search-top">
        <div className="search-input-wrap">
          <span className="search-input-icon">
            <SearchIcon size={15} />
          </span>
          <input
            ref={inputRef}
            className="search-input"
            type="text"
            value={query}
            placeholder={labels.search.placeholder}
            spellCheck={false}
            autoComplete="off"
            aria-label={labels.search.placeholder}
            onChange={(e) => onQueryChange(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'ArrowDown') {
                e.preventDefault();
                moveActive(1);
              } else if (e.key === 'Escape' && !query) {
                inputRef.current?.blur();
              } else {
                onListKeyDown(e);
              }
            }}
          />
          {query ? (
            <button
              type="button"
              className="search-input-clear"
              title={labels.common.clear}
              onClick={() => {
                setQuery('');
                void runText('', true);
                inputRef.current?.focus();
              }}
            >
              <CloseIcon size={13} />
            </button>
          ) : null}
          <span className={`kbd kbd-${isMac() ? 'cmd' : 'ctrl'}`}>{isMac() ? '⌘K' : 'Ctrl K'}</span>
        </div>

        <div className="search-actions">
          <Chip active={semantic} onClick={() => setSemantic((v) => !v)} title={labels.search.semanticHint}>
            {semantic ? labels.search.semanticOn : labels.search.semanticOff}
          </Chip>
          <Select
            ariaLabel={labels.search.resultLimit}
            value={String(limit)}
            onChange={(v) => setLimit(Number(v))}
            options={LIMITS.map((n) => ({ value: String(n), label: `${labels.search.resultLimit} ${n}` }))}
          />
        </div>
      </div>

      <div className="filters">
        <div className="filter-group">
          <span className="filter-label">{labels.search.filterKinds}</span>
          {kindOrder.map((k) => (
            <Chip
              key={k}
              active={kinds.includes(k)}
              title={labels.kinds[k]}
              onClick={() =>
                setKinds((prev) => (prev.includes(k) ? prev.filter((x) => x !== k) : [...prev, k]))
              }
            >
              {labels.kinds[k]}
            </Chip>
          ))}
        </div>

        <div className="filter-group">
          <span className="filter-label">{labels.search.filterRoot}</span>
          <Select
            ariaLabel={labels.search.filterRoot}
            value={rootId == null ? 'all' : String(rootId)}
            onChange={(v) => setRootId(v === 'all' ? null : Number(v))}
            options={[
              { value: 'all', label: labels.common.all },
              ...roots.map((r) => ({ value: String(r.id), label: r.label || r.path })),
            ]}
          />
        </div>

        <div className="filter-group">
          <span className="filter-label">{labels.search.filterSize}</span>
          <Select
            ariaLabel={labels.search.filterSize}
            value={String(sizeIdx)}
            onChange={(v) => setSizeIdx(Number(v))}
            options={labels.search.sizeOptions.map((t, i) => ({ value: String(i), label: t }))}
          />
        </div>

        <div className="filter-group">
          <span className="filter-label">{labels.search.filterDate}</span>
          <Select
            ariaLabel={labels.search.filterDate}
            value={String(dateIdx)}
            onChange={(v) => setDateIdx(Number(v))}
            options={labels.search.dateOptions.map((t, i) => ({ value: String(i), label: t }))}
          />
        </div>

        {filterCount > 0 ? (
          <button type="button" className="filter-clear" onClick={clearFilters}>
            {labels.search.activeFilters(filterCount)} · {labels.search.filterClear}
          </button>
        ) : null}
      </div>

      <div className="image-search">
        <span className="filter-label">
          {activeMedia === 'video' ? labels.search.videoModeTitle : labels.search.imageModeTitle}
        </span>
        <input
          className="input input-mono image-path"
          type="text"
          value={imagePath}
          placeholder={labels.search.imagePathPlaceholder}
          spellCheck={false}
          aria-label={labels.search.imagePathPlaceholder}
          onChange={(e) => setImagePath(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && imagePath.trim()) void runMedia(imagePath.trim(), kinds.length > 0);
          }}
        />
        <Btn
          onClick={() => {
            if (imagePath.trim()) void runMedia(imagePath.trim(), kinds.length > 0);
          }}
          disabled={!imagePath.trim() || loading}
        >
          {labels.search.imageSearchButton}
        </Btn>
        {mode === 'image' ? (
          <button
            type="button"
            className="filter-clear"
            onClick={() => {
              setImageActive(null);
              setMode('text');
              void runText(query, true);
            }}
          >
            {activeMedia === 'video' ? labels.search.videoModeClear : labels.search.imageModeClear}
          </button>
        ) : null}
      </div>

      {note ? <Note>{note}</Note> : null}
      {modelHint ? <Note>{modelHint}</Note> : null}
      {error ? <Note tone="error">{error}</Note> : null}

      <div className="results-head">
        <span className="results-count">
          {mode === 'image'
            ? activeMedia === 'video'
              ? labels.search.videoModeTitle
              : labels.search.imageModeTitle
            : query.trim()
              ? `“${query.trim()}”`
              : labels.search.recentFilesTitle}
        </span>
        {metaLine ? <span className="results-meta mono">{metaLine}</span> : null}
        {loading ? <span className="results-loading">{labels.search.loading}</span> : null}
      </div>

      <div className="results" tabIndex={0} onKeyDown={onListKeyDown} role="listbox" aria-label={labels.search.placeholder}>
        {shown.length === 0 ? (
          loading ? (
            <EmptyState title={labels.search.loading} hint={query ? undefined : labels.search.emptyIntro} />
          ) : query.trim() || mode === 'image' ? (
            <EmptyState title={labels.search.emptyNoResult} hint={labels.search.emptyNoResultHint} />
          ) : (
            <EmptyState title={labels.search.emptyNoResult} hint={labels.search.emptyIntro} />
          )
        ) : (
          shown.map((hit, i) => (
            <ResultRow
              key={`${hit.fileId}-${i}`}
              hit={hit}
              index={i}
              selected={selectedFileId === hit.fileId}
              handlers={handlers}
            />
          ))
        )}
      </div>

      {resp && shown.length > 0 && resp.total > shown.length ? (
        <div className="results-foot">
          <span className="muted">
            {labels.search.resultCount(resp.total)} · {formatMillis(resp.keywordMs + resp.semanticMs)}
          </span>
        </div>
      ) : null}
    </section>
  );
}
