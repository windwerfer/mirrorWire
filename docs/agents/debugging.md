# Debugging checklist

## Viewer lists the presenter but never connects ("connecting…", then timeout)

mDNS working proves multicast gets through, not that UDP 47800 does.

1. **Firewall on the presenter (most common on Fedora).** firewalld allows mDNS but blocks
   the presenter UDP port (default 47800) silently. In the presenter window, run the
   command from the firewall note or click **Open port for me…** if it is offered (one
   password dialog; the app does not run as root). The commands:

   ```
   On Fedora: sudo firewall-cmd --add-port=47800/udp
   To keep it after a reboot: sudo firewall-cmd --permanent --add-port=47800/udp && sudo firewall-cmd --reload
   Ubuntu with ufw: sudo ufw allow 47800/udp
   ```

   If you use another Linux distribution, look up how to allow a UDP port in its firewall.
   Replace `47800` with the port shown in the presenter if it was changed. The same
   short guide is in the README troubleshooting section. This (`firewall-cmd --add-port`)
   fixed the first real test.
2. **Wrong address.** Check the presenter panel's "Listening on" and the IP in the viewer's
   list/URI. VPNs, Docker bridges and a second NIC can make mDNS advertise an unreachable
   address. Try the pasted URI with the right IP, or `--advertise-ip`.
3. **macOS Local Network permission.** On first launch macOS asks to let MirrorWire find
   devices on the local network. If denied: System Settings › Privacy & Security › Local
   Network.
4. **Different networks / client isolation.** Guest Wi-Fi and some mesh routers block
   client-to-client traffic. Test both machines on cable or the same access point.
5. **Pairing / fingerprint.** A `rejected` reason (wrong code, protocol mismatch) or a
   fingerprint mismatch should show in the viewer. After deleting the presenter's config dir
   the fingerprint changes; re-scan or re-discover.
6. **Version mismatch.** Old Mac DMG vs new Flatpak: protocol mismatch is rejected with
   `protocol N unsupported`. Update both from the same build.

## Picture is blurry or blocky

Read the stats overlay (`S`, top left):

`WxH@fps codec (encoder) | decoder | renderer | capture->texture N ms` then the per-second
line `fps, Mbit/s, rtt, ms mean/p95/max: …stages…`.

- **Resolution ≠ screen resolution** (e.g. 2000×1000 from a 1920×1080 screen): the stream is
  being scaled, which smears text. Stream at native resolution.
- **Mbit/s far below the configured bitrate** (e.g. 2 Mbit/s): the encoder's rate control is
  starving static desktop content. Desktop text needs roughly 20–50 Mbit/s at 1080p.
- **4:2:0 chroma** softens coloured text even at high bitrate (H.264 limit; 4:4:4 is a
  possible future option where the hardware supports it).
- Window smaller/larger than the stream: viewer scaling. Compare in fullscreen (`F`).

## Lag

