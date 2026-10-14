#!/usr/bin/env python3
"""Measure server and client decode in lab/run-isolated.sh; no presentation claim."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import time
from loopback_link import LinkRelay

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('integration', ROOT / 'scripts/headless-integration.py')
integration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(integration)


def process_cpu(pid):
    fields = Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--width', type=int, choices=(1920, 3840), required=True)
    parser.add_argument('--workload', choices=('idle', 'typing', 'scroll', 'colored-text', 'window', 'video', 'resize'), required=True)
    parser.add_argument('--encoder', choices=('bitmap', 'nvenc', 'nvenc-fallback'), default='bitmap')
    parser.add_argument('--profile', choices=('speed', 'balanced', 'quality'), default='speed')
    parser.add_argument('--adaptive-graphics', action='store_true', help='opt into joint FPS/window adaptation for matched comparisons')
    parser.add_argument('--codec', choices=('avc420', 'avc444', 'auto'), default='avc420')
    parser.add_argument('--capture', choices=('auto', 'shm'), default='shm')
    parser.add_argument('--seconds', type=int, default=10)
    parser.add_argument('--link-mbps', type=int, default=0)
    parser.add_argument('--delay-ms', type=int, default=0)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--refresh-check', action='store_true', help='verify explicit refresh and suppression on a quiet workload')
    parser.add_argument('--direct', action='store_true', help='unshaped direct loopback transport without the relay')
    args = parser.parse_args()
    if ROOT != Path('/work') or Path('/dev/input').exists() or os.environ.get('DISPLAY'):
        parser.error('use lab/run-isolated.sh with private devices and display')
    if not 6 <= args.seconds <= 120 or not 0 <= args.delay_ms <= 1000 or (args.link_mbps and not 3 <= args.link_mbps <= 100):
        parser.error('seconds 6..120, delay 0..1000, link 0 or 3..100 Mbps')
    if args.direct and (args.link_mbps or args.delay_ms):
        parser.error('--direct requires an unshaped zero-delay link')
    report = args.report.resolve()
    if not report.is_relative_to(ROOT / 'artifacts'):
        parser.error('reports and synthetic screenshots belong in artifacts/')
    report.parent.mkdir(parents=True, exist_ok=True)
    height = 1080 if args.width == 1920 else 2160
    if args.encoder != 'bitmap' and os.environ.get('NIRI_RDP_LAB_NVENC') != '1':
        parser.error('NVENC requires --nvenc or --nvidia-renderer lab mode')
    env = {key: os.environ[key] for key in ('PATH', 'HOME', 'XDG_RUNTIME_DIR', 'LD_LIBRARY_PATH', 'LIBGL_ALWAYS_SOFTWARE', 'GALLIUM_DRIVER', 'NIRI_RDP_RENDERER', 'XKB_CONFIG_ROOT') if key in os.environ}
    env.update(NIRI_BACKEND='headless', XKB_DEFAULT_RULES='evdev', XKB_DEFAULT_MODEL='jp106', XKB_DEFAULT_LAYOUT='jp')
    children, logs = [], []
    relay = LinkRelay(args.link_mbps, args.delay_ms)
    with tempfile.TemporaryDirectory(prefix='niri-performance-', dir='/tmp') as folder:
        private = Path(folder)
        subprocess.run(['openssl', 'req', '-config', '/dev/null', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-subj', '/CN=localhost', '-keyout', str(private / 'key.pem'), '-out', str(private / 'cert.pem')], check=True, capture_output=True)
        (private / 'key.pem').chmod(0o600)

        def launch(command, environment, name):
            log = (private / name).open('w+')
            logs.append(log)
            process = subprocess.Popen(command, env=environment, stdout=log, stderr=log, start_new_session=True)
            children.append(process)
            return process

        try:
            niri = launch([str(ROOT / os.environ.get('NIRI_RDP_LAB_NIRI', 'compositor/upstream/target/release/niri')), '--config', str(ROOT / 'lab/headless.kdl')], env, 'niri.log')
            integration.wait_for(Path(env['XDG_RUNTIME_DIR']) / 'wayland-0', niri)
            app = launch([str(ROOT / 'lab/dynamic-app')], dict(env, WAYLAND_DISPLAY='wayland-0', NIRI_RDP_TEST_WORKLOAD=args.workload), 'app.log')
            username, password = 'performance-' + secrets.token_hex(8), secrets.token_hex(24)
            bridge_env = dict(env, NIRI_RDP_MODE='disposable', NIRI_RDP_CERT=str(private / 'cert.pem'), NIRI_RDP_KEY=str(private / 'key.pem'), NIRI_RDP_USERNAME=username, NIRI_RDP_PASSWORD=password, NIRI_RDP_WAYLAND_SOCKET=str(Path(env['XDG_RUNTIME_DIR']) / 'wayland-0'), NIRI_RDP_ENCODER='bitmap' if args.encoder == 'bitmap' else 'nvenc', NIRI_RDP_VIDEO_PROFILE=args.profile, NIRI_RDP_VIDEO_CODEC=args.codec, NIRI_RDP_GPU_CAPTURE=args.capture, NIRI_RDP_DIAGNOSTICS='1')
            bridge_env['NIRI_RDP_ADAPTIVE_GRAPHICS'] = '1' if args.adaptive_graphics else '0'
            if args.link_mbps:
                bridge_env['NIRI_RDP_LINK_MBPS'] = str(args.link_mbps)
            bridge = launch([str(ROOT / os.environ.get('NIRI_RDP_LAB_BRIDGE', 'bridge/target/release/niri-rdp-bridge'))], bridge_env, 'bridge.log')
            time.sleep(1)
            if not args.direct:
                relay.__enter__()
            client_env = dict(env, NIRI_RDP_TEST_USERNAME=username, NIRI_RDP_TEST_PASSWORD=password, NIRI_RDP_TEST_NLA='1', NIRI_RDP_TEST_PERFORMANCE_SECONDS=str(args.seconds), NIRI_RDP_TEST_WIDTH=str(args.width), NIRI_RDP_TEST_HEIGHT=str(height), NIRI_RDP_TEST_SCREENSHOT=str(report.with_suffix('.ppm')), NIRI_RDP_TEST_WORKLOAD=args.workload)
            if args.refresh_check:
                client_env['NIRI_RDP_TEST_REFRESH'] = '1'
            if args.encoder == 'nvenc':
                client_env['NIRI_RDP_TEST_GFX'] = '1'
            if args.codec != 'avc420':
                client_env['NIRI_RDP_TEST_AVC444'] = '1'
            client_env['NIRI_RDP_TEST_PORT'] = '33991' if args.direct else str(relay.listen_port)
            cpu_start = {name: process_cpu(process.pid) for name, process in (('frontend', bridge), ('compositor', niri), ('application', app))}
            began = time.monotonic()
            client = launch([str(ROOT / 'artifacts/rdp-desktop-client')], client_env, 'client.log')
            gpu_samples = []
            while client.poll() is None:
                if time.monotonic() - began > args.seconds + 20:
                    raise TimeoutError('performance client deadline')
                if args.encoder != 'bitmap':
                    sample = subprocess.run(['nvidia-smi', '--query-gpu=utilization.gpu,utilization.encoder,memory.used', '--format=csv,noheader,nounits'], capture_output=True, text=True, timeout=2)
                    if sample.returncode == 0:
                        gpu_samples.append({'elapsed_s': round(time.monotonic() - began, 3), 'gpu_encoder_percent_memory_mib': sample.stdout.strip()})
                time.sleep(.5)
            elapsed = time.monotonic() - began
            cpu = {name: round((process_cpu(process.pid) - cpu_start[name]) * 100 / elapsed, 2) for name, process in (('frontend', bridge), ('compositor', niri), ('application', app))}
            telemetry = [json.loads(line.split('RDP performance ', 1)[1]) for line in (private / 'bridge.log').read_text().splitlines() if 'RDP performance ' in line]
            decoded = [json.loads(line.split(' ', 1)[1]) for line in (private / 'client.log').read_text().splitlines() if line.startswith('NIRI_RDP_PERFORMANCE ')]
            if client.returncode or not decoded:
                raise RuntimeError('performance client did not complete: ' + (private / 'client.log').read_text())
            if decoded[-1]['elapsed_ms'] + 250 < args.seconds * 1000:
                raise RuntimeError('performance connection ended before its measurement window')
            have_pixels = decoded[-1]['decoded_updates'] > 0
            have_codec = args.encoder != 'nvenc' or (decoded[-1]['decoded_avc420_frames'] + decoded[-1]['decoded_avc444_streams'] > 0)
            if not args.link_mbps and not args.delay_ms and not (have_pixels and have_codec):
                raise RuntimeError('unshaped performance run did not decode the requested desktop path')
            result = {'schema': 1, 'workload': args.workload, 'dimensions': [args.width, height], 'encoder': args.encoder, 'profile': args.profile, 'codec_policy': args.codec, 'capture_policy': args.capture, 'link_mbps': args.link_mbps or 'unshaped', 'one_way_delay_ms': args.delay_ms, 'pcm_reserved_mbps': 1.7 if args.link_mbps else None, 'cpu_percent_one_core': cpu, 'server': telemetry, 'client_decode': decoded, 'physical_presentation_measured': False, 'listening_measured': False, 'content': 'synthetic private lab patterns', 'frontend_sha256': hashlib.sha256(Path(bridge.args[0]).read_bytes()).hexdigest()}
            result['link_relay'] = ({'kind': 'direct private loopback TCP', 'bytes': None} if args.direct else {'kind': 'bounded duplex userspace relay', 'bytes': dict(relay.bytes), 'max_queued_chunks_per_direction': 2, 'chunk_bytes': 16384})
            result['gpu_samples'] = gpu_samples
            bridge_log = (private / 'bridge.log').read_text()
            result['gpu_capture_activations'] = bridge_log.count('RDP GPU capture using EGL/CUDA DMA-BUF NVENC')
            result['gpu_capture_fallback_errors'] = (bridge_log.count('RDP GPU capture unavailable; restoring SHM capture:') + bridge_log.count('GPU capture unavailable; using SHM:'))
            result['compositor_sha256'] = hashlib.sha256(Path(niri.args[0]).read_bytes()).hexdigest()
            result['client_fixture_sha256'] = hashlib.sha256(Path(client.args[0]).read_bytes()).hexdigest()
            result['compositor_renderer_log'] = [line for line in (private / 'niri.log').read_text().splitlines() if 'headless renderer:' in line]
            result['client_library'] = 'FreeRDP ' + subprocess.check_output(['pkg-config', '--modversion', 'freerdp3'], env=dict(os.environ, PKG_CONFIG_PATH=str(ROOT / 'lab/sysroot/usr/lib/pkgconfig')), text=True).strip()
            result['refresh_check'] = args.refresh_check
            result['adaptive_graphics'] = args.adaptive_graphics
            result['decoded_path_within_window'] = have_pixels and have_codec
            report.write_text(json.dumps(result, indent=2) + '\n')
            print(json.dumps({'report': str(report), 'cpu': cpu, 'client_decode': decoded}))
        finally:
            if hasattr(relay, 'loop'):
                relay.__exit__()
            for process in reversed(children):
                integration.stop(process)
            for log in logs:
                log.close()
            for name in ('bridge.log', 'niri.log', 'app.log', 'client.log'):
                if (private / name).exists():
                    report.with_name(report.stem + '-' + name).write_bytes((private / name).read_bytes())


if __name__ == '__main__':
    main()
