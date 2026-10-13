# Private repository checkpoint — 2026-10-10

Repository cleanup on 2026-10-13 removed the obsolete 0.1.0 delivery archive and
its checksum from the current source tree. They remain available in Git history.
The inventory and publication account below are preserved as historical records.
Current binary packages, matching source bundles, and checksums belong in GitHub
Releases.

The project now uses Git on branch `main`, with SSH remote
`git@github.com:stein89jp/niri-remote-login.git`.
The user created the empty private GitHub repository after SSH authentication
was verified. The connected GitHub app reports `visibility: private` and push
permission. No API credential or software installation was needed for the push.

The initial import preserves the original tests, source snapshots, exact
dependency pins, maintained compositor patch, package recipes and decision
checkpoints. Historical inventories `000-foundations.json` and
`001-integration.json` remain unchanged snapshots. `002-private-repository.json`
records this publication's source inventory.

The validated installable delivery is copied unchanged into
`releases/arch-x86_64/0.1.0/niri-remote-login-arch-0.1.0.tar.gz`, with its original
checksum. Archive SHA-256:
`dec58a9700b5eeecd5332a2c4b4a9ced35974c6907dabf1ca63cd09e95b04754`.
This modest binary snapshot is tracked directly so cloning the private
repository provides the already-built packages without needing a host build or
additional release API authentication. Build caches, private runtime directories,
downloaded build dependencies, generated executables and test credentials remain
ignored; only the intentional final delivery archive is tracked.

Publication checks:

```sh
git init --initial-branch=main
git ls-remote git@github.com:stein89jp/niri-remote-login.git
python3 scripts/checkpoint.py 002-private-repository
git add -- .gitignore LICENSE Makefile README.md bridge broker checkpoints \
  compositor docs lab packaging poc prototype research scripts tests releases
git diff --cached --check
git commit -m 'Import packaged Niri Remote Login implementation and validation checkpoints'
git remote add origin git@github.com:stein89jp/niri-remote-login.git
git push -u origin main
git ls-remote origin refs/heads/main
git status --short
```

Before commit, candidate source files were checked for private-key/API-token
markers and unintended ELF executables. The exact Niri source and delivery
archive hashes match their pins. Package behavior did not change in this
publication; the earlier integration and unit results remain the evidence.
The staged import has 98 files. `git diff --cached --check` reports pre-existing
blank-at-EOF whitespace and blank unified-diff context lines inside the pinned
compositor patch. These are preserved to avoid changing baseline tests or the
validated patch hash; this is not recorded as a clean whitespace check.

Remaining work is unchanged: actual PAM/logind authentication and teardown,
Windows mstsc from cold boot with no local login, monitor-off/disconnected, and
real-session logout/compositor/frontend/broker crash cleanup need separate test
machines. No packages, services, accounts, GNOME settings or system configuration
were installed or modified on the working host.
