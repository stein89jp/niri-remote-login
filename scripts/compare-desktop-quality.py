#!/usr/bin/env python3
"""Compare actual decoded colored-text desktops against proven bitmap pixels.

Run after performance measurements are quiet. This reads existing private PPM
screenshots; it never launches clients or changes capture/encoder policy.
"""
import argparse
import hashlib
import json
import math
import mmap
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BACKGROUND = (240, 240, 240)
BLACK = (32, 32, 32)
BLUE = (32, 32, 255)
RED = (255, 32, 32)
# Exact bitmap glyph contract from lab/performance-pattern.h. N and R are
# asymmetric, so checking all pixels also detects horizontal/vertical mirrors.
LETTERS = (
    (17, 25, 21, 19, 17, 17, 17), (14, 4, 4, 4, 4, 4, 14),
    (30, 17, 17, 30, 20, 18, 17), (14, 4, 4, 4, 4, 4, 14),
    (0, 0, 0, 0, 0, 0, 0), (30, 17, 17, 30, 20, 18, 17),
    (30, 17, 17, 17, 17, 17, 30), (30, 17, 17, 30, 16, 16, 16),
)


class Ppm:
    def __init__(self, path):
        self.path = Path(path)
        with self.path.open('rb') as source:
            if source.readline(32) != b'P6\n':
                raise ValueError(f'{path}: expected native-client P6 screenshot')
            dimensions = source.readline(64).split()
            if len(dimensions) != 2:
                raise ValueError(f'{path}: malformed screenshot dimensions')
            self.width, self.height = map(int, dimensions)
            if not 1 <= self.width <= 8192 or not 1 <= self.height <= 8192:
                raise ValueError(f'{path}: screenshot dimensions out of bounds')
            if source.readline(32) != b'255\n':
                raise ValueError(f'{path}: expected eight-bit RGB screenshot')
            self.offset = source.tell()
            if self.path.stat().st_size != self.offset + self.width * self.height * 3:
                raise ValueError(f'{path}: screenshot is truncated or has trailing data')
            self.mapping = mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ)

    def close(self):
        self.mapping.close()

    def pixel(self, x, y):
        if not 0 <= x < self.width or not 0 <= y < self.height:
            raise ValueError('pixel outside screenshot')
        index = self.offset + (y * self.width + x) * 3
        return tuple(self.mapping[index:index + 3])

    def array(self, np):
        return np.frombuffer(self.mapping, dtype=np.uint8, offset=self.offset).reshape(
            self.height, self.width, 3)


def glyph_signature(image, x, y, foreground):
    if x < 0 or y < 0 or x + 56 > image.width or y + 7 > image.height:
        return False
    for letter, rows in enumerate(LETTERS):
        for row, bits in enumerate(rows):
            for column in range(7):
                expected = foreground if column < 5 and bits & (1 << (4 - column)) else BACKGROUND
                if image.pixel(x + letter * 7 + column, y + row) != expected:
                    return False
    return True


def content_rectangle(image):
    """Prove the origin and native glyph orientation from lossless pixels."""
    candidates = []
    for top in range(min(64, image.height - 8)):
        for left in range(min(64, image.width - 8)):
            if image.pixel(left, top) != BACKGROUND:
                continue
            if left and image.pixel(left - 1, top + 8) == BACKGROUND:
                continue
            if top and image.pixel(left + 8, top - 1) == BACKGROUND:
                continue
            if not glyph_signature(image, left + 16, top + 16, BLACK):
                continue
            right = left
            while right < image.width and image.pixel(right, top + 8) == BACKGROUND:
                right += 1
            bottom = top
            while bottom < image.height and image.pixel(left + 8, bottom) == BACKGROUND:
                bottom += 1
            if right - left < 112 or bottom - top < 56:
                continue
            center = left + (right - left) // 2
            if not glyph_signature(image, center, top + 16, BLUE):
                continue
            if not glyph_signature(image, center, top + 48, RED):
                continue
            candidates.append((left, top, right, bottom))
    if len(candidates) != 1:
        raise ValueError(f'{image.path}: expected one proven app rectangle, found {candidates}')
    return candidates[0]


def metric_accumulator():
    return {'pixels': 0, 'rgb_abs': [0.0] * 3, 'rgb_square': 0.0,
            'luma_square': 0.0, 'rgb_max': 0.0}


