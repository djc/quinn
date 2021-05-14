use std::{cmp, net::SocketAddr, time::Duration, time::Instant};

use super::pacing::Pacer;
use crate::{congestion, INITIAL_MAX_UDP_PAYLOAD_SIZE, TIMER_GRANULARITY};

/// Description of a particular network path
pub struct PathData {
    pub remote: SocketAddr,
    pub rtt: RttEstimator,
    /// Whether we're enabling ECN on outgoing packets
    pub sending_ecn: bool,
    /// Congestion controller state
    pub congestion: Box<dyn congestion::Controller>,
    /// Pacing state
    pub pacing: Pacer,
    pub challenge: Option<u64>,
    pub challenge_pending: bool,
    /// Whether we're certain the peer can both send and receive on this address
    ///
    /// Initially equal to `use_stateless_retry` for servers, and becomes false again on every
    /// migration. Always true for clients.
    pub validated: bool,
    /// Total size of all UDP datagrams sent on this path
    pub total_sent: u64,
    /// Total size of all UDP datagrams received on this path
    pub total_recvd: u64,
    pub max_udp_payload_size: u16,
}

impl PathData {
    pub fn new(
        remote: SocketAddr,
        initial_rtt: Duration,
        congestion: Box<dyn congestion::Controller>,
        now: Instant,
        validated: bool,
    ) -> Self {
        PathData {
            remote,
            rtt: RttEstimator::new(initial_rtt),
            sending_ecn: true,
            pacing: Pacer::new(
                initial_rtt,
                congestion.initial_window(),
                INITIAL_MAX_UDP_PAYLOAD_SIZE,
                now,
            ),
            congestion,
            challenge: None,
            challenge_pending: false,
            validated,
            total_sent: 0,
            total_recvd: 0,
            max_udp_payload_size: INITIAL_MAX_UDP_PAYLOAD_SIZE,
        }
    }

    pub fn from_previous(remote: SocketAddr, prev: &PathData, now: Instant) -> Self {
        let congestion = prev.congestion.clone_box();
        let smoothed_rtt = prev.rtt.get();
        PathData {
            remote,
            rtt: prev.rtt,
            pacing: Pacer::new(
                smoothed_rtt,
                congestion.window(),
                prev.max_udp_payload_size,
                now,
            ),
            sending_ecn: true,
            congestion,
            challenge: None,
            challenge_pending: false,
            validated: false,
            total_sent: 0,
            total_recvd: 0,
            max_udp_payload_size: prev.max_udp_payload_size,
        }
    }

    /// Indicates whether we're a server that hasn't validated the peer's address and hasn't
    /// received enough data from the peer to permit sending `bytes_to_send` additional bytes
    pub fn anti_amplification_blocked(&self, bytes_to_send: u64) -> bool {
        !self.validated && self.total_recvd * 3 < self.total_sent + bytes_to_send
    }
}

#[derive(Copy, Clone)]
pub struct RttEstimator {
    /// The most recent RTT measurement made when receiving an ack for a previously unacked packet
    latest: Duration,
    /// The smoothed RTT of the connection, computed as described in RFC6298
    smoothed: Option<Duration>,
    /// The RTT variance, computed as described in RFC6298
    var: Duration,
    /// The minimum RTT seen in the connection, ignoring ack delay.
    min: Duration,
}

impl RttEstimator {
    fn new(initial_rtt: Duration) -> Self {
        Self {
            latest: initial_rtt,
            smoothed: None,
            var: initial_rtt / 2,
            min: initial_rtt,
        }
    }

    pub fn update(&mut self, ack_delay: Duration, rtt: Duration) {
        self.latest = rtt;
        // min_rtt ignores ack delay.
        self.min = cmp::min(self.min, self.latest);
        // Based on RFC6298.
        if let Some(smoothed) = self.smoothed {
            let adjusted_rtt = if self.min + ack_delay <= self.latest {
                self.latest - ack_delay
            } else {
                self.latest
            };
            let var_sample = if smoothed > adjusted_rtt {
                smoothed - adjusted_rtt
            } else {
                adjusted_rtt - smoothed
            };
            self.var = (3 * self.var + var_sample) / 4;
            self.smoothed = Some((7 * smoothed + adjusted_rtt) / 8);
        } else {
            self.smoothed = Some(self.latest);
            self.var = self.latest / 2;
            self.min = self.latest;
        }
    }

    pub fn get(&self) -> Duration {
        self.smoothed.unwrap_or(self.latest)
    }

    /// Conservative estimate of RTT
    ///
    /// Takes the maximum of smoothed and latest RTT, as recommended
    /// in 6.1.2 of the recovery spec (draft 29).
    pub fn conservative(&self) -> Duration {
        self.get().max(self.latest)
    }

    pub fn pto_base(&self) -> Duration {
        self.get() + cmp::max(4 * self.var, TIMER_GRANULARITY)
    }

    pub fn min(&self) -> Duration {
        self.min
    }
}
