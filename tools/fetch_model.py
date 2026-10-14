"""Resumable EmbeddingGemma 2 fetcher — the behaviour the Rust model manager mirrors.

Writes a plain directory (not the HF hub cache layout) so loading is just
`SentenceTransformer(dir)` on every platform, with no symlinks.
Range-resumes into `.part`, rotates endpoints, backs off on throttling.
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

REPO = "google/embeddinggemma-2"
ENDPOINTS = ["https://hf-mirror.com", "https://huggingface.co"]
FILES = [
    ("1_Pooling/config.json", 90),
    ("2_Normalize/config.json", 97),
    ("README.md", 21730),
    ("chat_template.jinja", 1016),
    ("config.json", 4455),
    ("config_sentence_transformers.json", 1565),
    ("modules.json", 413),
    ("preprocessor_config.json", 511),
    ("processor_config.json", 1788),
    ("sentence_bert_config.json", 747),
    ("tokenizer_config.json", 1599),
    ("tokenizer.model", 4689013),
    ("tokenizer.json", 32170510),
    ("model.safetensors", 1488915288),
]
OUT = sys.argv[1] if len(sys.argv) > 1 else "/Users/guofeng/Code/solo/Sevqo/models/google/embeddinggemma-2"
UA = "Sevqo/0.1 (local search; +https://localhost)"
DEADLINE = float(os.environ.get("FETCH_DEADLINE_S", "0")) or None


def open_url(url: str, start: int, timeout: float):
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Range": f"bytes={start}-"})
    return urllib.request.urlopen(req, timeout=timeout)


def pull(path: str, size: int) -> bool:
    os.makedirs(os.path.join(OUT, os.path.dirname(path)), exist_ok=True)
    final = os.path.join(OUT, path)
    part = final + ".part"
    if os.path.exists(final) and os.path.getsize(final) == size:
        print(f"  have  {path}", flush=True)
        return True
    start = os.path.getsize(part) if os.path.exists(part) else 0
    if start > size:
        os.remove(part)
        start = 0
    for attempt in range(1, 60):
        ep = ENDPOINTS[(attempt - 1) % len(ENDPOINTS)]
        url = f"{ep}/{REPO}/resolve/main/{path}?download=true"
        try:
            if start and os.path.exists(part):
                r = open_url(url, start, 60)
                if r.status not in (200, 206):
                    raise OSError(f"http {r.status}")
                if r.status == 200 and start > 0:
                    # endpoint ignored Range: restart cleanly
                    start = 0
                    r.close()
                    r = open_url(url, 0, 60)
            else:
                r = open_url(url, 0, 60)
                start = 0
            mode = "ab" if start else "wb"
            got = start
            h = hashlib.sha256()
            if start:
                with open(part, "rb") as f:
                    while True:
                        blk = f.read(1 << 20)
                        if not blk:
                            break
                        h.update(blk)
            with open(part, mode) as out:
                last = time.time()
                while got < size:
                    blk = r.read(1 << 16)
                    if not blk:
                        break
                    out.write(blk)
                    h.update(blk)
                    got += len(blk)
                    if time.time() - last > 20:
                        pct = 100.0 * got / size
                        print(f"  ...   {path} {got/1e6:.0f}/{size/1e6:.0f} MB {pct:.1f}%", flush=True)
                        last = time.time()
            r.close()
            if got != size:
                raise OSError(f"truncated at {got}/{size}")
            digest = h.hexdigest()
            os.replace(part, final)
            json.dump({"path": path, "size": size, "sha256": digest}, open(final + ".sha256.json", "w"))
            print(f"  done  {path} sha256={digest[:16]}…", flush=True)
            return True
        except Exception as exc:
            if os.path.exists(part):
                start = os.path.getsize(part)
            wait = min(90, 5 * attempt)
            print(f"  retry {path} (attempt {attempt}, at {start/1e6:.0f}MB): {type(exc).__name__} {exc} -> sleep {wait}s", flush=True)
            time.sleep(wait)
            if DEADLINE and time.time() > DEADLINE:
                return False
    return False


def main() -> int:
    os.makedirs(OUT, exist_ok=True)
    failed = []
    for path, size in sorted(FILES, key=lambda x: x[1]):
        if not pull(path, size):
            failed.append(path)
    if failed:
        print(f"FAILED: {failed}", flush=True)
        return 1
    print(f"COMPLETE at {OUT}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
