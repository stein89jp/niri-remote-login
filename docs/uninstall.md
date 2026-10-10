# Uninstall from the separate test machine

Disconnect the test RDP client first. These commands affect the test installation
only; no uninstaller runs automatically on the working host.

```sh
sudo systemctl disable --now niri-rdp-frontend.service
sudo systemctl stop niri-rdp-broker.service
loginctl list-sessions
sudo pacman -R niri-remote-login niri-rdp-headless
sudo systemctl daemon-reload
```

Verify no `niri-rdp` logind session, compositor, frontend or port 33991 listener
remains. If broker cleanup failed, inspect the session's service, UID and leader
before terminating **that specific niri-rdp test session** with
`sudo loginctl terminate-session SESSION_ID`. Do not terminate GNOME sessions.

Pacman preserves changed configuration as `.pacsave`; inspect it before removal.
TLS identity files in `/etc/niri-rdp/tls` were created manually and are not
package-owned. Remove those files and the remaining project configuration only
after deciding to discard the experiment. Keep backups if rollback is intended.
If you enabled NVIDIA encoding, its copied drop-in
`/etc/systemd/system/niri-rdp-frontend.service.d/nvidia.conf` is also not
package-owned. Remove that file when discarding the experiment, then run
`sudo systemctl daemon-reload` again.
The service user and `niri-rdp-users` group also remain; remove them only if they
are unused. The dedicated human test account/home remains your choice; package
removal never deletes user data or accounts.

Stock Niri, GNOME Remote Login, GDM, their PAM files and system settings are
untouched by these packages. No replacement of stock Niri is required for either
installation or removal.
