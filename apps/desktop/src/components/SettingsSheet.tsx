import { useCallback, useEffect, useMemo, useState, type ReactElement } from 'react';

import type { Settings } from '../types';
import { labels } from '../i18n';
import { CloseIcon } from './Icons';
import { Btn, Note, SectionTitle, TextInput, Toggle } from './ui';

const MB = 1024 * 1024;

interface Draft {
  indexImages: boolean;
  indexVideo: boolean;
  indexDocuments: boolean;
  indexCode: boolean;
  resumeOnLaunch: boolean;
  vectorDim: string;
  inferThreads: string;
  inferBatch: string;
  ramBudgetMb: string;
  videoWindowS: string;
  videoStepS: string;
  videoFps: string;
  maxTextStoreKb: string;
  maxExtractMb: string;
  maxFileMb: string;
  modelRepo: string;
  modelEndpoints: string;
  inferPython: string;
  netProxy: string;
  updateCheck: boolean;
  excludeDirs: string;
  excludePathPrefixes: string;
  globalShortcut: string;
}

function toDraft(s: Settings): Draft {
  return {
    indexImages: s.indexImages,
    indexVideo: s.indexVideo,
    indexDocuments: s.indexDocuments,
    indexCode: s.indexCode,
    resumeOnLaunch: s.resumeOnLaunch,
    vectorDim: String(s.vectorDim),
    inferThreads: String(s.inferThreads),
    inferBatch: String(s.inferBatch),
    ramBudgetMb: String(s.ramBudgetMb),
    videoWindowS: String(s.videoWindowS),
    videoStepS: String(s.videoStepS),
    videoFps: String(s.videoFps),
    maxTextStoreKb: String(s.maxTextStoreKb),
    maxExtractMb: String(Math.round((s.maxExtractBytes / MB) * 10) / 10),
    maxFileMb: String(Math.round((s.maxFileBytes / MB) * 10) / 10),
    modelRepo: s.modelRepo,
    modelEndpoints: s.modelEndpoints.join('\n'),
    inferPython: s.inferPython ?? '',
    netProxy: s.netProxy ?? '',
    updateCheck: s.updateCheck,
    excludeDirs: s.excludeDirs.join('\n'),
    excludePathPrefixes: s.excludePathPrefixes.join('\n'),
    globalShortcut: s.globalShortcut,
  };
}

function num(raw: string, min: number): number | null {
  const t = raw.trim();
  if (!t) return null;
  const v = Number(t);
  if (!Number.isFinite(v) || v < min) return null;
  return v;
}

function list(raw: string): string[] {
  return raw
    .split('\n')
    .map((s) => s.trim())
    .filter(Boolean);
}

const numericFields: Array<{ key: keyof Draft; label: string; min: number }> = [
  { key: 'vectorDim', label: labels.settings.vectorDim, min: 1 },
  { key: 'inferThreads', label: labels.settings.inferThreads, min: 0 },
  { key: 'inferBatch', label: labels.settings.inferBatch, min: 1 },
  { key: 'ramBudgetMb', label: labels.settings.ramBudgetMb, min: 1 },
  { key: 'videoWindowS', label: labels.settings.videoWindowS, min: 0.1 },
  { key: 'videoStepS', label: labels.settings.videoStepS, min: 0.1 },
  { key: 'videoFps', label: labels.settings.videoFps, min: 0.1 },
  { key: 'maxTextStoreKb', label: labels.settings.maxTextStoreKb, min: 1 },
  { key: 'maxExtractMb', label: labels.settings.maxExtractBytes, min: 0 },
  { key: 'maxFileMb', label: labels.settings.maxFileBytes, min: 0 },
];

export interface SettingsSheetProps {
  settings: Settings;
  busy: boolean;
  onClose: () => void;
  onSave: (next: Settings) => Promise<boolean>;
}

