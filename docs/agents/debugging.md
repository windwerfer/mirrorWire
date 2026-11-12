# Debugging checklist

## Viewer lists the presenter but never connects ("connecting…", then timeout)

mDNS working proves multicast gets through, not that UDP 47800 does.

1. **Firewall on the presenter (most common on Fedora).** firewalld allows mDNS but blocks
   47800/udp silently.
   `sudo firewall-cmd --add-port=47800/udp` (until reboot), then
   `sudo firewall-cmd --permanent --add-port=47800/udp`. This fixed the first real test.
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
