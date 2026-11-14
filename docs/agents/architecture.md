# Architecture

## 1. Crates and layering

A Cargo workspace with five crates. Lower layers never depend on higher ones.

| Layer | Crate | Job | May depend on |
|---|---|---|---|
| L0 | `mirrorwire-proto` | Wire format: datagram fragment header, fragmentation and reassembly, control messages (JSON over a length-prefixed stream), clock-offset estimation, latency statistics, protocol constants (`PROTOCOL_VERSION`, `ALPN`, `MDNS_SERVICE`, `DEFAULT_PORT`). **No I/O, no async, no FFmpeg.** | `bytes`, `serde`, `serde_json`, `anyhow`, `reed-solomon-simd` (FEC) only |
| L1 | `mirrorwire-media` | Frame sources (test pattern, portal + PipeWire), H.264 encoder and decoder via FFmpeg (VAAPI / VideoToolbox / x264), YUV→RGBA conversion, the capture→encode loop and the `VideoSink` trait. | `mirrorwire-proto` |
| L1 | `mirrorwire-net` | Identity (self-signed cert), pinned TLS verifiers, QUIC presenter/viewer sessions, pairing (`paired_viewers.json`, pairing code, `mirrorwire://` URI, QR), mDNS announce/browse, persisted presenter settings (`settings.json`), Linux firewall detection and a best-effort firewalld D-Bus open-port request. | `mirrorwire-proto` |
| L2 | `mirrorwire-app` | The `mirrorwire` binary: CLI (`present`, `view`, `view --headless`, `discover`), presenter and viewer glue, the egui GUI (start screen, presenter status, viewer list, video, stats overlay). | all of the above |
| L2 | `mirrorwire-android` | The Android viewer: `libmirrorwire_android.so` loaded by the APK. egui (glow) via `android-activity` NativeActivity, MediaCodec decode (`ndk` crate, `AImageReader` → CPU BT.709 → texture), the viewer receive loop. No FFmpeg. On non-Android targets only its pure helpers (`h264`, `yuv`) compile, so host `cargo test` covers them. | `mirrorwire-proto`, `mirrorwire-net` |

```
mirrorwire-proto  <-  mirrorwire-media
                  <-  mirrorwire-net
                         ^       ^
                         mirrorwire-app   mirrorwire-android (proto + net only)
```

Rules:

- **`mirrorwire-media` and `mirrorwire-net` never depend on each other.** The presenter pushes
  encoded frames into a `VideoSink`; QUIC is one sink. A Miracast/RTSP sink or a file sink can be
  added beside it without touching the encoder.
- **Only `mirrorwire-app` and `mirrorwire-android` know about egui/eframe** (and only the app
  about clap and tracing-subscriber). Library crates
  log through `tracing` and return `anyhow::Result`.
- **`unsafe` is avoided (D15).** Today it exists as FFI in `mirrorwire-media` (`encode.rs`,
  `decode.rs`, `hw.rs`, one site in `source/portal.rs`), in `mirrorwire-app` (`yuv_glow.rs` GL
  calls, one in `applog.rs`) and in `mirrorwire-net/src/diag.rs`. All of it lacks `// SAFETY:`
  comments (see roadmap 0b). The goal is to remove the sites outside media where a safe API
  exists and to keep proto `unsafe`-free. Any new `unsafe` needs a note to the owner.
- New platform code (Android JNI, a pure-Rust encoder) goes behind the same traits
  (`FrameSource`, `VideoSink`, the encoder backend enum), not as special cases in `mirrorwire-app`.

## 2. Pipeline

```
Presenter                                                         Viewer
---------                                                         ------
xdg-desktop-portal ScreenCast  (one-time dialog, restore token)
  -> PipeWire stream (BGRx/BGRA/RGBx/RGBA), maxFramerate = --fps (30|60|max, default 60);
       the negotiated max is read back and caps the encoder's nominal fps
       cursor: portal cursor_mode METADATA (if AvailableCursorModes has it) + SPA_META_Cursor
         -> CursorSink -> transport cursor_loop -> pointer datagrams / image uni streams
            (bypasses the encoder; see protocol.md §3a); else EMBEDDED
       a changed cursor mode / fps cap reopens the source (Controls::request_source_restart,
         CaptureExit::RestartSource keeps the encoder; portal restore token = no dialog)
       DMA-BUF with modifiers offered first (VAAPI encoders), shared memory second
  -> FrameSource::poll_frame -> RawFrame (pixels in `data`, or a DMA-BUF fd in `dmabuf`)
  -> Encoder (FFmpeg): h264_vaapi | h264_videotoolbox | libx264
       capture paths (shown as "Capture path" in the presenter):
         dmabuf-zero-copy          DRM-PRIME -> av_hwframe_map -> VAAPI surface -> scale_vaapi -> NV12
         shm+upload (vaapi-vpp)    memcpy -> hwupload -> scale_vaapi -> NV12
         shm+upload (cpu convert)  swscale BGRx->NV12 on the CPU -> upload
         shm (cpu)                 x264 / VideoToolbox, CPU conversion
       a quiet source (KWin sends frames on damage only) gets the last frame
       re-encoded every 100 ms (REPEAT_AFTER), counted apart from real frames
       low latency: no B-frames, no lookahead, IDR on request,
       optional intra refresh, CBR-ish rate control
  -> VideoSink (QUIC)                                          QUIC endpoint (pinned cert)
       fragment frame into <= max_datagram_size pieces           reassemble fragments by frame_id,
       34-byte header each, unreliable datagrams  ---------->    detect loss, request keyframe
       (FEC viewers: 44-byte v2 header, Reed-Solomon             (FEC: rebuild a block from any
        parity per block, overhead from LinkReport)               k of n packets, report loss)
       control stream: Hello/StreamInfo/Ping/Pong/   <------>    over control stream
       RequestKeyframe/LinkReport/Rejected/Bye                             -> Decoder (FFmpeg; VideoToolbox/VAAPI
                                                                    with --hwdec, else software)
                                                                -> RgbaConverter (swscale, CPU)
                                                                -> egui texture, drawn by
                                                                   wgpu (Metal/Vulkan/GL) or glow (OpenGL)
```

