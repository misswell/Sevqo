import type { ReactElement } from 'react';

import { labels } from '../i18n';
import { formatBytes, formatNumber } from '../format';
import type { Backend } from '../useBackend';
import { GearIcon } from './Icons';
import { Btn, Note } from './ui';
import { IndexCard } from './IndexCard';
import { ModelCard } from './ModelCard';
import { RootsCard } from './RootsCard';
import { UpdateCard } from './UpdateCard';

export interface SidebarProps {
  backend: Backend;
  onOpenSettings: () => void;
}

export function Sidebar({ backend, onOpenSettings }: SidebarProps): ReactElement {
  const { overview, progress, model, roots, changes, busy, bootError } = backend;
  return (
    <aside className="sidebar">
      <header className="brand">
        <span className="brand-name">{labels.app.name}</span>
        <span className="brand-sub">{labels.app.tagline}</span>
        {overview ? (
          <span className="brand-stats">
            <span className="mono">{formatNumber(overview.files)}</span>
            <span className="brand-stat-label">{labels.index.statsFiles}</span>
            <span className="mono">{formatBytes(overview.bytes)}</span>
            <span className={`dot ${overview.semanticReady ? 'dot-ok' : 'dot-idle'}`} title={labels.model.ready} />
          </span>
        ) : null}
      </header>

      <div className="side-scroll">
        <RootsCard
          roots={roots}
          busy={busy}
          onAdd={backend.addRoot}
          onRemove={backend.removeRoot}
          onToggle={backend.toggleRoot}
          onRescanOne={(id) => backend.rescan(id)}
        />

        <IndexCard
          progress={progress}
          overview={overview}
          changes={changes}
          busy={busy}
          onPause={backend.pause}
          onResume={backend.resume}
          onCancel={backend.cancel}
          onRescan={() => backend.rescan(null)}
          onStart={(mode, maxFiles) => backend.startIndex({ mode, maxFiles })}
          onRebuild={backend.rebuild}
          onBackfill={backend.backfill}
          onCompact={backend.compact}
          onRestartWatch={backend.restartWatch}
        />

        <ModelCard
          model={model}
          download={backend.download}
          modelState={backend.modelState}
          busy={busy}
          onDownload={backend.downloadModel}
          onCancelDownload={backend.cancelDownload}
          onVerify={backend.verifyModel}
        />

        <UpdateCard
          version={backend.version}
          update={backend.update}
          busy={busy || backend.updateBusy}
          onCheck={backend.checkUpdate}
          onInstall={backend.installUpdate}
          onCancel={backend.cancelUpdate}
        />

        {bootError ? <Note tone="error">{bootError}</Note> : null}
      </div>

      <footer className="side-foot">
        <Btn tone="ghost" full onClick={onOpenSettings}>
          <GearIcon size={13} />
          <span>{labels.sidebar.settings}</span>
        </Btn>
      </footer>
    </aside>
  );
}
