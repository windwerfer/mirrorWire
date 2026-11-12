# MirrorWire

<p>
  <a href="https://github.com/windwerfer/mirrorWire/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/windwerfer/mirrorWire/actions/workflows/ci.yml/badge.svg?branch=main&style=flat-square"></a>
  <a href="https://github.com/windwerfer/mirrorWire/actions/workflows/build.yml"><img alt="Build" src="https://github.com/windwerfer/mirrorWire/actions/workflows/build.yml/badge.svg?branch=main&style=flat-square"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square"></a>
  <br>
  <a href="https://sonarcloud.io/project/overview?id=windwerfer_mirrorWire"><img alt="Scanned by SonarCloud" src="https://img.shields.io/badge/Scanned_by-SonarCloud-4b9fd5?style=flat-square&logo=sonarqubecloud&logoColor=white"></a>
  <a href="https://sonarcloud.io/summary/new_code?id=windwerfer_mirrorWire"><img alt="Quality Gate Status" src="https://sonarcloud.io/api/project_badges/measure?project=windwerfer_mirrorWire&metric=alert_status"></a>
</p>

Low-latency screen mirroring over a LAN, written in Rust. One binary (`mirrorwire`) runs in
**presenter** mode (captures and encodes the screen) or **viewer** mode
(receives, decodes and shows it).

- Capture: xdg-desktop-portal ScreenCast + PipeWire. No root, no `setcap`,
  works in a Flatpak sandbox. The portal restore token is saved, so the
  "share screen" dialog appears only once.
- Encode: H.264 through FFmpeg with low-latency settings (no B-frames, no
  lookahead, IDR on demand, optional intra refresh, High profile). The default
  output size is the native capture size (the portal's 1920×1080 is no longer
  scaled to 2000×1000). Default rate control is 40 Mbit/s CBR with a 2-second
  VBV, using wall-clock PTS so a static desktop still gets enough bits per
  frame. CQP/ICQ and a live bitrate control are available. VAAPI is tried first on
  Linux, VideoToolbox on macOS, and x264 (`ultrafast` + `zerolatency`) is the fallback.
  Colour conversion (BGRx→NV12) uses VAAPI VPP (`hwupload` + `scale_vaapi`) when
  present, otherwise CPU swscale.
- Transport: QUIC (`quinn`). Video goes as unreliable datagrams (frames are
  fragmented, reassembled, and loss is detected per frame). A reliable control
  stream carries hello/pairing, keyframe requests, and clock pings.
- Security: each install generates a self-signed certificate once. The viewer
  pins the presenter's SHA-256 fingerprint (from the QR code or mDNS). The
  presenter only streams to viewers that have shown the pairing code once; it
  remembers their certificate after that (`paired_viewers.json`).
- Discovery: mDNS (`_mirrorwire._udp.local.`) plus a QR code with a
  `mirrorwire://ip:port?fp=…&code=…&name=…` URI.
- Viewer UI: eframe/egui on wgpu (Metal on macOS, Vulkan/GL on Linux), with a list
  of discovered presenters, a box to paste a URI, the video, and a stats overlay.

## Layout

```
crates/
  mirrorwire-proto   wire format: datagram fragmentation/reassembly, control messages,
                 clock-offset estimation, latency statistics (no I/O)
  mirrorwire-media   frame sources (test pattern, portal/PipeWire), H.264 encoder and
                 decoder (FFmpeg, VAAPI/VideoToolbox/x264), capture->encode pipeline
                 and the VideoSink trait
  mirrorwire-net     identity/certificates, pinned TLS, QUIC presenter/viewer sessions,
                 mDNS, QR/URI pairing
  mirrorwire-app     the `mirrorwire` binary: CLI, presenter, headless viewer, egui viewer
scripts/
  loopback-test.sh   test pattern -> QUIC -> headless viewer on localhost
packaging/
  build-ffmpeg.sh    minimal static FFmpeg (+ x264 on Linux) used by all release builds
  macos/             .app bundling and UDZO DMG (Info.plist, icon, ad-hoc signing)
  flatpak/           Flatpak manifest
  appimage/          AppImage packaging (linuxdeploy)
  linux/, icons/     desktop entry, AppStream metainfo, icon
.github/workflows/
  ci.yml             clippy, unit tests, loopback test, and the supply-chain audit
  audit.yml          cargo audit and cargo-deny (also a weekly scheduled run)
  build.yml          macOS arm64 + Intel DMG, Flatpak, AppImage; Release on v* tags
.github/dependabot.yml  weekly updates for Actions SHA pins and Cargo
```