def finish_metric(value):
    count = value['pixels']
    if not count:
        raise ValueError('quality region contains no pixels')
    mse = value['rgb_square'] / (count * 3)
    luma_mse = value['luma_square'] / count
    return {'pixels': count, 'rgb_mae': sum(value['rgb_abs']) / (count * 3),
            'per_channel_mae_rgb': [channel / count for channel in value['rgb_abs']],
            'rgb_max_absolute_error': value['rgb_max'], 'exact_match': mse == 0,
            'rgb_psnr_db': 10 * math.log10(255 ** 2 / mse) if mse else None,
            'luma_psnr_db': 10 * math.log10(255 ** 2 / luma_mse) if luma_mse else None}


def capture_receipt(path, encoder, capture, codec=None):
    report = json.loads(path.with_suffix('.json').read_text())
    if report['workload'] != 'colored-text' or report['encoder'] != encoder:
        raise ValueError(f'{path}: screenshot receipt is not the requested static desktop path')
    if report['capture_policy'] != capture or report['one_way_delay_ms'] != 0 or report['link_mbps'] != 'unshaped':
        raise ValueError(f'{path}: screenshot receipt has different capture/link policy')
    if not report['client_decode'][-1]['decoded_updates']:
        raise ValueError(f'{path}: screenshot has no actual decoded paint')
    if encoder == 'nvenc':
        decode_key = 'decoded_avc444_streams' if codec == 'avc444' else 'decoded_avc420_frames'
        if report['codec_policy'] != codec or not report['client_decode'][-1][decode_key]:
            raise ValueError(f'{path}: screenshot receipt lacks actual {codec} decode')
    if capture == 'auto' and (report['gpu_capture_activations'] != 1 or report['gpu_capture_fallback_errors']):
        raise ValueError(f'{path}: screenshot receipt does not prove successful native GPU capture')
    if capture == 'shm' and report['gpu_capture_activations']:
        raise ValueError(f'{path}: SHM reference unexpectedly used native GPU capture')
    return {key: report[key] for key in (
        'dimensions', 'encoder', 'profile', 'codec_policy', 'capture_policy',
        'frontend_sha256', 'compositor_sha256', 'client_fixture_sha256',
        'compositor_renderer_log', 'gpu_capture_activations', 'gpu_capture_fallback_errors',
        'client_decode', 'adaptive_graphics')}


