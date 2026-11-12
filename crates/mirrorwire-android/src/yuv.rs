//! YUV 4:2:0 → RGBA on the CPU, for `AImage` planes with any row and pixel
//! stride (I420 has pixel stride 1, NV12/NV21 views have 2). BT.709,
//! full range, the same matrix as the desktop GL shader.

/// One plane of an image: bytes, row stride, pixel stride.
#[derive(Clone, Copy)]
pub struct Plane<'a> {
    pub data: &'a [u8],
    pub row_stride: usize,
    pub pixel_stride: usize,
}

impl Plane<'_> {
    #[inline]
    fn at(&self, x: usize, y: usize) -> u8 {
        self.data
            .get(y * self.row_stride + x * self.pixel_stride)
            .copied()
            .unwrap_or(128)
    }
}

/// Converts into `out` (resized to `w * h * 4`). Out-of-range plane reads
/// (short last rows on some drivers) give grey instead of a panic.
pub fn to_rgba(w: usize, h: usize, y: Plane, u: Plane, v: Plane, out: &mut Vec<u8>) {
    out.resize(w * h * 4, 255);
    for row in 0..h {
        let crow = row / 2;
        let line = &mut out[row * w * 4..(row + 1) * w * 4];
        for col in 0..w {
            let luma = y.at(col, row) as i32;
            let cb = u.at(col / 2, crow) as i32 - 128;
            let cr = v.at(col / 2, crow) as i32 - 128;
            // Fixed point (x1024) of r = y + 1.5748 cr, g = y - 0.1873 cb - 0.4681 cr, b = y + 1.8556 cb.
            let r = luma + ((1613 * cr) >> 10);
            let g = luma - ((192 * cb + 479 * cr) >> 10);
            let b = luma + ((1900 * cb) >> 10);
            let px = &mut line[col * 4..col * 4 + 4];
            px[0] = r.clamp(0, 255) as u8;
            px[1] = g.clamp(0, 255) as u8;
            px[2] = b.clamp(0, 255) as u8;
            px[3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(data: &[u8], row_stride: usize) -> Plane<'_> {
        Plane { data, row_stride, pixel_stride: 1 }
    }

    #[test]
    fn grey_stays_grey_and_short_planes_do_not_panic() {
        let y = [100u8; 16];
        let c = [128u8; 4];
        let mut out = Vec::new();
        to_rgba(4, 4, p(&y, 4), p(&c, 2), p(&c, 2), &mut out);
        assert!(out.chunks(4).all(|px| px == [100, 100, 100, 255]));
        // Truncated chroma (driver quirk): grey, no panic.
        to_rgba(4, 4, p(&y, 4), p(&c[..1], 2), p(&c[..1], 2), &mut out);
    }

    #[test]
    fn red_is_red() {
        // BT.709 full-range red: Y=54, Cb=99, Cr=255.
        let mut out = Vec::new();
        to_rgba(2, 2, p(&[54; 4], 2), p(&[99], 1), p(&[255], 1), &mut out);
        assert!(out[0] > 240 && out[1] < 20 && out[2] < 20, "{:?}", &out[..4]);
    }

    #[test]
    fn interleaved_chroma_uses_pixel_stride() {
        // NV12 view: U at even, V at odd bytes, pixel stride 2.
        let uv = [99u8, 255];
        let u = Plane { data: &uv, row_stride: 2, pixel_stride: 2 };
        let v = Plane { data: &uv[1..], row_stride: 2, pixel_stride: 2 };
        let mut out = Vec::new();
        to_rgba(2, 2, p(&[54; 4], 2), u, v, &mut out);
        assert!(out[0] > 240 && out[1] < 20 && out[2] < 20, "{:?}", &out[..4]);
    }
}