## Downloads

Every push to `main` builds these in GitHub Actions (Actions tab, then the run,
then Artifacts). A `v*` tag also attaches them to a GitHub Release:

| artifact | for | notes |
|---|---|---|
| `MirrorWire-<ver>-macos-x86_64.dmg` | Intel Macs, macOS 10.13 or newer | FFmpeg statically linked, no Homebrew needed |
| `MirrorWire-<ver>-macos-arm64.dmg` | Apple Silicon Macs, macOS 11 or newer | FFmpeg statically linked |
| `MirrorWire-x86_64.flatpak` | Linux (Fedora, …) | `flatpak install --user MirrorWire-x86_64.flatpak` |
| `MirrorWire-x86_64.AppImage` | Linux, glibc 2.39 or newer (Fedora 40+, Ubuntu 24.04+) | needs `libva` and `libpipewire` from the system |

`<ver>` is the version on a tagged release, or the short commit SHA on a
`main`-branch build. Each artifact is a single file (not a zip).

**Opening the unsigned Mac app.** The builds are ad-hoc signed, not notarized.
Open the DMG, drag `MirrorWire.app` to Applications, then right-click it and
choose **Open**, then **Open** again. On macOS 15 or newer, double-click once,
then go to System Settings, Privacy & Security, and click **Open Anyway**. If
Gatekeeper still blocks it, run
`xattr -dr com.apple.quarantine /Applications/MirrorWire.app`. When asked, allow
MirrorWire to find devices on your local network.

Launching the app without arguments opens a start screen with **Present this screen**
and **View**. Present starts capture and the pairing window (Linux only; the button is
disabled on macOS until capture exists there). The presenter window shows the pairing
code, a live quality panel (bitrate, CBR/VBR/CQP/ICQ, keyint), connection attempts,
and, on Fedora, a hint if firewalld may be blocking UDP 47800. **View** lists
presenters and, while connecting, a staged status (handshake, certificate, pairing,
first frame) plus a copyable log. After a timeout it suggests
`sudo firewall-cmd --add-port=47800/udp`. The CLI subcommands (`present`, `view`,
`discover`) still work, including headless tests.

Logs are written to `~/Library/Logs/MirrorWire/mirrorwire.log` on macOS and to
`$XDG_STATE_HOME/mirrorwire/mirrorwire.log` on Linux (under
`~/.var/app/io.github.windwerfer.MirrorWire/` for the Flatpak). The path is shown
in the window, with a **Copy log** button.

**Renderer.** The viewer uses wgpu (Metal on macOS, Vulkan/GL on Linux) by default,
except on macOS x86_64, where it uses OpenGL (`glow`) because older Intel Macs have
no Metal. If wgpu fails to start, the app retries with OpenGL. Override with
`--renderer gl|wgpu|auto` or `MIRRORWIRE_RENDERER`. Video is converted YUV→RGBA on
the CPU (swscale) before it is uploaded as an egui texture, so the OpenGL path does
not need a wgpu shader. Hardware H.264 decode (`--hwdec` or the View checkbox) uses
VideoToolbox or VAAPI when present, and falls back to software with a log line if
the device is missing or later fails.

The presenter can also be started from a terminal:

```sh
flatpak run io.github.windwerfer.MirrorWire present     # Flatpak
./MirrorWire-x86_64.AppImage present                     # AppImage
/Applications/MirrorWire.app/Contents/MacOS/mirrorwire view --hwdec   # Mac viewer from a terminal
```

**Flatpak permissions:** `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc`
(for X11 only), `--share=network` (QUIC and mDNS), and `--device=dri` (VAAPI and GPU
rendering). There is no host filesystem access. Screen capture goes through the
ScreenCast portal, which needs no extra permission. Settings live in
`~/.var/app/io.github.windwerfer.MirrorWire/config/mirrorwire`.

`mirrorwire-media` does not depend on `mirrorwire-net`. The presenter pushes encoded frames
into a `VideoSink`; the QUIC transport is one implementation, and a Miracast/RTSP
sink can be added next to it later.

## Build and run: presenter on Fedora KDE (Wayland)

