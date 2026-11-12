# Contributing to MirrorWire

Thanks for helping. Before you start:

1. Read **[AGENTS.md](AGENTS.md)**. Its rules apply to humans and AI agents alike: no special
   privileges, no new Flatpak permissions, no weakening of the pinned-certificate + pairing-code
   security model, protocol changes only with a version bump.
2. Skim **[docs/agents/architecture.md](docs/agents/architecture.md)** and the code you will touch.
3. Build and run as described in [README.md](README.md) (Fedora presenter, macOS viewer, or
   both on one Linux box with the test pattern).
4. Before you push or open a pull request, run the gates:

   ```sh
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   scripts/loopback-test.sh 10 --encoder x264
   ```

5. Keep commits small and focused. Say what you tested and on which hardware. Latency or quality
   claims need numbers from the loopback stats or a real-hardware run
   ([docs/agents/testing.md](docs/agents/testing.md)).

Bug reports are most useful with a photo or screenshot of the window with the stats overlay on
(`S` in the viewer) and the presenter's status panel. See
[docs/agents/debugging.md](docs/agents/debugging.md).

Contributions are licensed under MIT OR Apache-2.0, the project licence.
