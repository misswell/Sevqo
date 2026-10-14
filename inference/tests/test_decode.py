import sys
import types
import unittest
from unittest.mock import patch

from sevqo_embed import decode


class FakeContainer:
    def __init__(self):
        self.streams = [types.SimpleNamespace(type="video", time_base=0.25, start_time=0, duration=64)]

    def __enter__(self):
        return self

    def __exit__(self, *args):
        pass

    def seek(self, *args, **kwargs):
        pass

    def decode(self, stream):
        for pts in range(65):
            yield types.SimpleNamespace(pts=pts, to_image=lambda pts=pts: pts * 0.25)


class VideoSamplingTests(unittest.TestCase):
    def sample(self, **kwargs):
        av = types.SimpleNamespace(open=lambda _: FakeContainer())
        with patch.dict(sys.modules, {"av": av}), patch.object(decode, "_fit", lambda image, _: image):
            return decode.sample_video("fixture.mp4", **kwargs)

    def test_one_fps_covers_the_whole_window(self):
        clip = self.sample(start=0, end=16, fps=1)
        self.assertEqual(clip.frames, list(range(17)))

    def test_frame_cap_keeps_the_tail_of_the_window(self):
        clip = self.sample(start=0, end=16, fps=1, max_frames=4)
        self.assertEqual(len(clip.frames), 4)
        self.assertEqual(clip.frames[0], 0)
        self.assertEqual(clip.frames[-1], 16)

    def test_implicit_end_is_an_absolute_timestamp(self):
        clip = self.sample(start=12, fps=1)
        self.assertEqual(clip.frames, [12, 13, 14, 15, 16])

    def test_one_frame_cap_is_supported(self):
        self.assertEqual(self.sample(start=0, end=16, max_frames=1).frames, [8])


if __name__ == "__main__":
    unittest.main()
