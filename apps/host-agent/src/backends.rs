//! Choosing and making the picture source and the encoder, and the
//! stream's size, frame rate and bitrate.

use std::path::{Path, PathBuf};

use fernsicht_capture::{FrameSource, TestPattern};
use fernsicht_codec::Encoder;
use fernsicht_codec::synthetic::SyntheticEncoder;
#[cfg(feature = "vaapi")]
use fernsicht_codec::vaapi::{VaapiEncoder, VaapiEncoderConfig};
use fernsicht_core::colour::{Colour, Depth};
use fernsicht_proto::{Codec, CodecSet};

use crate::config::{CaptureKind, EncoderKind, SessionParams};

/// Size of the test pattern when the client leaves the size to the host.
pub const TEST_PATTERN_SIZE: (u32, u32) = (1920, 1080);

/// `native` scaled down to fit `max` (aspect ratio kept), even sizes.
pub(crate) fn fit(native: (u32, u32), max: (u16, u16)) -> (u16, u16) {
    let (w, h) = (native.0.max(2) as f64, native.1.max(2) as f64);
    let s = (f64::from(max.0) / w).min(f64::from(max.1) / h).min(1.0);
    let even = |v: f64| ((v.round() as u32).max(2) & !1).min(u32::from(u16::MAX) & !1) as u16;
    (even(w * s), even(h * s))
}

/// The session's frame rate: what the client asked for, but no more than
/// the monitor shows. An encoder told 144 fps on a 60 Hz monitor sizes each
/// frame for 144 and spends less than half the bitrate.
pub(crate) fn frame_rate(wanted: u16, refresh_hz: Option<u32>) -> u16 {
    match refresh_hz {
        Some(hz) if hz > 0 => wanted.min(hz.min(u32::from(u16::MAX)) as u16),
        _ => wanted,
    }
}

/// About what 20 Mbit/s are for 1080p60, the same bits per pixel for any
/// size and rate. Desktop text needs that much to stay sharp.
pub(crate) fn default_bitrate_kbps(width: u16, height: u16, fps: u16) -> u32 {
    let pixels_per_sec = f64::from(width) * f64::from(height) * f64::from(fps);
    let per_1080p60 = 1920.0 * 1080.0 * 60.0;
    (20_000.0 * pixels_per_sec / per_1080p60)
        .round()
        .clamp(1_000.0, 200_000.0) as u32
}

pub(crate) fn make_source(
    kind: &CaptureKind,
    p: &SessionParams,
) -> anyhow::Result<Box<dyn FrameSource>> {
    match kind {
        CaptureKind::TestPattern | CaptureKind::TestPatternWith { .. } => {
            let (w, h) = if p.width == 0 || p.height == 0 {
                TEST_PATTERN_SIZE
            } else {
                (u32::from(p.width), u32::from(p.height))
            };
            let pattern = TestPattern::new(w, h, u32::from(p.fps));
            Ok(Box::new(match kind {
                CaptureKind::TestPatternWith { off, still } => {
                    pattern.with_outage(off.clone()).with_still(still.clone())
                }
                _ => pattern,
            }))
        }
        #[cfg(feature = "kms")]
        CaptureKind::Kms { card, connector } => {
            let cap =
                fernsicht_capture::kms::KmsCapture::open(&fernsicht_capture::kms::KmsConfig {
                    card: card.as_ref().map(Into::into),
                    connector: connector.clone(),
                    fps: u32::from(p.fps),
                })?;
            log::info!(
                "capturing {}×{} over KMS ({:?})",
                cap.width(),
                cap.height(),
                cap.selection()
            );
            Ok(Box::new(cap))
        }
        #[cfg(not(feature = "kms"))]
        CaptureKind::Kms { .. } => {
            anyhow::bail!("this build has no KMS capture (cargo feature \"kms\")")
        }
    }
}

pub(crate) const NVIDIA: u16 = 0x10de;

