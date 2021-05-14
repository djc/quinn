use super::bbr_min_max::MinMax;
use std::fmt::{Debug, Formatter};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct BandwidthEstimation {
    total_acked: u64,
    prev_total_acked: u64,
    acked_time: Option<Instant>,
    prev_acked_time: Option<Instant>,
    total_sent: u64,
    prev_total_sent: u64,
    sent_time: Option<Instant>,
    prev_sent_time: Option<Instant>,
    max_filter: MinMax,
    acked_at_last_window: u64,
}

impl Default for BandwidthEstimation {
    fn default() -> Self {
        BandwidthEstimation {
            total_acked: 0,
            prev_total_acked: 0,
            acked_time: None,
            prev_acked_time: None,
            total_sent: 0,
            prev_total_sent: 0,
            sent_time: None,
            prev_sent_time: None,
            max_filter: MinMax::new(10),
            acked_at_last_window: 0,
        }
    }
}

impl Debug for BandwidthEstimation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.3} MB/s",
            self.get_estimate() as f32 / (1024 * 1024) as f32
        )
    }
}

impl BandwidthEstimation {
    pub fn on_sent(&mut self, now: Instant, bytes: u64) {
        self.prev_total_sent = self.total_sent;
        self.total_sent += bytes;
        self.prev_sent_time = self.sent_time;
        self.sent_time = Some(now);
    }

    pub fn on_ack(
        &mut self,
        now: Instant,
        _sent: Instant,
        bytes: u64,
        round: u64,
        app_limited: bool,
    ) {
        self.prev_total_acked = self.total_acked;
        self.total_acked += bytes;
        self.prev_acked_time = self.acked_time;
        self.acked_time = Some(now);

        if self.prev_sent_time.is_none() {
            return;
        }

        let send_rate;
        if self.sent_time.unwrap() > self.prev_sent_time.unwrap() {
            send_rate = BandwidthEstimation::bw_from_delta(
                self.total_sent - self.prev_total_sent,
                self.sent_time.unwrap() - self.prev_sent_time.unwrap(),
            )
            .unwrap_or(0);
        } else {
            send_rate = u64::MAX; // will take the min of send and ack, so this is just a skip
        }

        let ack_rate;
        if self.prev_acked_time.is_none() {
            ack_rate = 0;
        } else {
            ack_rate = BandwidthEstimation::bw_from_delta(
                self.total_acked - self.prev_total_acked,
                self.acked_time.unwrap() - self.prev_acked_time.unwrap(),
            )
            .unwrap_or(0);
        }

        let bandwidth = send_rate.min(ack_rate);
        if !app_limited && self.max_filter.get() < bandwidth {
            self.max_filter.update_max(round, bandwidth);
        }
    }

    pub fn bytes_acked_this_window(&self) -> u64 {
        self.total_acked - self.acked_at_last_window
    }

    pub fn end_acks(&mut self, _current_round: u64, _app_limited: bool) {
        self.acked_at_last_window = self.total_acked;
    }

    pub fn get_estimate(&self) -> u64 {
        self.max_filter.get()
    }

    pub const fn bw_from_delta(bytes: u64, delta: Duration) -> Option<u64> {
        let window_duration_ns = delta.as_nanos();
        if window_duration_ns == 0 {
            return None;
        }
        let b_ns = bytes * 1_000_000_000;
        let bytes_per_second = b_ns / (window_duration_ns as u64);
        Some(bytes_per_second)
    }
}