```sh
# Full FFmpeg and Intel's VAAPI driver come from RPM Fusion (Fedora's ffmpeg-free lacks H.264 encoding)
sudo dnf install https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm
sudo dnf swap ffmpeg-free ffmpeg --allowerasing
sudo dnf swap libva-intel-media-driver intel-media-driver --allowerasing
sudo dnf install ffmpeg-devel clang clang-devel pkgconf pipewire-devel libva-devel \
    dbus-devel libxkbcommon-devel wayland-devel libva-utils
vainfo | grep -i enc          # expect H264 VAEntrypointEncSlice lines

curl https://sh.rustup.rs -sSf | sh     # if Rust is not installed yet
cargo build --release
./target/release/mirrorwire present          # portal dialog appears once, then a QR code
```

Useful presenter options: `--fps 60 --bitrate-kbps 40000` (or `--bitrate-mbps 40`),
`--rc auto|cbr|vbr|cqp|icq`, `--qp 18`, `--keyint` (frames; 0 = 10 s),
`--encoder auto|vaapi|x264`, `--intra-refresh`, `--source test`, `--no-pairing`, `--port`.
The default size is the native capture. `--size 1920x1080` (or `--width` / `--height`)
overrides it and scales. If a viewer finds the presenter but cannot connect, open
the port on Fedora: `sudo firewall-cmd --add-port=47800/udp`.

## Build and run: viewer on macOS

```sh
brew install ffmpeg pkg-config
curl https://sh.rustup.rs -sSf | sh
cargo build --release -p mirrorwire-app --no-default-features --features gui
./target/release/mirrorwire view              # pick a discovered presenter, enter pairing code once
./target/release/mirrorwire view 'mirrorwire://192.168.1.20:47800?fp=…&code=…'
```

On macOS the viewer defaults to VideoToolbox decode and falls back to a
tuned software decoder (`--software-decode` forces it). A 2011 HD 6770M on
OCLP may not expose VT H.264. The OpenGL (glow) path converts YUV→RGB in a
shader instead of CPU swscale. On an old Intel Mac without Metal, the viewer defaults to OpenGL;
`--renderer wgpu` tries Metal first.
 Keys in the viewer: `F` or double-click
for fullscreen, `S` toggles stats, `1` toggles 1:1 pixel-exact display (nearest
neighbour, one stream pixel = one framebuffer pixel, taking `pixels_per_point`
into account), `Esc` disconnects. The stats overlay shows native resolution,
source fps (portal may send only damage, so this can be well below the configured
fps), bitrate, decode time, and the window/texture scale factor. The video texture
is the stream's native size; fit-to-window uses linear filtering.

4:4:4 chroma is not used: Intel UHD 620 VAAPI and VideoToolbox H.264 decode are
4:2:0-only, so a 4:4:4 encode would not decode in hardware on the iMac.

Homebrew only supports
recent macOS versions; on an older Mac, build FFmpeg from MacPorts instead and
point `PKG_CONFIG_PATH` at it.

## Testing without a screen

```sh
scripts/loopback-test.sh 10 --encoder x264            # 10 s, native test pattern 1920x1080@60, 40 Mbit/s
scripts/loopback-test.sh 10 --intra-refresh -- --simulate-loss 0.5
mirrorwire view --headless --duration 10 '<uri>'          # against a real presenter
mirrorwire discover                                       # list presenters via mDNS
```

The test pattern writes its frame number into the image as black/white blocks.
The viewer reads them back from the decoded picture and counts mismatches. That
proves the frame decoded is the frame sent.

Latency stages reported (ms, mean/p50/p95/max):
`capture_to_encoded` (presenter: capture, colour conversion, encode),
`encoded_to_sent`, `network` (clock-offset corrected), `reassembly`, `decode`,
`convert` (YUV->RGBA, GUI only) and `capture_to_decoded` end to end. The GUI
overlay also shows capture->texture.

## Status

Tested on a Debian 13 VM (8 vCPU Xeon, no GPU), loopback (x264 software):

| run | fps | bitrate | capture_to_encoded mean / p95 | capture->decoded mean / p95 | network mean | lost |
|---|---|---|---|---|---|---|
| before: 2000×1000@60, 20 Mbit/s cap, 8 s | 59.9 | 14.35 Mbit/s | 6.04 / 6.30 ms | 7.03 / 7.44 ms | 0.23 ms | 0 |
| after: 1920×1080@60, 40 Mbit/s CBR, 8 s | 59.9 | 37.42 Mbit/s | 6.79 / 8.36 ms | 8.20 / 9.77 ms | 0.29 ms | 0 |