- Presenter "Capture path" (also in the log as `capture path …`): `dmabuf-zero-copy` is the
  fast path. `shm+upload (…)` means shared memory; an orange note below it says why DMA-BUF or
  VPP was turned off (e.g. FFmpeg without libdrm, a modifier VAAPI can't import).
- Presenter `source N fps (+M repeats/s)`: N is what the compositor actually delivered. KWin
  sends frames only when the screen changes, so a still screen shows ~0 fps plus ~10
  repeats/s; moving a window should push N towards 60. Low N while things move points at
  the compositor or capture path, not the network.
- `capture_to_encoded` high (tens of ms with VAAPI): CPU-side scaling/conversion before the
  encoder, or the software fallback is in use. Check the encoder name in the overlay.
- `network` high or spiky: Wi-Fi. Try cable; note `rtt`.
- `decode` high on the iMac: software decode; try `--hwdec` / the View checkbox.
- `convert` high: CPU YUV→RGBA at large sizes (a GPU shader is on the roadmap).
- Frames queueing anywhere is a bug: every media-path channel is bounded and drops old frames.
  The viewer's decode queue holds 2 frames; when it overflows it is flushed and a keyframe is
  requested (log: `decoder behind: dropped N queued frames`). The presenter drops delta frames
  when the QUIC datagram queue is full (`dropped for backlog` in the per-second line).
- Cursor lag: presenter *Cursor* line says `separate` (sent as coordinates) or `embedded`; an
  orange note says why it fell back (portal without metadata cursor, older viewer). Viewer
  overlay: `cursor separate (N updates/s)`; N should follow mouse movement (KWin caps it at the
  frame rate). If the cursor misbehaves, switch Quality › Cursor to `embedded`.
- FEC: the viewer overlay line `fec +N%  rebuilt N/s  loss N%  unrecoverable frames N` (and the
  headless summary `fec overhead … packet loss … rebuilt packets … unrecoverable frames …`).
  `fec off` with a new presenter means the setting is off; the presenter window says
  `off for this viewer (older version)` for an old viewer. Loss well above ~5 % or growing
  unrecoverable frames: the link is worse than the parity covers; raise Quality › FEC to 30–50 %
  or lower the bitrate. Parity costs bandwidth: at 40 Mbit/s, auto adds 4–20 Mbit/s.
  Try the loss handling without Wi-Fi: `--simulate-loss 5 --simulate-burst 4` on a viewer.
- Frame rate: presenter line `Frame rate: asked up to N fps · compositor allows M`. M below N
  means the compositor capped it; "not reported" means KWin did not send a maxFramerate.
- Freeze on click (fixed 2026-11-13): egui `Response` methods must not be called inside
  `ctx.input(|i| …)`; `double_clicked()` re-locks the context and deadlocks against the decode
  thread's `request_repaint()`.

## Android: picture frozen, cursor still moves

Read the HUD (tap the video). Line 2 is the phone's decoder: `hardware decoder <name>` or
`SOFTWARE decoder <name> (<hardware name> failed: <reason>)`. Line 4 is
`in X  decoded Y  shown Z fps`:

- `in` > 0, `decoded` 0, red "decoder has returned no picture": MediaCodec takes input but
  gives nothing back. Check the `input full` / `errors` counters and the decoder name;
  `adb logcat -s mirrorwire` has the MediaCodec error.
- `waiting key N/s` high and the red "waiting for a keyframe" line: the viewer dropped the
  stream (loss or decode backlog) and the presenter is not sending keyframes; check the
  presenter's `keyframes` count.
- `flushes` keeps climbing: the decoder is slower than the stream, so the queue overflows and
  every overflow costs a keyframe. Lower the presenter's fps or resolution, or get a hardware
  decoder working. `late, not shown N/s` alone is fine: frames are decoded, only the
  superseded ones are not converted to RGBA.
- `decoded` > 0 but `shown` 0, red "none were shown": the UI does not pick up pictures (a GUI
  bug, not the network).

## Presenter side

- No share dialog: the portal backend (`xdg-desktop-portal-kde`) isn't running, or the restore
  token is stale; delete `portal_restore_token` in the config dir.
- `vainfo | grep -i enc` must list H264 `VAEntrypointEncSlice`; on Fedora that needs RPM
  Fusion's `intel-media-driver` for the native build. The Flatpak and AppImage bundle their own
  static FFmpeg (with x264); the Flatpak gets its VAAPI driver from the freedesktop runtime's GL
  extension and the GPU through `--device=dri`, the AppImage uses the host's `libva` and driver.
- Window system: the footer (and the viewer stats overlay) shows `Wayland (native)` or
  `X11 via XWayland`. Linux is Wayland-first (D13); XWayland means winit fell back, e.g.
  `WAYLAND_DISPLAY` missing in the environment.
- Force shared-memory capture to compare: `mirrorwire present --no-dmabuf` (or
  `MIRRORWIRE_NO_DMABUF=1` for the GUI).
- Terminal logs: `RUST_LOG=debug,mdns_sd=warn flatpak run io.github.windwerfer.MirrorWire present`.
