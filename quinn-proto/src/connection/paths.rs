use std::{cmp, net::SocketAddr, time::Duration, time::Instant};

use super::pacing::Pacer;
use crate::{congestion, TIMER_GRANULARITY};

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
    /// MTU discovery
    pub mtud: MtuDiscovery,
}

impl PathData {
    pub fn new(
        remote: SocketAddr,
        initial_rtt: Duration,
        congestion: Box<dyn congestion::Controller>,
        now: Instant,
    ) -> Self {
        PathData {
            remote,
            rtt: RttEstimator::new(initial_rtt),
            sending_ecn: true,
            pacing: Pacer::new(congestion.initial_window(), now),
            congestion,
            challenge: None,
            challenge_pending: false,
            mtud: MtuDiscovery::new(remote),
        }
    }

    pub fn from_previous(remote: SocketAddr, prev: &PathData, now: Instant) -> Self {
        let congestion = prev.congestion.clone_box();
        PathData {
            remote,
            rtt: prev.rtt,
            pacing: Pacer::new(congestion.initial_window(), now),
            sending_ecn: true,
            congestion,
            challenge: None,
            challenge_pending: false,
            mtud: MtuDiscovery::new(remote),
        }
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
            let adjusted_rtt = if self.min + ack_delay < self.latest {
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

    fn get(&self) -> Duration {
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
}

// Implements Datagram Packetization Layer Path Maximum Transmission Unit Discovery
//
// https://www.ietf.org/id/draft-ietf-tsvwg-datagram-plpmtud-21.html
pub struct MtuDiscovery {
    header_size: u16,
    // Current MTU for the path
    pub current: u16,
    // Packet number and probe size for the current probe
    probe_number: Option<u64>,
    probe_size: Option<u16>,
    // Failed probes at the current probe size
    probe_count: usize,
    phase: Phase,
}

impl MtuDiscovery {
    fn new(remote: SocketAddr) -> Self {
        Self {
            header_size: match remote {
                SocketAddr::V4(_) => 20,
                SocketAddr::V6(_) => 48,
            },
            current: BASE_PLPMTU,
            probe_number: None,
            probe_size: None,
            probe_count: 0,
            phase: Phase::Searching,
        }
    }

    pub fn poll_transmit(&mut self, next_packet_number: u64) -> Option<u16> {
        if self.probe_number.is_some() {
            return None;
        } else if let Phase::Complete = self.phase {
            return None;
        }

        if self.probe_size.is_none() {
            match LEVELS
                .iter()
                .find(|&&x| x > (self.current + self.header_size))
            {
                Some(v) => {
                    self.probe_size = Some(*v);
                }
                None => {
                    self.phase = Phase::Complete;
                    return None;
                }
            }
        }

        self.probe_number = Some(next_packet_number);
        self.probe_size
    }

    pub fn acked(&mut self, number: u64) {
        match self.probe_number {
            Some(probed) if probed == number => {}
            _ => return,
        };

        self.probe_number = None;
        let new = self.probe_size.take().unwrap();
        self.current = new - self.header_size;
        if self.current == MAX_PLPMTU {
            self.phase = Phase::Complete;
        }
    }

    pub fn lost(&mut self, number: u64) {
        match self.probe_number {
            Some(probed) if probed == number => {}
            _ => return,
        };

        self.probe_number = None;
        self.probe_count += 1;
        if self.probe_count == MAX_PROBES {
            self.probe_size = None;
            self.phase = Phase::Complete;
        }
    }
}

enum Phase {
    Searching,
    Complete,
}

const LEVELS: [u16; 4] = [1_350, 1_400, 1_450, 1_500];

const MAX_PROBES: usize = 3;
const MAX_PLPMTU: u16 = u16::MAX;
const BASE_PLPMTU: u16 = 1280;
