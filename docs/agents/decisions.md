# Fixed decisions

These were decided with the owner (windwerfer). Agents do not change them on their own. To
propose a change, write down the problem, the alternative and its cost, and ask. When the owner
agrees, update the entry (status, date, reason) in the same commit as the change.

Every new or changed decision also records **Alternatives considered**: what else was on the
table and why it lost (cost, risk, owner preference). The table stays short; put the details in
the decision log below the table. The point is that a different agent, months later, can see
not only what was chosen but what was already rejected and why, so it isn't proposed again
without new information.

| # | Decision | Why |
|---|---|---|
| D1 | **No special privileges.** Capture only via xdg-desktop-portal ScreenCast + PipeWire. No root, `setcap`, KMS/DRM capture, kernel modules, uinput. | The owner refuses tools that need elevated rights (the reason Sunshine was ruled out). Portals are the sanctioned Wayland path and work in a Flatpak. |
| D2 | **Flatpak-friendly, minimal permissions.** `finish-args`: `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc`, `--share=network`, `--device=dri`, `--system-talk-name=org.fedoraproject.FirewallD1`. Nothing else. | Sandboxed install without extra rights. Screen capture needs no permission because the portal asks the user. The firewalld name was added with the owner's approval (2026-11) so the presenter can detect a blocked port and offer a polkit-authenticated Open-port button; the app never runs as root. |
| D3 | **Transport: QUIC (`quinn`).** Video as unreliable datagrams with per-frame fragmentation; one reliable bidirectional control stream. | Low latency without TCP head-of-line blocking; TLS 1.3 built in; one UDP port. |
| D4 | **Security: pinned self-signed certs + pairing code.** Each install generates a cert once. The viewer pins the presenter's SHA-256 fingerprint (from QR/URI or mDNS). The presenter streams only to a viewer that sent the 6-digit pairing code once, then remembers it by client-cert fingerprint. No plaintext or skip-verify mode. | "Secure and easy": no CA, no accounts, keys generated once. |
| D5 | **Identifiers:** app id `io.github.windwerfer.MirrorWire`; mDNS service `_mirrorwire._udp`; default UDP port `47800`; ALPN `mirrorwire/1`; URI scheme `mirrorwire://`; config dir name `mirrorwire`. | Users' firewall rules (firewalld `47800/udp`), pairings and launchers depend on them. |
| D6 | **Wire versioning:** every incompatible change bumps the relevant version (see [protocol.md](protocol.md) §4) and updates protocol.md in the same commit. | Presenter and viewer are updated separately (Mac DMG vs Linux Flatpak). |
| D7 | **Codec: H.264, low-latency profile** (no B-frames, no lookahead, IDR on demand, optional intra refresh). Hardware encode first (VAAPI on Linux, VideoToolbox on macOS), x264 as fallback. | Universal hardware decode, including the 2011 iMac's CPU path. HEVC/AV1 decode is not available in hardware on the target viewer. |
| D8 | **Licence:** source `MIT OR Apache-2.0`. The Linux release builds statically link x264 (GPL-2.0-or-later), so those *binaries* are GPL; macOS builds use only FFmpeg's LGPL parts + VideoToolbox. Never copy GPL code into the source tree. | Permissive source; GPL only via packaging. A GPL-free Linux build is a roadmap item (pure-Rust encoder or system FFmpeg). |
| D9 | **macOS targets:** x86_64 minimum macOS 10.13 (`MACOSX_DEPLOYMENT_TARGET`), arm64 minimum 11.0. Artifacts are DMGs, ad-hoc signed, not notarized. | The target viewer is an old Intel iMac. Notarization needs a paid Apple account (owner decision). |
| D10 | **Renderer default:** OpenGL (`glow`) on macOS x86_64, wgpu elsewhere, with automatic fallback to glow if wgpu fails. | Old Intel Macs (Radeon HD 6770M) have no Metal; OCLP-patched macOS runs OpenGL more reliably. |
| D11 | **Target hardware.** Presenter: Lenovo ThinkPad T480s, Fedora, KDE Plasma (Wayland), Intel UHD 620 (VAAPI via RPM Fusion `intel-media-driver`). Viewer: iMac 27" Mid 2011 (i5, Radeon HD 6770M, 2560×1440) on macOS 15 Sequoia via OpenCore Legacy Patcher. | Every performance decision is judged on these two machines, not on a fast dev box. |
| D13 | **Linux is Wayland-first.** Native Wayland is the target for presenter and viewer. X11 that comes for free (winit's X11 backend, Flatpak `--socket=fallback-x11`) stays, but no X11-specific code paths or workarounds are written (no Xlib/XCB, X11 clipboard, or X11 capture). The app only *reports* which window system it runs on. | The owner's decision (2026-11). Fedora KDE is Wayland; X11 branches cost testing time and drift. Example: the clipboard uses egui/winit (smithay-clipboard on Wayland), not an X11 clipboard. |
| D14 | **Never ask for elevated privileges without an explicit user click after an explanation.** Nothing on the startup, reconnect or refresh path may cause a polkit/sudo/password dialog. A privileged action runs only from a button whose text says it will ask for the admin password; one click causes at most one dialog, a cancel or denial stops it with no retry, and the app does not ask again until the next click. If the needed information is only available with authorization, show "unknown" instead. | The owner (2026-11, after a polkit prompt opened at app start on the T480s): "we do NOT want a random app asking for root/user password, nobody will trust us." |
| D15 | **No `unsafe` Rust if at all possible; the owner is told about every new `unsafe`.** Safe APIs or another design come first. Unavoidable FFI `unsafe` (FFmpeg, VA-API, GL, JNI) is kept minimal, documented with `// SAFETY:`, never exposed in a public API, and never placed in proto or net. Every agent that adds or widens an `unsafe` block reports it to the owner with the file, the reason, and the safe alternative it rejected. | The owner (2026-11-14): "i would prefer no unsafe rust code, if at all possible. and being notified when you decide to do so." Alternatives considered: allowing `unsafe` freely in media (status quo, rejected as unreviewed risk); forbidding it entirely (rejected for now, because FFmpeg hardware encode/decode has no fully safe Rust binding). |
| D12 | **Push policy:** direct pushes to `main` are allowed (no PR required); never force-push or rewrite history; commit identity `Thomas Grok Bot Barth <windwerfer@gmail.com>`. | Small single-owner project; the owner wants fast iteration but an intact history. |

## Not goals (decided)

- WebAssembly *presenter*: capture is native. A browser *viewer* (WebTransport + WebCodecs) is
  planned, see [roadmap.md](roadmap.md); not built yet.
- AirPlay receiver/sender: proprietary, and old Macs can't receive it anyway.
- Uncompressed video (~2.9 Gbit/s at 2000×1000@60): infeasible over Wi-Fi/USB networking.
- Root-based capture (KMS grab) for lower latency: conflicts with D1.

## Decision log

Newest first. Add an entry when the owner decides something non-obvious.

- **2026-11 (Adaptive bitrate):** after a Wi-Fi test with 22–24 % loss, ~2000 unrecoverable
  frames and 9 fps at the viewer (auto FEC had climbed to +60 %/+100 %: more parity, more
  load, more loss), the owner approved: FEC parity is paid from the bitrate budget, auto FEC is
  capped at 35 % (delta) / 60 % (keyframes) and its boost does not grow under congestion, and an
  AIMD controller (floor 3 Mbit/s, ceiling = the bitrate setting) follows the viewer's loss
  reports and the RTT. On by default; `--adaptive-bitrate`, `MIRRORWIRE_ADAPTIVE_BITRATE`,
  `settings.json`, Quality panel. x264 dropped `nal-hrd=cbr` (filler padded every frame to the
  target and blocks live rate changes).

- **2026-11 (No unprompted password dialogs, D14):** on Fedora KDE (Flatpak) a polkit
  dialog opened as soon as presenting started, although 47800/udp was already open: the
  firewall probe called firewalld's `queryPort`/`queryService`, which need
  `org.fedoraproject.FirewallD1.config.info` (`auth_admin_keep` in firewalld's default
  server policy; firewalld always lets polkit interact). Cancelling the Open-port
  dialog showed it again for each following call. Now the probe uses only
  `.info` calls (rules only after a non-interactive polkit check), a viewer reaching
  the presenter counts as OK, and the opt-in button stops at the first cancelled
  authorization. The presenter window also scrolls, with diagnostics under "Details".

- **2026-11 (Presenter port and firewall):** the owner does not want a "magic always-open
  port" (none exists across distros). Keep default UDP 47800 (D5). The port is configurable
  (`--port`, `MIRRORWIRE_PORT`, a saved field on the presenter start screen). mDNS and the
  QR/URI already carry the port, so the viewer finds a changed port automatically. If a
  firewall is detected, the presenter shows a prompt: an **Open port for me…** button
  when firewalld is reachable over D-Bus (polkit password prompt, runtime + permanent; see D14),
  otherwise a short general guide (Fedora `firewall-cmd`, one Ubuntu `ufw` line, and "look
  up your own distro if different") — not a list of every OS. The same guide is in the
  README troubleshooting section. A `mirrorwire.xml` firewalld service (UDP 47800) is
  shipped in the Flatpak/AppImage for packagers; it only works once installed on the host.
- **2026-11 (FilmCraft encoder built):** `filmcraft-h264enc` is an optional presenter encoder
  behind cargo feature `filmcraft` (`--encoder filmcraft`, or the Encoder selector in the
  presenter's Quality panel). FFmpeg stays the default; `auto` never picks FilmCraft. The git
  dependency is pinned to rev `cb9783ba` of `storytold/filmcraft` (it pulls `craft-media` from
  `storytold/craft-libs` at a pinned rev); `deny.toml` allows exactly those two git sources.
  Input is the same swscale I420 picture x264 gets (shared-memory capture; a DMA-BUF stream is
  switched to shared memory). Settings: Constrained Baseline, `speed` preset, no B-frames,
  no scenecut, CBR/VBR/QP from the usual RC options, one slice per core (2–8,
  `MIRRORWIRE_FILMCRAFT_SLICES` overrides). FilmCraft has no force-IDR call and no intra
  refresh, so a keyframe request re-creates the encoder (rate-control history restarts).
  Enabled in the Flatpak and AppImage (+0.8 MB stripped, a few seconds of build time); not in
  the DMG (the Mac is a viewer) or the APK (viewer only). Box numbers are in
  [testing.md](testing.md) §3.1; T480s numbers are still untested on target.
- **2026-11 (Wayland-first, D13):** after "Copy log" failed on the T480s (arboard tried an X11
  connection in the Flatpak), the owner fixed Linux as Wayland-first: keep free X11 fallback,
  never write X11-specific code. Clipboard now goes through egui/winit (smithay-clipboard).
- **2026-11 (Android viewer):** the owner wants an installable APK (thin Kotlin/Java shell that
  is never touched, loading the Rust viewer, the neurofeed pattern), and later a browser
  viewer for people who don't want to install anything. Built as: one `NativeActivity`
  subclass in Java (multicast lock, link hand-off), all logic in `crates/mirrorwire-android`
  (egui/glow, MediaCodec, no FFmpeg), APK built without Gradle, debug-signed arm64-v8a,
  minSdk 26. The browser viewer (WebTransport + WebCodecs) is a roadmap sketch, not built.

- **2026-11 (GitHub CodeQL):** the owner wants CodeQL as a second, separate
  security check next to SonarCloud, on every push to `main`, on pull
  requests, and as part of tag/manual Builds, plus a weekly schedule.
  Rust and GitHub Actions are analyzed with `build-mode: none`. Default
  setup must stay off so the advanced workflow can upload results.
- **2026-11 (SonarCloud CI analysis):** the owner added a `SONAR_TOKEN` repository
  secret and wants scans to run from GitHub Actions (Clippy report + lcov)
  instead of Automatic Analysis. Automatic Analysis must be turned off under
  Administration › Analysis method, or the CI upload is rejected.
- **2026-11 (FilmCraft encoder):** the owner wants the FFmpeg version finished first, then the
  pure-Rust `filmcraft-h264enc` added as a switchable option for a side-by-side comparison. x264
  stays until then. Optional "use the system FFmpeg" mode accepted as a plan.
- **2026-11 (DMG packaging):** macOS artifacts are single DMG files uploaded with
  `archive: false`, no zip inside a zip.
- **2026-11 (renderer):** OpenGL default on macOS x86_64 (D10) after the target iMac turned
  out to be non-Metal.
- **2026-11 (name):** "MirrorWire" (ScreenBeam, MirrorCast, QuickMirror and "Wi-Fi Display"
  were taken or trademarked).
