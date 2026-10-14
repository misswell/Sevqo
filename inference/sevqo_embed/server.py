"""Stdio JSON-lines service driven by the Rust engine.

One request per line, one response per line. A request carries a batch of items; the
response maps each item key to its vector. Keeping decode and inference here lets the
Rust side stay free of Python/media dependencies.
"""

from __future__ import annotations

import json
import sys
import time
import traceback

from .config import BackendConfig, PROMPT_QUERY
from .decode import load_image, probe_media, sample_video, write_thumb
from .documents import extract_document_text
from .model import EmbeddingBackend, format_document


def _rss_mb() -> float:
    try:
        import psutil

        return round(psutil.Process().memory_info().rss / 1e6, 1)
    except Exception:
        return -1.0


class Service:
    def __init__(self, cfg: BackendConfig):
        self.cfg = cfg
        self.backend = EmbeddingBackend(cfg)

    def info(self) -> dict:
        report = self.backend.load()
        import os

        return {
            "ok": True,
            "device": report.device,
            "dtype": report.dtype,
            "dim": report.dim,
            "params_millions": report.params_millions,
            "load_seconds": report.load_seconds,
            "vision": self.cfg.use_vision,
            "audio": self.cfg.use_audio,
            "threads": self.cfg.threads or (os.cpu_count() or 0),
            "rss_mb": _rss_mb(),
        }

    def embed(self, items: list, batch_size: int = 8) -> dict:
        t0 = time.perf_counter()
        vectors: dict = {}
        errors: dict = {}
        thumbs: dict = {}

        texts, images, clips = [], [], []
        for it in items:
            kind = it.get("type")
            key = it.get("key")
            if kind == "text":
                texts.append((key, it))
            elif kind == "image":
                images.append((key, it))
            elif kind == "video":
                clips.append((key, it))
            else:
                errors[key] = f"unknown type {kind!r}"

        if texts:
            queries = [t for k, t in texts if t.get("role", "document") == "query"]
            docs = [t for k, t in texts if t.get("role", "document") != "query"]
            for chunk, is_query in ((queries, True), (docs, False)):
                for i in range(0, len(chunk), batch_size):
                    group = chunk[i : i + batch_size]
                    body = [
                        g.get("text", "") if is_query else format_document(g.get("text", ""), g.get("title"))
                        for g in group
                    ]
                    try:
                        vecs = self.backend.embed_texts(body, is_query=is_query)
                        for g, v in zip(group, vecs):
                            vectors[g["key"]] = v
                    except Exception as exc:  # keep the batch alive, report the item
                        for g in group:
                            errors[g["key"]] = f"{type(exc).__name__}: {exc}"

        for i in range(0, len(images), batch_size):
            group = images[i : i + batch_size]
            pils, keys = [], []
            for key, it in group:
                try:
                    pils.append(load_image(it["path"]))
                    keys.append(key)
                    if it.get("thumb"):
                        thumbs[key] = write_thumb(pils[-1], it["thumb"])
                except Exception as exc:
                    errors[key] = f"{type(exc).__name__}: {exc}"
            if not pils:
                continue
            try:
                vecs = self.backend.embed_images(pils)
                for key, v in zip(keys, vecs):
                    vectors[key] = v
            except Exception as exc:
                for key in keys:
                    errors[key] = f"batch {type(exc).__name__}: {exc}"

        for key, it in clips:
            try:
                clip = sample_video(
                    it["path"],
                    start=float(it.get("start", 0.0)),
                    end=it.get("end"),
                    fps=float(it.get("fps", 1.0)),
                )
                if it.get("thumb"):
                    thumbs[key] = write_thumb(clip.thumb, it["thumb"])
                v = self.backend.embed_video_clips([clip.frames])[0]
                vectors[key] = v
            except Exception as exc:
                errors[key] = f"{type(exc).__name__}: {exc}"

        return {
            "ok": not vectors or not errors,
            "vectors": vectors,
            "errors": errors,
            "thumbs": {k: v for k, v in thumbs.items() if isinstance(v, str)},
            "ms": round((time.perf_counter() - t0) * 1000, 1),
            "items": len(items),
            "rss_mb": _rss_mb(),
        }

    def serve(self) -> int:
        self.backend.load()
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except json.JSONDecodeError as exc:
                self._emit({"ok": False, "error": f"bad json: {exc}"})
                continue
            cmd = req.get("cmd", "embed")
            rid = req.get("id")
            try:
                if cmd == "info":
                    res = self.info()
                elif cmd == "embed":
                    res = self.embed(req.get("items", []), int(req.get("batch_size", 8)))
                elif cmd == "probe":
                    res = probe_media(req.get("path", ""))
                elif cmd == "text":
                    res = extract_document_text(req.get("path", ""), int(req.get("maxChars", 2_000_000)))
                elif cmd == "shutdown":
                    self._emit({"ok": True, "bye": True, "id": rid})
                    return 0
                else:
                    res = {"ok": False, "error": f"unknown cmd {cmd!r}"}
            except Exception as exc:
                traceback.print_exc(file=sys.stderr)
                res = {"ok": False, "error": f"{type(exc).__name__}: {exc}"}
            self._emit(dict(res, id=rid))

    @staticmethod
    def _emit(payload: dict) -> None:
        sys.stdout.write(json.dumps(payload, separators=(",", ":")) + "\n")
        sys.stdout.flush()
