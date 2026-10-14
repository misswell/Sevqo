// 模型下载的一行入口：谁看到「语义检索不可用」，谁下面就该能直接点下载，
// 所以这个组件自带状态订阅，可以原样放进 Spotlight、搜索结果提示和设置里。
import { useEffect, type ReactElement } from 'react';

import { formatBytes, formatPercent, formatSpeed } from '../format';
import { labels } from '../i18n';
import { useModel } from '../useModel';
import { DownloadIcon } from './Icons';
import { Bar, Btn, Note } from './ui';

export interface ModelPromptProps {
  /** 紧凑版：贴在提示行下面，少留白。 */
  compact?: boolean;
  /** 内容高度变了要通知宿主：Spotlight 按内容重算窗口高度。 */
  onChange?: () => void;
}

export function ModelPrompt({ compact = false, onChange }: ModelPromptProps): ReactElement | null {
  const { model, download, note, error, busy, incomplete, downloading, start, cancel } = useModel();
  const status = model?.status ?? null;
  const partial = !!status && status.present > 0;
  const needPython = !!model && model.status.complete && !model.hasPython;

  useEffect(() => {
    onChange?.();
  }, [onChange, model, download, note, error, incomplete, downloading, needPython]);

  if (!model) return null;
  // 权重齐了、也没卡在缺 Python，就没有要提醒的事。
  if (!incomplete && !downloading && !needPython) return null;

  const detail = status
    ? partial
      ? labels.model.remaining(status.missing.length, formatBytes(status.bytesOnDisk))
      : labels.model.size(status.totalFiles, formatBytes(status.totalBytes))
    : '';

  return (
    <div className={`model-prompt${compact ? ' model-prompt-compact' : ''}`}>
      <div className="model-prompt-line">
        <span className="model-prompt-text">
          <DownloadIcon size={13} />
          <span>{labels.model.downloadPrompt}</span>
          <span className="muted mono">{detail}</span>
        </span>
        {downloading ? (
          <Btn small onClick={() => void cancel()} disabled={busy}>
            {labels.model.cancelDownload}
          </Btn>
        ) : (
          <Btn small tone="primary" onClick={() => void start()} disabled={busy}>
            {partial ? labels.model.resumeDownload : labels.model.oneClick}
          </Btn>
        )}
      </div>

      {download ? (
        <div className="model-prompt-progress">
          <div className="download-line">
            <span className="download-file" title={download.file}>
              {download.file}
            </span>
            <span className="muted mono">
              {download.fileIndex}/{download.fileCount} · {formatPercent(download.doneBytes, download.totalBytes)}
            </span>
          </div>
          <Bar done={download.doneBytes} total={download.totalBytes} indeterminate={download.totalBytes <= 0} />
          <div className="download-line">
            <span className="muted mono">
              {formatBytes(download.overallBytes)} / {formatBytes(download.overallTotal)} ·{' '}
              {formatSpeed(download.bytesPerSecond)}
            </span>
            {download.endpoint ? <span className="muted download-endpoint">{download.endpoint}</span> : null}
          </div>
        </div>
      ) : null}

      {needPython && !downloading ? <Note>{labels.model.missingPythonHint}</Note> : null}
      {note && !downloading ? <Note>{note}</Note> : null}
      {error ? <Note tone="error">{error}</Note> : null}
    </div>
  );
}
