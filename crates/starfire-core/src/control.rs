// SPDX-License-Identifier: Apache-2.0
//! Control stream (ENet over UDP) — docs/protocol/06-control-enet.md.
//! Derived from protocol observation against Sunshine 2026.516.143833. Clean-room.
//!
//! A reliable-UDP control channel via `rusty_enet`. Connects to the control port
//! (from RTSP SETUP) using the `X-SS-Connect-Data` token; the carried messages
//! are AES-GCM with the RI key (framing resolved against live traffic). Carries
//! input, IDR requests, keepalive, stats, and host→client events.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use rusty_enet as enet;

/// Events surfaced from the control channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    Connected,
    Disconnected,
    /// A control packet (still AES-GCM framed) on `channel`.
    Message {
        channel: u8,
        data: Vec<u8>,
    },
}

/// The ENet control channel to one host.
pub struct ControlChannel {
    host: enet::Host<UdpSocket>,
    peer: enet::PeerID,
}

impl ControlChannel {
    /// Connect ENet to `server` using the RTSP `X-SS-Connect-Data` token and wait
    /// for the handshake to complete (or time out). `channels` is the requested
    /// ENet channel count (negotiated down to the host's).
    pub fn connect(
        server: SocketAddr,
        connect_data: u32,
        channels: usize,
        local_port: u16,
        timeout: Duration,
    ) -> crate::Result<Self> {
        // The host associates us by the client port advertised in RTSP SETUP, so
        // allow binding a specific local port (0 = ephemeral).
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, local_port))
            .or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))?;
        socket.set_nonblocking(true)?;
        let settings = enet::HostSettings {
            peer_limit: 1,
            // Sunshine's ENet enables the CRC32 packet checksum (Moonlight
            // customization); without it the host ignores our handshake.
            checksum: Some(Box::new(enet::crc32)),
            ..Default::default() // channel_limit defaults to the protocol maximum
        };
        let mut host = enet::Host::new(socket, settings)
            .map_err(|e| crate::Error::Protocol(format!("enet host: {e:?}")))?;
        let peer = host
            .connect(server, channels, connect_data)
            .map_err(|e| crate::Error::Protocol(format!("enet connect: {e:?}")))?
            .id();

        let deadline = Instant::now() + timeout;
        loop {
            // service() drives the handshake; the event borrow ends each iteration.
            host.service()
                .map_err(|e| crate::Error::Protocol(format!("enet service: {e}")))?;
            match host.peer(peer).state() {
                enet::PeerState::Connected => return Ok(Self { host, peer }),
                enet::PeerState::Disconnected => {
                    return Err(crate::Error::Protocol(
                        "enet: peer disconnected during connect".into(),
                    ));
                }
                _ => {}
            }
            if Instant::now() > deadline {
                return Err(crate::Error::Protocol("enet: connect timed out".into()));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Poll one control event (non-blocking); `None` when idle.
    pub fn poll(&mut self) -> crate::Result<Option<ControlEvent>> {
        let ev = self
            .host
            .service()
            .map_err(|e| crate::Error::Protocol(format!("enet service: {e}")))?;
        Ok(match ev {
            Some(enet::Event::Connect { .. }) => Some(ControlEvent::Connected),
            Some(enet::Event::Disconnect { .. }) => Some(ControlEvent::Disconnected),
            Some(enet::Event::Receive {
                channel_id, packet, ..
            }) => Some(ControlEvent::Message {
                channel: channel_id,
                data: packet.data().to_vec(),
            }),
            None => None,
        })
    }

    /// Send a raw (already-framed) control packet, reliably.
    pub fn send(&mut self, channel: u8, data: &[u8]) -> crate::Result<()> {
        let packet = enet::Packet::reliable(data);
        self.host
            .peer_mut(self.peer)
            .send(channel, &packet)
            .map_err(|e| crate::Error::Protocol(format!("enet send: {e:?}")))?;
        self.host.flush();
        Ok(())
    }
}
