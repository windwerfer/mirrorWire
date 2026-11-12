use bytes::Bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        4
    }

    pub(crate) fn to_ffmpeg(self) -> ffmpeg_next::format::Pixel {
        use ffmpeg_next::format::Pixel;
        match self {
            PixelFormat::Bgra => Pixel::BGRA,
            PixelFormat::Bgrx => Pixel::BGRZ,
            PixelFormat::Rgba => Pixel::RGBA,
            PixelFormat::Rgbx => Pixel::RGBZ,
        }
    }
}

/// One captured, uncompressed frame in packed 32-bit RGB layout.
///
/// Usually the pixels are in `data` (system memory). A Linux portal capture
/// that negotiated DMA-BUF instead sets `dmabuf` and leaves `data` empty: the
/// pixels stay in GPU memory and only the encoder's VAAPI path can read them.
#[derive(Debug)]
pub struct RawFrame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub capture_us: u64,
    pub dmabuf: Option<DmaBufFrame>,
}

/// A single-plane DMA-BUF (one GPU buffer object) holding a packed 32-bit
/// RGB frame. The file descriptor is our own duplicate, closed on drop.
#[cfg(unix)]
#[derive(Debug)]
pub struct DmaBufFrame {
    pub fd: std::os::fd::OwnedFd,
    /// Size of the whole buffer object in bytes (0 if unknown).
    pub size: usize,
    pub offset: u32,
    pub pitch: u32,
    /// DRM format modifier; `DRM_FORMAT_MOD_INVALID` for an implicit layout.
    pub modifier: u64,
}

/// Placeholder so `RawFrame` has the same shape on every platform.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct DmaBufFrame {
    _private: (),
}

/// `DRM_FORMAT_MOD_INVALID`: the buffer uses an implicit (driver-chosen) layout.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

impl PixelFormat {
    /// The DRM fourcc for this packed format (little-endian naming: DRM
    /// XRGB8888 is bytes B,G,R,X in memory, which PipeWire calls BGRx).
    pub fn drm_fourcc(self) -> u32 {
        const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
            (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
        }
        match self {
            PixelFormat::Bgra => fourcc(b'A', b'R', b'2', b'4'),
            PixelFormat::Bgrx => fourcc(b'X', b'R', b'2', b'4'),
            PixelFormat::Rgba => fourcc(b'A', b'B', b'2', b'4'),
            PixelFormat::Rgbx => fourcc(b'X', b'B', b'2', b'4'),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub seq: u64,
    pub keyframe: bool,
    pub data: Bytes,
    pub capture_us: u64,
    pub encoded_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_fourcc_matches_drm_fourcc_h() {
        // Values from drm_fourcc.h.
        assert_eq!(PixelFormat::Bgrx.drm_fourcc(), 0x3432_5258); // XRGB8888
        assert_eq!(PixelFormat::Bgra.drm_fourcc(), 0x3432_5241); // ARGB8888
        assert_eq!(PixelFormat::Rgbx.drm_fourcc(), 0x3432_4258); // XBGR8888
        assert_eq!(PixelFormat::Rgba.drm_fourcc(), 0x3432_4241); // ABGR8888
    }
}
