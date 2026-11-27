use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::clock::{Clock, SystemClock};

/// Maximum interval between two physical Esc presses to trigger the kill-switch.
pub const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(450);

#[derive(Default)]
struct KillSwitchState {
    last_esc: Option<Instant>,
    triggered_at: Option<DateTime<Utc>>,
}

/// Shared physical kill-switch state coordinating the OS global Esc monitor, the floating
/// Stop button, and the daemon's active control lease lifecycle.
///
/// Key invariants:
/// - Physical Esc presses are ignored completely while `control_active` is `false` (idle or
///   read-only observation), so normal user Esc presses never latch `stop_flag`.
/// - While `control_active` is `true`, triggering via Esc requires two physical Esc presses
///   within [`DOUBLE_ESC_WINDOW`] (450 ms) with no intervening non-Esc physical key.
/// - Clicking the floating Stop button immediately latches `stop_flag` while `control_active`
///   is `true`.
/// - Every state transition synchronizes through `state` so disarming and re-arming cannot race
///   with an in-flight Esc or Stop-button callback.
pub struct EscKillSwitch {
    stop_flag: Arc<AtomicBool>,
    control_active: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
    state: Mutex<KillSwitchState>,
}

impl EscKillSwitch {
    #[must_use]
    pub fn new(stop_flag: Arc<AtomicBool>, control_active: Arc<AtomicBool>) -> Self {
        Self::with_clock(stop_flag, control_active, Arc::new(SystemClock))
    }

