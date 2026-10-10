# Install on a separate Arch test machine

This is an experimental integration for a dedicated remote account. Read
[validation](validation.md) and [acceptance](acceptance.md) before enabling it.
The repository build never installs anything. The commands below deliberately
change **only the separate test machine on which you choose to run them**.
Keep the working GNOME Remote Login machine untouched.

The two packages are `niri-rdp-headless` and `niri-remote-login`. The former
installs its pinned patched Niri as `/usr/lib/niri-rdp/niri-headless`, alongside
stock `/usr/bin/niri`; it installs no local desktop session entry. The latter
installs the bridge, root broker, a dedicated PAM policy and two disabled system
services. No package enables a service, opens a firewall, or edits GNOME/GDM.

## Use the prepared binary artifacts

The portable delivery is `artifacts/installable/`. It contains two built x86_64
packages, their corresponding source bundles, checksums and validation records.
Verify `SHA256SUMS` and inspect the package contents before transferring them to
the separate test installation. These binaries were built against the Arch
library versions recorded in the checkpoints; rebuild on a disposable builder
if the target's ABI differs.

The original `0.1.0` delivery archives predate the NVENC encoder. Rebuild from
the current repository sources using the procedure below to use NVIDIA encoding;
changing an environment variable does not add it to those existing binaries.

On the **separate test machine**, follow its normal full upgrade process and
install the two exact files:

```sh
sha256sum -c SHA256SUMS
sudo pacman -U niri-rdp-headless-26.04.remote1-1-x86_64.pkg.tar.zst \
  niri-remote-login-0.1.0-1-x86_64.pkg.tar.zst
```

Then continue with account/TLS configuration below. This is an experimental
package set; actual PAM/logind boot and mstsc acceptance is still pending.

## Build packages

From the repository root, create source bundles:

```sh
python3 scripts/prepare-release.py
```

This produces `artifacts/release/niri-rdp-headless/` and
`artifacts/release/niri-remote-login/`, including hash-checked PKGBUILDs.
Copy those directories to a disposable Arch build VM. On that VM, perform a
full upgrade and install build dependencies:

```sh
sudo pacman -Syu --needed base-devel rust git pkgconf python seatd libinput \
  libdisplay-info mesa libglvnd wayland libxkbcommon pango cairo pixman \
  pam systemd dbus foot ffmpeg
cd niri-rdp-headless
makepkg
cd ../niri-remote-login
```