/// The hardware encoder for the GPUs found (render node, PCI vendor), in
/// order: VAAPI on AMD/Intel, NVENC on NVIDIA, as far as this build
/// supports them (`vaapi`, `nvenc`).
pub fn choose_encoder(gpus: &[(String, u16)], vaapi: bool, nvenc: bool) -> Option<EncoderKind> {
    gpus.iter().find_map(|(node, vendor)| match *vendor {
        NVIDIA if nvenc => Some(EncoderKind::Nvenc { gpu: 0 }),
        NVIDIA => None,
        _ if vaapi => Some(EncoderKind::Vaapi {
            render_node: node.clone(),
        }),
        _ => None,
    })
}

/// This machine's GPUs under `sys` (`/sys`, or a stand-in in tests):
/// render node and PCI vendor. GPUs that drive a monitor come first: KMS
/// captures the screen there, and an encoder on the same GPU takes the
/// picture without crossing devices (a laptop's iGPU before its dGPU).
pub(crate) fn render_nodes(sys: &Path) -> Vec<(String, u16)> {
    let drm = sys.join("class/drm");
    let entries: Vec<(String, PathBuf)> = std::fs::read_dir(&drm)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect();
    // The devices with a monitor on: a card with an enabled connector.
    let showing: Vec<PathBuf> = entries
        .iter()
        .filter(|(name, _)| name.starts_with("card") && !name.contains('-'))
        .filter(|(name, _)| {
            entries.iter().any(|(other, path)| {
                other.starts_with(&format!("{name}-"))
                    && std::fs::read_to_string(path.join("enabled"))
                        .is_ok_and(|s| s.trim() == "enabled")
            })
        })
        .filter_map(|(_, path)| path.join("device").canonicalize().ok())
        .collect();
    let mut gpus: Vec<(bool, String, u16)> = entries
        .iter()
        .filter(|(name, _)| name.starts_with("renderD"))
        .filter_map(|(name, path)| {
            let vendor = std::fs::read_to_string(path.join("device/vendor")).ok()?;
            let vendor = u16::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).ok()?;
            let device = path.join("device").canonicalize().ok();
            let shows = device.is_some_and(|d| showing.contains(&d));
            Some((!shows, format!("/dev/dri/{name}"), vendor))
        })
        .collect();
    gpus.sort();
    gpus.into_iter()
        .map(|(_, node, vendor)| (node, vendor))
        .collect()
}

/// [`choose_encoder`] for this machine's GPUs.
pub fn auto_encoder() -> anyhow::Result<EncoderKind> {
    let gpus = render_nodes(Path::new("/sys"));
    let kind = choose_encoder(&gpus, cfg!(feature = "vaapi"), cfg!(feature = "nvidia"))
        .ok_or_else(|| {
            anyhow::anyhow!("no GPU this build can encode with (found {gpus:x?}); pick --encoder")
        })?;
    log::info!("encoder: {kind:?}");
    Ok(kind)
}

/// Codecs in the order the host prefers them: each is as fast as the next
/// in hardware and needs less bitrate for the same picture (HEVC 30–50 %
/// less than H.264, AV1 another 10–20 % less than HEVC).
pub const CODEC_PREFERENCE: [Codec; 3] = [Codec::Av1, Codec::Hevc, Codec::H264];

/// The codecs to try for a client that can decode `client`, best first.
pub fn codec_candidates(client: CodecSet) -> Vec<Codec> {
    CODEC_PREFERENCE
        .iter()
        .copied()
        .filter(|&c| client.contains(c))
        .collect()
}

