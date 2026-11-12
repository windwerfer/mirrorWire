# MirrorWire

Low-latency screen mirroring over a LAN, written in Rust. One binary (`mirrorwire`) runs in
**presenter** mode (captures and encodes the screen) or **viewer** mode
(receives, decodes and shows it).

- Capture: xdg-desktop-portal ScreenCast + PipeWire. No root, no `setcap`,
  works in a Flatpak sandbox. The portal restore token is saved, so the
  "share screen" dialog appears only once.
- Encode: H.264 through FFmpeg with low-latency settings (no B-frames, no
  lookahead, IDR on demand, optional intra refresh). VAAPI is tried first on
  Linux, VideoToolbox on macOS, and x264 (`ultrafast` + `zerolatency`) is the fallback.
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
  macos/             .app bundling (Info.plist, icon, ad-hoc signing)
  flatpak/           Flatpak manifest
  appimage/          AppImage packaging (linuxdeploy)
  linux/, icons/     desktop entry, AppStream metainfo, icon
.github/workflows/
  ci.yml             clippy, unit tests, loopback test
  build.yml          macOS arm64 + Intel .app, Flatpak, AppImage; Release on v* tags
```

## Downloads

Every push to `main` builds these in GitHub Actions (Actions tab, then the run,
then Artifacts). A `v*` tag also attaches them to a GitHub Release:

| artifact | for | notes |
|---|---|---|
| `MirrorWire-macos-x86_64.zip` | Intel Macs, macOS 10.13 or newer | FFmpeg statically linked, no Homebrew needed |
| `MirrorWire-macos-arm64.zip` | Apple Silicon Macs, macOS 11 or newer | FFmpeg statically linked |
| `MirrorWire-x86_64.flatpak` | Linux (Fedora, …) | `flatpak install --user MirrorWire-x86_64.flatpak` |
| `MirrorWire-x86_64.AppImage` | Linux, glibc 2.35 or newer | needs `libva` and `libpipewire` from the system |

**Opening the unsigned Mac app.** The builds are ad-hoc signed, not notarized.
Unzip, move `MirrorWire.app` to Applications, then right-click it and choose
**Open**, then **Open** again. On macOS 15 or newer, double-click once, then go to
System Settings, Privacy & Security, and click **Open Anyway**. In a terminal,
`xattr -dr com.apple.quarantine /Applications/MirrorWire.app` does the same. When
asked, allow MirrorWire to find devices on your local network.

Launching the app without arguments opens the viewer. The presenter runs from a terminal:

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

Useful presenter options: `--width 2000 --height 1000 --fps 60 --bitrate-kbps 20000`,
`--encoder auto|vaapi|x264`, `--intra-refresh`, `--source test`, `--no-pairing`, `--port`.
The frame is scaled to `--width`x`--height`. Set them to the viewer's screen size.

## Build and run: viewer on macOS

```sh
brew install ffmpeg pkg-config
curl https://sh.rustup.rs -sSf | sh
cargo build --release -p mirrorwire-app --no-default-features --features gui
./target/release/mirrorwire view              # pick a discovered presenter, enter pairing code once
./target/release/mirrorwire view 'mirrorwire://192.168.1.20:47800?fp=…&code=…'
```

Add `--hwdec` for VideoToolbox decoding. Keys in the viewer: `F` or double-click
for fullscreen, `S` toggles stats, `Esc` disconnects. Homebrew only supports
recent macOS versions; on an older Mac, build FFmpeg from MacPorts instead and
point `PKG_CONFIG_PATH` at it.

## Testing without a screen

```sh
scripts/loopback-test.sh 10 --encoder x264            # 10 s, 2000x1000@60
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

Tested on a Debian 13 VM (8 vCPU Xeon, no GPU), loopback:

| run | fps | bitrate | capture->decoded mean / p95 | lost |
|---|---|---|---|---|
| 2000x1000@60, x264, 20 Mbit/s cap, 30 s | 60.0 | 13.5 Mbit/s | 8.9 / 11.2 ms | 0 |
| 1920x1080@60, x264, 30 Mbit/s cap, RGBA convert | 59.9 | 16.3 Mbit/s | 9.9 / 12.6 ms | 0 |

Most of the time is x264 software encoding on the VM CPU (`capture_to_encoded`
about 7.7 ms). Network plus reassembly on loopback is about 0.3 ms, decode about 0.9 ms.

| part | state |
|---|---|
| proto (fragmentation, reassembly, loss detection, clock sync) | unit-tested, loopback-tested |
| x264 encode, software H.264 decode, RGBA conversion | loopback-tested |
| QUIC datagram transport, control stream, keyframe recovery | loopback-tested, including simulated loss |
| pinned TLS, pairing code, paired-viewer memory | tested: wrong fingerprint and missing code are rejected |
| mDNS announce/browse | tested on one host |
| egui viewer | runs on the VM (llvmpipe, X11) and shows the stream |
| portal + PipeWire capture | compiles, **untested** on a real KDE session |
| VAAPI encode, VAAPI decode | compiles, **untested** (no GPU on the build VM) |
| VideoToolbox encode/decode, macOS build | **untested** |
| Flatpak, AppImage, macOS .app builds | built in CI, not yet run on the target machines |
| camera QR scanning | not started |
| Android presenter/viewer | later (MediaProjection + MediaCodec via JNI) |

## License

The source is MIT OR Apache-2.0. The Linux release builds statically link x264
(GPL-2.0-or-later), so those binaries are distributed under the GPL. The macOS
builds use only FFmpeg's LGPL parts plus VideoToolbox.

## Known limitations and next steps

1. Run on the T480s: portal capture, VAAPI encode, measure on Wi-Fi and cable.
2. Build the viewer on the actual Mac; choose software vs VideoToolbox decode.
3. Forward error correction (Reed-Solomon parity per frame) so single packet
   losses on Wi-Fi do not cost a keyframe or artifacts.
4. Zero-copy: PipeWire DMA-BUF -> VAAPI surface; VideoToolbox -> Metal texture,
   YUV->RGB in a wgpu shader instead of swscale + egui texture upload.
5. Adaptive bitrate from QUIC RTT/loss stats.
6. Vendor cargo sources (flatpak-cargo-generator) for a Flathub submission; notarized Mac builds.
7. QR scanning with the viewer's camera, virtual-monitor (extend) mode via portal.
8. Optional Miracast sink as a second `VideoSink`.
9. Android comes later. WebAssembly is out of scope (capture and codecs are native).
