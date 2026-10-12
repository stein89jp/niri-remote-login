# Install the Arch remote desktop host

Build on the **Arch Linux x86_64 machine that will host the remote desktop**.
Use an up-to-date Arch installation and a normal, unprivileged build account.
The repository is private, so cloning requires repository access. Rust 1.94 or
newer is required; `makepkg -fsi` installs the declared build/runtime dependencies.
The initial Rust/compositor build can take a while.

## Install, authorize, enable

```sh
sudo pacman -Syu --needed base-devel git
git clone https://github.com/stein89jp/niri-remote-login.git
cd niri-remote-login/packaging/arch/niri-rdp
makepkg -fsi
sudo usermod -aG niri-rdp-users "$USER"
sudo systemctl enable --now niri-rdp-frontend.service
```

For a fresh installation with an **NVENC-capable NVIDIA GPU and a working
NVIDIA driver**, use this block instead of the one above:

```sh
sudo pacman -Syu --needed base-devel git
git clone https://github.com/stein89jp/niri-remote-login.git
cd niri-remote-login/packaging/arch/niri-rdp
makepkg -fsi
sudo usermod -aG niri-rdp-users "$USER"

sudo install -d -m 755 /etc/systemd/system/niri-rdp-frontend.service.d
sudo install -m 644 /usr/share/doc/niri-remote-login/nvidia-frontend.conf \
  /etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf
sudo systemctl daemon-reload

sudo systemctl enable --now niri-rdp-frontend.service
```

When reinstalling from an existing checkout, skip `git clone` and use that
checkout's `packaging/arch/niri-rdp` directory. `makepkg -fsi` rebuilds from its
current files, including local edits. The `-f` matters when a package archive
already exists: without it, `makepkg -si` can reinstall that older archive
without compiling the changes. Then run the account authorization, optional
NVIDIA drop-in, and service commands from the appropriate block above.

