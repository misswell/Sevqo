"""Build the pipeline fixtures used by `cargo run -p sevqo-engine --example smoke`.

The corpus is deliberately tiny and hand-checkable: every file has one obvious correct
answer for the queries in `manifest.json`, so a passing smoke run means scan → extract →
embed → index → hybrid search all really work on the current machine.
"""

from __future__ import annotations

import base64
import io
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.path.join(ROOT, ".research", "flickr30k-cn")
OUT = os.path.join(ROOT, ".research", "fixtures")


def images(n: int) -> list[dict]:
    from PIL import Image

    os.makedirs(OUT, exist_ok=True)
    out = []
    with open(os.path.join(DATA, "test_imgs.tsv"), encoding="utf-8") as f:
        for line in f:
            if len(out) >= n:
                break
            parts = line.rstrip("\n").split("\t")
            if len(parts) < 2:
                continue
            iid, b64 = parts[0], parts[1]
            im = Image.open(io.BytesIO(base64.b64decode(b64)))
            if im.mode != "RGB":
                im = im.convert("RGB")
            im.thumbnail((640, 640))
            p = os.path.join(OUT, f"{iid}.jpg")
            im.save(p, "JPEG", quality=82)
            out.append({"file": os.path.basename(p), "kind": "image", "source_id": iid})
    return out


def captions_for(names: list[str], k: int = 1) -> dict:
    """Caption text per image id — written into the manifest so the report can show it."""
    want = set(names)
    found: dict[str, list[str]] = {}
    with open(os.path.join(DATA, "test_texts.jsonl"), encoding="utf-8") as f:
        for line in f:
            rec = json.loads(line)
            for g in rec["image_ids"]:
                if str(g) in want:
                    found.setdefault(str(g), []).append(rec["text"])
    return {k2: v[:k] for k2, v in found.items()}


def video(seconds: int) -> dict | None:
    """A synthetic clip whose colour flips once a second, so a window query has a
    definite answer and the 32-frame cap gets exercised."""
    import av
    import numpy as np

    path = os.path.join(OUT, "clip.mp4")
    if os.path.exists(path):
        os.remove(path)
    container = av.open(path, "w")
    stream = None
    for codec in ("libx264", "mpeg4"):
        try:
            stream = container.add_stream(codec, rate=4)
            break
        except Exception:
            continue
    if stream is None:
        return None
    stream.width, stream.height = 320, 240
    stream.pix_fmt = "yuv420p"
    frames = []
    palette = [(200, 40, 40), (40, 200, 60), (40, 80, 200), (220, 200, 40)]
    for t in range(seconds * 4):
        colour = palette[(t // 4) % len(palette)]
        arr = np.zeros((240, 320, 3), dtype="uint8")
        arr[:, :, 0] = colour[0]
        arr[:, :, 1] = colour[1]
        arr[:, :, 2] = colour[2]
        arr[60:120, 60 + (t % 16) * 8 : 140 + (t % 16) * 8] = (255, 255, 255)
        frames.append(av.VideoFrame.from_ndarray(arr, format="rgb24"))
    for fr in frames:
        for packet in stream.encode(fr):
            container.mux(packet)
    for packet in stream.encode():
        container.mux(packet)
    container.close()
    return {"file": "clip.mp4", "kind": "video", "seconds": seconds}


def pdf() -> dict:
    """A hand-built one-page PDF. `pypdf` must read the text back out of it."""
    text = "Sevqo PDF fixture: the lighthouse guided ships past the rocky shore."
    content = f"BT /F1 18 Tf 60 720 Td ({text}) Tj ET".encode("latin-1")
    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
        b"<< /Length " + str(len(content)).encode() + b" >>\nstream\n" + content + b"\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    body = b"%PDF-1.4\n"
    offsets = []
    for i, o in enumerate(objs, start=1):
        offsets.append(len(body))
        body += f"{i} 0 obj\n".encode() + o + b"\nendobj\n"
    xref = len(body)
    body += f"xref\n0 {len(objs) + 1}\n".encode()
    body += b"0000000000 65535 f \n"
    for off in offsets:
        body += f"{off:010d} 00000 n \n".encode()
    body += f"trailer\n<< /Size {len(objs) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    with open(os.path.join(OUT, "lighthouse.pdf"), "wb") as f:
        f.write(body)
    return {"file": "lighthouse.pdf", "kind": "document", "needle": "lighthouse"}


def texts() -> list[dict]:
    items = []
    md = "# 海边的日落\n\n照片记录了一次海边日落，天空很红，有人在沙滩上骑自行车。\n"
    with open(os.path.join(OUT, "beach.md"), "w", encoding="utf-8") as f:
        f.write(md)
    items.append({"file": "beach.md", "kind": "document", "needle": "骑自行车"})

    gbk = "老笔记：冬天的湖面结了冰，孩子们在滑冰。".encode("gbk")
    with open(os.path.join(OUT, "legacy.txt"), "wb") as f:
        f.write(gbk)
    items.append({"file": "legacy.txt", "kind": "document", "needle": "湖面"})
    return items


def main() -> int:
    limit = int(sys.argv[1]) if len(sys.argv) > 1 else 12
    os.makedirs(OUT, exist_ok=True)
    imgs = images(limit)
    caps = captions_for([i["source_id"] for i in imgs])
    docs = texts() + [pdf()]
    manifest = {
        "dir": OUT,
        "images": imgs,
        "captions": caps,
        "documents": docs,
        "video": video(40),
    }
    with open(os.path.join(OUT, "manifest.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)
    print(f"fixtures -> {OUT}  images={len(imgs)} video={'ok' if manifest['video'] else 'skipped'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
