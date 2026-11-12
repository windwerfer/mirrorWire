# Testing and verification

## 1. Gates

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/loopback-test.sh 10 --encoder x264
```

Build dependencies (Fedora and Ubuntu package lists) are in the README and in
`.github/workflows/ci.yml`. A headless box without a GPU or a portal can run all four gates.

**Formatting note.** `main` is not yet `cargo fmt`-clean (the code was written to a wider line
width and there is no `rustfmt.toml`), and CI does not check formatting. Until the owner picks
a width and one formatting-only commit lands, don't reformat files you aren't otherwise
changing: format only the files you edit (`rustfmt --edition 2021 path/to/file.rs`) and keep
the diff readable. See [roadmap.md](roadmap.md) "Known debt".

## 2. Test layers

| Kind | Where | What it proves |
|---|---|---|
| Unit | `#[cfg(test)]` in `mirrorwire-proto` | header encode/parse, fragmentation and reassembly (loss, reordering, late fragments), control framing, clock offset, stats |
| Unit | `#[cfg(test)]` in `mirrorwire-net` | identity load/create, URI round trip and rejection of bad fingerprints, QR generation |
| Loopback (end to end) | `scripts/loopback-test.sh` | test pattern → encoder → QUIC (pinned TLS, pairing) → reassembly → decoder, on localhost, with latency stats |
| Real hardware | the owner's T480s + iMac | portal capture, VAAPI, Wi-Fi, OpenGL on a non-Metal Mac (only he can run this) |

### The oracle: the frame marker

The test pattern stamps its frame number into the image as black/white blocks
(`pattern::stamp_marker`). The headless viewer reads it back from the *decoded* picture
(`pattern::read_marker`) and counts mismatches. That proves the frame shown is the frame sent,
independent of our own bookkeeping. Keep this oracle working: any change to scaling, colour
conversion or the decoder must still read the markers back.

Use FFmpeg's `ffprobe`/`ffmpeg` only as an **external** check (for example, decoding a dumped
bitstream to prove an encoder's output is standard H.264). Never make the app depend on a
command-line ffmpeg at runtime.

## 3. Measuring latency and quality

The loopback and every viewer print one stats line per second, and the presenter prints its own:

- viewer: `fps, Mbit/s, rtt, ms mean/p95/max:` per stage
- presenter: `fps, Mbit/s, keyframes, mtu, ms mean/p95/max:`

Stages: `capture_to_encoded` (presenter: capture, colour conversion/scaling, encode),
`encoded_to_sent`, `network` (clock-offset corrected), `reassembly`, `decode`, `convert`
(YUV→RGBA, GUI only), `capture_to_decoded` (end to end). The GUI overlay adds
`capture->texture`.

Rules (adapted from FilmCraft's "report progress honestly"):

- **Before/after on the same machine, same build profile, same arguments,** at least 10 s. Quote
  mean and p95 of `capture_to_decoded`, plus fps and Mbit/s. State the machine (CPU, GPU or "VM,
  no GPU") and that it was loopback.
- **Name what was measured vs estimated.** A VM result says nothing about VAAPI on UHD 620 or
  decode on the iMac. Write "untested on target" when that's the truth.
- Use `--simulate-loss <percent>` on the viewer to test loss recovery; quote lost frames and
  keyframe requests.
- Quality: compare at the same bitrate and resolution. Native resolution with no scaling is the
  baseline; a sharpness claim needs a photo/screenshot of text at 100 % or a PSNR/SSIM number
  from a decoded dump.

Useful runs:

```sh
scripts/loopback-test.sh 30 --encoder x264 --width 1920 --height 1080 --bitrate-kbps 30000
scripts/loopback-test.sh 10 --intra-refresh -- --simulate-loss 0.5
```

## 4. Real hardware

The owner tests on the target machines (decisions D11) and reports with **photos of the
screen**, not logs. So:

- Every state and error must be readable in the window: presenter status panel (phase,
  listen address, encoder, source, fingerprint, pairing code, connected viewer) and viewer
  connection status and stats overlay (`S`).
- Error messages say what to do ("no answer from 192.168.1.20:47800 — check the presenter's
  firewall for UDP 47800"), not just what failed.
- Ask for exactly what you need ("press S and photograph the top-left corner").
- Don't ask him to install a toolchain on the iMac for routine debugging; build artifacts in CI.

## 5. CI and artifacts

- `ci.yml` (push to `main`, PRs): clippy `-D warnings`, `cargo test`, 5 s loopback with x264.
- `build.yml` (push to `main`, `v*` tags): macOS arm64 DMG (`macos-15`), macOS x86_64 DMG
  (`macos-15-intel`, min 10.13), Flatpak, AppImage (Ubuntu 24.04); a `v*` tag creates a GitHub
  Release with all four. The Intel Mac job is the slowest (static FFmpeg build on older runners);
  that is normal.
- Artifacts are uploaded with `archive: false`, so each download is the bare `.dmg` /
  `.flatpak` / `.AppImage`.
- **Re-runs hide artifacts:** if a job is re-run, the run page shows the latest attempt, which
  only has the re-run job's artifact. The others are on `…/actions/runs/<id>/attempts/1`. When
  you hand out links, check every artifact exists and link the attempt that has it (`gh api
  repos/windwerfer/mirrorWire/actions/runs/<id>/artifacts`).
- Do not change workflows as a side effect of a feature. A workflow change is its own commit.
