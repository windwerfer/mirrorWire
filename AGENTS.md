# AGENTS.md: MirrorWire

Rules for every contributor, human or AI. This file holds the rules that must not be broken.
The details live in [`docs/agents/`](docs/agents/README.md). If a doc there disagrees with this
file, this file wins. If the code disagrees with a doc, the doc is the bug report: say so, don't
silently "fix" either side.

## What MirrorWire is

Low-latency screen mirroring over a LAN (Wi-Fi, Ethernet, USB networking). One Rust binary,
`mirrorwire`, runs as **presenter** (capture, encode, send) or **viewer** (receive, decode,
show). The main job: put a Fedora KDE laptop's screen on an old iMac with near-HDMI latency,
sharp text, and no special privileges.

**Non-goals:** remote desktop control (no input injection), internet/relay streaming, audio
(not yet planned), a WebAssembly *presenter* (a browser viewer is planned), AirPlay or Miracast *sources*, anything that needs root,
`setcap`, kernel modules or a display-server hack.

## Orientation, in this order

1. This file.
2. [docs/agents/decisions.md](docs/agents/decisions.md): fixed decisions you must not change alone.
3. [docs/agents/architecture.md](docs/agents/architecture.md): crates, layering, pipeline.
4. [docs/agents/protocol.md](docs/agents/protocol.md) if you touch anything that crosses the network.
5. [docs/agents/testing.md](docs/agents/testing.md) and [docs/agents/debugging.md](docs/agents/debugging.md).
6. [docs/agents/roadmap.md](docs/agents/roadmap.md): what's next, known debt, open questions.
7. The code you will touch, and its tests. Read before you edit.

## 1. Never break these

1. **No special privileges.** No root, `sudo`, `setcap`, udev rules, kernel modules,
   `/dev/uinput`, KMS/DRM grabbing or X11-only capture paths. Screen capture goes through the
   xdg-desktop-portal ScreenCast API and PipeWire only.
2. **No new Flatpak permissions.** The `finish-args` in
   `packaging/flatpak/io.github.windwerfer.MirrorWire.yml` are the ceiling:
   `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc`, `--share=network`,
   `--device=dri`, and `--system-talk-name=org.fedoraproject.FirewallD1` (owner-approved,
   so the presenter can query or open its UDP port through firewalld; a password prompt
   may appear). No `--filesystem`, no `--device=all`, no other `--talk-name`. Adding
   another permission needs the owner.
3. **The security model stays:** QUIC with TLS 1.3, a self-signed per-install certificate, the
   viewer pins the presenter's SHA-256 fingerprint, and the presenter only streams to a viewer
   that showed the pairing code once (then remembered by certificate fingerprint). Never add a
   plaintext, "insecure", or skip-verification mode, not even behind a flag.
4. **Protocol changes need a version bump and a doc update in the same commit**
   ([protocol.md](docs/agents/protocol.md)). Never change the meaning of an existing field
   silently.
5. **Fixed identifiers do not change:** app id `io.github.windwerfer.MirrorWire`, mDNS service
   `_mirrorwire._udp`, default UDP port `47800`, URI scheme `mirrorwire://`, config directory
   `mirrorwire`. Users have firewall rules, pairings and launchers that depend on them.
6. **No secrets in the repository**: no tokens, keys, certificates, pairing files,
   `paired_viewers.json`, or `identity.*.der`, not even test ones that look real. Tests
   generate identities at runtime.
7. **Measure, don't guess.** Latency, bitrate and quality claims come from the loopback stats
   or a real-hardware run, with the numbers in the commit message or docs. "Should be faster" is
   not evidence.
8. **Diagnostics are visible in the GUI.** The owner tests on real hardware and reports with
   photos of the screen. A failure that only shows in a terminal log is invisible to him. Any
   new failure mode gets a human-readable message in the window.
9. **Don't crash on hostile input.** Datagrams, control messages, mDNS records, QR/URI strings
   and config files come from outside. Bound lengths, use `get()`/checked arithmetic, return
   errors. A malformed packet is dropped and counted, never a panic.
