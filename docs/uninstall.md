# Uninstall the Arch remote desktop host

Save your remote desktop work, then run these commands from a local terminal or
an independent SSH session. Stopping the services ends the remote desktop.

```sh
sudo systemctl disable --now niri-rdp-frontend.service
sudo systemctl stop niri-rdp-broker.service
loginctl list-sessions
sudo pacman -R niri-remote-login
sudo systemctl daemon-reload
```

Verify no `niri-rdp` logind session, compositor, frontend or port 33991 listener
remains. If broker cleanup failed, inspect the session's service, UID and leader
before terminating **that specific niri-rdp session** with
`sudo loginctl terminate-session SESSION_ID`. Do not terminate GNOME sessions.

Older or manual installations can leave portal files behind, causing a
`conflicting files` error when reinstalling. Pacman normally removes these files
when uninstalling the package. If either remains, check its ownership:

```sh
pacman -Qo /usr/lib/niri-rdp/session-portals \
  /usr/share/xdg-desktop-portal/niri-rdp-portals.conf
```

If no package owns the remaining files, remove them before reinstalling:

```sh
sudo rm -f -- /usr/lib/niri-rdp/session-portals \
  /usr/share/xdg-desktop-portal/niri-rdp-portals.conf
```

If another package owns either file, resolve that package's ownership before
removing the file.

Pacman preserves changed configuration as `.pacsave`; inspect it before removal.
This includes `/etc/niri-rdp/broker.conf` (left as
`/etc/niri-rdp/broker.conf.pacsave` if you changed the grace period) and the
other `/etc/niri-rdp` files. Remove the broker settings when discarding the
experiment:

```sh
sudo rm -f -- /etc/niri-rdp/broker.conf /etc/niri-rdp/broker.conf.pacsave
```

If you copied the fail2ban example, `/etc/fail2ban/filter.d/niri-rdp.conf` and
`/etc/fail2ban/jail.d/niri-rdp.local` are not package-owned. Remove both and run
`sudo fail2ban-client reload` (or disable fail2ban if nothing else uses it).

TLS identity files in `/etc/niri-rdp/tls`, including the `generated` marker that
records a self-signed certificate for renewal, are generated on first start (or
provided manually) and are not package-owned. Remove those files and the
remaining project configuration only after deciding to discard the experiment. Keep backups if rollback is intended.
If you enabled NVIDIA encoding, its copied drop-in
`/etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf` is also not
package-owned. Remove that file when discarding the experiment, then run
`sudo systemctl daemon-reload` again. If you keep it for a reinstall, copy the
new package's drop-in over it afterwards as described in
[install](install.md#optional-nvidia-encoding); older copies lack the device
allow-list that NVENC now needs.
The NVIDIA device-preparation helper is package-owned and is removed by pacman.
If you installed the earlier manual workaround, also remove its
local `/etc/systemd/system/niri-rdp-nvidia-devices.service` and
`/etc/systemd/system/niri-rdp-frontend.service.d/nvidia-devices.conf`; pacman
does not own those local files.
The private PAM failure tally can optionally be removed with
`sudo rm -rf /run/faillock-niri-rdp`. It is a tmpfs directory and is also
cleared at reboot.
The service user and `niri-rdp-users` group also remain; remove them only if they
are unused. The dedicated human test account/home remains your choice; package
removal never deletes user data or accounts.

For the legacy split installation, also remove `niri-rdp-headless` after removing
`niri-remote-login`.

Stock Niri, GNOME Remote Login, GDM, their PAM files and system settings are
untouched by these packages. No replacement of stock Niri is required for either
installation or removal.
Your Niri configuration, input-method profiles, `environment.d`, and XDG autostart
entries are preserved and reused when you reinstall.
To reinstall your current checkout with new fixes, follow [install](install.md)
using `makepkg -fsi` from its `packaging/arch/niri-rdp` directory. Removing the
installed package does not remove old package archives from that build directory;
the `-f` ensures those archives are rebuilt from the current source.
