import { useEffect, useMemo, useState, type ReactElement } from 'react';

import { api, errorText } from '../api';
import type { FileRecord, Hit, Moment, Root } from '../types';
import { labels } from '../i18n';
import { formatBytes, formatDurationMs, formatNsAbsolute, formatScore, relativeFromNs } from '../format';
import { useClipThumb, useInView, useMoments, useThumb } from '../hooks';
import { KindIcon } from './Icons';
import { Badge, Btn, EmptyState, Note, Stat } from './ui';

function MomentRow({
  fileId,
  moment,
  onPlay,
}: {
  fileId: number;
  moment: Moment;
  onPlay: () => void;
}): ReactElement {
  const [ref, seen] = useInView<HTMLButtonElement>();
  const thumb = useClipThumb(seen ? fileId : null, seen ? moment.clipId : null);
  return (
    <button ref={ref} type="button" className="moment-row" onClick={onPlay} title={labels.detail.playFrom}>
      <span className="moment-thumb">
        {thumb ? <img src={thumb} alt="" loading="lazy" decoding="async" /> : <span className="moment-thumb-empty" />}
      </span>
      <span className="moment-time mono">{formatDurationMs(moment.startMs)}</span>
      <span className="moment-range mono">
        {formatDurationMs(moment.startMs)}–{formatDurationMs(moment.endMs)}
      </span>
      {moment.score > 0 ? <span className="moment-score mono">{formatScore(moment.score)}</span> : null}
    </button>
  );
}

export function DetailPane({
  hit,
  roots,
  onMoment,
}: {
  hit: Hit | null;
  roots: Root[];
  onMoment: (hit: Hit, moment: Moment) => void;
}): ReactElement {
  const [record, setRecord] = useState<FileRecord | null>(null);
  const [error, setError] = useState<string | null>(null);
  const fileId = hit?.fileId ?? null;
  const timeline = useMoments(fileId);
  // 本次查询命中的片段来自 Hit.moments（带真实相似度，服务端已按相关度排序）；
  // 完整时间轴另取一次，按时间排序、不带分数（P2-02）。
  const hitMoments = useMemo(
    () => [...(hit?.moments ?? [])].sort((a, b) => b.score - a.score),
    [hit],
  );
  const [previewRef, previewSeen] = useInView<HTMLDivElement>();
  const thumb = useThumb(fileId, previewSeen);

  useEffect(() => {
    let alive = true;
    setError(null);
    setRecord(null);
    if (fileId == null) return;
    void api
      .fileInfo(fileId)
      .then((r) => {
        if (alive) setRecord(r);
      })
      .catch((e) => {
        if (alive) setError(errorText(e));
      });
    return () => {
      alive = false;
    };
  }, [fileId]);

  if (!hit) {
    return (
      <aside className="pane">
        <EmptyState title={labels.detail.title} hint={labels.detail.empty} />
      </aside>
    );
  }

  const open = (reveal: boolean) => {
    const fn = reveal ? api.revealInFolder : api.openFile;
    fn(hit.path).catch((e) => setError(errorText(e)));
  };

  const root = roots.find((r) => r.id === record?.rootId) ?? null;
  const showsMoments = hit.kind === 'video' || hitMoments.length > 0 || (timeline?.length ?? 0) > 0;

  return (
    <aside className="pane">
      <div className="pane-scroll">
        <div className="pane-preview" ref={previewRef}>
          {thumb ? (
            <img src={thumb} alt="" loading="lazy" decoding="async" />
          ) : (
            <div className="pane-preview-empty">
              <span className={`kind-${hit.kind}`}>
                <KindIcon kind={hit.kind} size={26} />
              </span>
              <span className="pane-preview-hint">
                {hit.kind === 'image' || hit.kind === 'video' ? labels.detail.noThumb : labels.kinds[hit.kind]}
              </span>
            </div>
          )}
        </div>

        <h2 className="pane-name" title={hit.path}>
          {hit.name}
        </h2>
        <p className="pane-path" title={hit.path}>
          {hit.path}
        </p>

        <div className="pane-actions">
          <Btn tone="primary" full onClick={() => open(false)}>
            {labels.detail.openFile}
          </Btn>
          <Btn full onClick={() => open(true)}>
            {labels.detail.openFolder}
          </Btn>
        </div>

        <div className="pane-badges">
          <Badge tone="accent">{labels.kinds[hit.kind]}</Badge>
          <Badge tone={hit.channel === 'both' ? 'ok' : 'neutral'}>{labels.channels[hit.channel]}</Badge>
          {record ? (
            <Badge tone={record.state === 'done' ? 'ok' : record.state === 'failed' ? 'error' : 'warn'}>
              {labels.fileStates[record.state]}
            </Badge>
          ) : null}
          {hit.keywordRank != null ? <Badge>{`#${hit.keywordRank + 1}`}</Badge> : null}
          {hit.semanticRank != null ? <Badge>{`语义 #${hit.semanticRank + 1}`}</Badge> : null}
        </div>

        {hit.snippet ? <p className="pane-snippet">{hit.snippet}</p> : null}

        {showsMoments ? (
          <section className="pane-block">
            <header className="pane-block-title">{labels.detail.moments}</header>
            {hitMoments.length > 0 ? (
              <div className="moment-list">
                {hitMoments.slice(0, 12).map((m) => (
                  <MomentRow key={`hit-${m.clipId}-${m.startMs}`} fileId={hit.fileId} moment={m} onPlay={() => onMoment(hit, m)} />
                ))}
              </div>
            ) : timeline == null ? (
              <p className="muted">{labels.common.loading}</p>
            ) : (
              <p className="muted">{labels.detail.momentsEmpty}</p>
            )}
          </section>
        ) : null}

        {showsMoments && (timeline?.length ?? 0) > 0 ? (
          <section className="pane-block">
            <header className="pane-block-title">{labels.detail.timeline}</header>
            <p className="muted">{labels.detail.timelineHint}</p>
            <div className="moment-list">
              {[...(timeline ?? [])]
                .sort((a, b) => a.startMs - b.startMs)
                .slice(0, 12)
                .map((m) => (
                  <MomentRow key={`tl-${m.clipId}-${m.startMs}`} fileId={hit.fileId} moment={m} onPlay={() => onMoment(hit, m)} />
                ))}
            </div>
          </section>
        ) : null}

        <section className="pane-block">
          <header className="pane-block-title">{labels.detail.title}</header>
          <div className="pane-stats">
            <Stat label={labels.detail.kind} value={labels.kinds[hit.kind]} />
            <Stat label={labels.detail.size} value={formatBytes(hit.size)} />
            <Stat label={labels.detail.mtime} value={`${relativeFromNs(hit.mtimeNs)}`} />
            <Stat label={labels.detail.score} value={formatScore(hit.score)} />
            {record?.durationMs != null ? (
              <Stat label={labels.detail.duration} value={formatDurationMs(record.durationMs)} />
            ) : null}
            {record?.width && record?.height ? (
              <Stat label={labels.detail.resolution} value={`${record.width} × ${record.height}`} />
            ) : null}
            {record?.ext ? <Stat label={labels.detail.ext} value={record.ext} /> : null}
            {root ? <Stat label={labels.detail.root} value={root.label || root.path} /> : null}
            {record && record.indexedNs > 0 ? (
              <Stat label={labels.detail.indexed} value={formatNsAbsolute(record.indexedNs)} />
            ) : null}
          </div>
        </section>

        {error ? <Note tone="error">{error}</Note> : null}
        {record?.error ? <Note tone="error">{record.error}</Note> : null}
      </div>
    </aside>
  );
}
