import { useState, type ReactElement } from 'react';

import { api, errorText } from '../api';
import type { DownloadProgress, ModelInfo, ModelStatePayload, Runtime } from '../types';
import { labels } from '../i18n';
import { formatBytes, formatFixed, formatPercent, formatSeconds, formatSpeed } from '../format';
import { DownloadIcon } from './Icons';
import { Badge, Bar, Btn, Note, SectionTitle, Stat } from './ui';

export interface ModelCardProps {
  model: ModelInfo | null;
  download: DownloadProgress | null;
  modelState: ModelStatePayload | null;
  busy: boolean;
  onDownload: () => Promise<void>;
  onCancelDownload: () => Promise<void>;
  onVerify: () => Promise<void>;
}

function statusOf(model: ModelInfo | null, download: DownloadProgress | null): {
  text: string;
  tone: 'neutral' | 'ok' | 'warn' | 'error' | 'accent';
} {
  if (!model) return { text: labels.common.loading, tone: 'neutral' };
  if (model.downloading || download) return { text: labels.model.downloading, tone: 'accent' };
  if (model.ready) return { text: labels.model.ready, tone: 'ok' };
  // 权重没齐时先说权重：下载不需要 Python，报「缺 Python」会把人支到错误的那一步。
  if (!model.status.complete) return { text: labels.model.incomplete, tone: 'neutral' };
  if (!model.hasPython) return { text: labels.model.missingPython, tone: 'warn' };
  return { text: labels.model.notReady, tone: 'warn' };
}

export function ModelCard({
  model,
  download,
  modelState,
  busy,
  onDownload,
  onCancelDownload,
  onVerify,
}: ModelCardProps): ReactElement {
  const [runtime, setRuntime] = useState<Runtime | null>(null);
  const [error, setError] = useState<string | null>(null);

  const loadRuntime = async () => {
    setError(null);
    try {
      setRuntime(await api.modelRuntime());
    } catch (e) {
      const text = errorText(e);
      setError(text.includes('未就绪') || text.includes('not') ? `${text}（${labels.flash.runtimeMissing}）` : text);
    }
  };

  const st = statusOf(model, download);
  const status = model?.status ?? null;
  const showOverall = !!download && download.overallTotal > 0;

  return (
    <section className="card">
      <SectionTitle right={<Badge tone={st.tone}>{st.text}</Badge>}>{labels.sidebar.model}</SectionTitle>

      {status ? (
        <div className="stat-grid">
          <Stat
            label={labels.model.files}
            value={`${status.present} / ${status.totalFiles}`}
          />
          <Stat label={labels.common.bytes} value={`${formatBytes(status.bytesOnDisk)} / ${formatBytes(status.totalBytes)}`} />
          <Stat
            label={status.complete ? labels.model.complete : labels.model.incomplete}
            value={status.complete ? labels.model.ready : formatPercent(status.bytesOnDisk, status.totalBytes)}
          />
          {status.resumeBytes > 0 ? (
            <Stat label={labels.model.resume} value={formatBytes(status.resumeBytes)} />
          ) : null}
        </div>
      ) : (
        <p className="muted">{labels.common.loading}</p>
      )}

      {status && status.missing.length > 0 ? (
        <p className="model-files" title={status.missing.join('\n')}>
          {labels.model.missing}：{status.missing.slice(0, 2).join('、')}
          {status.missing.length > 2 ? ` 等 ${status.missing.length} 个` : ''}
        </p>
      ) : null}
      {status && status.mismatched.length > 0 ? (
        <p className="model-files model-files-warn">{labels.model.mismatched}：{status.mismatched.length}</p>
      ) : null}

      {download ? (
        <div className="download">
          <div className="download-line">
            <span className="download-file" title={download.file}>
              {download.file}
            </span>
            <span className="muted mono">
              {download.fileIndex}/{download.fileCount}
            </span>
          </div>
          <Bar done={download.doneBytes} total={download.totalBytes} indeterminate={download.totalBytes <= 0} />
          {showOverall ? (
            <>
              <div className="download-line">
                <span className="muted">{labels.model.files}</span>
                <span className="muted mono">
                  {formatBytes(download.overallBytes)} / {formatBytes(download.overallTotal)}
                </span>
              </div>
              <Bar done={download.overallBytes} total={download.overallTotal} tone="warn" />
            </>
          ) : null}
          <div className="download-meta">
            <span title={labels.model.speed}>
              <span className="mono">{formatSpeed(download.bytesPerSecond)}</span>
            </span>
            <span title={labels.model.endpoint} className="download-endpoint">
              {download.endpoint || labels.common.unknown}
            </span>
          </div>
          {download.note ? <p className="muted">{download.note}</p> : null}
        </div>
      ) : null}

      <div className="btn-grid">
        <Btn small tone="primary" onClick={() => void onDownload()} disabled={busy || !!model?.downloading}>
          <DownloadIcon size={12} />
          <span>{labels.model.download}</span>
        </Btn>
        <Btn small onClick={() => void onCancelDownload()} disabled={busy || !model?.downloading}>
          {labels.model.cancelDownload}
        </Btn>
        <Btn small tone="ghost" onClick={() => void onVerify()} disabled={busy}>
          {labels.model.verify}
        </Btn>
        <Btn small tone="ghost" onClick={() => void loadRuntime()} disabled={busy}>
          {labels.model.runtimeLoad}
        </Btn>
      </div>

      {/* 权重还没齐时先谈权重：下载不需要 Python，这时候喊缺 Python 会把人支到错误的那一步。 */}
      {model && !model.hasPython && model.status.complete ? <Note>{labels.model.missingPythonHint}</Note> : null}
      {modelState?.note && !model?.downloading ? <Note>{modelState.note}</Note> : null}
      {error ? <Note tone="error">{error}</Note> : null}

      {runtime ? (
        <div className="runtime">
          <span className="muted">{labels.model.runtime}</span>
          <div className="stat-grid">
            <Stat label={labels.model.device} value={runtime.device} />
            <Stat label={labels.model.dtype} value={runtime.dtype} />
            <Stat label={labels.model.dim} value={runtime.dim} />
            <Stat label={labels.model.params} value={`${formatFixed(runtime.paramsMillions, 1)} M`} />
            <Stat label={labels.model.loadSeconds} value={formatSeconds(runtime.loadSeconds)} />
            <Stat label={labels.model.threads} value={runtime.threads} />
            <Stat label={labels.model.rss} value={formatBytes(runtime.rssMb * 1024 * 1024)} />
            <Stat
              label={`${labels.model.vision} / ${labels.model.audio}`}
              value={`${runtime.vision ? labels.common.yes : labels.common.no} / ${
                runtime.audio ? labels.common.yes : labels.common.no
              }`}
            />
          </div>
        </div>
      ) : null}

      {model ? (
        <details className="paths">
          <summary>{labels.model.dir}</summary>
          <p className="path mono" title={model.modelDir}>
            {model.modelDir}
          </p>
          <p className="path mono" title={model.dbPath}>
            {labels.model.dbPath}：{model.dbPath}
          </p>
          <p className="path mono" title={model.cacheDir}>
            {labels.model.cacheDir}：{model.cacheDir}
          </p>
          {model.python ? (
            <p className="path mono" title={model.python}>
              {labels.model.python}：{model.python}
            </p>
          ) : null}
          {model.endpoints.length > 0 ? (
            <p className="path mono">
              {labels.model.endpoint}：{model.endpoints.join('、')}
            </p>
          ) : null}
        </details>
      ) : null}
    </section>
  );
}
