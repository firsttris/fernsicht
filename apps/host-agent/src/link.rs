//! How a session's packets reach the client: our UDP protocol (sealed in a
//! secure session, video in GSO chunks) or a browser's WebRTC thread.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::Sender;
use fernsicht_net::fec::FecConfig;
use fernsicht_net::socket;
use fernsicht_proto::{
    AudioHeader, MAX_DATAGRAM, Monitor, Monitors, SEALED_OVERHEAD, SealedHeader,
};
use fernsicht_secure::session::{SessionError, Transport};

use crate::web::WebOut;

/// Sends a session's packets to its client: over our UDP protocol, or to
/// a browser's data channel. Shared by the session's threads.
#[derive(Clone)]
pub(crate) enum Link {
    Udp(UdpLink),
    Web(Sender<WebOut>),
}

impl Link {
    pub(crate) fn send(&self, packet: &[u8]) -> std::io::Result<()> {
        match self {
            Link::Udp(l) => l.send(packet),
            Link::Web(tx) => {
                // A full queue drops the packet (pointer updates repeat).
                let _ = tx.try_send(WebOut::Packet(packet.to_vec()));
                Ok(())
            }
        }
    }

    /// Sends a chunk of video datagrams: on UDP in as few system calls as
    /// the kernel allows (see [`UdpLink::send_chunk`]).
    pub(crate) fn send_chunk<'p>(
        &self,
        packets: impl Iterator<Item = &'p [u8]>,
        scratch: &mut Vec<u8>,
    ) -> std::io::Result<()> {
        match self {
            Link::Udp(l) => l.send_chunk(packets, scratch),
            Link::Web(_) => packets.into_iter().try_for_each(|p| self.send(p)),
        }
    }

    pub(crate) fn encrypted(&self) -> bool {
        match self {
            Link::Udp(l) => l.crypto.is_some(),
            // DTLS-SRTP.
            Link::Web(_) => true,
        }
    }

    /// Sends one Opus frame: WebRTC carries it as it is, our protocol in a
    /// packet with the frame before it, so that one lost packet leaves no
    /// gap.
    pub(crate) fn send_audio(&self, header: AudioHeader, frame: &[u8], previous: &[u8]) {
        match self {
            Link::Web(tx) => {
                let _ = tx.try_send(WebOut::Audio {
                    data: frame.to_vec(),
                });
            }
            Link::Udp(l) => {
                let mut buf = [0u8; MAX_DATAGRAM];
                let n = header.encode(frame, previous, &mut buf);
                let _ = l.send(&buf[..n]);
            }
        }
    }

    /// Tells the client the host's monitors and which one it sees: a
    /// browser as JSON on the data channel.
    pub(crate) fn send_monitors(&self, current: u8, list: &[Monitor<'_>]) {
        match self {
            Link::Web(tx) => {
                let _ = tx.try_send(WebOut::Text(crate::web::monitors_message(current, list)));
            }
            Link::Udp(l) => {
                let mut buf = [0u8; MAX_DATAGRAM];
                let n = Monitors::encode(l.session_id, current, list, &mut buf);
                let _ = l.send(&buf[..n]);
            }
        }
    }

    /// The size of the video's FEC shards: datagrams that cross the path
    /// whole (a VPN's MTU is lower than a LAN's). The client takes the
    /// shard size from each packet.
    pub(crate) fn shard_size(&self) -> usize {
        let Link::Udp(l) = self else {
            return FecConfig::default().shard_size;
        };
        let datagram = socket::datagram_for(socket::path_mtu(l.peer), l.peer.is_ipv6());
        let shard = fernsicht_net::fec::shard_size_for(datagram);
        if datagram < MAX_DATAGRAM {
            log::info!(
                "session {:08x}: the path to {} carries {datagram}-byte datagrams; shards of {shard} bytes",
                l.session_id,
                l.peer
            );
        }
        shard
    }

    /// Opens a sealed packet from the client into `out`; `None` when the
    /// session is not sealed (a browser's, or a plain one in tests).
    pub(crate) fn open(
        &self,
        counter: u64,
        sealed: &[u8],
        out: &mut [u8],
    ) -> Option<Result<usize, SessionError>> {
        match self {
            Link::Udp(UdpLink {
                crypto: Some(crypto),
                ..
            }) => Some(crypto.open(counter, sealed, out)),
            _ => None,
        }
    }

    /// Notes that the client sent an authentic packet; true the first
    /// time, when it has proven it holds the session's keys.
    pub(crate) fn confirm(&self) -> bool {
        match self {
            Link::Udp(l) => !l.confirmed.swap(true, Ordering::AcqRel),
            // WebRTC proved it before the session started.
            Link::Web(_) => false,
        }
    }
}

