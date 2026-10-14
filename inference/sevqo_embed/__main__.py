from __future__ import annotations

import argparse
import json
import sys

from .config import DEFAULT_DIM, BackendConfig
from .server import Service


def _cfg(args) -> BackendConfig:
    return BackendConfig(
        home=args.home,
        dim=args.dim,
        device=args.device,
        dtype=args.dtype,
        use_vision=not args.text_only,
        use_audio=False,
        threads=args.threads,
    )


def cmd_probe(args) -> int:
    import time

    svc = Service(_cfg(args))
    out: dict = {"info": svc.info()}
    t0 = time.perf_counter()
    r = svc.embed(
        [
            {"key": "q1", "type": "text", "text": "海边日落时有人骑自行车", "role": "query"},
            {"key": "q2", "type": "text", "text": "someone riding a bike at sunset on the beach", "role": "query"},
            {"key": "d1", "type": "text", "text": "The northern lights are caused by charged particles.", "role": "document", "title": "physics"},
        ]
    )
    out["text_ms"] = round((time.perf_counter() - t0) * 1000, 1)
    out["text_errors"] = r["errors"]
    vecs = r["vectors"]
    out["dims"] = {k: len(v) for k, v in vecs.items()}
    if "q1" in vecs and "q2" in vecs:
        out["zh_en_cosine"] = round(svc.backend.similarity(vecs["q1"], vecs["q2"]), 4)
    if args.image:
        t0 = time.perf_counter()
        ri = svc.embed([{"key": "i1", "type": "image", "path": args.image}])
        out["image_ms"] = round((time.perf_counter() - t0) * 1000, 1)
        out["image_errors"] = ri["errors"]
        if "i1" in ri["vectors"] and "q1" in vecs:
            out["zh_vs_image"] = round(svc.backend.similarity(vecs["q1"], ri["vectors"]["i1"]), 4)
            out["en_vs_image"] = round(svc.backend.similarity(vecs["q2"], ri["vectors"]["i1"]), 4)
    print(json.dumps(out, ensure_ascii=False, indent=2))
    return 0 if not out["text_errors"] else 1


def cmd_serve(args) -> int:
    return Service(_cfg(args)).serve()


def main(argv=None) -> int:
    # The Rust host appends these after the subcommand, so they belong to the
    # subparsers — argparse would reject them there if they were top-level only.
    common = argparse.ArgumentParser(prog="sevqo_embed", description="EmbeddingGemma 2 inference sidecar", add_help=False)
    common.add_argument("--home", default=BackendConfig().home, help="model cache root")
    common.add_argument("--dim", type=int, default=DEFAULT_DIM, choices=[768, 512, 256])
    common.add_argument("--device", default="")
    common.add_argument("--dtype", default="", choices=["", "bfloat16", "float32"])
    common.add_argument("--threads", type=int, default=0)
    common.add_argument("--text-only", action="store_true")

    p = argparse.ArgumentParser(prog="sevqo_embed", description="EmbeddingGemma 2 inference sidecar")
    sub = p.add_subparsers(dest="cmd", required=True)
    pr = sub.add_parser("probe", parents=[common])
    pr.add_argument("--image", default="")
    pr.set_defaults(fn=cmd_probe)
    sub.add_parser("serve", parents=[common]).set_defaults(fn=cmd_serve)
    args = p.parse_args(argv)
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
