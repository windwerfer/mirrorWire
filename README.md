# mirror

Low-latency screen mirroring over a LAN, written in Rust. One binary runs in
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
- Discovery: mDNS (`_mirror._udp.local.`) plus a QR code with a
  `mirror://ip:port?fp=…&code=…&name=…` URI.
- Viewer UI: eframe/egui on wgpu (Metal on macOS, Vulkan/GL on Linux), with a list
  of discovered presenters, a box to paste a URI, the video, and a stats overlay.

## Layout

```
crates/
  mirror-proto   wire format: datagram fragmentation/reassembly, control messages,
                 clock-offset estimation, latency statistics (no I/O)
  mirror-media   frame sources (test pattern, portal/PipeWire), H.264 encoder and
                 decoder (FFmpeg, VAAPI/VideoToolbox/x264), capture->encode pipeline
                 and the VideoSink trait
  mirror-net     identity/certificates, pinned TLS, QUIC presenter/viewer sessions,
                 mDNS, QR/URI pairing
  mirror-app     the `mirror` binary: CLI, presenter, headless viewer, egui viewer
scripts/
  loopback-test.sh   test pattern -> QUIC -> headless viewer on localhost
```

`mirror-media` does not depend on `mirror-net`. The presenter pushes encoded frames
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
./target/release/mirror present          # portal dialog appears once, then a QR code
```

Useful presenter options: `--width 2000 --height 1000 --fps 60 --bitrate-kbps 20000`,
`--encoder auto|vaapi|x264`, `--intra-refresh`, `--source test`, `--no-pairing`, `--port`.
The frame is scaled to `--width`x`--height`. Set them to the viewer's screen size.

## Build and run: viewer on macOS

```sh
brew install ffmpeg pkg-config
curl https://sh.rustup.rs -sSf | sh
cargo build --release -p mirror-app --no-default-features --features gui
./target/release/mirror view              # pick a discovered presenter, enter pairing code once
./target/release/mirror view 'mirror://192.168.1.20:47800?fp=…&code=…'
```

Add `--hwdec` for VideoToolbox decoding. Keys in the viewer: `F` or double-click
for fullscreen, `S` toggles stats, `Esc` disconnects. Homebrew only supports
recent macOS versions; on an older Mac, build FFmpeg from MacPorts instead and
point `PKG_CONFIG_PATH` at it.

## Testing without a screen

```sh
scripts/loopback-test.sh 10 --encoder x264            # 10 s, 2000x1000@60
scripts/loopback-test.sh 10 --intra-refresh -- --simulate-loss 0.5
mirror view --headless --duration 10 '<uri>'          # against a real presenter
mirror discover                                       # list presenters via mDNS
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
| Flatpak packaging, camera QR scanning | not started |

## Known limitations and next steps

1. Run on the T480s: portal capture, VAAPI encode, measure on Wi-Fi and cable.
2. Build the viewer on the actual Mac; choose software vs VideoToolbox decode.
3. Forward error correction (Reed-Solomon parity per frame) so single packet
   losses on Wi-Fi do not cost a keyframe or artifacts.
4. Zero-copy: PipeWire DMA-BUF -> VAAPI surface; VideoToolbox -> Metal texture,
   YUV->RGB in a wgpu shader instead of swscale + egui texture upload.
5. Adaptive bitrate from QUIC RTT/loss stats.
6. Flatpak manifest (org.freedesktop.Platform + ffmpeg-full extension), macOS app bundle.
7. QR scanning with the viewer's camera, virtual-monitor (extend) mode via portal.
8. Optional Miracast sink as a second `VideoSink`.
