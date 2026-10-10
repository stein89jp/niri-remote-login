# Upgrade and patched Niri maintenance

Treat the bridge, broker and headless compositor as one tested release. Preserve
the old two package files, source bundles, checksums, configuration, checkpoint
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
Build both packages and inspect their contents. Repeat all separate-machine
acceptance checks, including Windows and monitor power-off, before promoting it.
Stock Niri's version changing does not justify automatically updating this fork.

On the test installation, close remote sessions, stop the frontend and broker,
follow Arch's full-upgrade practice for repository libraries, and install both
new exact package files with `pacman -U`. Review `.pacnew`/`.pacsave` changes;
never blindly replace the dedicated PAM policy. Reload systemd, start the
frontend and repeat rejection/cleanup checks before restoring boot startup.
Arch ABI changes require rebuilding local packages against the new libraries.
Avoid partial upgrades or workarounds involving library symlinks.

Rollback: stop both services, reinstall both retained package files, restore the
matching configuration, reload units and rerun the smoke checks. Rolling back
only a binary after an ABI-changing Arch upgrade is not guaranteed to work;
retain a disposable VM snapshot as the stronger recovery method.

When upstream Niri provides all required behavior, create a new tested headless
package from the upstream revision without the now-unnecessary patch. Keep the
private binary path and broker contract until the whole boot/capture/input/
cleanup matrix passes. Then a separately reviewed release can depend on stock
Niri and change the fixed worker path. No package performs this migration or
replaces `/usr/bin/niri` automatically.
