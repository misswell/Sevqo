import os
import sys
from dataclasses import dataclass, field

MODEL_ID = "google/embeddinggemma-2"
MODEL_REVISION = "main"

VALID_DIMS = (768, 512, 256)
DEFAULT_DIM = 512

# Model card: video is sampled at 1 fps and the processor caps a single input at
# max_frames=32, so a clip longer than ~32 s silently loses its tail.
VIDEO_FPS = 1.0
VIDEO_MAX_FRAMES = 32

IMAGE_MAX_PIXELS = 1024 * 1024
PROMPT_QUERY = "query"
PROMPT_DOCUMENT = "document"


@dataclass
class BackendConfig:
    home: str = field(
        default_factory=lambda: os.environ.get("SEVQO_MODEL_HOME")
        or os.path.expanduser("~/Library/Caches/Sevqo/models")
    )
    dim: int = DEFAULT_DIM
    device: str = ""
    dtype: str = ""
    use_vision: bool = True
    use_audio: bool = False
    threads: int = 0

    def resolved_device(self) -> str:
        if self.device:
            return self.device
        import torch

        if torch.backends.mps.is_available():
            return "mps"
        if torch.cuda.is_available():
            return "cuda"
        return "cpu"

    def resolved_dtype(self) -> str:
        """Never float16: EG2 activations overflow fp16 and yield NaN or silently
        degraded embeddings."""
        if self.dtype:
            return self.dtype
        import torch

        device = self.resolved_device()
        if device == "mps":
            # Apple's MPS backend has bf16 since torch 2.3; the weights ship as bf16
            # so keeping that dtype halves RAM on a 16 GB machine.
            return "bfloat16"
        if device == "cuda" and torch.cuda.is_bf16_supported():
            return "bfloat16"
        return "float32"

    def model_source(self) -> str:
        """`home` doubles as the download target on the desktop: when it already points
        at the snapshot directory, load it directly instead of re-resolving via the Hub."""
        if os.path.isfile(os.path.join(self.home, "config.json")):
            return self.home
        if os.path.isfile(os.path.join(self.home, MODEL_ID, "config.json")):
            return os.path.join(self.home, MODEL_ID)
        return MODEL_ID
