# Testing and verification

## 1. Gates

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/loopback-test.sh 10 --encoder x264
cargo audit
cargo deny --all-features --locked check advisories bans licenses sources
```

`cargo-audit` and `cargo-deny` are not part of the Rust toolchain; install them
with `cargo install cargo-audit cargo-deny` (or `taiki-e/install-action` in CI).

Build dependencies (Fedora and Ubuntu package lists) are in the README and in
`.github/workflows/ci.yml`. A headless box without a GPU or a portal can run all of these gates.

**Formatting note.** `main` is not yet `cargo fmt`-clean (the code was written to a wider line
width and there is no `rustfmt.toml`), and CI does not check formatting. Until the owner picks
a width and one formatting-only commit lands, don't reformat files you aren't otherwise
changing: format only the files you edit (`rustfmt --edition 2021 path/to/file.rs`) and keep
the diff readable. See [roadmap.md](roadmap.md) "Known debt".

## 2. Test layers

| Kind | Where | What it proves |
|---|---|---|
| Unit | `#[cfg(test)]` in `mirrorwire-proto` | header encode/parse, fragmentation and reassembly (loss, reordering, late fragments), control framing, clock offset, stats |
| Unit | `#[cfg(test)]` in `mirrorwire-net` | identity load/create, URI round trip (including a non-default port) and rejection of bad fingerprints, QR generation, port settings precedence, firewall guide text, ufw rule parsing |
| Unit (FilmCraft) | `mirrorwire-media/src/filmcraft.rs`, feature `filmcraft` | FilmCraft output via the real `Encoder` decodes with FFmpeg, markers read back, keyframe on request; an `#[ignore]` x264-vs-FilmCraft benchmark |
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

### 3.1 FilmCraft vs x264 (box, 2026-11-13)

Machine: the agent box, 8 vCPU Intel Xeon VM, no GPU, release build, so this says nothing about
the T480s (untested on target). Two measurements:

Encoder-only benchmark (synthetic gradient + moving pattern, 120 frames, CBR 15 Mbit/s, Y-PSNR
against a BT.601 reference, so the ceiling is ~50 dB):
`MIRRORWIRE_BENCH_SIZE=1920x1080 cargo test --release -p mirrorwire-media --features filmcraft -- --ignored bench_ --nocapture`

| Encoder | Size | encode mean / p95 | max fps | Mbit/s @60 | Y-PSNR |
|---|---|---|---|---|---|
| libx264 | 1920×1080 | 2.57 / 2.98 ms | ~389 | 14.4 | 42.10 dB |
| filmcraft-h264enc | 1920×1080 | 10.31 / 14.94 ms | ~97 | 14.6 | 41.71 dB |
| libx264 | 2000×1000 | 2.65 / 3.16 ms | ~377 | 14.3 | 41.87 dB |
| filmcraft-h264enc | 2000×1000 | 11.97 / 15.46 ms | ~84 | 14.7 | 41.54 dB |

End-to-end loopback, 15 s, test pattern at 60 fps, all frame markers read back, 0 mismatches
(`MIRRORWIRE_FEATURES=filmcraft scripts/loopback-test.sh 15 --encoder filmcraft --width W --height H --bitrate-kbps K`):

| Encoder | Size, bitrate | fps | Mbit/s | capture_to_encoded mean / p95 | capture_to_decoded mean / p95 |
|---|---|---|---|---|---|
| x264 | 1920×1080, 30 Mbit/s | 59.8 | 28.7 | 6.37 / 7.95 ms | 7.81 / 9.71 ms |
| filmcraft | 1920×1080, 30 Mbit/s | 59.9 | 21.0 | 14.67 / 19.23 ms | 15.74 / 20.82 ms |
| x264 | 2000×1000, 15 Mbit/s | 59.7 | 14.8 | 6.41 / 8.11 ms | 7.84 / 10.12 ms |
| filmcraft | 2000×1000, 15 Mbit/s | 59.9 | 15.0 | 15.61 / 20.13 ms | 17.13 / 22.70 ms |

Reading: FilmCraft keeps 60 fps at these sizes on 8 cores with similar quality per bit, but
costs ~8–9 ms more per frame than x264 (ultrafast/zerolatency) and much more CPU. At 30 Mbit/s
it undershot the target (21 Mbit/s) on this easy content. On the 4-core T480s expect it to be
slower still; VAAPI remains the default there.

Useful runs:

```sh
scripts/loopback-test.sh 30 --encoder x264 --width 1920 --height 1080 --bitrate-kbps 30000
MIRRORWIRE_FEATURES=filmcraft scripts/loopback-test.sh 15 --encoder filmcraft --width 1920 --height 1080 --bitrate-kbps 30000
scripts/loopback-test.sh 10 --intra-refresh -- --simulate-loss 0.5
```

