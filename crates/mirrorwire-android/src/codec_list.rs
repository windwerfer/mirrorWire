//! Which H.264 decoder to try first. The NDK has no `MediaCodecList`, so the
//! Java shell writes the device's decoders to [`CODEC_LIST_FILE`] (one per
//! line: `name|hw|sw|lowlatency`, flags `1`/`0`/`?`), and this module ranks
//! them: hardware first, software last, secure (DRM) decoders never.
//!
//! Pure logic, host-testable. The file comes from our own Java code, but it is
//! still parsed defensively (bounded, unknown lines skipped).

/// Written by `MainActivity` into the app's files directory.
pub const CODEC_LIST_FILE: &str = "h264_decoders.txt";

/// One `video/avc` decoder as reported by `MediaCodecList`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecEntry {
    pub name: String,
    /// Runs on dedicated hardware (`isHardwareAccelerated`, or the name heuristic before Android 10).
    pub hardware: bool,
    /// Supports `FEATURE_LowLatency` (Android 11+); `false` when unknown.
    pub low_latency: bool,
}

/// Google's and other well-known software decoders, by name. Used when the
/// platform does not say (Android 8/9) and as a cross-check.
pub fn is_software_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("omx.google.")
        || n.starts_with("c2.android.")
        || n.starts_with("omx.ffmpeg.")
        || n.starts_with("c2.ffmpeg.")
        || n.contains(".sw.")
        || n.ends_with(".sw")
}

fn flag(s: &str) -> Option<bool> {
    match s.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Parses the shell's list. Lines that don't fit are skipped; at most 64 entries.
pub fn parse(text: &str) -> Vec<CodecEntry> {
    let mut out = Vec::new();
    for line in text.lines().take(256) {
        let mut parts = line.split('|');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() || name.len() > 128 || !name.chars().all(|c| c.is_ascii_graphic()) {
            continue;
        }
        let hw = parts.next().and_then(flag);
        let sw = parts.next().and_then(flag);
        let low_latency = parts.next().and_then(flag).unwrap_or(false);
        let hardware = match (hw, sw) {
            (Some(true), _) => !is_software_name(name),
            (_, Some(true)) => false,
            // Unknown (Android 8/9) or "not hardware but not software-only either"
            // (some vendor decoders): trust the name.
            _ => !is_software_name(name),
        };
        out.push(CodecEntry {
            name: name.to_owned(),
            hardware,
            low_latency,
        });
        if out.len() >= 64 {
            break;
        }
    }
    out
}

/// Order to try: hardware decoders (in the platform's order, which already
/// puts the preferred vendor decoder first), then software. Secure decoders
/// need a protected surface and are dropped.
pub fn ranked(entries: &[CodecEntry]) -> Vec<CodecEntry> {
    let usable = entries
        .iter()
        .filter(|e| !e.name.to_ascii_lowercase().ends_with(".secure"));
    let (mut hw, sw): (Vec<_>, Vec<_>) = usable.cloned().partition(|e| e.hardware);
    hw.extend(sw);
    let mut seen = std::collections::HashSet::new();
    hw.retain(|e| seen.insert(e.name.clone()));
    hw
}

/// Vendor keys that switch a hardware decoder to output each frame as soon as
/// it is decoded (no reordering delay). Only for the vendor that defines them;
/// unknown keys are ignored by MediaCodec, but we stay specific anyway.
pub fn vendor_low_latency_keys(name: &str) -> &'static [&'static str] {
    let n = name.to_ascii_lowercase();
    if n.starts_with("c2.qti.") || n.starts_with("omx.qcom.") {
        &[
            "vendor.qti-ext-dec-low-latency.enable",
            "vendor.qti-ext-dec-picture-order.enable",
        ]
    } else if n.starts_with("c2.exynos.") || n.starts_with("omx.exynos.") {
        &["vendor.rtc-ext-dec-low-latency.enable"]
    } else {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_before_software_and_secure_dropped() {
        let list = parse(
            "c2.android.avc.decoder|0|1|1\n\
             c2.qti.avc.decoder|1|0|1\n\
             c2.qti.avc.decoder.secure|1|0|0\n\
             OMX.google.h264.decoder|0|1|0\n",
        );
        let names: Vec<_> = ranked(&list).into_iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "c2.qti.avc.decoder",
                "c2.android.avc.decoder",
                "OMX.google.h264.decoder"
            ]
        );
    }

    #[test]
    fn unknown_flags_fall_back_to_the_name() {
        // Android 8/9: the shell cannot ask isHardwareAccelerated.
        let list = parse("OMX.MTK.VIDEO.DECODER.AVC|?|?|?\nOMX.google.h264.decoder|?|?|?\n");
        assert!(list[0].hardware);
        assert!(!list[1].hardware);
        assert!(!list[0].low_latency);
    }

    #[test]
    fn software_name_wins_over_a_wrong_hardware_flag() {
        assert!(!parse("c2.android.avc.decoder|1|0|0")[0].hardware);
    }

    #[test]
    fn emulator_goldfish_decoder_counts_as_hardware() {
        let list = parse("c2.goldfish.h264.decoder|1|0|0\nc2.android.avc.decoder|0|1|1");
        assert_eq!(ranked(&list)[0].name, "c2.goldfish.h264.decoder");
    }

    #[test]
    fn garbage_is_skipped_without_panic() {
        let long = "x".repeat(500);
        let text = format!("\n|1|0\n{long}|1\nname with space|1|0|0\nc2.ok.avc.decoder\n");
        let list = parse(&text);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "c2.ok.avc.decoder");
        assert!(list[0].hardware);
        let many: String = (0..1000).map(|i| format!("c2.v{i}.dec|1|0|0\n")).collect();
        assert_eq!(parse(&many).len(), 64);
    }

    #[test]
    fn duplicates_are_tried_once() {
        let list = parse("c2.a.dec|1|0|0\nc2.a.dec|1|0|0");
        assert_eq!(ranked(&list).len(), 1);
    }

    #[test]
    fn vendor_keys_only_for_their_vendor() {
        assert_eq!(vendor_low_latency_keys("c2.qti.avc.decoder").len(), 2);
        assert_eq!(vendor_low_latency_keys("c2.exynos.h264.decoder").len(), 1);
        assert!(vendor_low_latency_keys("c2.android.avc.decoder").is_empty());
        assert!(vendor_low_latency_keys("c2.mtk.avc.decoder").is_empty());
    }
}
