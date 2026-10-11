//! Answering a browser's offer: the codecs both sides have, our UDP
//! socket and candidate.

use std::net::{IpAddr, UdpSocket};
use std::sync::Once;
use std::time::{Duration, Instant};

use fernsicht_proto::{Codec, CodecSet};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::{Candidate, Rtc, RtcConfig};

/// How often str0m reports statistics (the round trip among them).
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// The WebRTC side of a new session: answer sent, waiting for the browser.
pub(crate) struct WebSetup {
    pub(super) rtc: Rtc,
    pub(super) socket: UdpSocket,
    browser_codecs: CodecSet,
}

impl WebSetup {
    /// The video codecs the browser offered that we answered with.
    pub(crate) fn browser_codecs(&self) -> CodecSet {
        self.browser_codecs
    }
}

/// The video codecs a browser's offer lists that our encoders make: H.264,
/// and AV1, which Chrome and Firefox decode even without a hardware decoder.
/// HEVC is left out: few browsers take it over WebRTC.
pub(crate) fn codecs_in_offer(sdp: &str) -> CodecSet {
    let has = |name: &str| {
        sdp.lines().any(|l| {
            l.strip_prefix("a=rtpmap:")
                .and_then(|rest| rest.split_once(' '))
                .is_some_and(|(_, enc)| enc.eq_ignore_ascii_case(name))
        })
    };
    let mut set = CodecSet::EMPTY;
    if has("H264/90000") {
        set = set.with(Codec::H264);
    }
    if has("AV1/90000") {
        set = set.with(Codec::Av1);
    }
    set
}

/// H.264 as our encoders make it: High profile, and Constrained Baseline
/// for browsers that offer nothing else. (pt, rtx, profile-level-id)
const H264: [(u8, u8, u32); 2] = [(114, 115, 0x64_00_1f), (108, 109, 0x42_e0_1f)];

/// Prepares WebRTC for `offer`: a UDP socket on `local_ip` and our answer.
pub(crate) fn accept(offer: SdpOffer, local_ip: IpAddr) -> anyhow::Result<(WebSetup, SdpAnswer)> {
    static CRYPTO: Once = Once::new();
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
    let browser_codecs = codecs_in_offer(&offer.to_sdp_string());
    let mut cfg = RtcConfig::new()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_opus(true, false)
        .enable_av1(true)
        // For the round trip in the browser's receiver reports.
        .set_stats_interval(Some(STATS_INTERVAL));
    for (pt, rtx, profile) in H264 {
        cfg.codec_config()
            .add_h264(pt.into(), Some(rtx.into()), true, profile);
    }
    let mut rtc = cfg.build(Instant::now());
    let socket = UdpSocket::bind((local_ip, 0))?;
    // Mostly video (WMM video on Wi-Fi).
    if let Err(e) = fernsicht_net::socket::set_dscp(&socket, fernsicht_net::socket::Dscp::Video) {
        log::debug!("no DSCP marking: {e}");
    }
    let candidate = Candidate::host(socket.local_addr()?, "udp")
        .map_err(|e| anyhow::anyhow!("candidate: {e}"))?;
    rtc.add_local_candidate(candidate);
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| anyhow::anyhow!("offer: {e}"))?;
    Ok((
        WebSetup {
            rtc,
            socket,
            browser_codecs,
        },
        answer,
    ))
}

/// Our address towards `peer` (the route the kernel would take).
pub(crate) fn local_ip_towards(peer: IpAddr) -> std::io::Result<IpAddr> {
    let s = UdpSocket::bind((
        if peer.is_ipv4() {
            IpAddr::from([0u8; 4])
        } else {
            IpAddr::from([0u16; 8])
        },
        0,
    ))?;
    s.connect((peer, 9))?;
    Ok(s.local_addr()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_are_read_from_the_offer() {
        // Abridged from Chrome, Firefox and Safari offers.
        let chrome = "m=video 9 UDP/TLS/RTP/SAVPF 96 45 103\r\n\
                      a=rtpmap:96 VP8/90000\r\n\
                      a=rtpmap:45 AV1/90000\r\n\
                      a=rtpmap:103 H264/90000\r\n";
        assert_eq!(
            codecs_in_offer(chrome),
            CodecSet::of(&[Codec::H264, Codec::Av1])
        );
        let safari = "a=rtpmap:96 H264/90000\r\na=rtpmap:98 H265/90000\r\n";
        assert_eq!(codecs_in_offer(safari), CodecSet::H264);
        // Lower case, and a codec named only in an attribute: not offered.
        let odd = "a=rtpmap:41 av1/90000\r\na=fmtp:100 apt=AV1/90000\r\n";
        assert_eq!(codecs_in_offer(odd), CodecSet::of(&[Codec::Av1]));
        assert_eq!(codecs_in_offer(""), CodecSet::EMPTY);
    }

    #[test]
    fn the_route_to_localhost_is_localhost() {
        assert!(
            local_ip_towards("127.0.0.1".parse().unwrap())
                .unwrap()
                .is_loopback()
        );
    }
}
