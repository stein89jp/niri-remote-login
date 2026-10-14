#!/usr/bin/env python3
"""Repeat private desktop workloads; never configure the host network/display."""
import argparse
import hashlib
import itertools
import json
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
WORKLOADS = ('idle', 'typing', 'scroll', 'colored-text', 'window', 'video', 'resize')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'artifacts/performance')
    parser.add_argument('--seconds', type=int, default=6)
    parser.add_argument('--repeats', type=int, default=2)
    parser.add_argument('--encoders', nargs='+', choices=('bitmap', 'nvenc', 'nvenc-fallback'), default=['bitmap', 'nvenc', 'nvenc-fallback'])
    parser.add_argument('--widths', nargs='+', type=int, choices=(1920, 3840), default=[1920, 3840])
    parser.add_argument('--workloads', nargs='+', choices=WORKLOADS, default=WORKLOADS)
    parser.add_argument('--links', nargs='+', default=['0:0', '10:50'], help='Mbps:one-way-delay-ms; zero means unshaped')
    parser.add_argument('--nvidia-renderer', action='store_true')
    parser.add_argument('--codec', choices=('avc420', 'avc444', 'auto'), default='avc420')
    parser.add_argument('--capture', choices=('auto', 'shm'), default='shm')
    parser.add_argument('--profile', choices=('speed', 'balanced', 'quality'), default='speed')
    parser.add_argument('--adaptive-graphics', action='store_true')
    args = parser.parse_args()
    output = args.output.resolve()
    if not output.is_relative_to(ROOT / 'artifacts') or not 1 <= args.repeats <= 10 or not 6 <= args.seconds <= 120:
        parser.error('output must be under artifacts; repeats 1..10, seconds 6..120')
    try:
        links = [tuple(map(int, link.split(':'))) for link in args.links]
        if any(len(link) != 2 or (link[0] and not 3 <= link[0] <= 100) or not 0 <= link[1] <= 1000 for link in links):
            raise ValueError
    except ValueError:
        parser.error('links must be Mbps:delay-ms, Mbps 0 or 3..100 and delay 0..1000')
    output.mkdir(parents=True, exist_ok=True)
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    diff = subprocess.check_output(['git', 'diff', '--binary'], cwd=ROOT)
    metadata = {'revision': revision, 'tracked_source_diff_sha256': hashlib.sha256(diff).hexdigest(), 'kernel': subprocess.check_output(['uname', '-srvmo'], text=True).strip(), 'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(), 'created_unix': time.time(), 'physical_presentation_measured': False, 'listening_measured': False, 'reports': []}
    combinations = list(itertools.product(range(args.repeats), args.widths, args.workloads, args.encoders, links))
    for index, (repeat, width, workload, encoder, (mbps, delay)) in enumerate(combinations):
        name = f'{width}-{workload}-{encoder}-{mbps}mbps-{delay}ms-{repeat + 1}'
        report = output / (name + '.json')
        command = [str(ROOT / 'lab/run-isolated.sh')]
        if encoder != 'bitmap':
            command += ['--nvidia-renderer' if args.nvidia_renderer else '--nvenc']
        command += ['env']
        # Explicit artifact overrides survive the namespace's cleared environment.
        for variable in ('NIRI_RDP_LAB_NIRI', 'NIRI_RDP_LAB_BRIDGE'):
            import os
            if variable in os.environ:
                command.append(variable + '=' + os.environ[variable])
        command += ['python3', 'scripts/headless-performance.py', '--width', str(width), '--workload', workload, '--encoder', encoder, '--seconds', str(args.seconds), '--link-mbps', str(mbps), '--delay-ms', str(delay), '--codec', args.codec, '--capture', args.capture, '--profile', args.profile, '--report', str(Path('/work') / report.relative_to(ROOT))]
        if args.adaptive_graphics:
            command.append('--adaptive-graphics')
        print(f'[{index + 1}/{len(combinations)}] {name}', flush=True)
        with report.with_suffix('.log').open('w') as log:
            subprocess.run(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, check=True)
        metadata['reports'].append(str(report.relative_to(ROOT)))
        (output / 'manifest.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(f'Completed {len(combinations)} private workload runs in {output}')


if __name__ == '__main__':
    main()
