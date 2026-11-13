//! Pointer (mouse cursor) sent as coordinates instead of pixels.
//!
//! The presenter asks the screen-cast portal for cursor *metadata*: the
//! compositor leaves the cursor out of the video and reports its position and
//! bitmap separately. The presenter forwards both:
//!
//! - **Pointer updates** ([`PointerUpdate`]) as unreliable QUIC datagrams, one
//!   per change, not tied to video frames. A lost one is simply replaced by the
//!   next; the sequence number lets the viewer drop reordered stale ones. The
//!   first byte ([`POINTER_DATAGRAM_KIND`]) differs from the video fragment
//!   version byte, so both share the datagram channel.
//! - **Cursor images** ([`CursorImage`]) on a reliable unidirectional QUIC
//!   stream each time the shape changes. The viewer caches them by id; pointer
//!   updates name the image to draw.
//!
//! Only viewers that list [`CAP_CURSOR_METADATA`] in their `hello` get these;
//! older viewers keep the cursor drawn into the video.
//!
//! The pointer message is the base for later input tools (a pen with pressure
//! and tilt): the `tool` byte says what it is, and decoders ignore trailing
//! bytes, so new optional fields can be appended without breaking old peers.

use anyhow::{bail, ensure, Result};

/// Capability string a viewer sends in `hello.caps` when it can draw a
/// separately sent cursor.
pub const CAP_CURSOR_METADATA: &str = "cursor-metadata";

/// First byte of a pointer datagram. Video fragments start with their wire
/// version (1), so the two can never be confused.
pub const POINTER_DATAGRAM_KIND: u8 = 0xC1;
pub const POINTER_VERSION: u8 = 1;
/// Fixed part of a pointer datagram. Longer datagrams are accepted; the extra
/// bytes are reserved for future tools.
pub const POINTER_LEN: usize = 32;
const FLAG_VISIBLE: u8 = 0b0000_0001;

/// Largest cursor bitmap side accepted (compositors use 24–256 px).
pub const MAX_CURSOR_SIDE: u32 = 512;
pub const IMAGE_MAGIC: [u8; 4] = *b"MWCI";
pub const IMAGE_VERSION: u8 = 1;
pub const IMAGE_HEADER_LEN: usize = 28;
/// Upper bound for one image stream (header + 512×512 RGBA).
pub const MAX_IMAGE_STREAM: usize =
    IMAGE_HEADER_LEN + (MAX_CURSOR_SIDE * MAX_CURSOR_SIDE * 4) as usize;

/// What kind of pointing device a [`PointerUpdate`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerTool {
    /// The presenter's mouse cursor.
    Cursor,
    /// A value this build does not know (sent by a newer peer).
    Other(u8),
}

impl PointerTool {
    pub fn to_u8(self) -> u8 {
        match self {
            PointerTool::Cursor => 0,
            PointerTool::Other(v) => v,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => PointerTool::Cursor,
            other => PointerTool::Other(other),
        }
    }
}

/// Where the pointer is, in pixels of the captured frame (`frame_w`×`frame_h`,
/// the size before any encoder scaling). `x`/`y` is the hotspot position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerUpdate {
    pub seq: u32,
    pub tool: PointerTool,
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub frame_w: u16,
    pub frame_h: u16,
    /// [`CursorImage::id`] to draw; 0 = none known yet.
    pub image_id: u32,
    /// Presenter clock, microseconds.
    pub capture_us: u64,
}

