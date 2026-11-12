//! The synthetic test source stamps its frame number into the image as a row of
//! black/white blocks so the viewer can verify that the frame it decoded is the
//! frame the presenter sent.

pub const MARKER_BITS: u32 = 32;
pub const MARKER_BLOCK: usize = 24;
pub const MARKER_WIDTH: usize = MARKER_BITS as usize * MARKER_BLOCK;

/// Writes `value` as marker blocks into the top-left corner of a packed 4-byte-per-pixel image.
pub fn stamp_marker(data: &mut [u8], stride: usize, width: usize, height: usize, value: u32) {
    if width < MARKER_WIDTH || height < MARKER_BLOCK {
        return;
    }
    for bit in 0..MARKER_BITS as usize {
        let on = value >> (MARKER_BITS as usize - 1 - bit) & 1 == 1;
        let v = if on { 255 } else { 0 };
        for y in 0..MARKER_BLOCK {
            let row = y * stride + bit * MARKER_BLOCK * 4;
            for px in data[row..row + MARKER_BLOCK * 4].as_chunks_mut::<4>().0 {
                px[0] = v;
                px[1] = v;
                px[2] = v;
                px[3] = 255;
            }
        }
    }
}

/// Reads a marker back from an 8-bit luma plane.
pub fn read_marker(luma: &[u8], stride: usize, width: usize, height: usize) -> Option<u32> {
    if width < MARKER_WIDTH || height < MARKER_BLOCK {
        return None;
    }
    let mid = MARKER_BLOCK / 2;
    let mut value = 0u32;
    for bit in 0..MARKER_BITS as usize {
        let mut sum = 0u32;
        for dy in [mid - 4, mid, mid + 4] {
            for dx in [mid - 4, mid, mid + 4] {
                sum += luma[dy * stride + bit * MARKER_BLOCK + dx] as u32;
            }
        }
        value = value << 1 | u32::from(sum / 9 > 128);
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_and_read() {
        let (w, h) = (800usize, 40usize);
        let mut rgba = vec![0u8; w * h * 4];
        stamp_marker(&mut rgba, w * 4, w, h, 0xDEAD_BEEF);
        let luma: Vec<u8> = rgba.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        assert_eq!(read_marker(&luma, w, w, h), Some(0xDEAD_BEEF));
    }
}
