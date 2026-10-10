# Functionality and NVIDIA review — 2026-10-10

The reviewed source fixes the failures below and adds optional NVIDIA host
encoding. The existing 0.1.0 binary archives are unchanged; rebuild the frontend
package from this source to use the fixes. The rebuilt frontend package is
`niri-remote-login-0.1.0-2`; the compositor package is unchanged. Installation steps are in
[install.md](install.md#optional-nvidia-encoding).

| Failure | Resulting change | Regression evidence |
| --- | --- | --- |
| A Wayland FD attached to a later fragment of a broker reply was discarded by ordinary reads | Receive every fragment with `recvmsg`, preserving and validating SCM_RIGHTS | Fragmented reply with FD on the final fragment |
| Disconnect closed the broker lease before held-input releases reached Niri | Keep the lease until the capture worker completes release and compositor acknowledgment; dropping the last desktop also stops its worker | Lease-order test and real-app held-input disconnect/reconnect |
| Removing an unrelated Wayland output ended the selected desktop | Track the selected output; fail on its removal or unsupported runtime scale/transform changes | Selected/unrelated output regression |
| Authorized accounts with more than 64 supplementary groups were rejected | Resize and retry `getgrouplist` using its reported required size | Membership beyond index 63 and fail-closed lookup tests |
| Cleanup waited two seconds for a compositor already reaped by the broker | Handle `ECHILD` immediately while still killing remaining descendants | Native process cleanup tests |
| `make check` forced a missing bridge Cargo cache | Use the ordinary Cargo cache when a project-local cache is absent; allow overrides for both projects | Complete `make check` with the prepared bridge cache |

Same-width bitmap conversion now copies rows instead of individual pixels,
retaining row-padding, vertical-inversion, color-channel and alpha normalization.

## Host encoding

The baseline had no NVIDIA video encoding: it captured SHM and sent bitmap
updates, with the compositor forced to llvmpipe. The new
`NIRI_RDP_ENCODER=nvenc` path links FFmpeg's `h264_nvenc`, converts captured
BGR pixels to full-range BT.709 NV12 on the CPU, and sends hardware-encoded
H.264 AVC420 through IronRDP's graphics pipeline. The default
is `bitmap`. Clients without negotiated AVC420 support receive bitmap updates;
an explicit NVENC request fails startup if hardware initialization or an actual
preflight encode fails.

Encoding runs on blocking tasks outside the network/input thread. The adapter
limits graphics frames in flight, skips encoding for suppressed/minimized
output, and retains pending tasks across async cancellation. Decoder capability
re-advertisement invalidates old surfaces and pending reference frames; recovery
and dropped encoded frames require an IDR. Failed encoding ends the stream
without polling a completed task again. Tests cover each of these lifecycle
contracts.

The encoder pads coded dimensions to multiples of 16, replicating edge pixels.
The 1920×1080 desktop therefore uses a 1920×1088 H.264 bitstream; its RDP
surface and region mask keep the displayed height at 1080. This follows
[Microsoft's AVC420 stream requirement](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/5f12c20e-2ea1-4ad1-a2a0-019ee3893731).

The explicit pixel conversion also follows
[RDP's full-range BT.709 requirement](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/954d7546-6873-4466-95c8-20a7569c43e5).
FFmpeg's NVENC packed-RGB input would otherwise use limited-range BT.601.
Tests check decoded YUV values directly so an automatic decoder range conversion
cannot conceal an encoder mismatch.

The opt-in packaged systemd drop-in exposes NVIDIA devices to the frontend.
The Niri worker still renders through llvmpipe and uses SHM capture. This is
hardware video encoding, with CPU capture/copy work remaining. NVIDIA calls
the host encoder NVENC and the video decoder NVDEC; the decoder runs on the
remote RDP client. See [NVIDIA's FFmpeg guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/ffmpeg-with-nvidia-gpu/index.html).

## Validation

`make check BRIDGE_CARGO_HOME=/tmp/niri-rdp-bridge-cargo` passed, and the final
bridge rerun brings validation to 48 tests/native groups: 7 process-isolation
tests, 5 PoC Rust tests, 4 startup checks, 10 broker groups and 22 bridge tests.
The opt-in legacy PoC transport test is excluded
from that count. Socket tests ran outside the execution sandbox after the
sandbox denied their temporary Unix sockets. No host service was changed.

Release bridge/probe builds and strict all-target Clippy (`-D warnings`) passed.
Compositor lifecycle/patch checks, systemd unit validation, notice generation,
and disposable source-bundle generation also passed.

The bitmap real-desktop integration passed authentication rejection, changing
application pixels, keyboard/pointer/button feedback, held-input disconnect
releases, reconnect, and compositor crash with the frontend staying available.

The real NVENC probe passed on an NVIDIA GeForce RTX 4070 Ti with driver
615.71.09: 30 changing 1920×1080 desktop frames encoded and decoded without errors,
with aligned 1920×1088 H.264 4:2:0 output, full range and BT.709 metadata.
A release probe measured 6.79 ms per frame (147.2 synthetic frames/s), including
pixel conversion and encoding, excluding capture, input, transport, client decoding
and display. This measures the encoder's capacity on that GPU, not end-to-end
desktop frame rate or latency. The bridge's capture limit remains 30 fps.

The final release passed full NVENC-over-RDP validation through TLS with a
namespace test broker and a FreeRDP client. The test requires actual AVC420
decoding and five authenticated sessions using NVENC; it covers wrong-password
rejection, two application/input roundtrips, held-input release on disconnect,
reconnect, and compositor crash while the frontend remains available. All five
Wayland FD broker leases closed. A second run with the same NVENC configuration
and a client without graphics support passed through bitmap fallback, requiring
zero AVC420 activations. These tests use a test broker instead of Linux PAM.
The local receipts are `artifacts/nvenc-tls-integration.json` and
`artifacts/nvenc-bitmap-fallback-integration.json`.

The NVIDIA test namespace retains private PID, user, network and runtime
namespaces. It exposes NVIDIA devices and read-only sysfs for CUDA discovery;
it exposes no physical input devices, DRM devices or local display sockets.

## Remaining actual-machine gates

Real PAM/logind cold-boot login, installed service-account GPU access,
Windows mstsc behavior, physical-monitor power-off/disconnection, and
end-to-end latency/bitrate measurements still need the separate-machine
[acceptance procedure](acceptance.md). TLS-only broker authentication remains
incompatible with NLA-required client policies. Client-selected initial size and
live single-display resize are implemented; see the
[resolution contract](../bridge/README.md#dynamic-resolution). Arbitrary
keyboard layouts, Pause/E1, Unicode/IME, clipboard and audio retain the existing
documented limitations.

The frontend now links the distribution's FFmpeg libraries. Package metadata
and third-party notices record that native dependency and its build provenance;
original integration source remains MIT. The current
[Arch FFmpeg package](https://archlinux.org/packages/extra/x86_64/ffmpeg/) is
GPL-3.0-only; [FFmpeg's license page](https://ffmpeg.org/legal.html) describes
how its build configuration affects its license.