impl PointerUpdate {
    /// Byte layout (big endian):
    ///
    /// | Offset | Size | Field |
    /// |---|---|---|
    /// | 0 | 1 | kind `0xC1` |
    /// | 1 | 1 | version (1) |
    /// | 2 | 1 | flags: bit 0 visible |
    /// | 3 | 1 | tool (0 cursor) |
    /// | 4 | 4 | seq |
    /// | 8 | 4 | x (i32) |
    /// | 12 | 4 | y (i32) |
    /// | 16 | 2 | frame_w |
    /// | 18 | 2 | frame_h |
    /// | 20 | 4 | image_id |
    /// | 24 | 8 | capture_us |
    pub fn encode(&self) -> [u8; POINTER_LEN] {
        let mut out = [0u8; POINTER_LEN];
        out[0] = POINTER_DATAGRAM_KIND;
        out[1] = POINTER_VERSION;
        out[2] = if self.visible { FLAG_VISIBLE } else { 0 };
        out[3] = self.tool.to_u8();
        out[4..8].copy_from_slice(&self.seq.to_be_bytes());
        out[8..12].copy_from_slice(&self.x.to_be_bytes());
        out[12..16].copy_from_slice(&self.y.to_be_bytes());
        out[16..18].copy_from_slice(&self.frame_w.to_be_bytes());
        out[18..20].copy_from_slice(&self.frame_h.to_be_bytes());
        out[20..24].copy_from_slice(&self.image_id.to_be_bytes());
        out[24..32].copy_from_slice(&self.capture_us.to_be_bytes());
        out
    }

    pub fn decode(d: &[u8]) -> Option<Self> {
        if d.len() < POINTER_LEN || d[0] != POINTER_DATAGRAM_KIND || d[1] != POINTER_VERSION {
            return None;
        }
        let u16_at = |o: usize| u16::from_be_bytes([d[o], d[o + 1]]);
        let u32_at = |o: usize| u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&d[24..32]);
        Some(Self {
            visible: d[2] & FLAG_VISIBLE != 0,
            tool: PointerTool::from_u8(d[3]),
            seq: u32_at(4),
            x: u32_at(8) as i32,
            y: u32_at(12) as i32,
            frame_w: u16_at(16),
            frame_h: u16_at(18),
            image_id: u32_at(20),
            capture_us: u64::from_be_bytes(ts),
        })
    }
}

/// True for datagrams that carry a pointer update rather than video.
pub fn is_pointer_datagram(d: &[u8]) -> bool {
    d.first() == Some(&POINTER_DATAGRAM_KIND)
}

/// One cursor shape. Pixels are premultiplied RGBA, row-major, no padding.
/// A 0×0 image means "invisible cursor".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub rgba: Vec<u8>,
}