def compare(np, reference, candidate, rectangle):
    if (reference.width, reference.height) != (candidate.width, candidate.height):
        raise ValueError(f'{candidate.path}: candidate resolution differs from reference')
    left, top, right, bottom = rectangle
    source = reference.array(np)[top:bottom, left:right]
    decoded = candidate.array(np)[top:bottom, left:right]
    metrics = {name: metric_accumulator() for name in (
        'full_content', 'black_ink', 'colored_ink', 'black_with_1px_surround',
        'colored_with_1px_surround')}
    weights = np.array((0.2126, 0.7152, 0.0722), dtype=np.float32)
    # Chunking bounds temporary float arrays independently of desktop height.
    for start in range(0, bottom - top, 64):
        end = min(start + 64, bottom - top)
        delta = decoded[start:end].astype(np.float32) - source[start:end].astype(np.float32)
        halo_start, halo_end = max(0, start - 1), min(source.shape[0], end + 1)
        halo = source[halo_start:halo_end]
        black = np.all(halo == BLACK, axis=2)
        colored = np.all(halo == BLUE, axis=2) | np.all(halo == RED, axis=2)
        begin = start - halo_start
        masks = {'black_ink': black[begin:begin + end - start],
                 'colored_ink': colored[begin:begin + end - start]}
        for name, mask in (('black_with_1px_surround', black), ('colored_with_1px_surround', colored)):
            padded = np.pad(mask, ((1, 1), (1, 1)), constant_values=False)
            expanded = np.zeros(mask.shape, dtype=bool)
            for y in range(3):
                for x in range(3):
                    expanded |= padded[y:y + mask.shape[0], x:x + mask.shape[1]]
            masks[name] = expanded[begin:begin + end - start]
        for name, value in metrics.items():
            pixels = delta.reshape(-1, 3) if name == 'full_content' else delta[masks[name]]
            if pixels.size == 0:
                continue
            absolute = np.abs(pixels)
            value['pixels'] += pixels.shape[0]
            value['rgb_abs'] = [a + float(b) for a, b in zip(
                value['rgb_abs'], absolute.sum(axis=0, dtype=np.float64))]
            value['rgb_square'] += float(np.square(pixels).sum(dtype=np.float64))
            value['luma_square'] += float(np.square(pixels @ weights).sum(dtype=np.float64))
            value['rgb_max'] = max(value['rgb_max'], float(absolute.max()))
    return {name: finish_metric(value) for name, value in metrics.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bitmap-dir', type=Path, default=ROOT / 'artifacts/plan-upgrades/desktop-bitmap-final')
    parser.add_argument('--shm-dir', type=Path, default=ROOT / 'artifacts/plan-upgrades/desktop-gpu-shm')
    parser.add_argument('--gpu-dir', type=Path, default=ROOT / 'artifacts/plan-upgrades/desktop-gpu-auto')
    parser.add_argument('--avc444-dir', type=Path, help='optional paired hardware GPU AVC444 screenshots')
    parser.add_argument('--output', type=Path, default=ROOT / 'artifacts/plan-upgrades/desktop-decoded-quality.json')
    args = parser.parse_args()
    args.bitmap_dir = args.bitmap_dir.resolve()
    args.shm_dir = args.shm_dir.resolve()
    args.gpu_dir = args.gpu_dir.resolve()
    if args.avc444_dir is not None:
        args.avc444_dir = args.avc444_dir.resolve()
    if not args.output.resolve().is_relative_to(ROOT / 'artifacts'):
        parser.error('quality reports belong under artifacts/')
    # Import only for this explicit, deferred analysis; helpers/tests stay light.
    import numpy as np
    results = []
    for width in (1920, 3840):
        reference_path = args.bitmap_dir / f'{width}-colored-text-bitmap-0mbps-0ms-1.ppm'
        reference = Ppm(reference_path)
        try:
            receipt = capture_receipt(reference_path, 'bitmap', 'shm')
            rectangle = content_rectangle(reference)
            repeat = Ppm(args.bitmap_dir / f'{width}-colored-text-bitmap-0mbps-0ms-2.ppm')
            try:
                if content_rectangle(repeat) != rectangle:
                    raise ValueError('bitmap repeats have different app geometry')
                repeat_metrics = compare(np, reference, repeat, rectangle)
                if not repeat_metrics['full_content']['exact_match']:
                    raise ValueError('lossless bitmap repeats differ; inspect references before comparison')
            finally:
                repeat.close()
            result = {'dimensions': [reference.width, reference.height],
                      'validated_content_rectangle_ltrb': rectangle,
                      'reference': str(reference_path.relative_to(ROOT)),
                      'reference_sha256': hashlib.sha256(reference.mapping).hexdigest(),
                      'reference_receipt': receipt,
                      'repeat_reference_content_exact_match': True, 'candidates': []}
            candidates = [('shm', args.shm_dir, 'avc420'), ('auto', args.gpu_dir, 'avc420')]
            if args.avc444_dir is not None:
                candidates.append(('auto', args.avc444_dir, 'avc444'))
            controls = None
            for name, folder, codec in candidates:
                for number in (1, 2):
                    path = folder / f'{width}-colored-text-nvenc-0mbps-0ms-{number}.ppm'
                    candidate_receipt = capture_receipt(path, 'nvenc', name, codec)
                    for key in ('dimensions', 'frontend_sha256', 'compositor_sha256', 'client_fixture_sha256'):
                        if candidate_receipt[key] != receipt[key]:
                            raise ValueError(f'{path}: candidate has different {key}')
                    next_controls = (candidate_receipt['profile'], candidate_receipt['adaptive_graphics'])
                    if controls is not None and next_controls != controls:
                        raise ValueError(f'{path}: candidate profile/controller differs from paired comparisons')
                    controls = next_controls
                    candidate = Ppm(path)
                    try:
                        result['candidates'].append({'capture_policy': name, 'codec_policy': codec, 'repeat': number,
                            'path': str(path.relative_to(ROOT)),
                            'sha256': hashlib.sha256(candidate.mapping).hexdigest(),
                            'capture_receipt': candidate_receipt,
                            'metrics': compare(np, reference, candidate, rectangle)})
                    finally:
                        candidate.close()
            results.append(result)
        finally:
            reference.close()
    output = {'schema': 1, 'source': 'actual decoded final-package bitmap reference; deterministic colored-text app',
              'physical_presentation_measured': False,
              'notes': 'ROI proven with native asymmetric glyphs and exact blue/red rows. No mirroring or alignment search on lossy candidates. RGB and Rec.709 luma error; ink/surround metrics expose text degradation hidden by large flat backgrounds. Null PSNR means exact equality. GPU/SHM codec differences are measured, not assumed improvements.',
              'results': results}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2) + '\n')
    print(args.output)


if __name__ == '__main__':
    main()
