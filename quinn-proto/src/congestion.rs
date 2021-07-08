//! Logic for controlling the rate at which data is sent

use crate::connection::paths::RttEstimator;
use std::fmt::Debug;
use std::time::Instant;

mod bbr;
mod bbr_bw_estimation;
mod bbr_min_max;
mod cubic;
mod new_reno;

pub use bbr::{BBRConfig, BBR};
pub use cubic::{Cubic, CubicConfig};
pub use new_reno::{NewReno, NewRenoConfig};

/// Common interface for different congestion controllers
pub trait Controller: Send + Debug {
    #[allow(unused_variables)]
    fn on_sent(&mut self, now: Instant, bytes: u64) {}

    /// Packet deliveries were confirmed
    ///
    /// `app_limited` indicates whether the connection was blocked on outgoing
    /// application data prior to receiving these acknowledgements.
    #[allow(unused_variables)]
    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {}

    #[allow(unused_variables)]
    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {}

    /// Packets were deemed lost or marked congested
    ///
    /// `in_persistent_congestion` indicates whether all packets sent within the persistent
    /// congestion threshold period ending when the most recent packet in this batch was sent were
    /// lost.
    #[allow(unused_variables)]
    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
    ) {}

    #[allow(unused_variables)]
    fn on_loss(&mut self, now: Instant, sent: Instant, bytes: u64) {}

    #[allow(unused_variables)]
    fn update_last_sent(&mut self, packet_number: u64) {}

    /// Number of ack-eliciting bytes that may be in flight
    fn window(&self) -> u64;

    /// Duplicate the controller's state
    fn clone_box(&self) -> Box<dyn Controller>;

    /// Initial congestion window
    fn initial_window(&self) -> u64;
}

/// Constructs controllers on demand
pub trait ControllerFactory {
    /// Construct a fresh `Controller`
    fn build(&self, now: Instant) -> Box<dyn Controller>;
}
