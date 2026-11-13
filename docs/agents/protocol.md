# Protocol

Everything here lives in `mirrorwire-proto` (constants, framing) and `mirrorwire-net`
(handshake, URI, mDNS). If you change any of it, follow §4.

## 1. Connection

- QUIC over UDP, default port **47800** (`DEFAULT_PORT`), ALPN **`mirrorwire/1`** (`ALPN`).
  The presenter port is configurable (`--port`, `MIRRORWIRE_PORT`, saved `settings.json`).
  A changed port is advertised in mDNS (`ServiceInfo` port) and in the URI (`mirrorwire://ip:port?…`);
  the viewer always uses the advertised port, never a hard-coded 47800.
- TLS 1.3 with self-signed certificates on both sides (`rcgen`, ring provider).
  - The viewer's verifier accepts the presenter's cert **only** if its SHA-256 fingerprint
    (64 lowercase hex chars) equals the pinned one from the URI/mDNS.
  - The presenter accepts any client cert at the TLS layer, then authorizes in the handshake:
    the fingerprint is in `paired_viewers.json`, or the `Hello` carries the right pairing code
    (then the fingerprint is added), or pairing is off (`--no-pairing`).
- Idle timeout 10 s. Datagram send buffer 2 MiB (`DATAGRAM_SEND_BUFFER`); when it is too full
  for a delta frame, the presenter drops that frame and forces a keyframe instead of queueing
  (keyframes are always queued). Pacing 500 Mbit/s (`lan_cc.rs`).

## 2. Control stream

One bidirectional stream opened by the viewer. Each message is a 4-byte big-endian length
followed by a JSON body (`serde`, `tag = "type"`, `snake_case`). Bodies over
`MAX_CONTROL_MESSAGE` (64 KiB) are an error.

