# Roadmap, open issues, known debt

Status words: **done** (measured on target hardware), **built** (compiles/CI, untested on
target), **planned**, **idea**. Update this file when an item moves.

## State (2026-11)

| Area | State |
|---|---|
| Loopback pipeline (test pattern → x264 → QUIC → decode) | done on a VM (≈9 ms capture→decoded mean at 2000×1000@60) |
| Portal + PipeWire capture on Fedora KDE | done on the T480s (first real stream works) |
| VAAPI encode on UHD 620 | works on the T480s; was on `cpu+vaapi-upload` at 6–10 fps (photos 2026-11-12) |
| DMA-BUF zero-copy capture → VAAPI | built (2026-11-13), untested on the T480s; shm fallback built |
| Shared-memory VPP path | built: the VPP graph got pixel format "bgrz" (invalid for FFmpeg) so every BGRx frame fell back to the CPU; fixed, untested on hardware |
| Viewer on the iMac (OpenGL, DMG x86_64) | works on the target iMac |
| Pairing, pinned TLS, mDNS | works between the target machines (after opening 47800/udp) |
| Flatpak, AppImage, DMGs in CI | built; Flatpak and x86_64 DMG run on target |
| Android viewer APK (arm64-v8a, MediaCodec) | built (2026-11-13), untested on a device |
| Browser (WebAssembly) viewer | design sketch in Planned and Future, not started |
| Camera QR scan in the desktop viewer | not started (Android: the phone's camera app opens the `mirrorwire://` link) |
| Cursor sent as coordinates (portal METADATA cursor, pointer datagrams, viewer-drawn) | built (2026-11-13), loopback-tested with a synthetic cursor; untested with KWin on the T480s |
| 60 fps / max frame-rate setting, negotiated rate shown | built (2026-11-13); KWin's reported max untested |
| Click-freeze in the viewer (egui nested input lock) | fixed (2026-11-13), untested on the iMac |
| Forward error correction for video (Reed-Solomon per block, interleaved, adaptive from viewer loss reports; `--fec auto|off|N`, Quality panel) | built (2026-11-13), loopback-tested with simulated random and bursty loss (protocol.md §3b); untested on real Wi-Fi and on Android hardware |
| Adaptive bitrate (AIMD on viewer loss + QUIC RTT, FEC parity inside the budget, auto FEC capped at 35 %/60 %; `--adaptive-bitrate`, Quality panel) | built (2026-11-13), loopback-tested with a simulated bottleneck (testing.md §3.2); untested on real Wi-Fi |
| Latency spikes on scene changes (2 s VBV, unbounded QUIC queue, 4-frame decode backlog, no pacing) | fixed (2026-11-13): 100 ms VBV, 2 MiB datagram queue with delta-frame drop, 2-frame decode queue flushed on overflow, 500 Mbit/s pacing; untested on Wi-Fi hardware |

## Next (owner-agreed order)

0. **TOP PRIORITY: reliable, likable transport (owner, 2026-11-14).** The prototype only
   continues if streaming over real Wi-Fi is stable and pleasant. Do these first, in order:
   1. **Explain the adaptive-bitrate cuts:** log the reason for each cut (loss / RTT / backlog plus
      the values) and add a closed-loop simulation test (rate controller + `LossSim`/`BandwidthSim`
      + encoder model, no QUIC). Find out why "no limit + 2 % random loss" holds only ~17 of
      40 Mbit (testing.md §3); the 3-point excess-loss rule means 2 % alone cannot cut, so the
      trigger is RTT, backlog drops or window edge effects. Fix the doc explanation. ~½ day.
   2. **Delay-gradient signal** (GCC-style trendline): one-way delay trend from `capture_us` /
      arrival time reported in the `LinkReport` (protocol bump). Cut only on loss plus rising delay;
      loss with flat delay counts as random. ~250–400 LOC, 1–1.5 days.
   3. **Ignore loss right after a keyframe or VAAPI reopen** so reopen, then IDR burst, then another
      cut can't loop; spread IDR parity across frames. ~60–100 LOC.
   4. **Hysteresis and probing:** remember the last good budget, climb slowly near it, probe with
      extra FEC before more video, start from the last good value per peer. ~100–150 LOC.
   5. **Don't cut when FEC recovered everything** (`unrecoverable == 0`); smooth the loss shared by
      FEC and rate control. ~40–80 LOC.
   6. **Live VAAPI bitrate without a reopen** (VA-API misc-param buffers or a QP clamp), removing
      the 5 s reopen limit. ~200–300 LOC FFI, hardware-dependent.
   7. Small fixes: `lan_cc.rs` window never grows back after persistent congestion; the UI should
      say CQP/ICQ ignore adaptive bitrate.
   Alongside: fuzz the proto parsers (fragment, fec, cursor, control) and remove the panicking
   index/unwrap sites in proto/net, since hostile or corrupt packets must never crash a stream.

0b. **Structure improvements (owner, 2026-11-14): after item 0, before widening the scope**
   (bottleneck line, Extend mode, phone input, Mac presenter). Borrowed from PhotoCraft:
   1. **Lints enforced by tooling:** `[workspace.lints]` plus `clippy.toml` (unwrap/expect/panic
      allowed in tests only); `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,
      clippy::indexing_slicing)]` in proto and net; `forbid(unsafe_code)` everywhere except media
      and the glow renderer; `clippy::undocumented_unsafe_blocks` with a `// SAFETY:` comment on
      all remaining unsafe sites; first try to remove the `unsafe` in net/diag.rs,
      app/applog.rs and portal.rs via safe APIs (D15); lock poisoning via `unwrap_or_else(PoisonError::into_inner)`. ~1 day.
   2. **cargo-fuzz targets** for fragment, fec, cursor and control decode, plus a CI job (if not
      already done alongside item 0). ~½ day.
   3. **One formatting-only commit** (`cargo fmt`, `rustfmt.toml`), then a `cargo xtask ci` gate:
      fmt, clippy, tests, a crate-layer check (architecture.md made mechanical), loopback test.
   4. **Machine-checked perf budgets:** loopback p95 capture→decoded and the adaptive-bitrate
      simulation rows as TOML budgets checked in CI, instead of prose tables in testing.md.
   5. **Split the big files** (encode.rs, presenter.rs, transport.rs; move FecState and the rate
      wiring out of transport.rs) and merge the duplicated encoder-config comparison in
      pipeline.rs (`maybe_reopen` / `maybe_replace_encoder`).
   Already in force: decision records with alternatives (decisions.md) and small commits (AGENTS.md).

0c. **Remove `unsafe` Rust where possible (owner, 2026-11-14; after 0 and 0b, D15).** Go through
   every `unsafe` site (encode.rs, decode.rs, hw.rs, source/portal.rs, yuv_glow.rs, applog.rs,
   net/diag.rs) and check for a safe replacement: safe methods of `ffmpeg-next`/`ffmpeg-sys`
   wrappers, `glow`/egui-safe GL paths or a wgpu-only path, safe crates for the libc/socket calls,
   dropping the `unsafe impl Send` by restructuring threads. Remove what can go; for what must
   stay, write down why (per site) and report the list to the owner. Goal: no `unsafe` outside
   `mirrorwire-media`, then `#![forbid(unsafe_code)]` in proto, net and app.

1. **Quality and latency fix:** stream at the screen's native resolution (no fixed
   2000×1000 scaling), higher default bitrate for desktop text, move colour conversion and
   scaling to the GPU (VAAPI `scale_vaapi`/hw upload) so `capture_to_encoded` drops.
