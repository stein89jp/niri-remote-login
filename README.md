# Niri Remote Login for Arch Linux

A remote desktop host with a dedicated headless Niri compositor, an IronRDP
frontend, and a PAM/logind session broker. Install the complete host on an
up-to-date **Arch Linux x86_64** machine.

This is an **open-source project**. Original integration code is MIT-licensed;
upstream components retain their respective licenses. See the
[licensing and source-availability guide](docs/licensing.md).

## Install a binary release

Download the `niri-remote-login-<version>-<pkgrel>-x86_64.pkg.tar.zst` package
from [GitHub Releases](https://github.com/stein89jp/niri-remote-login/releases).
From the directory containing the downloaded package, run:

```sh
sudo pacman -Syu
sudo pacman -U ./niri-remote-login-1.6.0-1-x86_64.pkg.tar.zst
```

The example uses version `1.6.0-1`; replace the filename if you downloaded a
different release. Pacman installs the package and its required runtime
dependencies. Continue with the account authorization and service setup below.

## Build from source

To build the package yourself instead, run:

```sh
sudo pacman -Syu --needed base-devel git
git clone https://github.com/stein89jp/niri-remote-login.git
cd niri-remote-login/packaging/arch/niri-rdp
makepkg -si
```

## Authorize an account and start the service

After either installation method, authorize your Linux account and enable the
frontend:

```sh
sudo usermod -aG niri-rdp-users "$USER"
sudo systemctl enable --now niri-rdp-frontend.service
```

Open Windows Remote Desktop (`mstsc.exe`) and connect to `HOST:33991`.
Use your Linux account name and password. The frontend listens directly on the
network; no SSH tunnel is required. If the client omits password credentials,
a login screen inside the RDP connection authenticates through PAM before
starting the desktop. Windows policies requiring NLA are currently incompatible.
The user has confirmed the Windows 10 mstsc desktop and smooth cursor movement;
the detailed client acceptance checks are recorded separately.

For a new remote account, replace the `usermod` command with
`sudo niri-rdp-setup --create-user niri-remote`. **If using your current account,
log out of its local graphical session before connecting remotely.** The broker
rejects simultaneous local graphical sessions for the same account.

The package creates the service account and authorization group through Arch's
systemd hooks. The broker generates a self-signed certificate on first start and
renews it automatically shortly before it expires, which changes its fingerprint.
Certificates you provide are checked and never replaced. Verify the identity from
the client; see the [installation guide](docs/install.md#connect-with-windows-remote-desktop).
Allow TCP 33991 in your existing firewall if necessary. The listener address and
port are configurable in `/etc/niri-rdp/frontend.conf`.

Stock `/usr/bin/niri` stays alongside the patched
`/usr/lib/niri-rdp/niri-headless`. The remote session has its own display and uses your Niri
configuration. A dropped connection preserves the desktop and applications;
reconnect with the same Linux account within 45 seconds of broker-observed
disconnection to resume after fresh authentication. After that grace window
(`grace_seconds` in `/etc/niri-rdp/broker.conf`), the broker logs out the
desktop; the next login starts a fresh one.
Logout, compositor exit, or broker shutdown ends the desktop. Remote desktops
share `/tmp` with the account's other logins. The broker records logins,
rejections and logouts with the client address in the journal; an optional
fail2ban example is included but not enabled. Client-selected resolution and
single-display live resizing are supported. Bitmap transport is the default;
optional host NVIDIA NVENC H.264 encoding is available, with a 60 fps capture
budget, reusable capture buffers and unchanged-frame suppression. The compositor
prefers hardware EGL rendering, including NVIDIA, with software fallback.

Version 1.6.0 fixes Windows mstsc audio that played slowly and drifted behind the video: 44.1 kHz stereo PCM is now preferred. Version 1.5.6 fixed choppy remote motion. The headless compositor now presents
frames on a fixed refresh clock and keeps sending frame callbacks while the
pointer moves, and the H.264 frame window grows with the connection delay
instead of staying at two frames, which had held internet sessions to about
20–30 fps. Version 1.5.5 rebased the compositor onto current upstream Niri and added
microphone redirection, image and HTML clipboard, login-screen time limits with
per-address backoff, a failed-login lockout private to niri-rdp, automatic TLS
certificate renewal, a hardened frontend service, a login audit journal and a
per-connection summary line. NVENC sends fewer keyframes, steps its bitrate
through fixed levels and uses GPU capture by default; capture copies less and
repeated cursor shapes are cached. Earlier releases added client-local cursor
motion, damage-aware capture, explicit remote DPI scaling and optional AVC444;
see [performance controls](docs/performance.md). Since 2026-10-18, playback
drops one 20 ms block only when an estimated client-queue surplus is earned, so
the delay cannot keep growing. Since 2026-10-20 playback prefers 44.1 kHz
stereo PCM, resampled from the 48 kHz capture, because Windows 10 mstsc played
48 kHz PCM about 9% slow (25 s behind video after 5 minutes); 48 kHz stereo PCM
remains the fallback (`NIRI_RDP_AUDIO_RATE=48000` prefers it again). Mono PCM
(`NIRI_RDP_AUDIO_CHANNELS=mono`) is opt-in because it sounded pitched-up and
robotic in Windows 10 mstsc on 2026-10-20. The user confirmed a 20-minute
continuous YouTube playback in actual Windows 10 mstsc as clean and in sync on
2026-10-20; longer runs, constrained bandwidth and repeated reconnects remain
separate acceptance checks (see
[session audio](bridge/README.md#session-audio)). Upgrading from 1.5 needs a fresh remote
desktop and may need a `.pacnew` merge; see [upgrade](docs/upgrade.md#rebuild-the-installed-combined-package).

The clipboard carries Unicode text, HTML and images in both directions. Files are
not supported: copying files in Windows leaves the Linux clipboard unchanged.

Microphone redirection: with **Record from this computer** enabled in mstsc, the
Windows microphone appears in the remote desktop as the default source
**Remote microphone (RDP)** (48 kHz mono, about 0.8 Mbit/s upload). Windows
records only while a remote application listens. `NIRI_RDP_MICROPHONE=off`
disables it; see [remote microphone](docs/install.md#remote-microphone). The
microphone path is separate from playback, and the accepted PCM playback
configuration is unchanged.

The remote desktop uses your existing Niri configuration, including its bindings,
appearance and startup applications. It starts DankMaterialShell privately when
installed, otherwise a Niri Waybar, if your startup commands have not started a
bar. Accounts without a Niri configuration receive a terminal and bar with
Super+Enter for a terminal and Super+D for Fuzzel. An optional
`~/.config/niri-rdp/config.kdl` overrides these defaults; see the
[configuration guide](docs/install.md#configuration-and-troubleshooting).
See [upgrade instructions](docs/upgrade.md) for existing remote overrides and
`headless.kdl.pacnew` files.

The remote desktop inherits the host's configured input-method and locale
settings and runs the account's ordinary XDG autostart entries on its private
display and bus. It uses Niri's keyboard map rather than forcing a layout.
Existing Fcitx5, IBus, and other configured input methods keep their usual setup.

This remains **experimental**. Disposable Niri/RDP integration has demonstrated
changing application frames, keyboard/pointer input, authentication rejection,
reconnect, and cleanup. The user confirmed improved picture quality, reduced CPU
usage with GPU capture and smooth Windows 10 mstsc cursor movement after
reinstalling. Detailed PAM/logind policy, cold boot, monitor-off and long-running
playback acceptance remain in the [actual-machine checklist](docs/acceptance.md).

- [Installation and optional NVIDIA encoding](docs/install.md)
- [Actual-machine acceptance](docs/acceptance.md)
- [Uninstall](docs/uninstall.md) and [upgrade/rollback](docs/upgrade.md)
- [Compositor pins and patches](compositor/README.md)
- [Wayland/RDP bridge](bridge/README.md) and [session broker](broker/README.md)
- [Architecture](docs/architecture.md) and [validation record](docs/validation.md)

## Maintainer workflow

The combined PKGBUILD snapshots checksum-verified source files from the complete
checkout. Export a portable source package with `makepkg --allsource` from the
build folder. Dependency fetching happens during preparation; compilation and
Rust package checks use pinned lockfiles offline. Never run makepkg as root.

`make check` runs the Python, broker and bridge tests offline, after
`make lint` (`cargo fmt --check` and Clippy with `-D warnings` for the bridge).
The historical `poc/` transport prototype is no longer part of `make check`, and
nothing it builds is packaged; run it separately with `make check-poc` (and then
`make check-transport` for its opt-in FreeRDP interop test).
Cargo dependencies are looked up in `bridge/.cargo-cache` (and `poc/.cargo-cache`
for `check-poc`) first. If those are missing or empty, `$CARGO_HOME`, then the
makepkg cache under `packaging/arch/niri-rdp/src/cargo-home`, then `~/.cargo`
are used. To fill the project-local caches over the network, run `make fetch`
once. You can still name a cache directly with `BRIDGE_CARGO_HOME=` or
`POC_CARGO_HOME=`. GitHub Actions runs `make fetch` and `make check` in an Arch
Linux container (`.github/workflows/check.yml`); lab, namespace and GPU
integrations are not run there.

The separate-package source export is still available:

```sh
python3 scripts/prepare-release.py
```

It bundles the packaged sources, tests, `prototype/`, `lab/` and checkpoints;
the historical `poc/` prototype and the local `research/` copies are left out.

Publish binary packages, matching source bundles, and checksums through GitHub
Releases, together with dependency-source information and license notices; see
[the distribution requirements](docs/licensing.md#maintaining-binary-releases).
Local release downloads, build caches, package staging trees, and raw
validation evidence are ignored. Historical checkpoints record earlier source
inventories and test results; the obsolete 0.1.0 binary delivery is retained in
Git history, and the 0.1.0-era validation record is in
[docs/archive](docs/archive/validation-0.1.md). The
[release history](docs/upgrade.md#release-history) maps the "release N" names in
older notes to package versions.

Run Python tests with:

```sh
python3 -m unittest discover -s tests -v
```

`make check` also runs the Rust and broker checks. Disposable transport/desktop
integration fixtures live under `scripts/` and `lab/`; they do not start installed
host services or replace the actual-machine acceptance procedure.

## Author
Niri Remote Login was created and is maintained by [Rikard Stein](https://github.com/stein89jp).

The original integration code is released under the MIT License. Upstream components retain their respective licenses and copyrights. See [Licensing and Source Availability](docs/licensing.md) for details.

## License

The [MIT license](LICENSE) covers original integration source. The patched Niri
compositor is GPL-3.0-or-later, and IronRDP is MIT OR Apache-2.0. Packaged frontend
binaries linked to Arch's GPL-enabled FFmpeg require GPL-compliant distribution.
License texts, third-party notices, matching source bundles, and dependency-source
links are available with the [release downloads](https://github.com/stein89jp/niri-remote-login/releases).
See [docs/licensing.md](docs/licensing.md) for component scope and source access.
