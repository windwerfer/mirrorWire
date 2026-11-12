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
#[derive(Debug)]
pub struct RawFrame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub capture_us: u64,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub seq: u64,
    pub keyframe: bool,
    pub data: Bytes,
    pub capture_us: u64,
    pub encoded_us: u64,
}