2. **Connection diagnostics in the GUI:** per-step status (resolve, handshake, cert, pairing,
   stream) with actionable advice, for photo-based debugging.
3. **FilmCraft encoder comparison:** built (feature `filmcraft`, `--encoder filmcraft`, GUI
   selector; see decisions.md log and testing.md §3.1). Still open: the owner's side-by-side on
   the T480s (photos of the presenter status with each encoder), a real force-IDR / intra
   refresh upstream instead of re-creating the encoder, and switching back to DMA-BUF capture
   when the user returns to VAAPI without restarting.
4. **Optional system FFmpeg:** ship without x264 and use an installed FFmpeg for VAAPI when the
   user wants it (Flatpak: `org.freedesktop.Platform.ffmpeg-full` extension). Would make Linux
   builds GPL-free (D8).
5. **Bottleneck diagnosis line (next up):** a "Bottleneck: <stage> – <reason> + hint" line on
   the viewer HUD and the presenter status, from the per-stage stats (capture fps vs requested,
   encode time and hw/sw encoder, network loss/RTT/jitter, reassembly, decode time and hw/sw
   decoder, render). Examples: "network – 22 % loss, likely Wi-Fi; cable or 5 GHz helps",
   "presenter – desktop sends only 11 fps", "driver – CPU encoding, no hardware H.264 encoder",
   "viewer – software decode 11 ms, OK". A coarse rating (good / OK / weak), no percentages;
   documented thresholds with hysteresis so it does not flicker. The viewer reports its stats to
   the presenter so both show the same verdict (capability-negotiated, protocol.md update).
   Unit-test the classifier. LOW priority follow-up: "percent of achievable quality/speed, Wi-Fi
   vs cable detection" – unreliable, because the achievable maximum depends on the encoder,
   content and the other traffic on the link, and the OS does not tell us reliably whether the
   path crosses Wi-Fi.
