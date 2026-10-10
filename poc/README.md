# Isolated RDP transport proof of concept

This foreground-only binary reuses published `ironrdp-server = 0.13.0` for RDP
negotiation, TLS/CredSSP authentication, input decoding and bitmap encoding. Its
only display is a deterministic 640×480 synthetic test card. A cyan cross follows
the synthetic pointer; the top band turns green while a key/button is held.
Input remains in memory and cannot control your current desktop.

The endpoint is hardcoded to `127.0.0.1:33990`; there is no bind option and port
3389 is never used. TLS and explicit throwaway credentials are mandatory. It
does not use PAM, launch Niri, create a login session, access DRM or input devices,
write system configuration, or install/start services. Running it does not prove
Niri headless support or login-without-a-local-session. Windows `mstsc.exe`
compatibility remains unverified.

The published 0.13.0 package differs from the researched IronRDP main snapshot,
despite that snapshot retaining the same package version. In particular, published
mouse button callbacks discard the RDP PDU coordinates; this backend retains the
last move position. This needs resolution before a real compositor input bridge.
The published package declares Rust 1.89; `Cargo.lock` pins the tested dependency
graph. The synthetic backend serves fixed-size frames, without resize, audio,
clipboard, graphics acceleration, persistent session resumption or Unicode text composition.

## Build and test

From the repository root, keep Cargo downloads/builds inside the project:

```sh
CARGO_HOME="$PWD/poc/.cargo-cache" cargo build --locked --manifest-path poc/Cargo.toml
CARGO_HOME="$PWD/poc/.cargo-cache" cargo test --locked --manifest-path poc/Cargo.toml
python3 poc/tests/startup.py
```

The unit tests cover full BGRA frame layout, input feedback, extreme pointer
coordinates, distinct extended scancodes, loopback policy and clearing all held
input on failed disconnect/reconnect. Startup tests enforce mandatory TLS identity,
nonempty explicit credentials, rejection of CLI overrides and TLS key logging.

The optional integration test compiles a displayless client using the existing
FreeRDP 3 development libraries and uses existing `openssl`. It creates temporary
self-signed credentials/certificates, verifies specific NLA rejection for a wrong
password, authenticates correctly over TLS/CredSSP, decodes the 640×480 frame,
checks pixel changes from keyboard/pointer input, checks release, reconnects, and
stops the child:

```sh
POC_RDP_BINARY="$PWD/poc/target/debug/niri-rdp-transport-poc" python3 -m unittest tests.test_rdp_transport -v
```

The self-signed certificate bypass belongs only to the integration test client.
This checks the synthetic backend, not Niri, physical monitor power or Windows.

## Manual synthetic test

Create a certificate/key in a private temporary directory, then provide
`POC_CERT`, `POC_KEY`, `POC_USERNAME` and `POC_PASSWORD` through the process
environment. Use a newly generated disposable password, never your system login
password. Environment values are not logged; this prototype installs no tracing
subscriber because upstream diagnostic logs may include credentials.

Run `poc/target/debug/niri-rdp-transport-poc` without arguments and stop it with
Ctrl+C. It refuses `SSLKEYLOGFILE`, missing TLS files, missing credentials and
empty values. A Windows test should use an already authorized private SSH tunnel
to `127.0.0.1:33990`, with `mstsc.exe` targeting the tunnel's Windows loopback port.
Do not change the hardcoded bind to test it. No tunnel or network exposure is
created by this project.
