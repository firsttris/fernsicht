//! Whether the screen is HDR (D4): what the compositor told the monitor,
//! through the connector's KMS properties.
//!
//! An HDR desktop (KWin, gamescope, GNOME with HDR on) scans out its
//! framebuffer in the signal the monitor gets: BT.2020 primaries, PQ
//! encoded. It says so in the connector's `HDR_OUTPUT_METADATA` (a blob,
//! `struct hdr_output_metadata` in `include/uapi/drm/drm_mode.h`: a u32
//! metadata type, then the CTA-861-G infoframe, whose first byte is the
//! EOTF) and its `Colorspace` (`BT2020_RGB`). The framebuffer's values are
//! then streamed as they are, tagged BT.2020 and PQ.
//!
//! Compositors that move part of the colour pipeline into the display
//! hardware (plane colour pipelines, a CRTC's degamma and CTM) scan out
//! values that are not the signal yet; capture cannot undo that.

use fernsicht_core::colour::{Primaries, Transfer};

/// CTA-861-G's EOTF codes (`enum hdmi_eotf` in the kernel).
pub const EOTF_SDR: u8 = 0;
pub const EOTF_PQ: u8 = 2;
pub const EOTF_HLG: u8 = 3;

/// The EOTF in a `hdr_output_metadata` blob; `None` for a blob too short.
pub fn eotf(blob: &[u8]) -> Option<u8> {
    // u32 metadata_type, then hdr_metadata_infoframe { u8 eotf, ... }.
    blob.get(4).copied()
}

/// What the screen holds, from the connector's HDR metadata EOTF (`None`:
/// no metadata set) and the name of its `Colorspace` value.
pub fn signal(eotf: Option<u8>, colorspace: Option<&str>) -> (Primaries, Transfer) {
    let transfer = match eotf {
        Some(EOTF_PQ) => Transfer::Pq,
        // HLG desktops are rare; read as SDR until one shows up.
        _ => Transfer::Sdr,
    };
    let wide = colorspace.is_some_and(|c| c.contains("BT2020"));
    // HDR10 is BT.2020 whatever the colorspace property says.
    let primaries = if wide || transfer == Transfer::Pq {
        Primaries::Bt2020
    } else {
        Primaries::Bt709
    };
    (primaries, transfer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `hdr_output_metadata` as KWin sets it for an HDR10 monitor.
    fn blob(eotf: u8) -> Vec<u8> {
        let mut b = vec![0u8; 30];
        b[4] = eotf;
        // Max display mastering luminance, 1000 nits (not read).
        b[22..24].copy_from_slice(&1000u16.to_le_bytes());
        b
    }

    #[test]
    fn hdr10_is_pq_in_bt2020() {
        let pq = eotf(&blob(EOTF_PQ));
        assert_eq!(pq, Some(2));
        let hdr = (Primaries::Bt2020, Transfer::Pq);
        assert_eq!(signal(pq, Some("BT2020_RGB")), hdr);
        // Some drivers leave Colorspace at Default even with PQ.
        assert_eq!(signal(pq, Some("Default")), hdr);
        assert_eq!(signal(pq, None), hdr);
    }

    #[test]
    fn everything_else_is_sdr() {
        let sdr = (Primaries::Bt709, Transfer::Sdr);
        assert_eq!(signal(None, None), sdr, "no metadata set");
        assert_eq!(signal(eotf(&blob(EOTF_SDR)), Some("Default")), sdr);
        assert_eq!(signal(eotf(&blob(EOTF_HLG)), None), sdr, "HLG not yet");
        assert_eq!(eotf(&[0, 0, 0]), None, "a broken blob");
        // Wide gamut without PQ: BT.2020, SDR curve.
        assert_eq!(
            signal(None, Some("BT2020_RGB")),
            (Primaries::Bt2020, Transfer::Sdr)
        );
    }
}
