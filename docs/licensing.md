# Licensing and source availability

Niri Remote Login is an open-source project containing components with different
licenses. The top-level [LICENSE](../LICENSE) grants the MIT license for original
integration source. It does not replace upstream copyright notices or licenses.
Vendored and reference upstream files retain their original licenses. The patched
Niri compositor remains GPL-3.0-or-later; IronRDP is MIT OR Apache-2.0. Protocol
XML files retain the copyright and license notices embedded in them. These
component notes are kept here and in the README so the root license contains
the standard MIT text.

## Component licenses

| Component | License and scope |
| --- | --- |
| Original bridge, broker, helpers, tests, and documentation | MIT for original source, with upstream notices preserved where applicable. |
| Patched Niri compositor | GPL-3.0-or-later. The original source archive includes its license; the installed compositor package carries it too. |
| IronRDP | MIT OR Apache-2.0. The package's third-party notices include both license texts. |
| Wayland protocol XML | The copyright and permission notices embedded in each XML file apply. |
| Other Cargo dependencies | Their individual licenses apply, including MIT, Apache-2.0, BSD, ISC, MPL-2.0, and others listed in the package notices. |
| FFmpeg used by the published Arch frontend | Arch package `2:9.0.2-2`, licensed GPL-3.0-only. The recorded build enables GPL components and does not enable `--enable-nonfree`. |

The published frontend dynamically links FFmpeg's codec, utility, scaling, and
resampling libraries. Distribution of that frontend must comply with the GPL
requirements of the linked build. Original integration source retains its MIT
grant, and separate independent components retain their own licenses. The
assembled installer should be described using all its applicable licenses.

The installer includes license texts and dependency notices under
`/usr/share/licenses/niri-remote-login/` and
`/usr/share/licenses/niri-rdp-headless/`. Its `.PKGINFO` declares MIT,
GPL-3.0-only, and GPL-3.0-or-later. The same notices are available as a separate
release asset below.

Optional NVIDIA acceleration uses driver libraries supplied separately by the
host. The installer bundles neither NVIDIA drivers nor FFmpeg libraries.
External drivers and the Windows RDP client remain subject to their own terms.

## Source for the published installer

Release `v1.5` carries the `1.5-1` installer, built from the source tagged
`v1.5`. It includes the performance and picture-quality improvements,
bidirectional text clipboard and the confirmed mstsc cursor fix, along with the
README's author section and MIT copyright notice for Rikard Stein.

