// Inline SVG only — no icon library. All glyphs are 16x16, stroke = currentColor.
import type { ReactElement } from 'react';

import type { Kind } from '../types';

type Props = { size?: number; className?: string };

function svg(path: ReactElement, { size = 16, className }: Props = {}): ReactElement {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.3"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden="true"
      focusable="false"
    >
      {path}
    </svg>
  );
}

export function ImageIcon(p: Props): ReactElement {
  return svg(
    <>
      <rect x="1.8" y="2.6" width="12.4" height="10.8" rx="2" />
      <circle cx="5.6" cy="6.2" r="1.1" />
      <path d="M2.6 11.6l3.4-3.3 2.5 2.3 2-1.9 2.9 2.7" />
    </>,
    p,
  );
}

export function VideoIcon(p: Props): ReactElement {
  return svg(
    <>
      <rect x="1.6" y="3.4" width="9.6" height="9.2" rx="2" />
      <path d="M11.2 7l3.2-2v6l-3.2-2z" />
    </>,
    p,
  );
}

export function DocumentIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M4 1.9h5.2L12.6 5v9.1a1 1 0 01-1 1H4a1 1 0 01-1-1V2.9a1 1 0 011-1z" />
      <path d="M9 2v3.2h3.3M5.6 8.4h4.8M5.6 10.9h4.8" />
    </>,
    p,
  );
}

export function CodeIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M6.2 3.6L2.4 8l3.8 4.4M9.8 3.6L13.6 8l-3.8 4.4" />
    </>,
    p,
  );
}

export function AudioIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M4 12.2V4.4l8-1.6v7.4" />
      <circle cx="3.1" cy="12.4" r="1.5" />
      <circle cx="11.1" cy="10.6" r="1.4" />
    </>,
    p,
  );
}

export function FileIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M4.4 2h4.4L12 5.2v8.4H4.4z" />
      <path d="M8.8 2.2v3.2h3.2" />
    </>,
    p,
  );
}

export function SearchIcon(p: Props): ReactElement {
  return svg(
    <>
      <circle cx="6.8" cy="6.8" r="4.2" />
      <path d="M10 10l3.2 3.2" />
    </>,
    p,
  );
}

export function FolderIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M1.8 4.2h4.1l1.2 1.6h6.1v7.4H1.8z" />
    </>,
    p,
  );
}

export function PlusIcon(p: Props): ReactElement {
  return svg(<path d="M8 3.2v9.6M3.2 8h9.6" />, p);
}

export function TrashIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M3.4 4.8h9.2M6.4 3.2h3.2M4.6 4.8l.5 8.2h5.8l.5-8.2" />
    </>,
    p,
  );
}

export function GearIcon(p: Props): ReactElement {
  return svg(
    <>
      <circle cx="8" cy="8" r="2.2" />
      <path d="M8 1.6v1.6M8 12.8v1.6M1.9 4.9l1.4.8M12.7 10.3l1.4.8M1.9 11.1l1.4-.8M12.7 5.7l1.4-.8" />
    </>,
    p,
  );
}

export function CloseIcon(p: Props): ReactElement {
  return svg(<path d="M4.2 4.2l7.6 7.6M11.8 4.2l-7.6 7.6" />, p);
}

export function DownloadIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M8 2.6v7.6M4.8 7.4L8 10.6l3.2-3.2M2.8 12.9h10.4" />
    </>,
    p,
  );
}

export function PauseIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M6.2 3.4v9.2M9.8 3.4v9.2" />
    </>,
    p,
  );
}

export function PlayIcon(p: Props): ReactElement {
  return svg(<path d="M5.4 3.2l7 4.8-7 4.8z" />, p);
}

export function StopIcon(p: Props): ReactElement {
  return svg(<rect x="4.2" y="4.2" width="7.6" height="7.6" rx="1.4" />, p);
}

export function RefreshIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M13 8a5 5 0 11-1.6-3.7" />
      <path d="M13.2 2.4v2.9h-2.9" />
    </>,
    p,
  );
}

export function CheckIcon(p: Props): ReactElement {
  return svg(<path d="M3.4 8.4l3 3 6.2-6.8" />, p);
}

export function AlertIcon(p: Props): ReactElement {
  return svg(
    <>
      <path d="M8 2.6l6 10.8H2z" />
      <path d="M8 6.4v3.1M8 11.2v.6" />
    </>,
    p,
  );
}

export function ClockIcon(p: Props): ReactElement {
  return svg(
    <>
      <circle cx="8" cy="8" r="5.6" />
      <path d="M8 4.8V8l2.4 1.6" />
    </>,
    p,
  );
}

export function WindowIcon(p: Props): ReactElement {
  return svg(
    <>
      <rect x="1.8" y="3" width="12.4" height="10" rx="2" />
      <path d="M1.8 5.8h12.4" />
    </>,
    p,
  );
}

export function ArrowRightIcon(p: Props): ReactElement {
  return svg(<path d="M3.6 8h8.4M8.6 4.6L12 8l-3.4 3.4" />, p);
}

const kindIcons: Record<Kind, (p: Props) => ReactElement> = {
  image: ImageIcon,
  video: VideoIcon,
  document: DocumentIcon,
  code: CodeIcon,
  audio: AudioIcon,
  other: FileIcon,
};

export function KindIcon({ kind, ...p }: Props & { kind: Kind }): ReactElement {
  const Cmp = kindIcons[kind] ?? FileIcon;
  return <Cmp {...p} />;
}