Install the built headless package on that **build VM** to satisfy the frontend
package dependency, then run `makepkg` in the frontend directory. Never run
makepkg as root. Do not use `makepkg -si` on the working host. Rust must support
1.94 or newer. Cargo.lock pins both dependency graphs; preparation fetches these
dependencies, and build/check runs offline. A fresh builder needs network access
during preparation. Clean-chroot validation with Arch devtools is preferred
when available. See the [makepkg manual](https://man.archlinux.org/man/makepkg.8.en)
and [Arch clean-chroot workflow](https://wiki.archlinux.org/title/DeveloperWiki:Building_in_a_clean_chroot).

Inspect the package contents using `bsdtar -tf` and record `sha256sum` before
copying them to the separate Niri test installation. Follow its normal full
upgrade process, then install the two exact filenames with `sudo pacman -U`.
Package installation alone does not start remote login.

## Configure the test installation

Create a dedicated test account, or use a pre-existing dedicated account. Do not
add an account currently running GNOME or Niri locally. The broker rejects root,
service accounts, nonmembers of `niri-rdp-users`, and accounts with another
graphical logind session, including a closing one. SSH/TTY administrator access
can remain available for observation.

```sh
sudo systemd-sysusers /usr/lib/sysusers.d/niri-rdp.conf
sudo useradd -m -s /bin/bash niri-remote-test
sudo passwd niri-remote-test
sudo usermod -aG niri-rdp-users niri-remote-test
sudo install -d -m 700 /etc/niri-rdp/tls
sudo openssl req -x509 -newkey rsa:3072 -nodes -days 30 \
  -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
  -keyout /etc/niri-rdp/tls/key.pem \
  -out /etc/niri-rdp/tls/cert.pem
sudo chmod 600 /etc/niri-rdp/tls/key.pem
sudo openssl x509 -in /etc/niri-rdp/tls/cert.pem -noout -fingerprint -sha256
```

Use a trusted certificate for normal use and verify its identity from Windows.
The generated self-signed identity is for the dedicated experiment. systemd
passes the certificate and key through `LoadCredential`; the frontend runs as
`niri-rdp`, while only the broker runs as root. No login password is stored in
configuration, command arguments or logs. Only a single password conversation
is supported; MFA, expired-password changes, Unicode/IME input and general PAM
conversations are unsupported and fail closed.

Review `/etc/pam.d/niri-rdp`, `/etc/niri-rdp/frontend.conf`, and
`/etc/niri-rdp/headless.kdl`. The private desktop launches `foot` and uses a US
keyboard, software llvmpipe rendering, one `HEADLESS-1` output, scale 1 and normal transform. Super+Enter opens
a terminal; Super+Shift+Q exits Niri and should close the remote login session.
There is no persistent-session resume; reconnect creates a fresh login. The fixed
worker forces software rendering for the tested compositor baseline. Optional
NVENC encoding runs in the frontend; it does not require GPU access for the
remote account or change the compositor's rendering mode.

```sh
sudo systemctl daemon-reload
sudo systemctl start niri-rdp-frontend.service
sudo systemctl status niri-rdp-broker.service niri-rdp-frontend.service
ss -ltn 'sport = :33991'
```

The only TCP bind must be `127.0.0.1:33991`; GNOME Remote Login keeps its own
listener. Do not change the bind or take port 3389. The broker uses only
`/run/niri-rdp/broker.sock`. Test failure cleanup before enabling boot startup.

## Optional NVIDIA encoding

The default `NIRI_RDP_ENCODER=bitmap` sends bitmap updates. A rebuilt frontend
supports `NIRI_RDP_ENCODER=nvenc`, which passes captured desktop frames to
FFmpeg's `h264_nvenc` encoder and sends H.264 through the RDP graphics channel
as AVC420. The client must negotiate that graphics/codec support; otherwise the
connection uses bitmap updates. This is host-side encoding. Decoding happens
on the RDP client, whose hardware acceleration depends on its own GPU and client.
The current frontend source builds package release `0.1.0-2`; the archived
`0.1.0-1` frontend binaries contain neither these fixes nor NVENC support.
The frontend converts captured pixels to full-range BT.709 on the CPU before
hardware H.264 encoding; compositor rendering and capture also remain on the
tested software/SHM path.

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

## Windows test via an explicit private tunnel

With SSH already configured on the test machine, run this on Windows:

```powershell
ssh -N -L 127.0.0.1:33991:127.0.0.1:33991 administrator@ARCH_TEST_MACHINE
mstsc.exe niri-test.rdp
```

Create `niri-test.rdp` locally on Windows:

```text
full address:s:127.0.0.1:33991
username:s:niri-remote-test
enablecredsspsupport:i:0
authentication level:i:2
prompt for credentials:i:1
desktopwidth:i:1920
desktopheight:i:1080
```

Broker mode uses library-managed TLS-only RDP. PAM passwords do not supply an
NLA/NTLM backend. Microsoft's [RDP property reference](https://learn.microsoft.com/en-us/azure/virtual-desktop/rdp-properties)
describes `enablecredsspsupport`; the setting above is an explicit experimental
client choice, not a Windows policy change. NLA-required clients/policies are
currently incompatible. **This mstsc credential flow has not been tested**;
whether current Windows sends the required ClientInfo credentials is an open
acceptance gate. Do not report boot login complete if this configuration fails.
Never save the password in the RDP file.

After the authentication/cleanup checks pass on the test machine, enable boot:

```sh
sudo systemctl enable niri-rdp-frontend.service
```

Then perform a separate test-machine reboot and the complete
[acceptance procedure](acceptance.md). A running user service or lingering user
manager does not satisfy the no-local-login requirement.

See [uninstall](uninstall.md) to remove the experiment and
[upgrade](upgrade.md) for pinned-compositor maintenance.