## 4. Real hardware

The owner tests on the target machines (decisions D11) and reports with **photos of the
screen**, not logs. So:

- Every state and error must be readable in the window: presenter status panel (phase,
  listen address, encoder, source, fingerprint, pairing code, connected viewer) and viewer
  connection status and stats overlay (`S`).
- Error messages say what to do ("No answer from 192.168.1.20:47800 (UDP). … On Fedora:
  sudo firewall-cmd --add-port=47800/udp"), not just what failed.
- Ask for exactly what you need ("press S and photograph the top-left corner").
- Don't ask him to install a toolchain on the iMac for routine debugging; build artifacts in CI.

## 5. CI and artifacts

- `ci.yml` (push to `main`, PRs): clippy `-D warnings`, `cargo test`, 5 s loopback with x264,
  then clippy/tests/5 s loopback again with feature `filmcraft` and `--encoder filmcraft`,
  the reusable audit workflow, and the reusable SonarCloud scan (`needs: audit`).
- `sonarcloud.yml` (called from CI; also from Build on `v*` tags and
  `workflow_dispatch`; can be dispatched on its own): if `SONAR_TOKEN` is
  missing (fork PRs), the job succeeds and skips the upload. Otherwise it
  writes a Clippy JSON report (`cargo clippy --message-format=json`) and
  unit-test coverage (`cargo llvm-cov --lcov`), then runs
  `SonarSource/sonarqube-scan-action` pinned to a full SHA. Report paths
  live in `sonar-project.properties` (`sonar.rust.clippyReport.reportPaths`,
  `sonar.rust.lcov.reportPaths`). The loopback test is not included in
  coverage, so this stays a short extra compile. **Automatic Analysis must
  be off** under SonarCloud Administration › Analysis method for
  `windwerfer_mirrorWire`; if it is still on, the upload fails and should
  not be worked around in the workflow.
- `codeql.yml` (push to `main`, pull requests, weekly Wednesday cron, and
  `workflow_dispatch`; also called from Build on `v*` tags and
  `workflow_dispatch`): GitHub CodeQL advanced setup for `rust` and
  `actions`, both with `build-mode: none` (no Linux media deps, no workspace
  compile). `github/codeql-action` init/analyze are pinned to a full SHA.
  Results upload to Security › Code scanning. **CodeQL default setup must
  be off** under Settings › Code security › Code scanning; if it is still
  on, the advanced workflow fails and should not be worked around.
  Branch-push Builds do not re-run this job, because the standalone
  workflow already ran on the same push.
- `audit.yml` (called from CI and Build; also `workflow_dispatch` and a weekly Monday cron):
  `cargo audit` against the RustSec advisory DB, and
  `cargo deny check advisories bans licenses sources` with `deny.toml`.
  `cargo audit` fails only on vulnerabilities; yanked and unmaintained crates are warnings.
  `cargo-deny` does the same for yanked crates, ignores unmaintained advisories (it has no
  warn-only level for them; `cargo audit` still prints those), fails on unsound crates and
  on any licence or source outside the allow-list, and warns on duplicate crate versions.
  Only crates.io is an allowed source. If an advisory hits a transitive dependency with no
  fix yet, add a justified `{ id = "RUSTSEC-…", reason = "…" }` ignore in `deny.toml` (and
  `--ignore` for `cargo audit` if needed) and report it; do not silently pass a vulnerability.
- `build.yml` (push to `main`, `v*` tags): the macOS, AppImage and Flatpak jobs all `needs:
  audit`, so a vulnerable or policy-violating lockfile never produces an artifact. Then:
  macOS arm64 DMG (`macos-15`), macOS x86_64 DMG (`macos-15-intel`, min 10.13), Flatpak,
  AppImage (Ubuntu 24.04); a `v*` tag creates a GitHub Release with all four. The Intel Mac
  job is the slowest (static FFmpeg build on older runners); that is normal.
- Artifacts are uploaded with `archive: false`, so each download is the bare `.dmg` /
  `.flatpak` / `.AppImage`.
- **Re-runs hide artifacts:** if a job is re-run, the run page shows the latest attempt, which
  only has the re-run job's artifact. The others are on `…/actions/runs/<id>/attempts/1`. When
  you hand out links, check every artifact exists and link the attempt that has it (`gh api
  repos/windwerfer/mirrorWire/actions/runs/<id>/artifacts`).
- Do not change workflows as a side effect of a feature. A workflow change is its own commit.