export function SettingsSheet({ settings, busy, onClose, onSave }: SettingsSheetProps): ReactElement {
  const [draft, setDraft] = useState<Draft>(() => toDraft(settings));
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const set = useCallback(<K extends keyof Draft>(key: K, value: Draft[K]) => {
    setDraft((d) => ({ ...d, [key]: value }));
  }, []);

  const dirty = useMemo(() => JSON.stringify(draft) !== JSON.stringify(toDraft(settings)), [draft, settings]);

  const submit = useCallback(async () => {
    setError(null);
    const bad = numericFields.filter((f) => num(draft[f.key] as string, f.min) == null).map((f) => f.label);
    if (bad.length > 0) {
      setError(`${labels.settings.invalidNumber}：${bad.join('、')}`);
      return;
    }
    const dim = num(draft.vectorDim, 1) ?? settings.vectorDim;
    const next: Settings = {
      ...settings,
      indexImages: draft.indexImages,
      indexVideo: draft.indexVideo,
      indexDocuments: draft.indexDocuments,
      indexCode: draft.indexCode,
      resumeOnLaunch: draft.resumeOnLaunch,
      vectorDim: Math.round(dim),
      inferThreads: Math.round(num(draft.inferThreads, 0) ?? settings.inferThreads),
      inferBatch: Math.round(num(draft.inferBatch, 1) ?? settings.inferBatch),
      ramBudgetMb: Math.round(num(draft.ramBudgetMb, 1) ?? settings.ramBudgetMb),
      videoWindowS: num(draft.videoWindowS, 0.1) ?? settings.videoWindowS,
      videoStepS: num(draft.videoStepS, 0.1) ?? settings.videoStepS,
      videoFps: num(draft.videoFps, 0.1) ?? settings.videoFps,
      maxTextStoreKb: Math.round(num(draft.maxTextStoreKb, 1) ?? settings.maxTextStoreKb),
      maxExtractBytes: Math.round((num(draft.maxExtractMb, 0) ?? 0) * MB),
      maxFileBytes: Math.round((num(draft.maxFileMb, 0) ?? 0) * MB),
      modelRepo: draft.modelRepo.trim() || settings.modelRepo,
      modelEndpoints: list(draft.modelEndpoints),
      inferPython: draft.inferPython.trim() || null,
      netProxy: draft.netProxy.trim() || null,
      updateCheck: draft.updateCheck,
      excludeDirs: list(draft.excludeDirs),
      excludePathPrefixes: list(draft.excludePathPrefixes),
      globalShortcut: draft.globalShortcut.trim() || settings.globalShortcut,
    };
    const ok = await onSave(next);
    if (ok) {
      setSaved(true);
      onClose();
    }
  }, [draft, onClose, onSave, settings]);

  const numField = (key: keyof Draft, label: string, hint?: string) => (
    <label className="field" key={key}>
      <span className="field-label">{label}</span>
      <input
        className="input input-num"
        type="number"
        step="any"
        value={draft[key] as string}
        onChange={(e) => set(key, e.target.value as Draft[typeof key])}
      />
      {hint ? <span className="field-hint">{hint}</span> : null}
    </label>
  );

  return (
    <div className="sheet-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <section className="sheet" role="dialog" aria-label={labels.settings.title} aria-modal="true">
        <header className="sheet-head">
          <SectionTitle>{labels.settings.title}</SectionTitle>
          <button type="button" className="icon-btn" onClick={onClose} title={labels.common.close}>
            <CloseIcon size={14} />
          </button>
        </header>

        <div className="sheet-body">
          <div className="sheet-group">
            <h3>{labels.settings.groupContent}</h3>
            <Toggle
              checked={draft.indexImages}
              onChange={(v) => set('indexImages', v)}
              label={labels.settings.indexImages}
            />
            <Toggle checked={draft.indexVideo} onChange={(v) => set('indexVideo', v)} label={labels.settings.indexVideo} />
            <Toggle
              checked={draft.indexDocuments}
              onChange={(v) => set('indexDocuments', v)}
              label={labels.settings.indexDocuments}
            />
            <Toggle checked={draft.indexCode} onChange={(v) => set('indexCode', v)} label={labels.settings.indexCode} />
            <Toggle
              checked={draft.resumeOnLaunch}
              onChange={(v) => set('resumeOnLaunch', v)}
              label={labels.settings.resumeOnLaunch}
            />
          </div>

          <div className="sheet-group">
            <h3>{labels.settings.groupSemantic}</h3>
            <div className="field-grid">
              {numField('vectorDim', labels.settings.vectorDim)}
              {numField('inferThreads', labels.settings.inferThreads)}
              {numField('inferBatch', labels.settings.inferBatch)}
              {numField('ramBudgetMb', labels.settings.ramBudgetMb)}
            </div>
            <Note>{labels.settings.vectorDimHint}</Note>
          </div>

          <div className="sheet-group">
            <h3>{labels.settings.groupVideo}</h3>
            <div className="field-grid">
              {numField('videoWindowS', labels.settings.videoWindowS)}
              {numField('videoStepS', labels.settings.videoStepS)}
              {numField('videoFps', labels.settings.videoFps)}
            </div>
          </div>

          <div className="sheet-group">
            <h3>{labels.settings.groupLimits}</h3>
            <div className="field-grid">
              {numField('maxFileMb', labels.settings.maxFileBytes)}
              {numField('maxExtractMb', labels.settings.maxExtractBytes)}
              {numField('maxTextStoreKb', labels.settings.maxTextStoreKb)}
            </div>
          </div>

          <div className="sheet-group">
            <h3>{labels.settings.groupExclude}</h3>
            <label className="field">
              <span className="field-label">
                {labels.settings.excludeDirs}
                <span className="field-hint">{labels.settings.excludeDirsHint}</span>
              </span>
              <textarea
                className="input textarea"
                rows={4}
                value={draft.excludeDirs}
                spellCheck={false}
                onChange={(e) => set('excludeDirs', e.target.value)}
              />
            </label>
            <label className="field">
              <span className="field-label">
                {labels.settings.excludePrefixes}
                <span className="field-hint">{labels.settings.excludePrefixesHint}</span>
              </span>
              <textarea
                className="input textarea"
                rows={3}
                value={draft.excludePathPrefixes}
                spellCheck={false}
                onChange={(e) => set('excludePathPrefixes', e.target.value)}
              />
            </label>
          </div>

          <div className="sheet-group">
            <h3>{labels.settings.groupModel}</h3>
            <label className="field">
              <span className="field-label">{labels.settings.modelRepo}</span>
              <input className="input" value={draft.modelRepo} readOnly />
            </label>
            <label className="field">
              <span className="field-label">
                {labels.settings.modelEndpoints}
                <span className="field-hint">{labels.settings.modelEndpointsHint}</span>
              </span>
              <textarea
                className="input textarea"
                rows={3}
                value={draft.modelEndpoints}
                spellCheck={false}
                onChange={(e) => set('modelEndpoints', e.target.value)}
              />
            </label>
            <label className="field">
              <span className="field-label">
                {labels.settings.inferPython}
                <span className="field-hint">{labels.settings.inferPythonHint}</span>
              </span>
              <TextInput value={draft.inferPython} onChange={(v) => set('inferPython', v)} mono />
            </label>
          </div>

          <div className="sheet-group">
            <h3>{labels.settings.groupNetwork}</h3>
            <label className="field">
              <span className="field-label">
                {labels.settings.netProxy}
                <span className="field-hint">{labels.settings.netProxyHint}</span>
              </span>
              <TextInput value={draft.netProxy} onChange={(v) => set('netProxy', v)} mono />
            </label>
            <Toggle
              checked={draft.updateCheck}
              onChange={(v) => set('updateCheck', v)}
              label={labels.settings.updateCheck}
              hint={labels.settings.updateCheckHint}
            />
          </div>

          <div className="sheet-group">
            <h3>{labels.settings.groupShortcut}</h3>
            <label className="field">
              <span className="field-label">
                {labels.settings.globalShortcut}
                <span className="field-hint">{labels.settings.globalShortcutHint}</span>
              </span>
              <TextInput value={draft.globalShortcut} onChange={(v) => set('globalShortcut', v)} mono />
            </label>
          </div>

          {error ? <Note tone="error">{error}</Note> : null}
          {saved ? <Note>{labels.settings.saved}</Note> : null}
        </div>

        <footer className="sheet-foot">
          <Btn tone="ghost" onClick={onClose}>
            {labels.settings.discard}
          </Btn>
          <Btn tone="primary" onClick={() => void submit()} disabled={busy || !dirty}>
            {labels.common.save}
          </Btn>
        </footer>
      </section>
    </div>
  );
}
