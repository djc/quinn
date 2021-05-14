use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Controller, ControllerFactory};
use crate::connection::paths::RttEstimator;
use std::cmp;

/// CUBIC Constants.
///
/// These are recommended value in RFC8312.
const BETA_CUBIC: f64 = 0.7;

const C: f64 = 0.4;

/// CUBIC State Variables.
///
/// We need to keep those variables across the connection.
/// k, w_max, w_last_max is described in the RFC.
#[derive(Debug, Default, Clone)]
pub struct State {
    k: f64,

    w_max: f64,

    w_last_max: f64,

    // Used in CUBIC fix (see on_packet_sent())
    last_sent_time: Option<Instant>,

    // Store cwnd increment during congestion avoidance.
    cwnd_inc: u64,
}

/// CUBIC Functions.
///
/// Note that these calculations are based on a count of cwnd as bytes,
/// not packets.
/// Unit of t (duration) and RTT are based on seconds (f64).
impl State {
    // K = cbrt(w_max * (1 - beta_cubic) / C) (Eq. 2)
    fn cubic_k(&self, max_datagram_size: u64) -> f64 {
        let w_max = self.w_max / max_datagram_size as f64;
        (w_max * (1.0 - BETA_CUBIC) / C).cbrt()
    }

    // W_cubic(t) = C * (t - K)^3 - w_max (Eq. 1)
    fn w_cubic(&self, t: Duration, max_datagram_size: u64) -> f64 {
        let w_max = self.w_max / max_datagram_size as f64;

        (C * (t.as_secs_f64() - self.k).powi(3) + w_max) * max_datagram_size as f64
    }

    // W_est(t) = w_max * beta_cubic + 3 * (1 - beta_cubic) / (1 + beta_cubic) *
    // (t / RTT) (Eq. 4)
    fn w_est(&self, t: Duration, rtt: Duration, max_datagram_size: u64) -> f64 {
        let w_max = self.w_max / max_datagram_size as f64;
        (w_max * BETA_CUBIC
            + 3.0 * (1.0 - BETA_CUBIC) / (1.0 + BETA_CUBIC) * t.as_secs_f64() / rtt.as_secs_f64())
            * max_datagram_size as f64
    }
}

/// The RFC8312 congestion controller, as widely used for TCP
#[derive(Debug, Clone)]
pub struct Cubic {
    config: Arc<CubicConfig>,
    /// Maximum number of bytes in flight that may be sent.
    window: u64,
    /// Slow start threshold in bytes. When the congestion window is below ssthresh, the mode is
    /// slow start and the window grows by the number of bytes acknowledged.
    ssthresh: u64,
    /// The time when QUIC first detects a loss, causing it to enter recovery. When a packet sent
    /// after this time is acknowledged, QUIC exits recovery.
    recovery_start_time: Option<Instant>,
    cubic_state: State,
}

impl Cubic {
    /// Construct a state using the given `config` and current time `now`
    pub fn new(config: Arc<CubicConfig>, _now: Instant) -> Self {
        Self {
            window: config.initial_window,
            ssthresh: u64::MAX,
            recovery_start_time: None,
            config,
            cubic_state: Default::default(),
        }
    }
}

impl Controller for Cubic {
    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        if app_limited
            || self
                .recovery_start_time
                .map(|recovery_start_time| sent <= recovery_start_time)
                .unwrap_or(false)
        {
            return;
        }