impl CursorImage {
    /// Byte layout (big endian), then `width*height*4` bytes of RGBA:
    ///
    /// | Offset | Size | Field |
    /// |---|---|---|
    /// | 0 | 4 | magic `MWCI` |
    /// | 4 | 1 | version (1) |
    /// | 5 | 3 | reserved, 0 |
    /// | 8 | 4 | id |
    /// | 12 | 2 | width |
    /// | 14 | 2 | height |
    /// | 16 | 4 | hotspot_x (i32) |
    /// | 20 | 4 | hotspot_y (i32) |
    /// | 24 | 4 | reserved, 0 |
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(IMAGE_HEADER_LEN + self.rgba.len());
        out.extend_from_slice(&IMAGE_MAGIC);
        out.push(IMAGE_VERSION);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(&(self.width as u16).to_be_bytes());
        out.extend_from_slice(&(self.height as u16).to_be_bytes());
        out.extend_from_slice(&self.hotspot_x.to_be_bytes());
        out.extend_from_slice(&self.hotspot_y.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&self.rgba);
        out
    }

    pub fn decode(d: &[u8]) -> Result<Self> {
        ensure!(
            d.len() >= IMAGE_HEADER_LEN,
            "cursor image too short ({} bytes)",
            d.len()
        );
        ensure!(d[0..4] == IMAGE_MAGIC, "not a cursor image");
        ensure!(
            d[4] == IMAGE_VERSION,
            "cursor image version {} unsupported",
            d[4]
        );
        let u16_at = |o: usize| u16::from_be_bytes([d[o], d[o + 1]]) as u32;
        let u32_at = |o: usize| u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
        let (width, height) = (u16_at(12), u16_at(14));
        if width > MAX_CURSOR_SIDE || height > MAX_CURSOR_SIDE {
            bail!("cursor image {width}x{height} too large");
        }
        let len = (width * height * 4) as usize;
        let body = &d[IMAGE_HEADER_LEN..];
        ensure!(
            body.len() == len,
            "cursor image {width}x{height} has {} bytes, expected {len}",
            body.len()
        );
        Ok(Self {
            id: u32_at(8),
            width,
            height,
            hotspot_x: u32_at(16) as i32,
            hotspot_y: u32_at(20) as i32,
            rgba: body.to_vec(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// What a capture source reports about the cursor; the transport assigns
/// sequence numbers and sends it to viewers that can draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorEvent {
    Pointer(PointerUpdate),
    Image(CursorImage),
}

/// An axis-aligned rectangle in the viewer's drawing units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Where to draw `image` on screen when the video occupies `video` (the
/// same fit/scale rectangle the video quad uses). Returns `None` when there
/// is nothing to draw (hidden pointer, empty image, unknown frame size).
pub fn cursor_rect(p: &PointerUpdate, image: &CursorImage, video: RectF) -> Option<RectF> {
    if !p.visible || image.is_empty() || p.frame_w == 0 || p.frame_h == 0 {
        return None;
    }
    let sx = video.w / p.frame_w as f32;
    let sy = video.h / p.frame_h as f32;
    Some(RectF {
        x: video.x + (p.x - image.hotspot_x) as f32 * sx,
        y: video.y + (p.y - image.hotspot_y) as f32 * sy,
        w: image.width as f32 * sx,
        h: image.height as f32 * sy,
    })
}

/// How many images the viewer keeps (pointer, text beam, hand, resize…).
const IMAGE_CACHE: usize = 32;

/// Viewer-side cursor state: newest pointer update plus the image cache.
#[derive(Debug, Default)]
pub struct ViewerCursor {
    latest: Option<PointerUpdate>,
    images: Vec<std::sync::Arc<CursorImage>>,
    /// Pointer updates applied.
    pub updates: u64,
    /// Updates dropped because a newer one had already arrived.
    pub stale: u64,
    /// Images received.
    pub images_received: u64,
}

impl ViewerCursor {
    /// Apply an update; returns false for a reordered older one.
    pub fn apply(&mut self, p: PointerUpdate) -> bool {
        if let Some(prev) = &self.latest {
            // Wrapping comparison: newer if the forward distance is < 2^31.
            if p.seq == prev.seq || p.seq.wrapping_sub(prev.seq) > u32::MAX / 2 {
                self.stale += 1;
                return false;
            }
        }
        self.latest = Some(p);
        self.updates += 1;
        true
    }

    pub fn add_image(&mut self, image: CursorImage) {
        self.images_received += 1;
        self.images.retain(|i| i.id != image.id);
        if self.images.len() >= IMAGE_CACHE {
            self.images.remove(0);
        }
        self.images.push(std::sync::Arc::new(image));
    }

    pub fn latest(&self) -> Option<PointerUpdate> {
        self.latest
    }

    pub fn image(&self, id: u32) -> Option<std::sync::Arc<CursorImage>> {
        self.images.iter().rev().find(|i| i.id == id).cloned()
    }

    /// The pointer to draw now with its image, if both are known.
    pub fn current(&self) -> Option<(PointerUpdate, std::sync::Arc<CursorImage>)> {
        let p = self.latest?;
        if !p.visible {
            return None;
        }
        Some((p, self.image(p.image_id)?))
    }

    /// Forget everything (new connection).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PointerUpdate {
        PointerUpdate {
            seq: 7,
            tool: PointerTool::Cursor,
            visible: true,
            x: -3,
            y: 1079,
            frame_w: 1920,
            frame_h: 1080,
            image_id: 42,
            capture_us: 0x0102_0304_0506_0708,
        }
    }

    #[test]
    fn pointer_layout_is_pinned() {
        let bytes = sample().encode();
        assert_eq!(bytes.len(), POINTER_LEN);
        assert_eq!(bytes[0], 0xC1);
        assert_eq!(bytes[1], 1);
        assert_eq!(bytes[2], 1);
        assert_eq!(bytes[3], 0);
        assert_eq!(&bytes[4..8], &[0, 0, 0, 7]);
        assert_eq!(&bytes[8..12], &(-3i32).to_be_bytes());
        assert_eq!(&bytes[16..18], &1920u16.to_be_bytes());
        assert_eq!(&bytes[24..32], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(PointerUpdate::decode(&bytes), Some(sample()));
    }

    #[test]
    fn pointer_ignores_trailing_bytes_and_rejects_junk() {
        let mut long = sample().encode().to_vec();
        long.extend_from_slice(&[9, 9, 9, 9]);
        assert_eq!(PointerUpdate::decode(&long), Some(sample()));
        assert!(PointerUpdate::decode(&long[..POINTER_LEN - 1]).is_none());
        let mut wrong_version = sample().encode();
        wrong_version[1] = 2;
        assert!(PointerUpdate::decode(&wrong_version).is_none());
        assert!(PointerUpdate::decode(&[]).is_none());
        // A video fragment (version byte 1) is never taken for a pointer.
        assert!(!is_pointer_datagram(&[1, 0, 0]));
        assert!(is_pointer_datagram(&sample().encode()));
    }

    #[test]
    fn unknown_tool_survives_roundtrip() {
        let mut p = sample();
        p.tool = PointerTool::Other(5);
        p.visible = false;
        assert_eq!(PointerUpdate::decode(&p.encode()), Some(p));
    }

    #[test]
    fn image_roundtrip_and_bounds() {
        let img = CursorImage {
            id: 3,
            width: 2,
            height: 3,
            hotspot_x: 1,
            hotspot_y: -1,
            rgba: (0..24).collect(),
        };
        let wire = img.encode();
        assert_eq!(wire.len(), IMAGE_HEADER_LEN + 24);
        assert_eq!(&wire[0..4], b"MWCI");
        assert_eq!(CursorImage::decode(&wire).unwrap(), img);
        // Truncated body, bad magic, oversize: all errors, no panic.
        assert!(CursorImage::decode(&wire[..wire.len() - 1]).is_err());
        let mut bad = wire.clone();
        bad[0] = b'X';
        assert!(CursorImage::decode(&bad).is_err());
        let mut huge = wire.clone();
        huge[12..14].copy_from_slice(&1000u16.to_be_bytes());
        assert!(CursorImage::decode(&huge).is_err());
        assert!(CursorImage::decode(&wire[..10]).is_err());
    }

    #[test]
    fn maps_through_fit_rect() {
        let img = CursorImage {
            id: 1,
            width: 24,
            height: 24,
            hotspot_x: 4,
            hotspot_y: 2,
            rgba: vec![0; 24 * 24 * 4],
        };
        let mut p = sample();
        p.x = 960;
        p.y = 540;
        // 1920x1080 shown at half size with a 100/50 letterbox offset.
        let video = RectF {
            x: 100.0,
            y: 50.0,
            w: 960.0,
            h: 540.0,
        };
        let r = cursor_rect(&p, &img, video).unwrap();
        assert_eq!(
            r,
            RectF {
                x: 100.0 + 478.0,
                y: 50.0 + 269.0,
                w: 12.0,
                h: 12.0
            }
        );
        // 1:1 display: cursor is exactly at its pixel position minus hotspot.
        let one = RectF {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        let r = cursor_rect(&p, &img, one).unwrap();
        assert_eq!((r.x, r.y, r.w), (956.0, 538.0, 24.0));
        p.visible = false;
        assert!(cursor_rect(&p, &img, video).is_none());
        p.visible = true;
        p.frame_w = 0;
        assert!(cursor_rect(&p, &img, video).is_none());
    }

    #[test]
    fn viewer_drops_stale_and_wraps() {
        let mut c = ViewerCursor::default();
        let mut p = sample();
        p.seq = u32::MAX - 1;
        assert!(c.apply(p));
        p.seq = 1; // wrapped forward
        assert!(c.apply(p));
        p.seq = u32::MAX; // older than 1 after the wrap
        assert!(!c.apply(p));
        assert_eq!((c.updates, c.stale), (2, 1));
        assert!(c.current().is_none(), "no image yet");
        c.add_image(CursorImage {
            id: 42,
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: vec![0; 4],
        });
        assert_eq!(c.current().unwrap().1.id, 42);
        for id in 100..140 {
            c.add_image(CursorImage {
                id,
                width: 0,
                height: 0,
                hotspot_x: 0,
                hotspot_y: 0,
                rgba: Vec::new(),
            });
        }
        assert!(c.image(42).is_none(), "cache is bounded");
        assert!(c.image(139).is_some());
    }
}
