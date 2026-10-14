"""Media decoding shared by indexing and query paths."""

from __future__ import annotations

import io
import math
import os
from dataclasses import dataclass

from .config import IMAGE_MAX_PIXELS, VIDEO_FPS, VIDEO_MAX_FRAMES

_HEIF_REGISTERED = False


def _ensure_formats() -> None:
    global _HEIF_REGISTERED
    if _HEIF_REGISTERED:
        return
    try:
        from pillow_heif import register_heif_opener

        register_heif_opener()
    except ImportError:
        pass
    _HEIF_REGISTERED = True


def _fit(img, max_pixels: int = IMAGE_MAX_PIXELS):
    from PIL import Image

    w, h = img.size
    if w * h <= max_pixels:
        return img
    scale = (max_pixels / (w * h)) ** 0.5
    return img.resize((max(1, int(w * scale)), max(1, int(h * scale))), Image.LANCZOS)


def load_image(path: str):
    """Decode still images including HEIC, honouring EXIF orientation."""
    from PIL import Image, ImageOps

    _ensure_formats()
    with Image.open(path) as im:
        im = ImageOps.exif_transpose(im)
        if im.mode not in ("RGB", "L"):
            im = im.convert("RGB")
        return _fit(im)


@dataclass
class Clip:
    frames: list
    timestamps: list
    thumb: object

    @property
    def count(self) -> int:
        return len(self.frames)


def sample_video(
    path: str,
    start: float = 0.0,
    end: float | None = None,
    fps: float = VIDEO_FPS,
    max_frames: int = VIDEO_MAX_FRAMES,
) -> Clip:
    """Decode a time window into frames at `fps`, capped at `max_frames`.

    Over-long windows are subsampled uniformly so the clip embedding covers the whole
    span instead of only its first `max_frames / fps` seconds.
    """
    import av

    if not math.isfinite(fps) or fps <= 0 or max_frames < 1:
        raise ValueError("invalid video sampling settings")

    with av.open(path) as container:
        stream = next((s for s in container.streams if s.type == "video"), None)
        if stream is None:
            raise ValueError("no video stream")
        tb = float(stream.time_base)
        origin = float(stream.start_time or 0) * tb
        total = float(stream.duration) * tb if stream.duration else 0.0
        if end is None:
            end = total or start + max_frames / fps
        window_start, window_end = start, min(end, total) if total else end
        if window_end <= window_start:
            raise ValueError("empty window")

        container.seek(int((origin + window_start) / tb), stream=stream, backward=True)
        step = 1.0 / fps
        next_sample = window_start
        pairs = []
        for frame in container.decode(stream):
            t = float(frame.pts or 0) * tb - origin
            if t < window_start:
                continue
            if t > window_end:
                break
            if t + 1e-6 < next_sample:
                continue
            pairs.append((t, _fit(frame.to_image(), 448 * 448)))
            next_sample = window_start + (math.floor((t - window_start) / step + 1e-6) + 1) * step
        if not pairs:
            raise ValueError("no frames decoded")

        if max_frames == 1:
            pairs = [pairs[len(pairs) // 2]]
        elif len(pairs) > max_frames:
            last = len(pairs) - 1
            pairs = [pairs[int(round(i * last / (max_frames - 1)))] for i in range(max_frames)]

        mid = (window_start + window_end) / 2
        thumb = min(pairs, key=lambda p: abs(p[0] - mid))[1]
        return Clip(
            frames=[p[1] for p in pairs],
            timestamps=[p[0] for p in pairs],
            thumb=thumb,
        )


def write_thumb(img, out_path: str, size: int = 320, quality: int = 78) -> str:
    from PIL import Image

    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    img = img.convert("RGB")
    img.thumbnail((size, size), Image.LANCZOS)
    img.save(out_path, "JPEG", quality=quality, optimize=True)
    return out_path


def probe_media(path: str) -> dict:
    """Container metadata the Rust engine cannot get without ffmpeg: duration, size,
    and whether an audio track exists (audio indexing is a P1 feature)."""
    import av

    out = {"ok": True, "durationS": 0.0, "width": 0, "height": 0, "hasVideo": False, "hasAudio": False, "fps": 0.0}
    with av.open(path) as container:
        vs = next((s for s in container.streams if s.type == "video"), None)
        as_ = next((s for s in container.streams if s.type == "audio"), None)
        if container.duration:
            out["durationS"] = round(container.duration / av.time_base, 3)
        if vs is not None:
            out["hasVideo"] = True
            out["width"] = int(vs.width or 0)
            out["height"] = int(vs.height or 0)
            if not out["durationS"] and vs.duration:
                out["durationS"] = round(float(vs.duration) * float(vs.time_base), 3)
            try:
                out["fps"] = round(float(vs.average_rate or 0), 3)
            except (TypeError, ZeroDivisionError):
                out["fps"] = 0.0
        out["hasAudio"] = as_ is not None
    return out