        if self.window < self.ssthresh {
            // Slow start
            self.window += bytes;
        } else {
            // Congestion avoidance.
            let ca_start_time;

            match self.recovery_start_time {
                Some(t) => ca_start_time = t,
                None => {
                    // When we come here without congestion_event() triggered,
                    // initialize congestion_recovery_start_time, w_max and k.
                    ca_start_time = now;
                    self.recovery_start_time = Some(now);

                    self.cubic_state.w_max = self.window as f64;
                    self.cubic_state.k = 0.0;
                }
            }

            let t = now - ca_start_time;

            // w_cubic(t + rtt)
            let w_cubic = self
                .cubic_state
                .w_cubic(t + rtt.get(), self.config.max_datagram_size);

            // w_est(t)
            let w_est = self
                .cubic_state
                .w_est(t, rtt.get(), self.config.max_datagram_size);

            let mut cubic_cwnd = self.window;

            if w_cubic < w_est {
                // TCP friendly region.
                cubic_cwnd = cmp::max(cubic_cwnd, w_est as u64);
            } else if cubic_cwnd < w_cubic as u64 {
                // Concave region or convex region use same increment.
                let cubic_inc = (w_cubic - cubic_cwnd as f64) / cubic_cwnd as f64
                    * self.config.max_datagram_size as f64;

                cubic_cwnd += cubic_inc as u64;
            }

            // Update the increment and increase cwnd by MSS.
            self.cubic_state.cwnd_inc += cubic_cwnd - self.window;

            // cwnd_inc can be more than 1 MSS in the late stage of max probing.
            // however RFC9002 §7.3.3 (Congestion Avoidance) limits
            // the increase of cwnd to 1 max_datagram_size per cwnd acknowledged.
            if self.cubic_state.cwnd_inc as u64 >= self.config.max_datagram_size {
                self.window += self.config.max_datagram_size;
                self.cubic_state.cwnd_inc = 0;
            }
        }
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        _is_persistent_congestion: bool,
    ) {
        if self
            .recovery_start_time
            .map(|recovery_start_time| sent <= recovery_start_time)
            .unwrap_or(false)
        {
            return;
        }

        self.recovery_start_time = Some(now);

        // Fast convergence
        #[allow(clippy::branches_sharing_code)]
        // https://github.com/rust-lang/rust-clippy/issues/7198
        if self.cubic_state.w_max < self.cubic_state.w_last_max {
            self.cubic_state.w_last_max = self.cubic_state.w_max;
            self.cubic_state.w_max = self.cubic_state.w_max as f64 * (1.0 + BETA_CUBIC) / 2.0;
        } else {
            self.cubic_state.w_last_max = self.cubic_state.w_max;
        }

        self.cubic_state.w_max = self.window as f64;
        self.ssthresh = (self.cubic_state.w_max * BETA_CUBIC) as u64;
        self.ssthresh = cmp::max(
            self.ssthresh,
            self.config.max_datagram_size * self.config.minimum_window,
        );
        self.window = self.ssthresh;
        self.cubic_state.k = self.cubic_state.cubic_k(self.config.max_datagram_size);

        self.cubic_state.cwnd_inc = (self.cubic_state.cwnd_inc as f64 * BETA_CUBIC) as u64;
    }

    fn window(&self) -> u64 {
        self.window
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.config.initial_window
    }
}

/// Configuration for the `Cubic` congestion controller
#[derive(Debug, Clone)]
pub struct CubicConfig {
    max_datagram_size: u64,
    initial_window: u64,
    minimum_window: u64,
}

impl CubicConfig {
    /// The sender’s maximum UDP payload size. Does not include UDP or IP overhead.
    ///
    /// Used for calculating initial and minimum congestion windows.
    pub fn max_datagram_size(&mut self, value: u64) -> &mut Self {
        self.max_datagram_size = value;
        self
    }

    /// Default limit on the amount of outstanding data in bytes.
    ///
    /// Recommended value: `min(10 * max_datagram_size, max(2 * max_datagram_size, 14720))`
    pub fn initial_window(&mut self, value: u64) -> &mut Self {
        self.initial_window = value;
        self
    }

    /// Default minimum congestion window.
    ///
    /// Recommended value: `2 * max_datagram_size`.
    pub fn minimum_window(&mut self, value: u64) -> &mut Self {
        self.minimum_window = value;
        self
    }
}

impl Default for CubicConfig {
    fn default() -> Self {
        const MAX_DATAGRAM_SIZE: u64 = 1232;
        Self {
            max_datagram_size: MAX_DATAGRAM_SIZE,
            initial_window: 14720.max(2 * MAX_DATAGRAM_SIZE).min(10 * MAX_DATAGRAM_SIZE),
            minimum_window: 2 * MAX_DATAGRAM_SIZE,
        }
    }
}

impl ControllerFactory for Arc<CubicConfig> {
    fn build(&self, now: Instant) -> Box<dyn Controller> {
        Box::new(Cubic::new(self.clone(), now))
    }
}
