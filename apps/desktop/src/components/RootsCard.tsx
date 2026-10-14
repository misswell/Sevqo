import { useState, type ReactElement } from 'react';

import type { Root } from '../types';
import { labels } from '../i18n';
import { relativeFromNs } from '../format';
import { FolderIcon, PlusIcon, RefreshIcon, TrashIcon } from './Icons';
import { Btn, EmptyState, SectionTitle, TextInput } from './ui';

export interface RootsCardProps {
  roots: Root[];
  busy: boolean;
  onAdd: (path: string, label?: string) => Promise<void>;
  onRemove: (root: Root) => Promise<void>;
  onToggle: (root: Root, enabled: boolean) => Promise<void>;
  onRescanOne: (rootId: number) => Promise<void>;
}

export function RootsCard({ roots, busy, onAdd, onRemove, onToggle, onRescanOne }: RootsCardProps): ReactElement {
  const [path, setPath] = useState('');
  const [name, setName] = useState('');

  const submit = async () => {
    const trimmed = path.trim();
    if (!trimmed) return;
    await onAdd(trimmed, name.trim() || undefined);
    setPath('');
    setName('');
  };

  return (
    <section className="card">
      <SectionTitle right={<span className="muted mono">{roots.length}</span>}>{labels.sidebar.scope}</SectionTitle>

      {roots.length === 0 ? <EmptyState title={labels.roots.empty} hint={labels.roots.emptyHint} /> : null}

      <ul className="root-list">
        {roots.map((root) => (
          <li key={root.id} className={`root-item${root.enabled ? '' : ' root-item-off'}`}>
            <label className="root-check" title={root.enabled ? labels.common.enabled : labels.common.disabled}>
              <input
                type="checkbox"
                checked={root.enabled}
                disabled={busy}
                onChange={(e) => void onToggle(root, e.target.checked)}
              />
            </label>
            <span className="root-glyph">
              <FolderIcon size={14} />
            </span>
            <span className="root-text">
              <span className="root-label" title={root.path}>
                {root.label || root.path}
              </span>
              <span className="root-sub" title={root.path}>
                {root.path}
              </span>
              <span className="root-time">
                {root.lastScanNs > 0
                  ? `${labels.roots.lastScan} ${relativeFromNs(root.lastScanNs)}`
                  : labels.roots.neverScanned}
              </span>
            </span>
            <span className="root-actions">
              <button
                type="button"
                className="icon-btn"
                title={busy ? labels.roots.busy : labels.index.rescanAll}
                disabled={busy}
                onClick={() => void onRescanOne(root.id)}
              >
                <RefreshIcon size={13} />
              </button>
              <button
                type="button"
                className="icon-btn icon-btn-danger"
                title={labels.common.remove}
                disabled={busy}
                onClick={() => void onRemove(root)}
              >
                <TrashIcon size={13} />
              </button>
            </span>
          </li>
        ))}
      </ul>

      <div className="root-add">
        <TextInput
          value={path}
          onChange={setPath}
          placeholder={labels.roots.pathPlaceholder}
          mono
          onEnter={() => void submit()}
          ariaLabel={labels.roots.pathPlaceholder}
        />
        <div className="root-add-row">
          <TextInput
            value={name}
            onChange={setName}
            placeholder={labels.roots.labelPlaceholder}
            onEnter={() => void submit()}
            ariaLabel={labels.roots.labelPlaceholder}
          />
          <Btn tone="primary" onClick={() => void submit()} disabled={busy || !path.trim()}>
            <PlusIcon size={13} />
            <span>{labels.roots.add}</span>
          </Btn>
        </div>
      </div>
    </section>
  );
}
