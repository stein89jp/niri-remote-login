#!/usr/bin/env bash
# Disposable Arch userspace. No host DISPLAY, runtime directory, physical input,
# network, service manager, home, PAM files or writable /usr are visible.
# --nvenc explicitly exposes only NVIDIA encoder device nodes.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
args=(--unshare-all --die-with-parent --new-session --ro-bind /usr /usr
      --symlink usr/lib /lib --symlink usr/lib /lib64
      --ro-bind /etc/os-release /etc/os-release --proc /proc --dev /dev
      --tmpfs /tmp --tmpfs /run --dir /etc --dir /home --dir /home/test
      --bind "$repo" /work --chdir /work
      --clearenv --setenv PATH /usr/bin --setenv HOME /home/test
      --setenv XDG_RUNTIME_DIR /tmp/niri-rdp-runtime
      --setenv NIRI_BACKEND headless --setenv LIBGL_ALWAYS_SOFTWARE 1
      --setenv GALLIUM_DRIVER llvmpipe --setenv XKB_CONFIG_ROOT /usr/share/X11/xkb
      --setenv LD_LIBRARY_PATH /work/lab/sysroot/usr/lib)
[[ ! -d /etc/fonts ]] || args+=(--ro-bind /etc/fonts /etc/fonts)
if [[ ${1:-} == --nvenc ]]; then
    shift
    shopt -s nullglob
    devices=(/dev/nvidia*)
    if [[ ${#devices[@]} == 0 ]]; then
        echo 'NVENC test requested but no /dev/nvidia* devices exist' >&2
        exit 1
    fi
    for device in "${devices[@]}"; do
        [[ -c "$device" || -d "$device" ]] || continue
        args+=(--dev-bind "$device" "$device")
    done
    # CUDA discovers NVIDIA PCI devices through sysfs even though the
    # compositor stays on llvmpipe. Driver metadata is read-only.
    args+=(--ro-bind /sys /sys)
    args+=(--setenv NIRI_RDP_LAB_NVENC 1)
fi
exec bwrap "${args[@]}" /usr/bin/bash -c 'mkdir -m 700 /tmp/niri-rdp-runtime; exec "$@"' bash "$@"
