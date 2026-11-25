//! The outage probe (transport.md 6.5): a congestion controller around quinn's Cubic that keeps the link probing
//! through an outage, so it recovers within about one round of the link's return instead of after QUIC's doubled
//! probe timeout.
//!
//! - **The problem** [V, quinn-proto 0.11.19]: during an outage nothing is acknowledged, so the bytes in flight stay
//!   at the congestion window and quinn holds every packet but its tail-loss probes. The probe wait doubles with
//!   each probe and resets only when an ACK of our own packets arrives, so after an outage of L seconds a side
//!   waits up to about L more for its next probe, and each direction waits for its own.
//! - **The trip:** with packets in flight and no ACK for `T_trip` = max(`quiet`, 3 x smoothed RTT), the window
//!   opens (`window()` returns [`OPEN`]) until the next ACK. Every packet then goes out as it is written: the
//!   app's per-tick traffic and the keep-alive PINGs (`T_keepalive` of silence) act as probes, and the first to
//!   cross the returned link is acknowledged, which closes the window again and resets quinn's backoff.
//! - **What pe-net budgets with:** [`Controller::metrics`] reports the inner controller's window, never the open
//!   one, and pe-net's budget reads it there (4.1), so the trip lifts only quinn's own gate: pe-net still lets out
//!   what Cubic's window allows.
//! - **Time:** only the `now` quinn passes in, so the harness stays deterministic (8). `window()` has no `now`, so
//!   the trip is decided on each send; the first send after `T_trip` is a tail-loss probe, which quinn sends
//!   whatever the window says.
//! - **The risk:** a real stall with no ACK for `T_trip` (a queue far longer than 3 RTTs) trips it too, and the
//!   link then sends into the queue until an ACK comes back. pe-net's budget still bounds what it hands quinn.
//!   And an outage longer than `T_idle` sends into a dead path all the way to the close.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn_proto::RttEstimator;
use quinn_proto::congestion::{Controller, ControllerFactory, ControllerMetrics};

/// The window while tripped: past `u32::MAX`, which also turns quinn's pacer off.
pub const OPEN: u64 = 1 << 40;

/// Builds an [`Outage`] around each connection's inner controller.
pub struct OutageConfig {
    /// The controller it wraps: Cubic in pe-net.
    pub inner: Arc<dyn ControllerFactory + Send + Sync>,
    /// The least silence that trips it: `T_trip`'s floor.
    pub quiet: Duration,
}

impl ControllerFactory for OutageConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        let inner = self.inner.clone().build(now, current_mtu);
        Box::new(Outage { inner, quiet: self.quiet, srtt: None, quiet_since: None, open: false })
    }
}

/// See the module docs.
pub struct Outage {
    inner: Box<dyn Controller>,
    quiet: Duration,
    /// The smoothed RTT at the last ACK.
    srtt: Option<Duration>,
    /// Since when nothing has been acknowledged while something is in flight; `None` with nothing in flight.
    quiet_since: Option<Instant>,
    open: bool,
}

impl Outage {
    /// Whether the window is open now.
    pub fn is_open(&self) -> bool {
        self.open
    }

    fn trip_after(&self) -> Duration {
        self.srtt.map_or(self.quiet, |r| self.quiet.max(r * 3))
    }
}

impl Controller for Outage {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
        match self.quiet_since {
            None => self.quiet_since = Some(now),
            Some(since) => {
                if now.saturating_duration_since(since) >= self.trip_after() {
                    self.open = true;
                }
            }
        }
    }

    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, rtt: &RttEstimator) {
        self.srtt = Some(rtt.get());
        self.open = false;
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(&mut self, now: Instant, in_flight: u64, app_limited: bool, largest_packet_num_acked: Option<u64>) {
        // The silence restarts at each ACK; with nothing left in flight there is none.
        self.quiet_since = (in_flight > 0).then_some(now);
        self.open = false;
        self.inner.on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(&mut self, now: Instant, sent: Instant, is_persistent_congestion: bool, lost_bytes: u64) {
        self.inner.on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        if self.open { OPEN } else { self.inner.window() }
    }

    /// The inner controller's, never the open window: what pe-net's budget reads (4.1).
    fn metrics(&self) -> ControllerMetrics {
        self.inner.metrics()
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Outage {
            inner: self.inner.clone_box(),
            quiet: self.quiet,
            srtt: self.srtt,
            quiet_since: self.quiet_since,
            open: self.open,
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quinn_proto::congestion::CubicConfig;

    fn outage(now: Instant) -> Box<dyn Controller> {
        let cfg = OutageConfig { inner: Arc::new(CubicConfig::default()), quiet: Duration::from_millis(300) };
        Arc::new(cfg).build(now, 1200)
    }

    #[test]
    fn it_opens_after_the_quiet_and_closes_on_an_ack() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut c = outage(t0);
        let cubic = c.window();
        c.on_sent(ms(0), 1200, 1);
        c.on_sent(ms(299), 1200, 2);
        assert_eq!(c.window(), cubic, "not yet");
        c.on_sent(ms(300), 1200, 3);
        assert_eq!(c.window(), OPEN);
        assert_eq!(c.metrics().congestion_window, cubic, "the budget still sees Cubic's window");
        c.on_end_acks(ms(320), 2400, false, Some(3));
        assert_eq!(c.window(), cubic, "an ACK closes it");
        // The silence restarts at the ACK.
        c.on_sent(ms(500), 1200, 4);
        assert_ne!(c.window(), OPEN);
        c.on_sent(ms(620), 1200, 5);
        assert_eq!(c.window(), OPEN);
    }

    #[test]
    fn nothing_in_flight_is_no_silence() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut c = outage(t0);
        c.on_sent(ms(0), 1200, 1);
        c.on_end_acks(ms(20), 0, true, Some(1));
        // An idle second, then one send: the silence starts at the send.
        c.on_sent(ms(1_020), 1200, 2);
        c.on_sent(ms(1_100), 1200, 3);
        assert_ne!(c.window(), OPEN);
    }
}
