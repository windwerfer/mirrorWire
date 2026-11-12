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
| Camera QR scan, Android | not started |

## Next (owner-agreed order)

1. **Quality and latency fix:** stream at the screen's native resolution (no fixed
   2000×1000 scaling), higher default bitrate for desktop text, move colour conversion and
   scaling to the GPU (VAAPI `scale_vaapi`/hw upload) so `capture_to_encoded` drops.
2. **Connection diagnostics in the GUI:** per-step status (resolve, handshake, cert, pairing,
   stream) with actionable advice, for photo-based debugging.
3. **FilmCraft encoder comparison:** add `filmcraft-h264enc` (pure Rust, MIT/Apache) as a
   switchable software encoder next to x264; pin a git revision (not on crates.io, very young
   project, no intra refresh). Compare with the loopback stats on the T480s.
4. **Optional system FFmpeg:** ship without x264 and use an installed FFmpeg for VAAPI when the
   user wants it (Flatpak: `org.freedesktop.Platform.ffmpeg-full` extension). Would make Linux
   builds GPL-free (D8).

## Planned

- **FEC for Wi-Fi:** Reed-Solomon parity fragments per frame so single losses don't cost a
  keyframe. Needs a `WIRE_VERSION` bump ([protocol.md](protocol.md) §4).
- **Adaptive bitrate** from QUIC RTT/loss and reassembly loss.
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
- **Android** presenter/viewer later (MediaProjection + MediaCodec via JNI).

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
