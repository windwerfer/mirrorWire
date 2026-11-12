# Architecture

## 1. Crates and layering

A Cargo workspace with four crates. Lower layers never depend on higher ones.

| Layer | Crate | Job | May depend on |
|---|---|---|---|
| L0 | `mirrorwire-proto` | Wire format: datagram fragment header, fragmentation and reassembly, control messages (JSON over a length-prefixed stream), clock-offset estimation, latency statistics, protocol constants (`PROTOCOL_VERSION`, `ALPN`, `MDNS_SERVICE`, `DEFAULT_PORT`). **No I/O, no async, no FFmpeg.** | `bytes`, `serde`, `serde_json`, `anyhow` only |
| L1 | `mirrorwire-media` | Frame sources (test pattern, portal + PipeWire), H.264 encoder and decoder via FFmpeg (VAAPI / VideoToolbox / x264), YUV→RGBA conversion, the capture→encode loop and the `VideoSink` trait. | `mirrorwire-proto` |
| L1 | `mirrorwire-net` | Identity (self-signed cert), pinned TLS verifiers, QUIC presenter/viewer sessions, pairing (`paired_viewers.json`, pairing code, `mirrorwire://` URI, QR), mDNS announce/browse. | `mirrorwire-proto` |
| L2 | `mirrorwire-app` | The `mirrorwire` binary: CLI (`present`, `view`, `view --headless`, `discover`), presenter and viewer glue, the egui GUI (start screen, presenter status, viewer list, video, stats overlay). | all of the above |

```
mirrorwire-proto  <-  mirrorwire-media
                  <-  mirrorwire-net
                         ^       ^
                         mirrorwire-app
```

Rules:

- **`mirrorwire-media` and `mirrorwire-net` never depend on each other.** The presenter pushes
  encoded frames into a `VideoSink`; QUIC is one sink. A Miracast/RTSP sink or a file sink can be
  added beside it without touching the encoder.
- **Only `mirrorwire-app` knows about egui/eframe, clap and tracing-subscriber.** Library crates
  log through `tracing` and return `anyhow::Result`.
- **Only `mirrorwire-media` touches FFmpeg and `unsafe`.** The `unsafe` is FFmpeg FFI
  (hardware frame contexts, raw `AVCodecContext` fields, `Send` impls for FFmpeg wrappers). Keep
  it in `encode.rs`, `decode.rs` and `hw.rs`, give each block a `// SAFETY:` comment (existing
  blocks lack them, see roadmap), and keep the public API safe. No `unsafe` in the other crates.
- New platform code (Android JNI, a pure-Rust encoder) goes behind the same traits
  (`FrameSource`, `VideoSink`, the encoder backend enum), not as special cases in `mirrorwire-app`.

## 2. Pipeline

```
Presenter                                                         Viewer
---------                                                         ------
xdg-desktop-portal ScreenCast  (one-time dialog, restore token)
  -> PipeWire stream (BGRx/BGRA/RGBx/RGBA, CPU copy today; DMA-BUF later)
  -> FrameSource::next_frame -> RawFrame
  -> Encoder (FFmpeg): h264_vaapi | h264_videotoolbox | libx264
       low latency: no B-frames, no lookahead, IDR on request,
       optional intra refresh, CBR-ish rate control
  -> VideoSink (QUIC)                                          QUIC endpoint (pinned cert)
       fragment frame into <= max_datagram_size pieces           reassemble fragments by frame_id,
       34-byte header each, unreliable datagrams  ---------->    detect loss, request keyframe
       control stream: Hello/StreamInfo/Ping/Pong/   <------>    over control stream
       RequestKeyframe/Rejected/Bye                             -> Decoder (FFmpeg; VideoToolbox/VAAPI
                                                                    with --hwdec, else software)
                                                                -> RgbaConverter (swscale, CPU)
                                                                -> egui texture, drawn by
                                                                   wgpu (Metal/Vulkan/GL) or glow (OpenGL)
```

Encoder selection (`--encoder auto|vaapi|x264|videotoolbox`): `auto` tries VAAPI on Linux,
VideoToolbox on macOS, then x264. The chosen encoder is reported in `StreamInfo` and shown in
both GUIs.

Planned encoder options (see [roadmap.md](roadmap.md)): the pure-Rust `filmcraft-h264enc`
(MIT/Apache, no GPL in Linux builds) as a selectable software encoder for comparison with x264,
and an optional "use the system FFmpeg" mode (Flatpak: the `org.freedesktop.Platform.ffmpeg-full`
extension) so bundled builds can drop x264.

Renderer selection (`--renderer gl|wgpu|auto`, env `MIRRORWIRE_RENDERER`): `auto` is glow
(OpenGL) on macOS x86_64 and wgpu elsewhere, retrying with glow if wgpu fails to start.

## 3. Threads (today)

- Presenter: a capture+encode thread (`run_capture_encode`), a tokio runtime for QUIC,
  mDNS in its own daemon thread (`mdns-sd`), the GUI thread polling `PresenterHandle::poll()`.
- Viewer: tokio runtime receiving datagrams and the control stream, a decode thread, the GUI
  thread taking the newest RGBA frame from a bounded channel (old frames are dropped, never
  queued: latency beats smoothness).

Bounded channels everywhere on the media path. A queue that can grow is a latency bug.

## 4. Files on disk

Config dir: `~/.config/mirrorwire` (Flatpak: `~/.var/app/io.github.windwerfer.MirrorWire/config/mirrorwire`,
macOS: `~/Library/Application Support/mirrorwire`), overridable with `--config-dir`.

| File | Written by | Contents |
|---|---|---|
| `identity.cert.der`, `identity.key.der` | both | per-install self-signed cert and key (key file mode 0600) |
| `paired_viewers.json` | presenter | fingerprints and names of viewers that showed the pairing code |
| `portal_restore_token` | presenter (Linux) | ScreenCast restore token, so the share dialog appears once |

Deleting the directory resets identity and pairings; viewers then see a new fingerprint.

## 5. Packaging

`packaging/build-ffmpeg.sh` builds a minimal static FFmpeg used by every release build (with
x264 on Linux only). `packaging/macos/` makes the `.app` and a UDZO DMG (ad-hoc signed),
`packaging/flatpak/` the Flatpak (runtime `org.freedesktop.Platform//25.08`),
`packaging/appimage/` the AppImage (built on Ubuntu 24.04, needs glibc 2.39+). See
[testing.md](testing.md) §5 for CI.