Most of the time is x264 software encoding on the VM CPU. Network plus reassembly
on loopback is about 0.4 ms, decode about 0.9 ms. On real hardware (T480s VAAPI
presenter, 2011 iMac viewer) the picture was previously scaled from a 1920×1080
portal to 2000×1000 (wrong aspect, blur) and the bitrate collapsed to ~2.3 Mbit/s
when the portal only sent 6.8 fps, because PTS advanced by 1/60 s per frame.

| part | state |
|---|---|
| proto (fragmentation, reassembly, loss detection, clock sync) | unit-tested, loopback-tested |
| x264 encode, software H.264 decode, RGBA conversion | loopback-tested |
| QUIC datagram transport, control stream, keyframe recovery | loopback-tested, including simulated loss |
| pinned TLS, pairing code, paired-viewer memory | tested: wrong fingerprint and missing code are rejected |
| mDNS announce/browse | tested on one host |
| egui viewer | runs on the VM (llvmpipe, X11) and on a 2011 iMac (OpenGL / glow, macOS 15 via OCLP) |
| portal + PipeWire capture | tested on Fedora KDE (T480s, 1920×1080) |
| VAAPI encode | tested on T480s UHD 620 (`h264_vaapi`); default is now 40 Mbit/s CBR, High profile, native size |
| VAAPI decode | compiles, unused on the iMac (software H.264 there) |
| VideoToolbox encode/decode, macOS build | software H.264 decode works on the 2011 iMac |
| Flatpak, AppImage, macOS .app / DMG builds | built in CI; Flatpak presenter and Intel DMG viewer start on the real machines |
| camera QR scanning | not started |
| Android presenter/viewer | later (MediaProjection + MediaCodec via JNI) |


## Security

SonarQube Cloud runs static analysis on `main` after each push (the Free plan
does not analyze pull requests). `cargo audit` and `cargo deny` run on every
push and pull request, and again on a weekly schedule; the Build workflow
waits for those checks and will not publish artifacts if they fail. GitHub
Actions are pinned to full commit SHAs and updated by Dependabot. Downloads
in the packaging scripts are fetched over HTTPS and checked against pinned
SHA-256 digests.

The transport is QUIC with TLS 1.3: each install generates a self-signed
certificate, the viewer pins the presenter's SHA-256 fingerprint, and the
presenter only streams to a viewer that has shown the pairing code once.
macOS builds are ad-hoc signed and not notarized, so Gatekeeper will warn
on first launch (see [Downloads](#downloads)).

To report a vulnerability, see [SECURITY.md](SECURITY.md).

## License

The source is MIT OR Apache-2.0. The Linux release builds statically link x264
(GPL-2.0-or-later), so those binaries are distributed under the GPL. The macOS
builds use only FFmpeg's LGPL parts plus VideoToolbox.

## Known limitations and next steps

1. Re-test on the T480s + iMac after this quality/diagnostics build (40 Mbit/s,
   native 1920×1080, firewall hint, GPU convert when VPP is in the runtime).
2. If firewalld is running: `sudo firewall-cmd --add-port=47800/udp` (add
   `--permanent` and reload to keep it).
3. Forward error correction (Reed-Solomon parity per frame) so single packet
   losses on Wi-Fi do not cost a keyframe or artifacts.
4. Zero-copy: PipeWire DMA-BUF -> VAAPI surface; VideoToolbox -> Metal texture,
   YUV->RGB in a wgpu shader instead of swscale + egui texture upload.
5. Adaptive bitrate from QUIC RTT/loss stats.
6. Vendor cargo sources (flatpak-cargo-generator) for a Flathub submission; notarized Mac builds.
7. QR scanning with the viewer's camera, virtual-monitor (extend) mode via portal.
8. Optional Miracast sink as a second `VideoSink`.
9. Android comes later. WebAssembly is out of scope (capture and codecs are native).

## Contributing

Humans and AI agents: read [AGENTS.md](AGENTS.md) first (rules and fixed decisions), then
[CONTRIBUTING.md](CONTRIBUTING.md) and the maps in [docs/agents/](docs/agents/README.md).