6. **Extend mode / virtual monitor (next up):** presenter setting Mirror/Extend (CLI, env,
   settings.json, start screen, Quality panel). Extend asks the ScreenCast portal for source
   type VIRTUAL (4) after checking AvailableSourceTypes (KDE Plasma 6 and GNOME mutter support
   it); Mirror asks for MONITOR. Switching mode re-opens the portal dialog; one restore token per
   mode. If VIRTUAL is unavailable, fall back to Mirror with a clear note. Ideally the viewer
   sends its window/display size in hello; otherwise the UI says resolution and position are set
   in the system display settings. Must work in the Flatpak (check the portal version needed).
   Tests, docs.

## Planned

- **Adaptive bitrate follow-ups:** tune the thresholds on the T480s → iMac Wi-Fi; a live
  bitrate change for VAAPI (FFmpeg's h264_vaapi only reads the rate when opened, so today it
  reopens with a keyframe, rate-limited); per-frame pacing to the budget.
- **FEC follow-ups:** spread a keyframe's parity over the next frame interval (covers longer
  bursts at the cost of latency); measure on real Wi-Fi and tune the auto curve.
- **Zero-copy (viewer):** VideoToolbox → texture and a YUV→RGB shader on the wgpu path too
  (glow already has the shader). The presenter side (DMA-BUF → VAAPI) is built.
- **DMA-BUF modifiers from VAAPI:** today a fixed list (Intel Y/X-tiled, linear, implicit) is
  offered; query the driver's importable modifiers instead (AMD needs this).
- **Camera QR scanning** in the viewer (the URI is already in the QR).
- **Virtual monitor (extend) mode** via the portal's virtual source type.
- **GitHub Releases** for stable download links (release job exists for `v*` tags, never run;
  the owner decides when to tag).
- Flathub (vendored cargo sources), notarized Mac builds (needs a paid Apple account).
- Miracast sink as a second `VideoSink`.
- **Android viewer polish:** zero-copy MediaCodec → `SurfaceTexture`/`AHardwareBuffer` → GL
  texture instead of the CPU YUV→RGBA copy; soft keyboard for the link field (today: QR via
  the camera app, or the keypad + mDNS list); a stable release keystore so updates install
  over each other (CI debug keys change per run).
