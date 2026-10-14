import type { ReactElement } from 'react';

import type { UpdateEvent, UpdateState } from '../types';
import { labels } from '../i18n';
import { formatBytes } from '../format';
import { DownloadIcon, RefreshIcon } from './Icons';
import { Badge, Bar, Btn, Note, SectionTitle, Stat } from './ui';

type Tone = 'neutral' | 'ok' | 'warn' | 'error' | 'accent';

const tones: Record<UpdateState, Tone> = {
  checking: 'accent',
  upToDate: 'ok',
  available: 'accent',
  downloading: 'accent',
  installing: 'warn',
  installed: 'ok',
  failed: 'error',
};

export interface UpdateCardProps {
  version: string | null;
  update: UpdateEvent | null;
  busy: boolean;
  onCheck: () => Promise<void>;
  onInstall: () => Promise<void>;
  onCancel: () => Promise<void>;
}

export function UpdateCard({
  version,
  update,
  busy,
  onCheck,
  onInstall,
  onCancel,
}: UpdateCardProps): ReactElement {
  const state = update?.state ?? null;
  const info = update?.info ?? null;
  const busyState = state === 'checking' || state === 'downloading' || state === 'installing';
  const transferring = state === 'downloading' || state === 'installing';
  const total = update?.total ?? 0;

  return (
    <section className="card">
      <SectionTitle
        right={
          <Badge tone={state ? tones[state] : 'neutral'}>
            {state ? labels.update.state[state] : labels.update.state.idle}
          </Badge>
        }
      >
        {labels.update.title}
      </SectionTitle>

      <div className="stat-grid">
        <Stat label={labels.update.current} value={version ?? labels.common.loading} />
        {info && state !== 'upToDate' ? (
          <Stat label={labels.update.latest} value={info.version} />
        ) : null}
        {info?.publishedAt ? (
          <Stat label={labels.update.published} value={info.publishedAt.slice(0, 10)} />
        ) : null}
      </div>

      {transferring ? (
        <div className="download">
          <div className="download-line">
            <span className="muted">
              {state === 'installing' ? labels.update.state.installing : labels.update.bytes}
            </span>
            <span className="muted mono">
              {total > 0
                ? `${formatBytes(update?.downloaded ?? 0)} / ${formatBytes(total)} · ${update?.percent ?? 0}%`
                : formatBytes(update?.downloaded ?? 0)}
            </span>
          </div>
          <Bar done={update?.downloaded ?? 0} total={total} indeterminate={total <= 0} />
        </div>
      ) : null}

      <div className="btn-grid">
        <Btn small tone="primary" onClick={() => void onCheck()} disabled={busy || busyState}>
          <RefreshIcon size={12} />
          <span>{labels.update.check}</span>
        </Btn>
        <Btn small onClick={() => void onInstall()} disabled={busy || state !== 'available'}>
          <DownloadIcon size={12} />
          <span>{labels.update.install}</span>
        </Btn>
        <Btn small tone="ghost" onClick={() => void onCancel()} disabled={!transferring}>
          {labels.common.cancel}
        </Btn>
      </div>

      {state === 'available' ? <Note>{labels.update.signatureHint}</Note> : null}
      {state === 'installed' ? <Note>{labels.update.installedHint}</Note> : null}
      {update?.error ? <Note tone="error">{update.error}</Note> : null}

      {info?.notes ? (
        <details className="paths">
          <summary>{labels.update.notes}</summary>
          <p className="update-notes">{info.notes}</p>
        </details>
      ) : null}
    </section>
  );
}
