"""Retrieval quality + throughput harness for the Sevqo indexing pipeline.

Corpus: Flickr30k test split (1000 images) with human Chinese captions
(OFA-Sys/chinese-clip-eval Flickr30k-CN) and human English captions (nlphuji/flickr30k),
so the same images serve as a cross-lingual A/B: any ZH/EN gap is a model gap, not a
data gap.

Measures:
  * text->image and image->text Recall@1/5/10 at 768/512/256 dims
  * whether slicing a 768d vector equals encode(truncate_dim=...)
  * effect of the `task: search result | query:` prefix
  * per-image latency / throughput / peak RSS  -> sizes the indexer
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "inference"))

from sevqo_embed.config import BackendConfig  # noqa: E402
from sevqo_embed.model import EmbeddingBackend, format_document  # noqa: E402

DATA = os.path.join(os.path.dirname(__file__), "..", ".research", "flickr30k-cn")


def load_images(limit: int):
    from PIL import Image

    out = []
    path = os.path.join(DATA, "test_imgs.tsv")
    with open(path, encoding="utf-8") as f:
        for line in f:
            if len(out) >= limit:
                break
            parts = line.rstrip("\n").split("\t")
            if len(parts) < 2:
                continue
            img_id, b64 = parts[0], parts[1]
            im = Image.open(io.BytesIO(base64.b64decode(b64)))
            if im.mode != "RGB":
                im = im.convert("RGB")
            out.append((img_id, im))
    return out


def load_zh_queries(limit_images: int):
    ids = {i for i, _ in load_image_ids(limit_images)}
    out = []
    with open(os.path.join(DATA, "test_texts.jsonl"), encoding="utf-8") as f:
        for line in f:
            rec = json.loads(line)
            gold = [str(g) for g in rec["image_ids"] if str(g) in ids]
            if gold:
                out.append((rec["text"], gold))
    return out


def load_image_ids(limit: int):
    out = []
    with open(os.path.join(DATA, "test_imgs.tsv"), encoding="utf-8") as f:
        for n, line in enumerate(f):
            if n >= limit:
                break
            out.append((line.split("\t", 1)[0], 0))
    return out


def load_en_queries(limit_images: int):
    ids = {i for i, _ in load_image_ids(limit_images)}
    out = []
    path = os.path.join(DATA, "en_annotations.csv")
    if not os.path.exists(path):
        return out
    import csv

    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            fn = (row.get("filename") or "").strip()
            img_id = os.path.splitext(fn)[0]
            if img_id not in ids:
                continue
            try:
                caps = json.loads(row["raw"])
            except Exception:
                continue
            for c in caps:
                if isinstance(c, str) and c.strip():
                    out.append((c.strip(), [img_id]))
    return out


def recall(qv, dv, gold, ks=(1, 5, 10)):
    import numpy as np

    q = np.asarray(qv, "float32")
    d = np.asarray(dv, "float32")
    sim = q @ d.T
    kmax = min(max(ks), sim.shape[1])
    idx = np.argpartition(-sim, kmax - 1, axis=1)[:, :kmax]
    rank = np.take_along_axis(sim, idx, axis=1)
    order = np.argsort(-rank, axis=1)
    ranked = np.take_along_axis(idx, order, axis=1)
    res = {k: 0 for k in ks}
    for i, gset in enumerate(gold):
        want = {str(g) for g in gset}
        hits = [j for j in range(ranked.shape[1]) if str(ranked[i, j]) in want]
        for k in ks:
            # a pool smaller than k can only be searched to the pool size
            if any(j < min(k, kmax) for j in hits):
                res[k] += 1
    n = max(1, len(gold))
    return {f"R@{k}": round(res[k] / n, 4) for k in ks} | {"n": n}


def truncate_and_renorm(mat, dim):
    import numpy as np

    t = mat[:, :dim]
    return t / np.maximum(np.linalg.norm(t, axis=1, keepdims=True), 1e-12)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--images", type=int, default=1000)
    ap.add_argument("--queries", type=int, default=2000)
    ap.add_argument("--device", default="")
    ap.add_argument("--dtype", default="")
    ap.add_argument("--threads", type=int, default=0)
    ap.add_argument("--prefix-ablation", action="store_true")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "..", ".research", "eval_results.json"))
    args = ap.parse_args()

    import numpy as np
    import psutil

    cfg = BackendConfig(device=args.device, dtype=args.dtype, threads=args.threads, dim=768)
    backend = EmbeddingBackend(cfg)
    report = backend.load()
    print(f"[load] device={report.device} dtype={report.dtype} params={report.params_millions}M in {report.load_seconds}s", flush=True)

    imgs = load_images(args.images)
    print(f"[data] {len(imgs)} images", flush=True)
    t0 = time.perf_counter()
    full = []
    per_img = []
    peak_rss = 0.0
    for i, (iid, im) in enumerate(imgs):
        s = time.perf_counter()
        v = backend.embed_images([im])[0]
        per_img.append(time.perf_counter() - s)
        full.append(v)
        if (i + 1) % 50 == 0:
            rss = psutil.Process().memory_info().rss / 1e6
            peak_rss = max(peak_rss, rss)
            print(f"  embedded {i+1}/{len(imgs)} mean={sum(per_img)/len(per_img):.2f}s rss={rss:.0f}MB", flush=True)
    img_seconds = time.perf_counter() - t0
    full = np.asarray(full, "float32")
    peak_rss = max(peak_rss, psutil.Process().memory_info().rss / 1e6)

    ids = [iid for iid, _ in imgs]
    id_to_row = {iid: r for r, iid in enumerate(ids)}

    zh = load_zh_queries(args.images)
    if args.queries and len(zh) > args.queries:
        step = len(zh) / args.queries
        zh = [zh[int(i * step)] for i in range(args.queries)]
    en = load_en_queries(args.images)
    if args.queries and len(en) > args.queries:
        step = len(en) / args.queries
        en = [en[int(i * step)] for i in range(args.queries)]

    results: dict = {
        "meta": {
            "report": report.__dict__,
            "images": len(imgs),
            "image_embed_seconds": round(img_seconds, 1),
            "image_embed_mean_seconds": round(sum(per_img) / max(1, len(per_img)), 3),
            "image_embed_p50": round(float(np.percentile(per_img, 50)), 3),
            "image_embed_p95": round(float(np.percentile(per_img, 95)), 3),
            "images_per_minute": round(60 * len(imgs) / max(0.1, img_seconds), 1),
            "peak_rss_mb": round(peak_rss, 0),
            "zh_queries": len(zh),
            "en_queries": len(en),
        }
    }

    def eval_arm(name, queries, prefix, dim):
        if not queries:
            return
        texts = [q for q, _ in queries]
        gold_str = [[str(id_to_row[g]) for g in gs] for _, gs in queries]
        s = time.perf_counter()
        qv = np.asarray(backend.embed_texts(texts, is_query=prefix), "float32")
        dt = time.perf_counter() - s
        if dim != 768:
            d = truncate_and_renorm(full, dim)
            q = truncate_and_renorm(qv, dim)
        else:
            d, q = full, qv
        r2i = recall(q, d, gold_str)
        # reverse direction: an image retrieves the captions that describe it
        rev: list[list[str]] = [[] for _ in ids]
        for qi, (_, gs) in enumerate(queries):
            for g in gs:
                row = id_to_row.get(str(g))
                if row is not None:
                    rev[row].append(str(qi))
        keep = [r for r in range(len(ids)) if rev[r]]
        i2t = recall(d[keep], q, [rev[r] for r in keep]) if keep else {"R@1": 0.0, "R@5": 0.0, "R@10": 0.0, "n": 0}
        results[name] = {"text_to_image": r2i, "image_to_text": i2t, "query_embed_seconds": round(dt, 1)}
        print(f"[{name}] t2i {r2i} i2t {i2t} ({dt:.1f}s for {len(texts)} queries)", flush=True)

    for dim in (768, 512, 256):
        eval_arm(f"zh_searchprefix_{dim}", zh, True, dim)
        eval_arm(f"zh_noprefix_{dim}", zh, False, dim)
    for dim in (768, 512):
        eval_arm(f"en_searchprefix_{dim}", en, True, dim)

    # does slicing + renormalising equal encode(truncate_dim=dim)?
    if zh:
        n = min(64, len(imgs))
        # keep only queries whose gold image lies inside the first n rows
        sample = [(t, [g for g in gs if str(g) in id_to_row and id_to_row[str(g)] < n]) for t, gs in zh]
        sample = [(t, gs) for t, gs in sample if gs][:n]
        frames = [im for _, im in imgs[:n]]
        gold_str = [[str(id_to_row[str(g)]) for g in gs] for _, gs in sample]
        raw_q = np.asarray(backend.embed_texts([t for t, _ in sample], is_query=True), "float32")
        for dim in (512, 256):
            direct_q = np.asarray(
                backend.model.encode(
                    [t for t, _ in sample], prompt_name="query", normalize_embeddings=True,
                    truncate_dim=dim, convert_to_tensor=False, show_progress_bar=False,
                ),
                "float32",
            )
            direct_i = np.asarray(
                backend.model.encode(
                    [{"image": im} for im in frames], normalize_embeddings=True,
                    truncate_dim=dim, convert_to_tensor=False, show_progress_bar=False,
                ),
                "float32",
            )
            sliced_q = truncate_and_renorm(raw_q, dim)
            sliced_i = truncate_and_renorm(full[:n], dim)
            cos = float(np.mean(np.sum(direct_q * sliced_q, axis=1)))
            results.setdefault("truncation", {})[f"dim{dim}"] = {
                "queries": len(sample),
                "query_cosine_direct_vs_sliced": round(cos, 6),
                "r_at_1_direct": recall(direct_q, direct_i, gold_str)["R@1"],
                "r_at_1_sliced": recall(sliced_q, sliced_i, gold_str)["R@1"],
                "mean_abs_image_delta": round(float(np.abs(direct_i - sliced_i).mean()), 6),
            }
            print(f"[truncation {dim}] {results['truncation'][f'dim{dim}']}", flush=True)

    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=2)
    print(f"[done] wrote {args.out}")


if __name__ == "__main__":
    main()