- **Android presenter** later (MediaProjection + MediaCodec encode; needs Java/JNI glue).
- **Browser viewer (WebAssembly), sketch:** see also [Future: phone/tablet as input device](#future-phonetablet-as-input-device) for the agreed trust model and input-mode scope.
  - Presenter: add an HTTP/3 WebTransport endpoint on the presenter's own UDP port (any port, including 47800; not 443), e.g. `wtransport` or `h3` + `quinn`. Video goes as WebTransport datagrams with the existing fragment header; control messages on a bidirectional stream. Decode with **WebCodecs**.
  - Trust: browsers reject self-signed certs unless pinned via `serverCertificateHashes` (cert valid ≤ 14 days, ECDSA P-256). The presenter mints a short-lived cert and puts its hash in the QR/URL. The web page itself must come from a normal HTTPS origin (e.g. GitHub Pages); only the WebTransport session goes to the laptop on the LAN. Pairing is bound to the pairing identity, not to one certificate, so pairings survive automatic cert rotation.
  - Page: egui compiled to wasm (or plain JS) + WebCodecs `VideoDecoder` (`avc1.42E0xx`, `optimizeForLatency: true`), drawing `VideoFrame`s onto a canvas.
  - Wire: reuse `mirrorwire-proto` compiled to wasm (it has no I/O by design, see architecture.md §1).
  - The viewer and the input modes in the Future section can all run in the browser. The presenter stays a native app.


## Future: phone/tablet as input device

**Status: idea.** Parked until screen mirroring is solid and the owner sends a UI
sketch. Do not start this work until then.

The phone or tablet already shows the laptop screen as a MirrorWire viewer.
These modes add a reverse input channel so the same device can control the
presenter: a PowerPoint remote, a laser pointer, a drawing overlay, a
touchpad, and (later) a Wacom-style pen tablet. They all share one foundation.

### Foundation

Build the reverse input channel first, over the existing paired, encrypted QUIC
connection (do not switch to TCP).

- Unreliable datagrams for pointer and pen samples (high rate; loss is acceptable).
  **Reuse the cursor path:** the presenter→viewer pointer datagram
  (`mirrorwire-proto/src/cursor.rs`, protocol.md §3a) already has a `tool` byte and accepts
  trailing fields, so pen pressure/tilt/buttons are appended there (and the same message goes
  viewer→presenter for input), with `cursor_rect()` inverted to map tablet touches back to
  presenter pixels.
- Reliable stream for pen-down / pen-up, key events, and other edges, so a
  stroke or a key press never gets stuck.
- Virtual input on the Linux presenter via `/dev/uinput`. That needs a udev or
  permission rule, and the Flatpak sandbox cannot open uinput as-is. Plan
  around that: a small privileged helper, or the xdg-desktop-portal
  `RemoteDesktop` portal, which may cover mouse and keyboard without uinput.
  Pen-tablet injection may still need uinput.

### Modes (rough sizes)

| Mode | Notes | Rough size |
|---|---|---|
| Presenter remote | Next, previous, and click as arrow / space keys. PowerPoint, LibreOffice Impress, and PDF viewers already understand these. | ~300–500 LOC |
| Laser pointer | Finger position drawn as a coloured dot in a transparent overlay on the presenter; colours selectable. The KDE Wayland overlay is the hard part. | ~300–600 LOC |
| Drawing overlay | The same overlay, plus strokes and colours, with an optional fade-out like old Linux screen-annotation / "flame" effects. | ~800–1500 LOC |
| Touchpad | One finger moves the cursor, a tap clicks, two fingers scroll or right-click. | ~500–800 LOC |
| Pen tablet (Wacom emulation) | Pressure, tilt, hover, and buttons. Two display modes on the tablet: screen-visible (the existing viewer picture) and grey (no picture). Linux apps such as Krita and GIMP see a virtual tablet. Photoshop would need a macOS presenter, which is a separate and harder job (macOS makes faking a tablet difficult). Total includes tablet-side capture, laptop-side uinput, protocol, UI, tests, and docs. | ~1500–2500 LOC total |

The "screen shines through" mode is essentially free, because the tablet is
already a viewer. The grey mode is just "don't show the picture".

### Web viewer (WASM)

The viewer and all of the input modes above can run in the browser. The
presenter stays a native app (screen capture, virtual input, and the overlay
need access a browser cannot give).

- Transport: WebTransport over HTTP/3 on the presenter's own port (any port,
  including the existing 47800; not 443).
