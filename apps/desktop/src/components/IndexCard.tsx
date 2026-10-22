import { useState, type ReactElement } from 'react';

import type { Overview, Phase, Progress, WatchChange } from '../types';
import { kindOrder, labels } from '../i18n';
import { formatBytes, formatNumber, formatSeconds, shortenPath } from '../format';
import { PauseIcon, PlayIcon, StopIcon } from './Icons';
import { Badge, Bar, Btn, NumberInput, SectionTitle, Stat } from './ui';

export interface IndexCardProps {
  progress: Progress;
  overview: Overview | null;
  changes: WatchChange[];
  busy: boolean;
  onPause: () => Promise<void>;
  onResume: () => Promise<void>;
  onCancel: () => Promise<void>;
  onRescan: () => Promise<void>;
  onStart: (mode: 'full' | 'text', maxFiles: number | null) => Promise<void>;
  onRebuild: (clearVectors: boolean) => Promise<void>;
  onBackfill: () => Promise<void>;
  onCompact: () => Promise<void>;
  onRestartWatch: () => Promise<void>;
}

const phaseTone: Record<Phase, 'neutral' | 'ok' | 'warn' | 'error' | 'accent'> = {
  idle: 'neutral',
  scanning: 'accent',
  indexing: 'accent',
  watching: 'ok',
  paused: 'warn',
  failed: 'error',
};

