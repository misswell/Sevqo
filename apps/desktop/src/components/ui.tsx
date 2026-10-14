import type { ChangeEvent, ReactElement, ReactNode } from 'react';

import { AlertIcon } from './Icons';

type BtnTone = 'default' | 'primary' | 'danger' | 'ghost';

export function Btn({
  children,
  onClick,
  tone = 'default',
  disabled,
  title,
  full,
  small,
  className,
}: {
  children: ReactNode;
  onClick?: () => void;
  tone?: BtnTone;
  disabled?: boolean;
  title?: string;
  full?: boolean;
  small?: boolean;
  className?: string;
}): ReactElement {
  const cls = ['btn', `btn-${tone}`, full ? 'btn-full' : '', small ? 'btn-sm' : '', className ?? '']
    .filter(Boolean)
    .join(' ');
  return (
    <button type="button" className={cls} onClick={onClick} disabled={disabled} title={title}>
      {children}
    </button>
  );
}

export function Chip({
  children,
  active,
  onClick,
  title,
}: {
  children: ReactNode;
  active?: boolean;
  onClick?: () => void;
  title?: string;
}): ReactElement {
  return (
    <button
      type="button"
      className={`chip${active ? ' chip-active' : ''}`}
      onClick={onClick}
      title={title}
      aria-pressed={active ? true : undefined}
    >
      {children}
    </button>
  );
}

export function Badge({
  children,
  tone = 'neutral',
}: {
  children: ReactNode;
  tone?: 'neutral' | 'ok' | 'warn' | 'error' | 'accent';
}): ReactElement {
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

export function Note({ children, tone }: { children: ReactNode; tone?: 'error' | 'hint' }): ReactElement | null {
  if (!children) return null;
  const text = typeof children === 'string' ? children : null;
  if (tone === 'error') {
    return (
      <p className="note note-error" role="status">
        <AlertIcon size={13} />
        <span>{text ?? children}</span>
      </p>
    );
  }
  return (
    <p className="note" role="status">
      {text ?? children}
    </p>
  );
}

export function TextInput({
  value,
  onChange,
  placeholder,
  onEnter,
  mono,
  className,
  autoFocus,
  spellCheck = false,
  ariaLabel,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  onEnter?: () => void;
  mono?: boolean;
  className?: string;
  autoFocus?: boolean;
  spellCheck?: boolean;
  ariaLabel?: string;
}): ReactElement {
  return (
    <input
      type="text"
      className={`input${mono ? ' input-mono' : ''}${className ? ` ${className}` : ''}`}
      value={value}
      placeholder={placeholder}
      autoFocus={autoFocus}
      spellCheck={spellCheck}
      aria-label={ariaLabel}
      onChange={(e: ChangeEvent<HTMLInputElement>) => onChange(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' && onEnter) {
          e.preventDefault();
          onEnter();
        }
      }}
    />
  );
}

export function NumberInput({
  value,
  onChange,
  placeholder,
  min = 0,
  step = 1,
  ariaLabel,
}: {
  value: number | string;
  onChange: (v: number | null) => void;
  placeholder?: string;
  min?: number;
  step?: number;
  ariaLabel?: string;
}): ReactElement {
  return (
    <input
      type="number"
      className="input input-num"
      value={value}
      min={min}
      step={step}
      placeholder={placeholder}
      aria-label={ariaLabel}
      onChange={(e) => {
        const raw = e.target.value;
        if (raw === '') {
          onChange(null);
          return;
        }
        const n = Number(raw);
        onChange(Number.isFinite(n) ? n : null);
      }}
    />
  );
}

export function Select({
  value,
  onChange,
  options,
  ariaLabel,
  className,
}: {
  value: string;
  onChange: (v: string) => void;
  options: Array<{ value: string; label: string }>;
  ariaLabel?: string;
  className?: string;
}): ReactElement {
  return (
    <select
      className={`select${className ? ` ${className}` : ''}`}
      value={value}
      aria-label={ariaLabel}
      onChange={(e) => onChange(e.target.value)}
    >
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}

export function Toggle({
  checked,
  onChange,
  label,
  hint,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: ReactNode;
  hint?: string;
}): ReactElement {
  return (
    <label className="toggle">
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="toggle-box"
      />
      <span className="toggle-text">
        <span>{label}</span>
        {hint ? <span className="toggle-hint">{hint}</span> : null}
      </span>
    </label>
  );
}

/** Thin progress bar: `total <= 0` renders an indeterminate rail. */
export function Bar({
  done,
  total,
  tone = 'accent',
  indeterminate,
}: {
  done: number;
  total: number;
  tone?: 'accent' | 'warn';
  indeterminate?: boolean;
}): ReactElement {
  const pct = total > 0 ? Math.min(100, Math.max(0, (done / total) * 100)) : 0;
  return (
    <div className={`bar bar-${tone}`}>
      <div
        className={`bar-fill${indeterminate || total <= 0 ? ' bar-fill-indeterminate' : ''}`}
        style={total > 0 ? { width: `${pct}%` } : undefined}
      />
    </div>
  );
}

export function Stat({ label, value }: { label: ReactNode; value: ReactNode }): ReactElement {
  return (
    <div className="stat">
      <span className="stat-label">{label}</span>
      <span className="stat-value">{value}</span>
    </div>
  );
}

export function EmptyState({
  title,
  hint,
  action,
}: {
  title: ReactNode;
  hint?: ReactNode;
  action?: ReactNode;
}): ReactElement {
  return (
    <div className="empty">
      <p className="empty-title">{title}</p>
      {hint ? <p className="empty-hint">{hint}</p> : null}
      {action ? <div className="empty-action">{action}</div> : null}
    </div>
  );
}

export function SectionTitle({ children, right }: { children: ReactNode; right?: ReactNode }): ReactElement {
  return (
    <header className="section-title">
      <span className="section-title-text">{children}</span>
      {right ? <span className="section-title-right">{right}</span> : null}
    </header>
  );
}