Download these files from the [release](https://github.com/stein89jp/niri-remote-login/releases/tag/v1.5):

- [Matching source-build bundle](https://github.com/stein89jp/niri-remote-login/releases/download/v1.5/niri-remote-login-1.5-1.src.tar.gz).
- [Dependency-source index](https://github.com/stein89jp/niri-remote-login/releases/download/v1.5/DEPENDENCY-SOURCES.json).
- [License texts and third-party notices](https://github.com/stein89jp/niri-remote-login/releases/download/v1.5/THIRD-PARTY-NOTICES.txt).
- [README with author attribution](https://github.com/stein89jp/niri-remote-login/releases/download/v1.5/README.md) and [MIT license](https://github.com/stein89jp/niri-remote-login/releases/download/v1.5/LICENSE), also included in the installer and source bundle.
- [SHA256SUMS](https://github.com/stein89jp/niri-remote-login/releases/download/v1.5/SHA256SUMS), covering the installer and these source/notice assets.

The source-build bundle is the preserved `makepkg --allsource` output. It contains
the pinned Niri source archive, maintained patch, original integration source,
Cargo lockfile, build scripts, configuration, and package recipe. The recipe's
SHA-256 matches `pkgbuild_sha256sum` in the installer's `.BUILDINFO`. The generated
source index records that identity and the source archive's checksum.

Extract the bundle and build as an unprivileged user on a prepared Arch builder:

```sh
tar -xzf niri-remote-login-1.5-1.src.tar.gz
cd niri-remote-login
makepkg
```

The recipe declares the required build/runtime packages. Its preparation step
fetches locked Cargo dependencies. Compilation and checks then run offline.
See [the README source-build instructions](../README.md#build-from-source) for
the ordinary checkout build.

## Dependency sources

The project source bundle does not contain a vendored copy of every dependency.
`DEPENDENCY-SOURCES.json` lists exact source-download URLs for both complete
Cargo lockfiles, including optional, build, test, and other-platform packages.
Registry archives have the checksums recorded by Cargo; Git dependencies have
their exact revisions. Local workspace packages are included with project source.

The principal upstream sources are:

| Dependency | Exact source |
| --- | --- |
| Niri | [Commit `fc9e8c425e2267d877a65918f030c86f3c698179`](https://github.com/niri-wm/niri/tree/fc9e8c425e2267d877a65918f030c86f3c698179) (upstream `main`), also included in the matching source bundle. The maintained patch series is included there and in this repository. |
| IronRDP | [Commit `7b976a6965c96c76ff949313e174ea55dada8c44`](https://github.com/Devolutions/IronRDP/tree/7b976a6965c96c76ff949313e174ea55dada8c44). |
| Smithay | [Commit `8a270232f1bfc416c781273c98a3f52a812a0bba`](https://github.com/Smithay/smithay/tree/8a270232f1bfc416c781273c98a3f52a812a0bba). |
| FFmpeg upstream | [FFmpeg 9.0.2 source archive](https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz). |
| Arch FFmpeg packaging | [Exact packaging revision `5c4a25f039c060c421a1cd887551d491dff4d55d`](https://gitlab.archlinux.org/archlinux/packaging/packages/ffmpeg/-/tree/5c4a25f039c060c421a1cd887551d491dff4d55d), tagged `2-9.0.2-2`. Its recipe supplies the distribution's source inputs and build configuration. |

These are the current source pins. The published `1.5-1` source bundle retains
the pins it was built from, including Niri `0be60b742762905b7bee1ea67bb5a2fb00859bf0` with two
maintained patches and Smithay `ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef`.
The license inventory is unchanged by the rebase.

The repository does not need to track the Niri source archive. `makepkg`
downloads it from the pinned commit URL and verifies the SHA-256 recorded in the
PKGBUILD and `compositor/pins.json`; each release's `makepkg --allsource` bundle
preserves that exact archive with the patch series, so it remains the
corresponding-source copy even if the download URL changes.

Native dependency names are declared in the matching PKGBUILD; their installed
versions are recorded in the binary's `.BUILDINFO`. Use the **Source Files** link
on each [Arch package page](https://archlinux.org/packages/) to obtain its packaging
and upstream inputs, selecting the recorded version. FFmpeg's build configuration
and package provenance are also retained in `THIRD-PARTY-NOTICES.txt`.

## Maintaining binary releases

Preserve upstream copyright notices, license texts, and attribution notices in
source and binary distributions. Provide complete corresponding source and the
scripts needed to build/install the GPL-covered programs, subject to the GPL's
source definition and system-library exceptions. A project source ZIP or a list
of license names alone does not establish that all those requirements are met.

Keep matching source assets and clear dependency-source directions alongside
every published binary. GPLv3 section 6(d) permits corresponding source on a
different server when equivalent access and clear directions are provided;
the distributor remains responsible for its availability. If external sources
become unavailable, restore access or supply preserved source copies. A release
that must support offline rebuilding should also vendor its dependency sources.

For future releases, generate notices using `scripts/collect-licenses.py`, export
the matching source package, record the actual platform libraries, and update
the source index and checksums. Retain each release's original source information
when build recipes or distribution packages change.

Copyright licenses and codec patents are separate issues. H.264/AAC distribution
may require additional assessment for a commercial product or particular
jurisdiction; an open-source license is not a blanket patent clearance.

The applicable upstream texts govern. This guide records the project's component
licenses and source access; it is not legal certification.

References: [GPLv3](https://opensource.org/license/gpl-3.0),
[FFmpeg licensing](https://ffmpeg.org/legal.html),
[GitHub repository licensing](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/licensing-a-repository).
