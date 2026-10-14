"""EmbeddingGemma 2 inference backend for Sevqo.

Runs as a stdio JSON-lines service driven by the Rust engine: the Rust side owns
files, scheduling and indexes, this side owns model inference and media decoding.
"""

from .config import BackendConfig
from .model import EmbeddingBackend

__all__ = ["BackendConfig", "EmbeddingBackend"]