export function IndexCard({
  progress,
  overview,
  changes,
  busy,
  onPause,
  onResume,
  onCancel,
  onRescan,
  onStart,
  onRebuild,
  onBackfill,
  onCompact,
  onRestartWatch,
}: IndexCardProps): ReactElement {
  const [maxFiles, setMaxFiles] = useState<number | null>(null);

  const running = progress.phase === 'indexing' || progress.phase === 'scanning';
  const paused = progress.phase === 'paused';
  // 暂停只是让索引线程睡着，许可仍被它握着：这时再点开始/重建只会被后端拒绝。
  const working = running || paused;
  const processed = progress.done + progress.failed + progress.skipped;
  const total = progress.totalEstimate > 0 ? progress.totalEstimate : processed + progress.queued;
  const pct = total > 0 ? Math.min(100, Math.round((processed / total) * 100)) : 0;

  return (
    <section className="card">
      <SectionTitle right={<Badge tone={phaseTone[progress.phase]}>{labels.index.phase[progress.phase]}</Badge>}>
        {labels.sidebar.index}
      </SectionTitle>

      <div className="prog">
        <Bar done={processed} total={total} indeterminate={running && total === 0} />
        <div className="prog-line">
          <span className="mono">
            {formatNumber(processed)} / {formatNumber(total)}
          </span>
          <span className="muted mono">{total > 0 ? `${pct}%` : labels.index.queueEmpty}</span>
        </div>
        {progress.current ? (
          <p className="prog-current" title={progress.current}>
            {labels.index.current}：{shortenPath(progress.current, 8, 34)}
          </p>
        ) : null}
        {progress.message ? <p className="prog-message">{progress.message}</p> : null}
        <div className="prog-stats">
          <span title={labels.index.discovered}>
            {labels.index.discovered} <span className="mono">{formatNumber(progress.discovered)}</span>
          </span>
          <span title={labels.index.queued}>
            {labels.index.queued} <span className="mono">{formatNumber(progress.queued)}</span>
          </span>
          <span title={labels.index.failed}>
            {labels.index.failed} <span className="mono">{formatNumber(progress.failed)}</span>
          </span>
          <span title={labels.index.skipped}>
            {labels.index.skipped} <span className="mono">{formatNumber(progress.skipped)}</span>
          </span>
          {progress.imagesPerMin > 0 ? (
            <span title={labels.index.rate}>
              <span className="mono">{Math.round(progress.imagesPerMin)}</span> {labels.index.rateUnit}
            </span>
          ) : null}
          {progress.elapsedS > 0 ? (
            <span title={labels.index.elapsed}>
              <span className="mono">{formatSeconds(progress.elapsedS)}</span>
            </span>
          ) : null}
        </div>
      </div>

      <div className="btn-grid">
        <Btn small onClick={() => void onPause()} disabled={!running || busy} title={labels.index.pause}>
          <PauseIcon size={12} />
          <span>{labels.index.pause}</span>
        </Btn>
        <Btn small onClick={() => void onResume()} disabled={!paused || busy} title={labels.index.resume}>
          <PlayIcon size={12} />
          <span>{labels.index.resume}</span>
        </Btn>
        <Btn
          small
          tone="danger"
          onClick={() => void onCancel()}
          disabled={!working || busy}
          title={labels.index.cancel}
        >
          <StopIcon size={12} />
          <span>{labels.index.cancel}</span>
        </Btn>
        <Btn small onClick={() => void onRescan()} disabled={busy || working}>
          {labels.index.scan}
        </Btn>
        <Btn small onClick={() => void onStart('text', maxFiles)} disabled={busy || working}>
          {labels.index.textPass}
        </Btn>
        <Btn small tone="primary" onClick={() => void onStart('full', maxFiles)} disabled={busy || working}>
          {labels.index.fullPass}
        </Btn>
      </div>

      <label className="inline-field">
        <span>{labels.index.maxFiles}</span>
        <NumberInput value={maxFiles ?? ''} onChange={(v) => setMaxFiles(v)} min={1} placeholder="0" />
        <span className="muted">{labels.common.unlimited}</span>
      </label>

      <div className="btn-grid btn-grid-quiet">
        <Btn small tone="ghost" onClick={() => void onRebuild(false)} disabled={busy || working}>
          {labels.index.rebuildText}
        </Btn>
        <Btn small tone="ghost" onClick={() => void onRebuild(true)} disabled={busy || working}>
          {labels.index.rebuildVectors}
        </Btn>
        <Btn small tone="ghost" onClick={() => void onBackfill()} disabled={busy || working}>
          {labels.index.backfill}
        </Btn>
        <Btn small tone="ghost" onClick={() => void onCompact()} disabled={busy || working}>
          {labels.index.compact}
        </Btn>
        <Btn small tone="ghost" onClick={() => void onRestartWatch()} disabled={busy}>
          {labels.index.watchRestart}
        </Btn>
      </div>

      {overview ? (
        <>
          <div className="stat-grid">
            <Stat label={labels.index.statsFiles} value={formatNumber(overview.files)} />
            <Stat label={labels.index.statsSize} value={formatBytes(overview.bytes)} />
            <Stat label={labels.index.statsVectors} value={formatNumber(overview.vectors)} />
            <Stat label={labels.index.statsClips} value={formatNumber(overview.clips)} />
            <Stat label={labels.index.statsChunks} value={formatNumber(overview.chunks)} />
            <Stat label={labels.index.statsDead} value={formatNumber(overview.deadRows)} />
          </div>
          <div className="kind-usage">
            <span className="muted">{labels.index.byKind}</span>
            {kindOrder.map((k) => (
              <span key={k} className="kind-usage-item">
                <span className={`kind-dot kind-dot-${k}`} />
                {labels.kinds[k]} <span className="mono">{formatNumber(overview.byKind[k] ?? 0)}</span>
              </span>
            ))}
          </div>
        </>
      ) : (
        <p className="muted">{labels.common.loading}</p>
      )}

      {changes.length > 0 ? (
        <div className="watch-log">
          <span className="muted">{labels.index.watchLog}</span>
          <ul>
            {changes.map((c, i) => (
              <li key={`${c.path}-${i}`} title={c.path}>
                <span className={`applied applied-${c.applied}`}>{labels.index.applied[c.applied]}</span>
                <span className="watch-path">{shortenPath(c.path, 6, 26)}</span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </section>
  );
}