    #[must_use]
    pub fn with_clock(
        stop_flag: Arc<AtomicBool>,
        control_active: Arc<AtomicBool>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            stop_flag,
            control_active,
            clock,
            state: Mutex::new(KillSwitchState::default()),
        }
    }

    #[must_use]
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_flag)
    }

    #[must_use]
    pub fn control_active(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.control_active)
    }

    #[must_use]
    pub fn is_control_active(&self) -> bool {
        self.control_active.load(Ordering::Acquire)
    }

    /// Arm or disarm the physical kill-switch when a mutating control lease starts or ends.
    /// Always clears any pending single-Esc timestamp so presses cannot span across sessions.
    pub fn set_control_active(&self, active: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.last_esc = None;
        if active {
            st.triggered_at = None;
        }
        self.control_active.store(active, Ordering::Release);
    }

    /// Take the timestamp recorded when the user last triggered double-Esc or the Stop button.
    #[must_use]
    pub fn take_triggered_at(&self) -> Option<DateTime<Utc>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .triggered_at
            .take()
    }

    /// Record a non-synthetic, non-repeat physical Esc press at `Instant::now()`.
    /// Returns `true` if this press completed a double-Esc and latched `stop_flag`.
    pub fn on_physical_esc(&self) -> bool {
        self.on_physical_esc_at(Instant::now())
    }

    /// Record a non-synthetic, non-repeat physical Esc press at an explicit timestamp `now`.
    /// Returns `true` if this press completed a double-Esc within [`DOUBLE_ESC_WINDOW`].
    pub fn on_physical_esc_at(&self, now: Instant) -> bool {
        if !self.control_active.load(Ordering::Acquire) {
            return false;
        }

        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.control_active.load(Ordering::Acquire) {
            st.last_esc = None;
            return false;
        }

        if let Some(prev) = st.last_esc
            && now
                .checked_duration_since(prev)
                .is_some_and(|elapsed| elapsed <= DOUBLE_ESC_WINDOW)
        {
            st.last_esc = None;
            if self.control_active.swap(false, Ordering::AcqRel) {
                st.triggered_at = Some(self.clock.now());
                self.stop_flag.store(true, Ordering::Release);
                return true;
            }
            return false;
        }

        st.last_esc = Some(now);
        false
    }

    /// Clear any pending single-Esc arm when the user presses a different physical key, so
    /// unrelated sequences like `Esc -> j -> Esc` do not accidentally trigger the kill-switch.
    pub fn on_other_physical_key(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.last_esc = None;
    }

    /// Trigger an immediate stop from the floating Stop button while a control lease is active.
    /// Returns `true` if a control lease was active and `stop_flag` was latched.
    pub fn trigger_stop_button(&self) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.last_esc = None;
        if self.control_active.swap(false, Ordering::AcqRel) {
            st.triggered_at = Some(self.clock.now());
            self.stop_flag.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_switch() -> EscKillSwitch {
        EscKillSwitch::new(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn idle_esc_never_latches_stop_flag() {
        let ks = make_switch();
        let t0 = Instant::now();

        assert!(!ks.on_physical_esc_at(t0));
        assert!(!ks.on_physical_esc_at(t0 + Duration::from_millis(100)));
        assert!(!ks.on_physical_esc_at(t0 + Duration::from_millis(200)));
        assert!(!ks.stop_flag().load(Ordering::Acquire));

        // Even activating control immediately afterward must not pair with an idle Esc.
        ks.set_control_active(true);
        assert!(!ks.on_physical_esc_at(t0 + Duration::from_millis(250)));
        assert!(!ks.stop_flag().load(Ordering::Acquire));
    }

    #[test]
    fn single_esc_ignored_and_double_esc_within_450ms_triggers() {
        let ks = make_switch();
        ks.set_control_active(true);
        let t0 = Instant::now();

        // First Esc only arms the window; stop_flag stays false.
        assert!(!ks.on_physical_esc_at(t0));
        assert!(!ks.stop_flag().load(Ordering::Acquire));
        assert!(ks.is_control_active());

        // Second Esc outside 450 ms (451 ms) does NOT trigger; becomes the new first Esc.
        let t1 = t0 + Duration::from_millis(451);
        assert!(!ks.on_physical_esc_at(t1));
        assert!(!ks.stop_flag().load(Ordering::Acquire));

        // Exact 450 ms boundary from t1 DOES trigger.
        let t2 = t1 + DOUBLE_ESC_WINDOW;
        assert!(ks.on_physical_esc_at(t2));
        assert!(ks.stop_flag().load(Ordering::Acquire));
        assert!(!ks.is_control_active());
        assert!(ks.take_triggered_at().is_some());
    }

    #[test]
    fn intervening_physical_key_resets_double_esc_window() {
        let ks = make_switch();
        ks.set_control_active(true);
        let t0 = Instant::now();

        assert!(!ks.on_physical_esc_at(t0));
        ks.on_other_physical_key();
        assert!(!ks.on_physical_esc_at(t0 + Duration::from_millis(150)));
        assert!(!ks.stop_flag().load(Ordering::Acquire));

        // Follow-up Esc within 450 ms of the second Esc triggers cleanly.
        assert!(ks.on_physical_esc_at(t0 + Duration::from_millis(350)));
        assert!(ks.stop_flag().load(Ordering::Acquire));
    }

    #[test]
    fn disarming_clears_pending_first_esc_across_sessions() {
        let ks = make_switch();
        ks.set_control_active(true);
        let t0 = Instant::now();

        assert!(!ks.on_physical_esc_at(t0));
        ks.set_control_active(false);
        ks.set_control_active(true);

        // Esc within 100 ms in the new session must not pair with the previous session's Esc.
        assert!(!ks.on_physical_esc_at(t0 + Duration::from_millis(100)));
        assert!(!ks.stop_flag().load(Ordering::Acquire));
    }

    #[test]
    fn stop_button_only_latches_when_active() {
        let ks = make_switch();
        assert!(!ks.trigger_stop_button());
        assert!(!ks.stop_flag().load(Ordering::Acquire));

        ks.set_control_active(true);
        assert!(ks.trigger_stop_button());
        assert!(ks.stop_flag().load(Ordering::Acquire));
        assert!(!ks.is_control_active());
        assert!(ks.take_triggered_at().is_some());
    }
}
