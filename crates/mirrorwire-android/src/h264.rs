//! Minimal Annex-B helpers: MediaCodec wants SPS and PPS as `csd-0`/`csd-1`.

/// Iterates the NAL units (without start codes) of an Annex-B access unit.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // Back up over the start code and any zero byte of a 4-byte code.
            let mut e = s - 3;
            while e > 0 && data[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(s, e)| s < e)
        .map(move |(s, e)| &data[s..e])
}

/// NAL unit type (low 5 bits of the first byte).
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// SPS (type 7) and PPS (type 8) of an access unit, each with a 4-byte start code.
pub fn parameter_sets(data: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut sps = None;
    let mut pps = None;
    for nal in nal_units(data) {
        let with_start = || [&[0u8, 0, 0, 1][..], nal].concat();
        match nal_type(nal) {
            Some(7) if sps.is_none() => sps = Some(with_start()),
            Some(8) if pps.is_none() => pps = Some(with_start()),
            _ => {}
        }
    }
    (sps, pps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sps_pps_with_3_and_4_byte_start_codes() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, 3, // SPS
            0, 0, 1, 0x68, 4, 5, // PPS
            0, 0, 0, 1, 0x65, 9, 9, 0, // IDR (trailing zero kept as data)
        ];
        let types: Vec<u8> = nal_units(&au).filter_map(nal_type).collect();
        assert_eq!(types, vec![7, 8, 5]);
        let (sps, pps) = parameter_sets(&au);
        assert_eq!(sps.unwrap(), vec![0, 0, 0, 1, 0x67, 1, 2, 3]);
        assert_eq!(pps.unwrap(), vec![0, 0, 0, 1, 0x68, 4, 5]);
    }

    #[test]
    fn garbage_does_not_panic() {
        for au in [&[][..], &[0], &[0, 0, 1], &[0, 0, 1, 0, 0, 1], &[1, 2, 3, 4]] {
            let _ = parameter_sets(au);
        }
    }
}