/// An encoder of `kind` with the best codec the client can decode and the
/// GPU can encode: a GPU without HEVC encoding falls back to H.264.
pub(crate) fn make_encoder(
    kind: &EncoderKind,
    p: &SessionParams,
) -> anyhow::Result<Box<dyn Encoder>> {
    if *kind == EncoderKind::Synthetic {
        return Ok(Box::new(SyntheticEncoder::new(
            p.bitrate_kbps,
            u32::from(p.fps),
        )));
    }
    let candidates = codec_candidates(p.codecs);
    if candidates.is_empty() {
        anyhow::bail!("the client decodes none of AV1, HEVC and H.264");
    }
    let mut errors = Vec::new();
    for codec in candidates {
        let colour = stream_colour(p.screen, p.ten_bit.contains(codec));
        // In 10 bit where the screen and the client have it, else (or if
        // the GPU does not encode it) in 8.
        let colours = [colour, colour.with_depth(Depth::Eight)];
        for colour in colours
            .iter()
            .take(if colour.depth == Depth::Ten { 2 } else { 1 })
        {
            match make_hardware_encoder(kind, p, codec, *colour) {
                Ok(e) => return Ok(e),
                Err(e) => {
                    log::info!("no {codec:?} encoder in {} bit: {e:#}", colour.depth.bits());
                    errors.push(format!("{codec:?}: {e:#}"));
                }
            }
        }
    }
    anyhow::bail!("no encoder ({})", errors.join("; "))
}

/// The stream's colour for a screen in `screen`: 10 bit where the screen
/// has more than 8 bits (or is HDR) and the client decodes the codec in
/// 10 bit; its primaries and transfer as the screen's.
pub fn stream_colour(screen: Colour, ten_bit: bool) -> Colour {
    let deep = screen.depth == Depth::Ten || screen.is_hdr();
    screen.with_depth(if deep && ten_bit {
        Depth::Ten
    } else {
        Depth::Eight
    })
}

#[allow(unused_variables)]
fn make_hardware_encoder(
    kind: &EncoderKind,
    p: &SessionParams,
    codec: Codec,
    colour: Colour,
) -> anyhow::Result<Box<dyn Encoder>> {
    match kind {
        EncoderKind::Synthetic => unreachable!("handled by make_encoder"),
        #[cfg(feature = "vaapi")]
        EncoderKind::Vaapi { render_node } => {
            Ok(Box::new(VaapiEncoder::new(&VaapiEncoderConfig {
                render_node: render_node.clone(),
                codec,
                width: u32::from(p.width),
                height: u32::from(p.height),
                fps: u32::from(p.fps),
                bitrate_kbps: p.bitrate_kbps,
                colour,
            })?))
        }
        #[cfg(not(feature = "vaapi"))]
        EncoderKind::Vaapi { .. } => {
            anyhow::bail!("this build has no VAAPI support (cargo feature \"vaapi\")")
        }
        #[cfg(feature = "nvidia")]
        EncoderKind::Nvenc { gpu } => Ok(Box::new(fernsicht_codec::nvidia::NvencEncoder::new(
            &fernsicht_codec::nvidia::NvencConfig {
                gpu: *gpu,
                codec,
                width: u32::from(p.width),
                height: u32::from(p.height),
                fps: u32::from(p.fps),
                bitrate_kbps: p.bitrate_kbps,
                intra_refresh: p.refresh,
                colour,
            },
        )?)),
        #[cfg(not(feature = "nvidia"))]
        EncoderKind::Nvenc { .. } => {
            anyhow::bail!("this build has no NVENC support (cargo feature \"nvidia\")")
        }
        #[cfg(feature = "vulkan")]
        EncoderKind::Vulkan { render_node } => {
            Ok(Box::new(fernsicht_codec::vulkan::VulkanEncoder::new(
                &fernsicht_codec::vulkan::VulkanEncoderConfig {
                    render_node: render_node.clone(),
                    codec,
                    width: u32::from(p.width),
                    height: u32::from(p.height),
                    fps: u32::from(p.fps),
                    bitrate_kbps: p.bitrate_kbps,
                    colour,
                },
            )?))
        }
        #[cfg(not(feature = "vulkan"))]
        EncoderKind::Vulkan { .. } => {
            anyhow::bail!("this build has no Vulkan Video support (cargo feature \"vulkan\")")
        }
    }
}
