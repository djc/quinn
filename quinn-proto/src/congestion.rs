//! Logic for controlling the rate at which data is sent

use std::time::Instant;

mod new_reno;
mod cubic;

pub use new_reno::{NewReno, NewRenoConfig};
pub use cubic::{Cubic, CubicConfig};

/// Common interface for different congestion controllers
pub trait Controller: Send {
    /// Packet deliveries were confirmed
    ///
    /// `app_limited` indicates whether the connection was blocked on outgoing
    /// application data prior to receiving these acknowledgements.
    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool);

    /// Packets were deemed lost or marked congested
    ///
    /// `in_persistent_congestion` indicates whether all packets sent within the persistent
    /// congestion threshold period ending when the most recent packet in this batch was sent were
    /// lost.
    fn on_congestion_event(&mut self, now: Instant, sent: Instant, is_persistent_congestion: bool);

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
