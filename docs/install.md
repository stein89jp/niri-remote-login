# Install the Arch remote desktop host

Build on the **Arch Linux x86_64 machine that will host the remote desktop**.
Use an up-to-date Arch installation and a normal, unprivileged build account.
The repository is private, so cloning requires repository access. Rust 1.94 or
newer is required; `makepkg -si` installs the declared build/runtime dependencies.
The initial Rust/compositor build can take a while.

## Install, authorize, enable

```sh
sudo pacman -Syu --needed base-devel git
git clone https://github.com/stein89jp/niri-remote-login.git
cd niri-remote-login/packaging/arch/niri-rdp
makepkg -si
sudo usermod -aG niri-rdp-users "$USER"
sudo systemctl enable --now niri-rdp-frontend.service
```

For a fresh installation with an **NVENC-capable NVIDIA GPU and a working
NVIDIA driver**, use this block instead of the one above:

```sh
sudo pacman -Syu --needed base-devel git
git clone https://github.com/stein89jp/niri-remote-login.git
cd niri-remote-login/packaging/arch/niri-rdp
makepkg -si
sudo usermod -aG niri-rdp-users "$USER"

sudo install -d -m 755 /etc/systemd/system/niri-rdp-frontend.service.d
sudo install -m 644 /usr/share/doc/niri-remote-login/nvidia-frontend.conf \
  /etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf
sudo systemctl daemon-reload

sudo systemctl enable --now niri-rdp-frontend.service
```

