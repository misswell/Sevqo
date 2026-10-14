import { memo, type ReactElement } from 'react';

import type { Hit, Moment } from '../types';
import { labels } from '../i18n';
import { formatBytes, formatDurationMs, formatScore, parentDir, relativeFromNs, shortenPath } from '../format';
import { useInView, useThumb } from '../hooks';
import { KindIcon } from './Icons';

export interface RowHandlers {
  onSelect: (hit: Hit) => void;
  onOpen: (hit: Hit, reveal: boolean) => void;
  onMoment: (hit: Hit, moment: Moment) => void;
}

function MomentChip({
  hit,
  moment,
  onMoment,
}: {
  hit: Hit;
  moment: Moment;
  onMoment: RowHandlers['onMoment'];
}): ReactElement {
  return (
    <button
      type="button"
      className="moment-chip mono"
      title={`${labels.detail.playFrom} ${formatDurationMs(moment.startMs)}`}
      onClick={(e) => {
        e.stopPropagation();
        onMoment(hit, moment);
      }}
    >
      {formatDurationMs(moment.startMs)}
    </button>
  );
}

export const ResultRow = memo(function ResultRow({
  hit,
  index,
  selected,
  handlers,
}: {
  hit: Hit;
  index: number;
  selected: boolean;
  handlers: RowHandlers;
}): ReactElement {
  const [ref, inView] = useInView<HTMLDivElement>();
  const thumb = useThumb(hit.fileId, inView);
  const wantsThumb = hit.kind === 'image' || hit.kind === 'video';
  const moments = hit.moments.slice(0, 3);

  return (
    <div
      ref={ref}
      className={`row${selected ? ' row-selected' : ''}`}
      data-index={index}
      role="option"
      aria-selected={selected}
      tabIndex={-1}
      onClick={() => handlers.onSelect(hit)}
      onDoubleClick={(e) => {
        e.preventDefault();
        handlers.onOpen(hit, e.metaKey || e.ctrlKey);
      }}
      onMouseDown={(e) => {
        if (e.button === 1) e.preventDefault();
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        handlers.onOpen(hit, true);
      }}
    >
      <div className={`row-thumb${wantsThumb && thumb ? ' row-thumb-image' : ''}`}>
        {thumb ? (
          <img src={thumb} alt="" loading="lazy" decoding="async" />
        ) : (
          <span className={`row-thumb-icon kind-${hit.kind}`}>
            <KindIcon kind={hit.kind} size={18} />
          </span>
        )}
      </div>

      <div className="row-body">
        <div className="row-title-line">
          <span className="row-name">{hit.name}</span>
          {hit.channel !== 'keyword' ? (
            <span className={`row-channel channel-${hit.channel}`}>{labels.channels[hit.channel]}</span>
          ) : null}
        </div>

        {hit.snippet ? <p className="row-snippet">{hit.snippet}</p> : null}

        <div className="row-meta">
          <span className="row-dir" title={hit.path}>
            {shortenPath(parentDir(hit.path), 8, 34)}
          </span>
          <span className="row-dot">·</span>
          <span>{labels.kinds[hit.kind]}</span>
          <span className="row-dot">·</span>
          <span className="mono">{formatBytes(hit.size)}</span>
          <span className="row-dot">·</span>
          <span>{relativeFromNs(hit.mtimeNs)}</span>
          {hit.durationMs ? (
            <>
              <span className="row-dot">·</span>
              <span className="mono">{formatDurationMs(hit.durationMs)}</span>
            </>
          ) : null}
        </div>

        {moments.length > 0 ? (
          <div className="row-moments">
            <span className="row-moments-label">{labels.detail.moments}</span>
            {moments.map((m) => (
              <MomentChip key={`${m.clipId}-${m.startMs}`} hit={hit} moment={m} onMoment={handlers.onMoment} />
            ))}
          </div>
        ) : null}
      </div>

      <div className="row-side">
        <span className="row-score mono" title={labels.detail.score}>
          {formatScore(hit.score)}
        </span>
      </div>
    </div>
  );
});