The NVIDIA drop-in enables hardware video encoding with a default 60 fps capture
budget and gives the frontend service GPU access. Your remote Linux account also
needs GPU device access for hardware compositor rendering; see
[Optional NVIDIA encoding](#optional-nvidia-encoding) for permissions and checks.

Package release 14 automatically prepares NVIDIA device nodes before every
frontend start, including after a reboot. No manual helper installation,
udev rule, or `nvidia-smi` invocation is needed. The preparation helper
skips hosts without a loaded NVIDIA GPU and does not need to be enabled separately.

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

Remote-session playback uses a private PipeWire output and travels inside the
encrypted RDP connection. The package includes the required PipeWire,
PipeWire-Pulse, WirePlumber and Pulse client utilities. In Windows mstsc, select
**Local Resources → Remote audio → Settings → Play on this computer**; saved
`.rdp` profiles use `audiomode:i:0`. The remote output is **Remote Desktop Audio**.
After upgrading, end any retained remote desktop and reconnect so the new session
starts its private audio services. See the [audio contract](../bridge/README.md#session-audio).

The compositor installs at `/usr/lib/niri-rdp/niri-headless`, alongside stock
`/usr/bin/niri`. It launches a separate remote desktop. It does not share your
existing local desktop. It reuses the account's usual Niri configuration,
configured input-method environment, and XDG autostart entries on a private
display and bus. An optional `~/.config/niri-rdp/config.kdl` overrides the usual
Niri configuration.

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

## Flatpak applications and updates

Flatpak's Bubblewrap helper needs read and traversal access to every parent of
the private runtime directory. The broker therefore uses mode 0755 for
`/run/niri-rdp` and `/run/niri-rdp/desktops`; individual desktop directories and
session records remain private at mode 0700.

Older packages used mode 0711 for these parents, causing application launches
to fail with `bwrap: Can't mkdir parents for .../.dbus-proxy/: Permission denied`.
After upgrading, the following also fixes an already-running remote desktop
without restarting the broker or disconnecting:

```sh
sudo chmod 0755 /run/niri-rdp /run/niri-rdp/desktops
```

System-wide Flatpak changes can require administrator authentication in a
remote login. A graphical updater needs a working Polkit authentication agent
in the remote desktop. To update system-wide applications from a terminal with
administrator authentication:

```sh
sudo flatpak update --system
```

Applications installed per user instead use `flatpak update --user` without
sudo. A private desktop portal supplies your configured file open/save dialogs,
including Firefox downloads, on the remote display. Keyring integration may require
additional setup.

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

The remote desktop loads your existing `~/.config/niri/config.kdl`, preserving
your bindings, appearance, includes and startup applications. DankMaterialShell
(when installed), otherwise Waybar, starts on the private session if configured
startup commands have not already launched a bar. Accounts without a Niri
configuration receive `foot` and a bar: Super+Enter opens a terminal, Super+D
opens Fuzzel, Super+Q closes a window, and Super+Shift+Q logs out.
Disconnecting or losing the network preserves the desktop, open windows and
applications for 45 seconds after the broker observes the connection detach. Reconnect with
the same Linux account and authenticate again to resume it; held keys and mouse
buttons are released when the old connection closes. When that grace window
expires, the broker fully logs out the desktop and cleans up its applications
and session; a subsequent login starts a fresh desktop after cleanup.
Super+Shift+Q logs out and ends the desktop; compositor exit or stopping
the broker also ends it. A frontend restart preserves the desktop only if you reconnect
within the same 45-second grace window. After a silent network
failure, the frontend may temporarily reject a reconnect while it still considers
the old connection active; retry once it detects that connection has closed.
Each resume rechecks PAM authentication, account policy and group authorization;
a revoked or expired account cannot resume.
The compositor
tries headless GPU rendering automatically, preferring NVIDIA hardware, and falls
back to surfaceless rendering when hardware is unavailable. Optional NVIDIA
encoding is described below. This remains
experimental: real PAM/logind, installed-service boot, Windows, and physical
monitor tests are tracked in [acceptance](acceptance.md).

## Configuration and troubleshooting

Configuration priority is `~/.config/niri-rdp/config.kdl`, then your usual
`~/.config/niri/config.kdl`, then `/etc/niri-rdp/headless.kdl`. `XDG_CONFIG_HOME`
is honored when set. Each selected configuration is included by a temporary
wrapper that starts the private desktop portal and ordinary XDG autostart after
Niri creates its display.
For your usual configuration, the wrapper also keeps `HEADLESS-1` at scale 1 with
normal orientation and supplies a private bar startup helper. Physical output
settings do not apply to this virtual output.
Your original files are preserved. The helper waits up to two seconds for
configured DMS/Waybar/Quickshell startup before supplying a bar; unusually slow or
custom bars should use an explicit remote configuration.

To opt into separate remote settings, create `~/.config/niri-rdp/config.kdl`.
This override keeps its own output and bar settings, so include your desired `spawn-at-startup`
commands for the bar. The remote session uses its own Wayland socket and D-Bus
bus. Local systemd desktop services are not started or given the remote environment.

The portal startup helper exports the remote runtime, display, and audio settings
to the private bus only. Portal service activation uses `XDG_CURRENT_DESKTOP=niri:niri-rdp`
so your installed Niri portal configuration and preferred chooser take precedence.
For example, the GNOME backend supplies Nautilus file dialogs. The packaged
`niri-rdp-portals.conf` supplies GTK when there is no Niri portal configuration.
User portal settings under `~/.config/xdg-desktop-portal/` also take precedence.
The helper replaces GNOME/GTK backends already activated before the remote display
was available, without starting unused backends. It does not change application
configuration directories or Firefox's download preferences.
Upgrading fixes newly created remote sessions; reconnecting to
a preserved session retains its old startup environment until that session logs out.

### Input methods and keyboard layouts

The worker reads the host's saved XKB layout, model, variant, and options from
localed's `/etc/vconsole.conf` and `/etc/X11/xorg.conf.d/00-keyboard.conf`.
User environment and `environment.d` settings take precedence. The selected
Niri configuration can override them as usual. This also works with an empty
`xkb` section in the headless build, including Japanese `jp` / `jp106` settings.
The login screen uses XKB with the host defaults for physical keyboard events;
Unicode input supplied by the RDP client is preserved directly.

The remote desktop runs the same XDG autostart entries under `~/.config/autostart/`
and the system XDG configuration directories, honoring user overrides and disabled
entries. When the configured environment selects Fcitx or IBus, that framework
starts on the private display before startup applications launch. Installing a
framework alone does not select it. Disabled autostart overrides are respected,
and duplicate framework entries are skipped. Fcitx receives a private copy of
its configured profile so remote layout changes cannot rewrite the host profile.
IBus receives a private address and discovery file. D-Bus activation receives
the remote display before startup applications launch.
Fcitx5 startup waits for its configured groups to be available while Wayland
continues serving clients. Its ordinary typing, input-method switching, and
native Wayland composition use the existing profile and hotkeys automatically.
The private bus and display remain session-specific; shared user systemd services
are not restarted or given the remote display.

Changes to the remote Fcitx profile last for that remote session. Make persistent
profile changes in the host configuration before starting a fresh remote login.

The RDP bridge takes its physical keyboard map from Niri, including your configured
layouts, variants, and options. It preserves layout switching and updates when
Niri's keyboard configuration changes. On a fresh installation, keep your usual
Niri, input-method, environment, and autostart configuration. No Niri-RDP-specific
language configuration is required.

Log out and start a fresh remote login after upgrading startup settings; a
reconnect preserves the running desktop and its environment.

Review `/etc/niri-rdp/frontend.conf`, `/etc/niri-rdp/headless.kdl`,
`/etc/niri-rdp/waybar.json`, `/etc/niri-rdp/renderer.conf`, and
`/etc/pam.d/niri-rdp`. `NIRI_RDP_LISTEN` accepts a numeric IP address and port,
including bracketed IPv6. To use the standard RDP port on a host where it is free,
set `NIRI_RDP_LISTEN=0.0.0.0:3389`. To retain tunnel-only access, set
`NIRI_RDP_LISTEN=127.0.0.1:33991`. Restart the frontend after changing its config.

```sh
sudo systemctl status niri-rdp-broker.service niri-rdp-frontend.service
sudo journalctl -b -u niri-rdp-broker.service -u niri-rdp-frontend.service --no-pager
ss -ltn 'sport = :33991'
```

The login screen reports **Login denied. Check account and password.** for
authentication or account-policy failures. **Desktop unavailable. Try again.**
means the broker could not start or resume the desktop; check both service
journals for the cause. These startup failures do not consume the login screen's
failed-password attempt limit. Release 14 waits for the private WirePlumber
default-device policy before selecting the remote audio output, fixing a startup
race that previously rejected a correctly authenticated login.
The **Password (length hidden)** field shows eight stars for any nonempty entry;
physical dead keys and Compose sequences produce the completed character.

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
1–100 megabits per second (default 12) as the NVENC bitrate ceiling. Each
connection starts at that ceiling and adapts down toward 1 Mbps when graphics
acknowledgements slow relative to the connection's baseline, stall, or report a
client decoding backlog. It recovers gradually when delivery stays healthy.
Changes apply to the live encoder without resetting the desktop. Reduce the
frame rate or ceiling for limited networks; increasing the ceiling can improve
AVC420 quality when the connection has enough bandwidth. Bitmap fallback does
not use this bitrate controller.

Audio automatically prefers MS ADPCM when the client supports it, reducing
stereo playback traffic from about 1.54 Mbps to 0.39 Mbps before framing. Other
clients receive PCM. Blocks are paced by their exact sample duration and keep
recent capture samples when the source or network stalls. Playback feedback
continues to be tracked after receipt confirmations, including long delays and
block-number wrapping, so an excessive client buffer can drain before current
sound resumes. Recovery may skip sound during congestion. No manual delay or
codec setting is needed. These controls limit accumulating delay; actual mstsc
audio/video presentation still needs the playback check below.

To inspect playback, the frontend journal reports `RDP audio MS ADPCM` or
`RDP audio PCM`, plus `RDP audio client playback delay` during buffer recovery
and `RDP graphics adaptive H.264 target` during video adaptation. Test a clip with visible
claps or simultaneous flash/tone for several minutes, then repeat after a short
network stall. Record whether the offset stays constant or keeps growing; see
the [audio/video acceptance check](acceptance.md). When upgrading from an active
RDP desktop, build first and arrange the service restart from a local terminal
or SSH: restarting the frontend disconnects the client, and stopping the broker
logs out the desktop.

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

The frontend automatically starts `niri-rdp-nvidia-devices.service` first.
It runs `nvidia-modprobe -c 0 -c 255` and `nvidia-modprobe -u -c 0` as root
to prepare `/dev/nvidia0`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, and
`/dev/nvidia-uvm-tools`. Release 13 omitted the control-device minor and only
loaded UVM without explicitly creating its nodes. A headless boot can otherwise
have the driver loaded but lack the devices CUDA needs, causing
`CUDA_ERROR_NO_DEVICE`. The frontend keeps `NoNewPrivileges=yes`, so it cannot
use NVIDIA's setuid fallback to create those nodes itself. The helper also runs
with existing NVIDIA drop-ins from older package releases. Inspect its journal
if device preparation fails:

```sh
sudo journalctl -b -u niri-rdp-nvidia-devices.service --no-pager
```

After reinstalling, reboot and use SSH or a TTY to inspect the journals before
starting a local graphical session or running `nvidia-smi`, FFmpeg, or a GPU
application. Those can create the missing nodes and conceal a boot failure:

```sh
sudo journalctl -b -u niri-rdp-nvidia-devices.service \
  -u niri-rdp-frontend.service --no-pager
systemctl is-active niri-rdp-frontend.service
```

The NVIDIA helper must succeed and the frontend journal must report
`NVENC H.264 preflight passed`. The frontend must be active without a manual
GPU initialization command.

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