10. **Linux is Wayland-first.** The presenter and viewer target native Wayland. X11 support
    that comes for free stays (winit's X11 backend, the Flatpak `--socket=fallback-x11`), but
    never write X11-specific code: no X11 workarounds, no Xlib/XCB calls, no X11 clipboard or
    capture paths, no "if X11 then…" branches beyond reporting which window system is in use.
    Fix a Wayland problem on Wayland (portals, smithay/winit, wayland-data-control). See
    [decisions.md](docs/agents/decisions.md) D13.
11. **Licences.** The source is `MIT OR Apache-2.0`. New dependencies must be permissive (MIT,
    Apache-2.0, BSD, ISC, Zlib, Unicode, CC0, MPL-2.0 unmodified). GPL code is only ever
    *linked at build time by packaging* (x264 in the Linux release builds), never vendored or
    copied into the source tree. See [decisions.md](docs/agents/decisions.md) D8.
12. **No `unsafe` Rust if at all possible.** Prefer a safe crate API, a safe wrapper that
    already exists, or a different design. If `unsafe` really is unavoidable (FFmpeg/VA-API
    FFI, GL, JNI), **tell the owner before or in the same message as the change**: which file,
    why there is no safe way, and what alternative was rejected. Keep the block as small as
    possible, give it a `// SAFETY:` comment stating the invariant, and keep the public API
    safe. Never add `unsafe` silently, and never add it to proto or net (parsers of network
    input). See [decisions.md](docs/agents/decisions.md) D15.

## 2. Working rules

- **Understand before editing.** Open the definition before using it. Confirm a bug's cause in
  the code (or with a test) before fixing it, even when someone names a location.
- **Keep changes narrow.** Fix what was asked, where the problem is. No drive-by refactors or
  reformatting of files you didn't otherwise change.
- **Test-first where it's cheap.** A bug fix in `mirrorwire-proto` or `mirrorwire-net` comes
  with a unit test that fails without the fix.
- **No new heavy dependency without a reason** written in the commit message: what it replaces,
  binary-size and build-time cost, licence, and why a small hand-written version is worse.
  Check `Cargo.toml` before assuming a crate is available.
- **Keep the docs in sync.** A change to behaviour, CLI flags, defaults, protocol, packaging or
  a fixed decision updates `README.md` and the matching file in `docs/agents/` in the same
  commit.
- **Report only what ran.** Say what you tested, on what machine, and what you could not test
  (no GPU, no Mac, no portal on the build box). Never mark hardware paths "working" from a VM.

## 3. Gates before every push

```sh
cargo fmt --all                                    # see the formatting note in testing.md
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/loopback-test.sh 10 --encoder x264         # end to end on localhost
cargo audit                                        # RustSec advisories; fail on vulnerabilities
cargo deny --all-features --locked check advisories bans licenses sources
```

Install `cargo-audit` and `cargo-deny` once (`cargo install cargo-audit cargo-deny`).
CI (`.github/workflows/ci.yml`) runs clippy, tests, a 5 s loopback, the audit
workflow (`.github/workflows/audit.yml`), and SonarCloud on every push and pull
request. CodeQL (`.github/workflows/codeql.yml`) is a second, separate scan
of the Rust code and GitHub Actions workflows on those same events, plus a
weekly schedule. The build workflow (`build.yml`) waits for that same audit
to pass before it produces any artifact. Audit also runs weekly on a
schedule so new advisories against existing pins are caught. Details:
[testing.md](docs/agents/testing.md).

## 4. Git

- Commit identity: `Thomas Grok Bot Barth <windwerfer@gmail.com>`. Never
  `bot@users.noreply.github.com` or a bot account as author.
- Small, focused commits with an imperative subject (`Add FEC parity to fragment header`). One
  topic per commit. Only commit states that pass the gates.
  Split a session's work into several commits (e.g. feature, its tests, docs, a refactor) instead
  of one 1–2k-line commit; a commit that mixes unrelated fixes is hard to review and bisect.
  Formatting-only and move-only changes always get their own commit.
- **Pushing directly to `main` is allowed in this repository** (the owner's decision; no PR
  needed). `git pull --rebase` before pushing; other agents may be working in parallel.
- **Never force-push, never rewrite pushed history** (`rebase -i`, `commit --amend`,
  `reset --hard` on pushed commits). Never delete branches or tags you didn't create.
- After pushing, check that CI and Build go green. A red `main` is fixed before anything else.

## 5. Ask the owner first

Stop and ask before: adding a Flatpak permission or any privilege; changing anything in
[decisions.md](docs/agents/decisions.md); a protocol change that breaks old peers; a new
licence outside the list above; publishing a GitHub Release, tag, Flathub submission or
notarization; spending money; adding a remote or CI secret; destructive git. Everything else:
decide, record the choice, keep going.

## 6. Definition of done

- [ ] The gates pass locally, and CI + Build are green after the push.
- [ ] Network-facing changes have unit tests in `mirrorwire-proto` / `mirrorwire-net`.
- [ ] Performance-relevant changes include before/after loopback numbers (same machine, same args).
- [ ] New failure modes show a readable message in the GUI.
- [ ] `README.md` and `docs/agents/` reflect the change; [roadmap.md](docs/agents/roadmap.md)
      is updated if an item moved.
- [ ] The commit message says what was tested and what wasn't.
