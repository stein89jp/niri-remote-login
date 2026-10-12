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
`/usr/lib/niri-rdp/niri-headless`. The remote session has its own display and uses your Niri
configuration. A dropped connection preserves the desktop and applications;
reconnect with the same Linux account within 45 seconds of broker-observed
disconnection to resume after fresh authentication. After that grace window,
the broker logs out the desktop; the next login starts a fresh one.
Logout, compositor exit, or broker shutdown ends the desktop. Client-selected resolution and
single-display live resizing are supported. Bitmap transport is the default;
optional host NVIDIA NVENC H.264 encoding is available, with a 60 fps capture
budget, reusable capture buffers and unchanged-frame suppression. The compositor
prefers hardware EGL rendering, including NVIDIA, with software fallback.

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
