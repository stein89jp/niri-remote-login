"""Protect the native SIMD conversion boundary with inaccessible input padding."""
from pathlib import Path
import os
import shlex
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class NativeVideoTests(unittest.TestCase):
    @unittest.skipUnless(os.environ.get("NIRI_RDP_TEST_NVENC") == "1",
                         "opt-in NVIDIA hardware test: NIRI_RDP_TEST_NVENC=1")
    def test_live_nvenc_bitrate_changes_and_decoder_continuity(self):
        flags = subprocess.run(
            ["pkg-config", "--cflags", "--libs", "libavcodec", "libavutil", "libswscale"],
            check=True, capture_output=True, text=True,
        )
        with tempfile.TemporaryDirectory() as directory:
            executable = str(Path(directory) / "video-rate")
            stream = str(Path(directory) / "adaptive.h264")
            subprocess.run(
                ["cc", "-O2", "-Wall", "-Wextra", str(ROOT / "tests/video_rate.c"),
                 "-o", executable, *shlex.split(flags.stdout)],
                check=True, capture_output=True, text=True,
            )
            result = subprocess.run([executable, stream], capture_output=True, text=True,
                                    timeout=20)
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            self.assertIn("PASS:", result.stdout)
            print(result.stdout.strip())
            decoded = subprocess.run(
                ["ffmpeg", "-v", "error", "-threads", "1", "-i", stream,
                 "-f", "rawvideo", "-pix_fmt", "bgra", "-threads", "1", "pipe:1"],
                capture_output=True, timeout=20,
            )
            self.assertEqual(decoded.returncode, 0, decoded.stderr.decode())
            self.assertEqual(decoded.stderr, b"", "decoder reference continuity failed")
            self.assertEqual(len(decoded.stdout), 640 * 368 * 4 * 180)

    def test_tightly_packed_input_and_yuv_edge_padding(self):
        if not shutil.which("cc") or not shutil.which("pkg-config"):
            self.skipTest("C compiler and FFmpeg development libraries required")
        flags = subprocess.run(
            ["pkg-config", "--cflags", "--libs", "libavcodec", "libavutil", "libswscale"],
            capture_output=True, text=True,
        )
        if flags.returncode:
            self.skipTest("FFmpeg development libraries required")
        with tempfile.TemporaryDirectory() as directory:
            executable = str(Path(directory) / "video-guard")
            for layout in ("planar", "nv12"):
                with self.subTest(layout=layout):
                    defines = ["-DNIRI_VIDEO_TEST_NV12"] if layout == "nv12" else []
                    subprocess.run(
                        ["cc", "-O2", "-Wall", "-Wextra", *defines,
                         str(ROOT / "tests/video_guard.c"), "-o", executable,
                         *shlex.split(flags.stdout)],
                        check=True, capture_output=True, text=True,
                    )
                    result = subprocess.run([executable], capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("PASS:", result.stdout)


if __name__ == "__main__":
    unittest.main()