/// Our UDP protocol, sealed when the session is secure.
#[derive(Clone)]
pub(crate) struct UdpLink {
    pub(crate) socket: Arc<UdpSocket>,
    pub(crate) peer: SocketAddr,
    pub(crate) session_id: u32,
    pub(crate) crypto: Option<Arc<Transport>>,
    /// The client proved it holds the session's keys (its first sealed
    /// packet came). Until then nothing is sent: a replayed handshake from
    /// a forged address would otherwise turn one small packet into seconds
    /// of video at someone else.
    pub(crate) confirmed: Arc<AtomicBool>,
    /// UDP GSO works on the way to the client (until a send says not).
    pub(crate) gso: Arc<AtomicBool>,
}

impl UdpLink {
    pub(crate) fn send(&self, packet: &[u8]) -> std::io::Result<()> {
        if !self.confirmed.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(crypto) = &self.crypto else {
            return self.socket.send_to(packet, self.peer).map(|_| ());
        };
        let mut buf = [0u8; MAX_DATAGRAM];
        let (counter, n) = crypto
            .seal(packet, &mut buf[SealedHeader::LEN..])
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        SealedHeader {
            session_id: self.session_id,
            counter,
        }
        .write(&mut buf);
        self.socket
            .send_to(&buf[..SealedHeader::LEN + n], self.peer)
            .map(|_| ())
    }

    /// Sends datagrams in one go: sealed (or copied) one after the other
    /// into `scratch`, then cut by the kernel (UDP GSO). Video packets of a
    /// frame have one length; a packet of another length starts a new run.
    pub(crate) fn send_chunk<'p>(
        &self,
        packets: impl Iterator<Item = &'p [u8]>,
        scratch: &mut Vec<u8>,
    ) -> std::io::Result<()> {
        if !self.confirmed.load(Ordering::Acquire) {
            return Ok(());
        }
        let overhead = if self.crypto.is_some() {
            SEALED_OVERHEAD
        } else {
            0
        };
        scratch.clear();
        let mut segment = 0;
        for p in packets {
            let len = p.len() + overhead;
            if segment != 0 && len != segment {
                self.send_run(scratch, segment)?;
                scratch.clear();
            }
            segment = len;
            let at = scratch.len();
            scratch.resize(at + len, 0);
            let Some(crypto) = &self.crypto else {
                scratch[at..].copy_from_slice(p);
                continue;
            };
            let (counter, _) = crypto
                .seal(p, &mut scratch[at + SealedHeader::LEN..])
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            SealedHeader {
                session_id: self.session_id,
                counter,
            }
            .write(&mut scratch[at..]);
        }
        if scratch.is_empty() {
            return Ok(());
        }
        self.send_run(scratch, segment)
    }

    /// Sends `data`, datagrams of `segment` bytes: in one call each GSO
    /// allows, else one by one.
    fn send_run(&self, data: &[u8], segment: usize) -> std::io::Result<()> {
        let one_by_one = |d: &[u8]| {
            d.chunks(segment).try_for_each(|g| {
                socket::send_marked(&self.socket, self.peer, g, socket::Dscp::Video).map(|_| ())
            })
        };
        let per_call = (socket::MAX_SEGMENTS_BYTES / segment.max(1)).clamp(1, socket::MAX_SEGMENTS);
        for run in data.chunks(segment * per_call) {
            if run.len() <= segment || !self.gso.load(Ordering::Relaxed) {
                one_by_one(run)?;
                continue;
            }
            let bufs = [std::io::IoSlice::new(run)];
            let video = Some(socket::Dscp::Video);
            if let Err(e) =
                socket::send_segments(&self.socket, self.peer, &bufs, segment as u16, video)
            {
                // One by one tells whether the network or GSO fails.
                one_by_one(run)?;
                log::info!(
                    "UDP GSO does not work on the way to {} ({e}); one datagram per call",
                    self.peer
                );
                self.gso.store(false, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}