- Trust: `serverCertificateHashes` with the certificate fingerprint already
  carried in the QR link, so the page can say "trust exactly this certificate".
  No public hostname or internet address is required for the video/input
  connection.
- The web page itself must be served from a normal HTTPS origin (for example
  a free GitHub Pages site). Only the WebTransport session then goes to the
  laptop on the LAN.
- Browsers accept these certificates for at most 14 days, and they must be
  ECDSA. The presenter rotates the short-lived cert automatically. Pairing
  must be bound to the pairing identity, not to one certificate, so existing
  pairings survive rotation.
- Video decode via WebCodecs.

### Build order

1. Reverse input channel and virtual mouse/keyboard on the Linux presenter.
2. Presenter remote and touchpad (they rest on that channel).
3. Overlay (laser pointer, then drawing).
4. Pen tablet.
5. Web viewer in parallel when ready.


## Open questions

- Default bitrate and whether to offer 4:4:4 where the encoder supports it.
- VideoToolbox decode on the HD 6770M under OCLP: the photos show it unavailable (no H.264
  decoder for that GPU); software decode takes ≈2.4 ms/frame there, so it is fine as default.
- Line width for `rustfmt` (see Known debt).

## Known debt and doc/code mismatches

Found while writing these docs (2026-11-12, commit `f7677c7`). Fix them in their own commits.

- **Hardcoded 2000×1000:** `--width/--height` default to 2000×1000 and the GUI presenter uses
  them, so a 1920×1080 screen is scaled and distorted. The docs describe native resolution as
  the goal. (Being fixed under Next 1.)
- **Formatting:** `cargo fmt --all --check` fails on `main` (≈40 hunks in `mirrorwire-media`,
  `mirrorwire-net`, `mirrorwire-proto`); there is no `rustfmt.toml` and CI has no fmt step.
  Decide a width, add `rustfmt.toml`, one formatting-only commit, then add
  `cargo fmt --all --check` to `ci.yml`.
- **`unsafe` without `// SAFETY:` comments** in `mirrorwire-media` (`encode.rs`, `decode.rs`,
  `hw.rs`, incl. `unsafe impl Send` for FFmpeg wrappers). No workspace lint forbids `unsafe`
  elsewhere yet (consider `unsafe_code = "forbid"` in proto/net/app).
- **Panicking shortcuts in production code:** `Mutex::lock().expect(..)` throughout
  `mirrorwire-app` and `mirrorwire-net` (a poisoned lock crashes instead of recovering), plus a
  few `expect`s (`Control::encode`, scaler init, URI building) and `unwrap()` in
  `stats.rs`/`fragment.rs` header parsing (safe today after the length check, but brittle). No
  clippy lints (`unwrap_used`, `expect_used`) enforce the rule.
- **No rate limit on pairing attempts:** a 6-digit code with unlimited `hello` retries per
  presenter run. Consider a per-address attempt limit or a lockout after N wrong codes.
- **ALPN vs protocol:** `ALPN` is `mirrorwire/1` and `PROTOCOL_VERSION` is 1; nothing ties them
  together in code. Bump both together (protocol.md §4).
- **macOS presenter:** the **Present** button is disabled on macOS (no capture source there
  yet); VideoToolbox encode is built but untested.