The NVIDIA drop-in enables hardware video encoding with a default 60 fps capture
budget and gives the frontend service GPU access. Your remote Linux account also
needs GPU device access for hardware compositor rendering; see
[Optional NVIDIA encoding](#optional-nvidia-encoding) for permissions and checks.

If the frontend was already running when you added the NVIDIA drop-in, restart
it to apply the change; `enable --now` does not restart an active service:

```sh
sudo systemctl restart niri-rdp-frontend.service
```

This single `niri-remote-login` package contains the patched headless compositor,
RDP frontend, PAM/logind broker, defaults, and systemd units. Arch's package hooks
create the `niri-rdp` service account and `niri-rdp-users` authorization group.
On first service start the root broker creates a self-signed TLS certificate
and private key. Existing certificates are preserved and validated. No separate
release-preparation command, build VM, certificate-generation commands, or SSH
tunnel is required. Services start only when you enable/start them.

The compositor installs at `/usr/lib/niri-rdp/niri-headless`, alongside stock
`/usr/bin/niri`. It launches a separate remote desktop. It does not share your
existing local desktop or import its Niri configuration.

**An account cannot have an existing local X11/Wayland session when it logs in
remotely.** If you authorize your current user, log out of that user's local
graphical session before connecting. SSH/TTY logins are allowed. Root, service
accounts, and accounts outside `niri-rdp-users` are rejected. Rejoining the group
does not require a new local login for the broker to see the membership.

For a separate account, replace the `usermod` command above with:

```sh
sudo niri-rdp-setup --create-user niri-remote
```

The helper creates the home, prompts for a Linux password, and adds the account
to the group. To configure an existing user and optionally use a DNS name/IP
for a certificate that has not yet been created:

```sh
sudo niri-rdp-setup --user "$USER" --hostname YOUR_HOST_NAME_OR_IP
```

## Connect with Windows Remote Desktop

Open the regular Windows Remote Desktop client (`mstsc.exe`). Enter the host's
reachable DNS name or IP followed by the port, for example:

```text
192.168.1.50:33991
```

Use the Linux account name without a Windows domain prefix. The server negotiates
TLS RDP. If the client sends Linux username/password credentials, the broker
checks them directly. If it omits the password or sends remembered Windows domain credentials, a
login screen inside the remote connection accepts the Linux account name and
password before starting Niri. Windows domain credentials are never sent to PAM.
NLA/CredSSP authentication is not provided; Windows policies that require NLA
remain incompatible. Actual `mstsc` interoperability is still an acceptance gate,
not a demonstrated result. Protocol fallback and a login screen do not replace a
Windows machine test.

The default listens on `0.0.0.0:33991`, so clients connect directly over the
network. If the host has a firewall, allow TCP 33991 from the intended client
network using your existing firewall configuration. Port 3389 is left available
for another remote desktop service. Nothing opens the firewall automatically.

The initial self-signed certificate lasts 365 days. Compare the certificate's
SHA-256 fingerprint with the client certificate details before accepting it:

```sh
sudo openssl x509 -in /etc/niri-rdp/tls/cert.pem -noout -fingerprint -sha256
```

You can provide a trusted certificate and unencrypted private key at those same
paths before starting the service. The TLS directory must be root-owned mode
700, the key root-owned mode 600, and the certificate root-owned without group
or world write access. Invalid, expired, mismatched, or incomplete identities
fail startup rather than silently replacing your certificate. Replacing an
identity requires restarting both services so the frontend loads new credentials.

The defaults launch `foot`. Super+Enter opens a terminal; Super+Shift+Q logs out.
Disconnect ends the desktop; reconnect creates a fresh session. The compositor
tries headless GPU rendering automatically, preferring NVIDIA hardware, and falls
back to surfaceless rendering when hardware is unavailable. Optional NVIDIA
encoding is described below. This remains
experimental: real PAM/logind, installed-service boot, Windows, and physical
monitor tests are tracked in [acceptance](acceptance.md).

## Configuration and troubleshooting

Review `/etc/niri-rdp/frontend.conf`, `/etc/niri-rdp/headless.kdl`,
`/etc/niri-rdp/renderer.conf`, and
`/etc/pam.d/niri-rdp`. `NIRI_RDP_LISTEN` accepts a numeric IP address and port,
including bracketed IPv6. To use the standard RDP port on a host where it is free,
set `NIRI_RDP_LISTEN=0.0.0.0:3389`. To retain tunnel-only access, set
`NIRI_RDP_LISTEN=127.0.0.1:33991`. Restart the frontend after changing its config.

```sh
sudo systemctl status niri-rdp-broker.service niri-rdp-frontend.service
sudo journalctl -b -u niri-rdp-broker.service -u niri-rdp-frontend.service --no-pager
ss -ltn 'sport = :33991'
```

A TLS-only fallback profile is available if a client's normal connection setup
fails. Save this as `niri.rdp` on Windows, replace the address and account, and
open it with the regular Remote Desktop client:

```text
full address:s:192.168.1.50:33991
username:s:niri-remote
enablecredsspsupport:i:0
authentication level:i:2
prompt for credentials:i:1
desktopwidth:i:1920
desktopheight:i:1080
```

Never store the password in the profile. This does not override an NLA-required
Windows policy. Client-selected initial resolution and single-display live
resizing are supported; see [the resolution contract](../bridge/README.md#dynamic-resolution).
Only password PAM authentication is implemented; MFA and password-expiration
conversations fail closed.

## Maintainer builds and older artifacts

The older split package workflow remains available:

```sh
python3 scripts/prepare-release.py
```

It exports hash-checked source bundles under `artifacts/release/` for separate
`niri-rdp-headless` and `niri-remote-login` builds. The new direct build folder
needs the complete repository checkout; copy or clone the repository rather than
copying only the PKGBUILD. `makepkg --allsource` exports a portable source bundle.
The combined package provides and conflicts with `niri-rdp-headless`, allowing
pacman to replace a previously installed split compositor package.

The archived `0.1.0-1` delivery is a preserved snapshot. It lacks current
resolution support, NVENC, direct listening, automatic certificate setup, and
the login screen. Rebuild current source to use this installation procedure.
Use [uninstall](uninstall.md) and [upgrade](upgrade.md) for removal and maintenance.

## Optional NVIDIA encoding

The compositor now uses EGL device rendering to support NVIDIA headless graphics,
without a local desktop or display server. NVIDIA documents the
[EGL device platform](https://developer.nvidia.com/docs/drive/drive-os/6.0.6/public/drive-os-linux-sdk/api_reference/EGL_EXT_platform_device.html).
The logged-in account needs permission to use the GPU's render node and NVIDIA
device nodes. Inspect their ownership on the test machine; if group access is
needed, add the remote login account to the appropriate `render`/`video` groups
and create a fresh remote session. The frontend drop-in below affects the encoding
service account; it does not grant compositor GPU access to remote login accounts.

`/etc/niri-rdp/renderer.conf` contains one word: `auto` (default), `hardware`, or
`software`. Set `hardware` during GPU validation so missing drivers or permissions
fail compositor startup rather than quietly using the fallback. Set `software`
to force the previous llvmpipe behavior. Changes apply to new sessions. Inspect
the broker journal for `headless renderer: hardware EGL device` and the render
node path and `nvidia=true` to verify NVIDIA selection. The isolated lab has
validated actual NVIDIA rendering with NVENC and FreeRDP decoding on an RTX
4070 Ti; installed session permissions and end-to-end throughput still need
the [actual-machine checks](acceptance.md).

`NIRI_RDP_FPS` in `frontend.conf` limits capture/transmission to 1–60 frames per
second (default 60 for NVENC, 30 for bitmap). `NIRI_RDP_BITRATE_MBPS` accepts
1–100 megabits per second (default 12). Reduce the frame rate
or bitrate for limited networks; increasing bitrate can improve AVC420 quality.

The default `NIRI_RDP_ENCODER=bitmap` sends bitmap updates. A rebuilt frontend
supports `NIRI_RDP_ENCODER=nvenc`, which passes captured desktop frames to
FFmpeg's `h264_nvenc` encoder and sends H.264 through the RDP graphics channel
as AVC420. The client must negotiate that graphics/codec support; otherwise the
connection uses bitmap updates. This is host-side encoding. Decoding happens
on the RDP client, whose hardware acceleration depends on its own GPU and client.
The current combined package includes NVENC; the archived
`0.1.0-1` frontend binaries contain neither these fixes nor NVENC support.
The frontend converts captured pixels to full-range BT.709 on the CPU before
hardware H.264 encoding. Compositor rendering can run on the GPU, while capture
still reads pixels through SHM; this is not a zero-copy GPU capture/encode path.

On the separate test installation, use an NVENC-capable NVIDIA GPU, a working
NVIDIA driver with its userspace libraries, and an FFmpeg build providing
`h264_nvenc`. The driver must support the NVENC API version used by that FFmpeg
build. Listing the encoder confirms build support; it does not prove that the
GPU can initialize it:

```sh
nvidia-smi
ffmpeg -hide_banner -h encoder=h264_nvenc
```

The bridge links the distribution's FFmpeg libraries; its build and runtime do
not need a separately installed CUDA toolkit or Video Codec SDK. NVIDIA's
[FFmpeg guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html)
describes the runtime requirements for a compiled FFmpeg installation. Installing
or building FFmpeg itself is a separate task.

The standard frontend unit hides GPU devices. Enable the packaged NVIDIA
drop-in, which exposes the host's device tree to the frontend and adds the
`video` and `render` supplementary groups:

```sh
sudo install -d -m 755 /etc/systemd/system/niri-rdp-frontend.service.d
sudo install -m 644 /usr/share/doc/niri-remote-login/nvidia-frontend.conf \
  /etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf
sudo systemctl daemon-reload
sudo systemctl restart niri-rdp-frontend.service
sudo journalctl -u niri-rdp-frontend.service -n 50 --no-pager
```

Explicit NVENC configuration fails startup if hardware encoder initialization
fails. Check the journal for missing driver libraries, device permissions or an
incompatible GPU/driver. Client codec fallback does not mask that host startup
failure. Actual NVIDIA encoding and Windows graphics negotiation require the
[acceptance checks](acceptance.md); the existing software-rendered bitmap
integration result does not establish their performance.

Test the encoder independently using the same service account, supplementary
groups and GPU visibility. This probe opens no RDP listener and uses synthetic
frames. Its output path must not already exist:

```sh
sudo systemd-run --unit=niri-rdp-nvenc-probe --wait --pipe --collect \
  --property=User=niri-rdp --property=Group=niri-rdp \
  --property='SupplementaryGroups=video render' --property=PrivateDevices=no \
  /usr/lib/niri-rdp/encoder-probe /tmp/niri-rdp-nvenc.h264
ffmpeg -v error -i /tmp/niri-rdp-nvenc.h264 -f null -
ffprobe -v error -select_streams v:0 -count_frames \
  -show_entries stream=codec_name,width,height,nb_read_frames \
  -of default=noprint_wrappers=1 /tmp/niri-rdp-nvenc.h264
```

Require a successful probe, error-free decoding and `codec_name=h264`,
`width=1920`, `height=1088`, `nb_read_frames=30`. AVC420 requires coded dimensions
aligned to 16 pixels, so the standalone recording includes eight padded rows.
RDP region masks crop those rows to the logical 1920×1080 desktop. This tests the hardware encoder
and device permissions; it does not establish RDP client negotiation or recreate
every frontend service hardening setting.

To restore the standard bitmap configuration:

```sh
sudo rm /etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf
sudo systemctl daemon-reload
sudo systemctl restart niri-rdp-frontend.service
```