DMA-BUF fallback: if the encoder is not VAAPI, FFmpeg lacks libdrm, or the VAAPI import
fails, the pipeline drops that frame and calls `FrameSource::disable_dmabuf`; the portal
source renegotiates shared memory with the compositor. `--no-dmabuf` or
`MIRRORWIRE_NO_DMABUF=1` skips DMA-BUF entirely. The release FFmpeg builds need
`--enable-libdrm` for the zero-copy path.

Encoder selection (`--encoder auto|vaapi|x264|videotoolbox|filmcraft`): `auto` tries VAAPI on
Linux, VideoToolbox on macOS, then x264. The chosen encoder is reported in `StreamInfo` and shown
in both GUIs. The presenter GUI's Quality panel has an Encoder selector; Apply re-opens the
encoder live, and if the new one can't open, the old one keeps running (logged).

`filmcraft` is the pure-Rust `filmcraft-h264enc` (MIT/Apache), compiled only with cargo feature
`filmcraft` (`mirrorwire-media/src/filmcraft.rs`). It takes the same swscale I420 picture as
x264, so the two software encoders compare like for like. Encoder name and per-frame encode
time (last and 1 s mean) are in the presenter's per-second log line and status panel.

Planned encoder option (see [roadmap.md](roadmap.md)): an optional "use the system FFmpeg" mode (Flatpak: the `org.freedesktop.Platform.ffmpeg-full`
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
| `settings.json` | presenter | Saved UDP port from the start-screen field. Overridden by `--port` or `MIRRORWIRE_PORT` for one run. |

Deleting the directory resets identity and pairings; viewers then see a new fingerprint.

The presenter UDP port defaults to 47800. A changed port is what mDNS advertises and
what the QR/`mirrorwire://` URI contains, so the viewer does not need a matching
setting. On Linux the presenter probes firewalld over the system bus when it can
(and ufw's readable config otherwise), using only calls polkit allows without
authentication (D14): `getDefaultZone`/`getActiveZones` (`…FirewallD1.info`), and
`queryPort`/`queryService` (`…config.info`, `auth_admin_keep` in firewalld's default
server policy) only after a non-interactive polkit `CheckAuthorization` says yes (in
the Flatpak the bus proxy blocks polkit, so the state is "unknown"). A viewer connection
or attempt counts as "firewall OK". If the port is blocked or unknown, the GUI shows
the command with a Copy button and **Open port for me…** (D-Bus add-port, runtime +
permanent per zone, stops at the first cancelled or denied authorization: one click,
at most one dialog). The logic is `mirrorwire-net/src/firewall.rs` against a
`Firewalld` trait; the D-Bus code is `firewall/system.rs`. The app never runs as root.

## 5. Packaging

`packaging/build-ffmpeg.sh` builds a minimal static FFmpeg used by every release build (with
x264 on Linux only). `packaging/macos/` makes the `.app` and a UDZO DMG (ad-hoc signed),
`packaging/flatpak/` the Flatpak (runtime `org.freedesktop.Platform//25.08`),
`packaging/appimage/` the AppImage (built on Ubuntu 24.04, needs glibc 2.39+). See
[testing.md](testing.md) §5 for CI.

## Android viewer (APK)

```
packaging/android/MainActivity.java   extends NativeActivity; the only Java. Holds the Wi-Fi
                                      MulticastLock (mDNS), keeps the screen on, writes an
                                      opened mirrorwire:// link to files/pending_uri.txt, and
                                      lists the H.264 decoders (MediaCodecList, no NDK API)
                                      into files/h264_decoders.txt
crates/mirrorwire-android             android_main -> eframe (glow) -> ViewerApp
  app.rs      home (mDNS list, 6-digit keypad, link field), connecting steps, video + overlay;
              polls pending_uri.txt every 500 ms (QR scanned with the camera app opens the link)
  session.rs  QUIC receive loop (like mirrorwire-app's viewer, but a 6-deep decode queue)
              + a decode thread that also polls MediaCodec every 8 ms for late pictures
  codec.rs    AMediaCodec by name from codec_list.rs (hardware decoders first, then
              software; a decoder that fails to start or errors 3x is skipped, the reason
              is shown) -> AImageReader YUV_420_888 -> yuv.rs -> RGBA. Pictures that a
              newer queued frame supersedes are released unrendered (no RGBA copy).
  codec_list.rs  parses/ranks h264_decoders.txt; vendor low-latency keys (Qualcomm, Exynos)
  stats.rs    pipeline counters -> HUD "in / decoded / shown fps" + a stall warning that
              names the stuck stage
packaging/android/build-apk.sh        cargo-ndk + javac/d8 + aapt2 + zipalign + apksigner (no Gradle)
```

The Java shell stays minimal: new features go into the Rust crate. It only grows for what has
no NDK API and would otherwise need JNI (`unsafe`), handed over as a file (pending link, decoder
list). Debug output:
`adb logcat -s mirrorwire`.

