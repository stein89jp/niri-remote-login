# Niri Remote Login for Arch Linux

A remote desktop host with a dedicated headless Niri compositor, an IronRDP
frontend, and a PAM/logind session broker. Install the complete host from one
build folder on an up-to-date **Arch Linux x86_64** machine:

```sh
sudo pacman -Syu --needed base-devel git
git clone https://github.com/stein89jp/niri-remote-login.git
cd niri-remote-login/packaging/arch/niri-rdp
makepkg -si
sudo usermod -aG niri-rdp-users "$USER"
sudo systemctl enable --now niri-rdp-frontend.service
```

Open Windows Remote Desktop (`mstsc.exe`) and connect to `HOST:33991`.
Use your Linux account name and password. The frontend listens directly on the
network; no SSH tunnel is required. If the client omits password credentials,
a login screen inside the RDP connection authenticates through PAM before
starting the desktop. Windows policies requiring NLA are currently incompatible.
Actual Windows interoperability still needs validation.

For a new remote account, replace the `usermod` command with
`sudo niri-rdp-setup --create-user niri-remote`. **If using your current account,
log out of its local graphical session before connecting remotely.** The broker
rejects simultaneous local graphical sessions for the same account.

The package creates the service account and authorization group through Arch's
systemd hooks. The broker generates a self-signed certificate on first start,
checks existing certificates, and preserves them across restarts. Verify its
identity from the client; see the [installation guide](docs/install.md).
Allow TCP 33991 in your existing firewall if necessary. The listener address and
port are configurable in `/etc/niri-rdp/frontend.conf`.

Stock `/usr/bin/niri` stays alongside the patched
`/usr/lib/niri-rdp/niri-headless`. The remote session has its own desktop and
configuration. Reconnect creates a new session. Client-selected resolution and
single-display live resizing are supported. Bitmap transport is the default;
optional host NVIDIA NVENC H.264 encoding is available.

This remains **experimental**. Disposable Niri/RDP integration has demonstrated
changing application frames, keyboard/pointer input, authentication rejection,
reconnect, and cleanup. Real installed PAM/logind login, cold boot, Windows
`mstsc`, and monitor-off acceptance remain pending. Installing the package does
not establish those results.

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

The separate-package source export is still available:

```sh
python3 scripts/prepare-release.py
```

The binary delivery archive under `releases/arch-x86_64/0.1.0/` is a preserved old
snapshot. It predates resolution support, NVENC, direct listening, automatic TLS
setup, and the login screen. Use a current source build for the workflow above.

Run Python tests with:

```sh
python3 -m unittest discover -s tests -v
```

`make check` also runs the Rust and broker checks. Disposable transport/desktop
integration fixtures live under `scripts/` and `lab/`; they do not start installed
host services or replace the actual-machine acceptance procedure.
