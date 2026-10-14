"""Document text extraction for formats Rust cannot decode reliably.

PDF is the case that matters: text is drawn with per-document font encodings, and
without the embedded ToUnicode CMaps the bytes come out as mojibake. pypdf applies those
maps, so Chinese PDFs stay searchable.
"""

from __future__ import annotations

import os


def extract_pdf(path: str, max_chars: int = 2_000_000, max_pages: int = 400) -> dict:
    from pypdf import PdfReader

    reader = PdfReader(path)
    try:
        meta = reader.metadata or {}
        title = (meta.get("/Title") or "").strip() or None
    except Exception:
        title = None
    total = len(reader.pages)
    parts = []
    truncated = False
    for i, page in enumerate(reader.pages[:max_pages]):
        if i >= max_pages:
            truncated = True
            break
        try:
            parts.append(page.extract_text() or "")
        except Exception as exc:
            parts.append(f"\n[第 {i + 1} 页解析失败: {type(exc).__name__}]\n")
        if sum(len(p) for p in parts) > max_chars:
            truncated = True
            break
    text = "\n".join(parts)[:max_chars]
    return {
        "ok": True,
        "text": text,
        "title": title,
        "pages": total,
        "truncated": truncated or total > max_pages,
        "chars": len(text),
    }


def extract_document_text(path: str, max_chars: int = 2_000_000) -> dict:
    if not os.path.exists(path):
        return {"ok": False, "error": "文件不存在"}
    ext = os.path.splitext(path)[1].lower().lstrip(".")
    if ext != "pdf":
        return {"ok": False, "error": f"sidecar 不处理 .{ext}"}
    try:
        return extract_pdf(path, max_chars)
    except Exception as exc:
        return {"ok": False, "error": f"{type(exc).__name__}: {exc}"}
