# Upgrade and patched Niri maintenance

Treat the bridge, broker and headless compositor as one tested release. Preserve
the old combined package (or both split package files), source bundles, checksums, configuration, checkpoint
and machine acceptance record before upgrading the **separate test machine**.

Current maintained base: Niri PR #3953 at
`0be60b742762905b7bee1ea67bb5a2fb00859bf0`, Smithay
`ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef`, and the patch under
`compositor/patches/`. `compositor/pins.json` records source/lock hashes and the
comparison with PR #3800. IronRDP is pinned to
`bf019ca502d1e52616853a5ded68c885cec1e322` by `bridge/Cargo.toml` and its lockfile.
No floating PR branch, git `main` or implicit dependency update is a release.

For a new compositor base, download its exact revision in an isolated checkout,
review the upstream diff and reapply the local patch. Reevaluate startup backend,
render clock, output bounds/lifecycle, SHM capture, rejected buffers and privileged
virtual input. Regenerate the lockfile intentionally, record the new source and
patch hashes, then run the retained tests and disposable real-desktop integration.
Build the combined package (or both split packages) and inspect its contents. Repeat all separate-machine
acceptance checks, including Windows and monitor power-off, before promoting it.
Stock Niri's version changing does not justify automatically updating this fork.

On the test installation, close remote sessions, stop the frontend and broker,
follow Arch's full-upgrade practice for repository libraries, and install the new
combined package file with `pacman -U` (or both files for a split build). Review `.pacnew`/`.pacsave` changes;
never blindly replace the dedicated PAM policy. Reload systemd, start the
frontend and repeat rejection/cleanup checks before restoring boot startup.
Arch ABI changes require rebuilding local packages against the new libraries.
Avoid partial upgrades or workarounds involving library symlinks.

Rollback: stop both services, reinstall the retained combined package (or both split files), restore the
matching configuration, reload units and rerun the smoke checks. Rolling back
only a binary after an ABI-changing Arch upgrade is not guaranteed to work;
retain a disposable VM snapshot as the stronger recovery method.

When upstream Niri provides all required behavior, create a new tested headless
package from the upstream revision without the now-unnecessary patch. Keep the
private binary path and broker contract until the whole boot/capture/input/
cleanup matrix passes. Then a separately reviewed release can depend on stock
Niri and change the fixed worker path. No package performs this migration or
replaces `/usr/bin/niri` automatically.

## Rebuild the installed combined package

On the intended host, update the checkout and run `makepkg -si` again from
`packaging/arch/niri-rdp`. The local sources are copied under content-addressed
names so changed files cannot accidentally reuse stale cached inputs. Stop both
services before updating and review `.pacnew` files before restarting them.
Existing TLS files are not package-owned and are preserved by first-start setup.
When migrating from the old loopback configuration, pacman preserves your edited
`frontend.conf`; choose `NIRI_RDP_LISTEN` explicitly if you want direct access.

The remote desktop now loads your existing Niri configuration automatically,
with a private DankMaterialShell/Waybar startup helper. Existing optional
`~/.config/niri-rdp/config.kdl` files still take precedence; keep one for remote
customization, or move it aside to use your usual Niri settings on the next login.
Accounts without either user configuration get the system defaults, which start
a bar and bind Super+D to Fuzzel. Pacman keeps
an edited `/etc/niri-rdp/headless.kdl` and may place the new defaults in
`/etc/niri-rdp/headless.kdl.pacnew`. Merge the bar `spawn-at-startup` line and
updated `binds` into your existing remote configuration before reconnecting.
If you use `~/.config/niri-rdp/config.kdl`, that file takes precedence over the
system defaults and must be updated separately. Existing remote sessions need a
disconnect and fresh login to run new startup commands. The remote compositor
has its own Wayland runtime and D-Bus bus; shared systemd desktop services are
not started or given its environment.
