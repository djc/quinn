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

/// To help differentiate between packet sizes and packet numbers
pub type PacketNumber = u64;

/// Common interface for different congestion controllers
pub trait Controller: Send + Debug {
    fn on_sent(&mut self, _now: Instant, _bytes: u64) {}

    /// Packet deliveries were confirmed
    ///
    /// `app_limited` indicates whether the connection was blocked on outgoing
    /// application data prior to receiving these acknowledgements.
    fn on_ack(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _bytes: u64,
        _app_limited: bool,
        _rtt: &RttEstimator,
    ) {
    }

    fn on_end_acks(
        &mut self,
        _now: Instant,
        _in_flight: u64,
        _app_limited: bool,
        _largest_packet_acked: Option<PacketNumber>,
    ) {
    }

    /// Packets were deemed lost or marked congested
    ///
    /// `in_persistent_congestion` indicates whether all packets sent within the persistent
    /// congestion threshold period ending when the most recent packet in this batch was sent were
    /// lost.
    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
    ) {
    }

    fn on_loss(&mut self, _now: Instant, _sent: Instant, _bytes: u64) {}

    fn update_last_sent(&mut self, _packet_number: PacketNumber) {}

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
