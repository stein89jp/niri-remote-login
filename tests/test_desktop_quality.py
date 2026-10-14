"""The offline quality reference must prove content geometry and orientation."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('desktop_quality', ROOT / 'scripts/compare-desktop-quality.py')
QUALITY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QUALITY)


def reference_image():
    width, height = 192, 128
    data = bytearray(bytes((32, 48, 64)) * width * height)

    def pixel(x, y, color):
        offset = (y * width + x) * 3
        data[offset:offset + 3] = bytes(color)

    for y in range(16, height - 16):
        for x in range(16, width - 16):
            pixel(x, y, QUALITY.BACKGROUND)
    for x, y, foreground in ((32, 32, QUALITY.BLACK), (96, 32, QUALITY.BLUE), (96, 64, QUALITY.RED)):
        for letter, rows in enumerate(QUALITY.LETTERS):
            for row, bits in enumerate(rows):
                for column in range(5):
                    color = foreground if bits & (1 << (4 - column)) else QUALITY.BACKGROUND
                    pixel(x + letter * 7 + column, y + row, color)
    return width, height, data


class DesktopQualityReferenceTests(unittest.TestCase):
    def test_actual_pixels_prove_crop_and_reject_both_mirrors(self):
        width, height, data = reference_image()
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'desktop.ppm'
            for transform in ('normal', 'horizontal', 'vertical'):
                rows = [data[y * width * 3:(y + 1) * width * 3] for y in range(height)]
                if transform == 'horizontal':
                    rows = [b''.join(row[x * 3:x * 3 + 3] for x in reversed(range(width))) for row in rows]
                elif transform == 'vertical':
                    rows.reverse()
                path.write_bytes(f'P6\n{width} {height}\n255\n'.encode() + b''.join(rows))
                image = QUALITY.Ppm(path)
                try:
                    if transform == 'normal':
                        self.assertEqual(QUALITY.content_rectangle(image), (16, 16, 176, 112))
                    else:
                        with self.assertRaises(ValueError):
                            QUALITY.content_rectangle(image)
                finally:
                    image.close()

    def test_incomplete_or_blank_screenshot_cannot_become_a_reference(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'desktop.ppm'
            path.write_bytes(b'P6\n192 128\n255\n' + bytes((240, 240, 240)) * (192 * 128 - 1))
            with self.assertRaises(ValueError):
                QUALITY.Ppm(path)
            path.write_bytes(b'P6\n192 128\n255\n' + bytes((240, 240, 240)) * 192 * 128)
            image = QUALITY.Ppm(path)
            try:
                with self.assertRaises(ValueError):
                    QUALITY.content_rectangle(image)
            finally:
                image.close()


if __name__ == '__main__':
    unittest.main()
