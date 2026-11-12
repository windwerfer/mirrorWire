# Protocol

Everything here lives in `mirrorwire-proto` (constants, framing) and `mirrorwire-net`
(handshake, URI, mDNS). If you change any of it, follow §4.

## 1. Connection

- QUIC over UDP, default port **47800** (`DEFAULT_PORT`), ALPN **`mirrorwire/1`** (`ALPN`).
- TLS 1.3 with self-signed certificates on both sides (`rcgen`, ring provider).
  - The viewer's verifier accepts the presenter's cert **only** if its SHA-256 fingerprint
    (64 lowercase hex chars) equals the pinned one from the URI/mDNS.
  - The presenter accepts any client cert at the TLS layer, then authorizes in the handshake:
    the fingerprint is in `paired_viewers.json`, or the `Hello` carries the right pairing code
    (then the fingerprint is added), or pairing is off (`--no-pairing`).
- Idle timeout 10 s. Datagram send buffer 4 MiB.

## 2. Control stream

One bidirectional stream opened by the viewer. Each message is a 4-byte big-endian length
followed by a JSON body (`serde`, `tag = "type"`, `snake_case`). Bodies over
`MAX_CONTROL_MESSAGE` (64 KiB) are an error.

| `type` | Direction | Fields |
|---|---|---|
| `hello` | viewer → presenter | `protocol` (u32, must equal `PROTOCOL_VERSION`), `name`, `pairing_code` (optional) |
| `rejected` | presenter → viewer | `reason` (shown to the user) |
| `stream_info` | presenter → viewer | `width`, `height`, `fps`, `codec`, `encoder`, `bitrate_kbps`, `intra_refresh` |
| `request_keyframe` | viewer → presenter | `reason` |
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

## 4. Versioning rules

| Change | Bump |
|---|---|
| Datagram header layout or flag meaning | `WIRE_VERSION` (`fragment.rs`) |
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
