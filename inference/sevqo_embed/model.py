from __future__ import annotations

import os
import time
from dataclasses import dataclass

from .config import PROMPT_QUERY, VALID_DIMS


def format_document(content: str, title: str | None = None) -> str:
    """Model card: documents are sent as `title: {t} | text: {c}`, with the literal
    `none` when there is no title. Documents carry no task prefix."""
    head = (title or "").strip() or "none"
    return f"title: {head} | text: {content.strip()}"


@dataclass
class LoadReport:
    device: str
    dtype: str
    dim: int
    modalities: tuple
    params_millions: float
    load_seconds: float


class EmbeddingBackend:
    """Owns one loaded EmbeddingGemma 2 instance for the lifetime of the process."""

    def __init__(self, cfg):
        if cfg.dim not in VALID_DIMS:
            raise ValueError(f"dim {cfg.dim} not one of {VALID_DIMS}")
        self.cfg = cfg
        self.model = None
        self.report: LoadReport | None = None

    def load(self) -> LoadReport:
        if self.report is not None:
            return self.report
        import numpy as np  # noqa: F401  (torch tensors already present)
        import torch
        from sentence_transformers import SentenceTransformer

        device = self.cfg.resolved_device()
        dtype_name = self.cfg.resolved_dtype()
        if dtype_name == "float16":
            raise ValueError("float16 unsupported: EG2 activations overflow fp16 and return NaN")
        dtype = {"bfloat16": torch.bfloat16, "float32": torch.float32}[dtype_name]

        if self.cfg.threads:
            torch.set_num_threads(self.cfg.threads)
        elif device == "cpu":
            torch.set_num_threads(max(1, (os.cpu_count() or 2) - 1))

        config_kwargs: dict = {}
        if not self.cfg.use_vision:
            config_kwargs["vision_config"] = None
        if not self.cfg.use_audio:
            config_kwargs["audio_config"] = None

        t0 = time.perf_counter()
        self.model = SentenceTransformer(
            self.cfg.model_source(),
            device=device,
            model_kwargs={"torch_dtype": dtype},
            config_kwargs=config_kwargs or None,
            cache_folder=self.cfg.home,
            local_files_only=True,
            truncate_dim=self.cfg.dim,
        )
        self.model.max_seq_length = min(getattr(self.model, "max_seq_length", 8192), 8192)
        params = sum(p.numel() for p in self.model.parameters())
        self.report = LoadReport(
            device=device,
            dtype=dtype_name,
            dim=self.cfg.dim,
            modalities=tuple(getattr(self.model, "modalities", ()) or ()),
            params_millions=round(params / 1e6, 1),
            load_seconds=round(time.perf_counter() - t0, 2),
        )
        return self.report

    def _to_vectors(self, out) -> list:
        import numpy as np

        arr = np.asarray(out, dtype="float32")
        if arr.ndim == 1:
            arr = arr[None, :]
        return [row.tolist() for row in arr]

    def _encode(self, inputs, prompt_name: str | None = None):
        return self.model.encode(
            inputs,
            prompt_name=prompt_name,
            normalize_embeddings=True,
            truncate_dim=self.cfg.dim,
            convert_to_tensor=False,
            show_progress_bar=False,
        )

    def embed_texts(self, texts: list[str], is_query: bool = False) -> list:
        if not texts:
            return []
        return self._to_vectors(self._encode(texts, prompt_name=PROMPT_QUERY if is_query else None))

    def embed_images(self, images: list) -> list:
        if not images:
            return []
        return self._to_vectors(self._encode([{"image": im} for im in images]))

    def embed_video_clips(self, frame_lists: list[list]) -> list:
        if not frame_lists:
            return []
        return self._to_vectors(self._encode([{"video": frames} for frames in frame_lists]))

    def similarity(self, a, b):
        import numpy as np

        return float(np.dot(np.asarray(a, "float32"), np.asarray(b, "float32")))