| `type` | Direction | Fields |
|---|---|---|
| `hello` | viewer → presenter | `protocol` (u32, must equal `PROTOCOL_VERSION`), `name`, `pairing_code` (optional), `caps` (optional list; `cursor-metadata` = can draw a separately sent cursor; `fec-rs1` = can rebuild FEC video packets, §3b) |
| `rejected` | presenter → viewer | `reason` (shown to the user) |
| `stream_info` | presenter → viewer | `width`, `height`, `fps`, `codec`, `encoder`, `bitrate_kbps`, `intra_refresh`, `cursor` (optional: `separate`, `embedded`, `hidden`; absent = embedded), `fec` (optional: the presenter's FEC setting `off`/`auto`/`20%`, informational; present = the presenter understands `link_report`). Sent after `hello`, and **again** whenever it changes (older viewers ignore the repeat) |
| `request_keyframe` | viewer → presenter | `reason` |
| `link_report` | viewer → presenter | `loss_permille`, `packets`, `recovered`, `unrecoverable` over the last ~0.5 s. Sent **only** if `stream_info.fec` is present: an older presenter fails on unknown message types |
| `ping` / `pong` | viewer ↔ presenter | `t_viewer_us`, `t_presenter_us` (clock offset + RTT) |
| `bye` | either | — |

Adding an **optional** field with `#[serde(default)]` is backward compatible. Removing or
renaming a field, changing its meaning or unit, or adding a required field is not.

## 3. Video datagrams

Each encoded frame (H.264 Annex B access unit) is split into fragments that fit
`conn.max_datagram_size()`. Every fragment starts with a **34-byte big-endian header**
(`HEADER_LEN`):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | wire version (`WIRE_VERSION`, currently 1); other values are dropped |
| 1 | 1 | flags: bit 0 = keyframe; other bits reserved, send 0 |
| 2 | 4 | `frame_id` (u32, increments per frame) |
| 6 | 2 | fragment `index` |
| 8 | 2 | fragment `count` |
| 10 | 8 | `capture_us` (presenter clock) |
| 18 | 8 | `encoded_us` |
| 26 | 8 | `sent_us` |

The receiver reassembles by `frame_id`, drops incomplete frames that are overtaken, reports the
loss, and asks for a keyframe unless the stream uses intra refresh.

## 3a. Cursor as coordinates (capability `cursor-metadata`)

Only for viewers that listed `cursor-metadata` in `hello.caps`; older viewers never see these
and get the cursor in the video (the presenter reopens the capture with an embedded cursor
while such a viewer is connected). All in `mirrorwire-proto/src/cursor.rs`. No
`PROTOCOL_VERSION`/`WIRE_VERSION` bump: nothing an old peer receives changed.

**Pointer datagram** (unreliable, one per change, not tied to video frames). The first byte
`0xC1` can't collide with a video fragment (whose first byte is `WIRE_VERSION` = 1).
32 bytes, big endian; receivers ignore extra trailing bytes (reserved for later tools such as
a pen with pressure/tilt):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | kind `0xC1` |
| 1 | 1 | version 1 (other values dropped) |
| 2 | 1 | flags: bit 0 visible |
| 3 | 1 | tool: 0 = cursor (future: pen, touch) |
| 4 | 4 | `seq` (u32, wrapping; the viewer drops older/duplicate ones) |
| 8 | 4 | `x` (i32, hotspot position in captured-frame pixels) |
| 12 | 4 | `y` (i32) |
| 16 | 2 | `frame_w` (captured frame width, before encoder scaling) |
| 18 | 2 | `frame_h` |
| 20 | 4 | `image_id` (0 = none yet) |
| 24 | 8 | `capture_us` (presenter clock) |

**Cursor image**: one unidirectional QUIC stream per shape (presenter → viewer), at most
`MAX_IMAGE_STREAM` bytes: magic `MWCI`, version 1, 3 reserved, `id` u32, `width` u16, `height`
u16 (each ≤ 512), `hotspot_x` i32, `hotspot_y` i32, 4 reserved, then `width*height*4` bytes of
premultiplied RGBA. 0×0 = invisible cursor. The viewer caches the last 32 images by id; a new
viewer gets every cached image and the last position on connect.

**Drawing**: `cursor_rect()` maps `(x - hotspot, y - hotspot)` and the image size through the
rectangle the video is drawn in (`video.w / frame_w`), so fit, letterbox and 1:1 modes all
line up. The same message is meant to carry the future pen/Wacom mode.

## 3b. Forward error correction (capability `fec-rs1`)

Only viewers that list `fec-rs1` in `hello.caps` get these packets, and only while the
presenter's FEC setting isn't `off`; everyone else gets the version-1 fragments of §3. Code in
`mirrorwire-proto/src/fec.rs` (erasure code: `reed-solomon-simd`, MIT AND BSD-3-Clause). No
`PROTOCOL_VERSION` bump: a new wire version is only sent to viewers that asked for it.

The frame is cut into `data_count` equal shards of `shard_len` bytes (even; the last data shard
goes out unpadded and is zero-padded for coding). Data shard `i` belongs to block
`i % blocks`, parity shard `j` to block `j % blocks` (`blocks = ceil(data_count / 64)`, at most
255), so consecutive packets hit different blocks and a burst costs each block one or two
packets. Each block is a Reed-Solomon code over its data and parity shards: **any** `k_b` of
its `k_b + m_b` shards rebuild it, immediately, without a round trip. Data packets go out first,
then parity (paced by the congestion controller like everything else). 44-byte header, big
endian:

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | wire version 2 |
| 1 | 1 | flags: bit 0 = keyframe |
| 2 | 4 | `frame_id` |
| 6 | 2 | `index`: `0..data_count` data, then `data_count..data_count+parity_count` parity |
| 8 | 2 | `data_count` |
| 10 | 2 | `parity_count` |
| 12 | 4 | `frame_len` (bytes of the encoded frame, ≤ 64 MiB) |
| 16 | 2 | `shard_len` |
| 18 | 1 | `blocks` |
| 19 | 1 | reserved, 0 |
| 20 | 24 | `capture_us`, `encoded_us`, `sent_us` (as in §3) |

Receivers drop packets whose header is inconsistent (zero blocks, `frame_len` not matching the
counts, payload length not what the index implies) or whose layout differs from the first packet
seen for that frame, and keep at most 64 partial frames.

**Overhead** (`FecMode::overhead`): `auto` gives delta frames 10 % + 3 × loss + boost (10–35 %)
parity and keyframes 25 % + 5 × loss + 1.5 × boost (25–60 %), from the loss the viewer reports in
`link_report` (smoothed: rises at once, decays slowly). The boost (0–15 points) grows by 5 for
every report with unrecoverable frames and shrinks by 1 per clean report, because bursty loss
needs more parity than its average suggests; it does not grow while the adaptive bitrate sees
congestion (parity would only add load). A fixed setting `N%` gives delta frames N % and
keyframes 1.5 × N %. At least one parity packet per block (two for keyframes). Parity is paid
from the bitrate budget: the encoder target is budget / (1 + delta overhead).

**Adaptive bitrate** (presenter only, no wire change; `mirrorwire-proto/src/rate.rs`): the
same `link_report` loss plus the presenter's QUIC RTT and backlog drops drive an AIMD
controller. Its ceiling is the bitrate setting, its floor 3 Mbit/s. Congestion (loss ≥ 3 points
above the learned background loss in two reports in a row, ≥ 10 points once, RTT > 1.5 × min
+ 25 ms twice, or a backlog drop) cuts the budget ×0.85 (×0.75 at ≥ 10 points, ×0.6 at ≥ 20),
at most once per second; a clean link (< 1.5 points excess, no queue) adds 4 % of the ceiling
per second from 4 s after the last cut. Loss that stays at ≥ 70 % of its level for 3 s after a
cut is learned as background loss (random Wi-Fi loss that FEC covers). x264 takes the new rate
live (no keyframe); VAAPI/FilmCraft reopen (keyframe), at most every 5 s and only for ≥ 15 %.

**Loss accounting**: the viewer counts, per frame, packets sent (`data_count + parity_count`)
against packets received, once the frame is 30 frames old (so late packets still count). Frames
that never arrive at all are not in the packet count; they show up as unrecoverable frames.

## 4. Versioning rules

| Change | Bump |
|---|---|
| Datagram header layout or flag meaning | `WIRE_VERSION` (`fragment.rs`; FEC packets are version 2, `fec.rs`) |
| Control message incompatible change, handshake/auth semantics | `PROTOCOL_VERSION` (`lib.rs`) **and** the ALPN suffix (`mirrorwire/2`) |
| mDNS TXT keys or meaning | TXT `v` |
| URI parameters (removal or meaning) | document it; keep parsing the old form |

In the same commit: update this file, the README if users see it, and add a test in
`mirrorwire-proto` / `mirrorwire-net` that pins the new layout (encode → bytes → decode).
A presenter must reject an incompatible viewer with a `rejected` message the viewer GUI shows
("presenter is version X, update the viewer"), not a timeout. Ask the owner before any change
that makes old peers stop working (D6).

## 5. Discovery and pairing

- **mDNS:** service `_mirrorwire._udp.local.`, instance name = presenter name (hostname by
  default), TXT `fp` (fingerprint), `v` (`1`), `res` (`WxH`).
- **URI / QR:** `mirrorwire://<ip>:<port>?fp=<64 hex>&code=<6 digits>&name=<name>`. `fp` is
  required; `code` and `name` are optional. IPv6 hosts are bracketed.
- **Pairing code:** 6 decimal digits from `getrandom`, new on each presenter start, shown in the
  presenter window and terminal. Only needed on the first connection of a viewer.
