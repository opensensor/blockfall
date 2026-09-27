//! Lock-delay timer: 500 ms grounded at 60 Hz, reset on successful
//! move/rotate, max 15 resets then force-lock (T6).
//!
//! The timer is a passive component owned by the [`game`] facade (T8), which
//! reports the piece's grounded state each tick and notifies it of *successful*
//! manipulations. A failed move or kick-out must NOT call
//! [`LockTimer::reset_on_success`] — the plan's validation requires failed
//! moves to leave the countdown untouched.
//!
//! Hard drop bypasses this timer entirely: T8 merges the piece on the same
//! tick as [`Action::HardDrop`](crate::actions::Action) without consulting the
//! timer at all. If T8 instead prefers a uniform "locks next tick" path,
//! [`LockTimer::force_now`] arms a force-lock that the next
//! [`LockTimer::tick`] reports, which is equivalent from the caller's view.

use crate::gravity::TICK_HZ;

/// Grounded lock-delay window: 500 ms at [`TICK_HZ`] = 30 ticks.
pub const LOCK_DELAY_TICKS: u32 = TICK_HZ / 2;

/// Maximum number of lock-delay resets before the piece force-locks on the
/// next successful manipulation (guideline move-reset cap).
pub const MAX_RESETS: u32 = 15;

/// Per-piece grounded lock-delay countdown.
#[derive(Clone, Debug, Default)]
pub struct LockTimer {
    resting: bool,
    ticks_left: u32,
    resets: u32,
    forced: bool,
    locked: bool,
}

impl LockTimer {
    /// Fresh timer for a newly spawned piece: airborne, idle, zero resets.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance one logical tick. Returns `true` on the tick the piece must
    /// lock — either the grounded countdown expired or a force was armed
    /// ([`LockTimer::reset_on_success`] exhaustion, [`LockTimer::force_now`]).
    /// Once locked, keeps returning `true`.
    pub fn tick(&mut self) -> bool {
        if self.locked || self.forced {
            self.locked = true;
            return true;
        }
        if self.resting {
            self.ticks_left -= 1;
            if self.ticks_left == 0 {
                self.locked = true;
                return true;
            }
        }
        false
    }

    /// Report the piece as grounded. Starts the countdown on the first
    /// ground contact and re-arms a full window when re-grounding after
    /// [`LockTimer::on_ungrounded`]. Idempotent while already resting.
    pub fn on_grounded(&mut self) {
        if !self.locked && !self.forced && !self.resting {
            self.resting = true;
            self.ticks_left = LOCK_DELAY_TICKS;
        }
    }

    /// Report the piece as airborne again (e.g. it moved off a ledge):
    /// cancels the countdown without consuming a reset. The reset budget
    /// persists until the piece locks.
    pub fn on_ungrounded(&mut self) {
        self.resting = false;
    }

    /// Notify a *successful* move or rotation. While the piece is resting,
    /// the countdown is re-armed to a full window and one reset is consumed.
    /// Returns `false` once [`MAX_RESETS`] are exhausted — the timer then
    /// force-locks on the next [`LockTimer::tick`] — and after locking.
    /// A failed move/rotate must not call this at all.
    ///
    /// Airborne manipulations neither consume the reset budget nor force a
    /// lock (PRD §6.5 scopes the delay to "500 ms grounded"): sliding or
    /// spinning a falling piece can never lock it in mid-air.
    pub fn reset_on_success(&mut self) -> bool {
        if self.locked || self.forced {
            return false;
        }
        if self.resting {
            if self.resets >= MAX_RESETS {
                self.forced = true;
                return false;
            }
            self.resets += 1;
            self.ticks_left = LOCK_DELAY_TICKS;
        }
        true
    }

    /// Arm an immediate force-lock (next [`LockTimer::tick`] returns `true`).
    /// Hard drop does not need this — it locks the same tick outside the
    /// timer — but it makes a "lock on next tick" flow trivial.
    pub fn force_now(&mut self) {
        if !self.locked {
            self.forced = true;
        }
    }

    /// True once the piece must lock (countdown expired or forced).
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// True while the grounded countdown is running (piece resting, not yet
    /// locked or forced).
    pub fn pending(&self) -> bool {
        self.resting && !self.locked && !self.forced
    }

    /// Resets consumed so far (diagnostics/tests).
    pub fn resets(&self) -> u32 {
        self.resets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airborne_does_not_countdown() {
        let mut t = LockTimer::new();
        for _ in 0..100 {
            assert!(!t.tick());
        }
        assert!(!t.pending());
        assert!(!t.is_locked());
    }

    #[test]
    fn grounded_locks_after_30_ticks() {
        let mut t = LockTimer::new();
        t.on_grounded();
        assert!(t.pending());
        for _ in 0..LOCK_DELAY_TICKS - 1 {
            assert!(!t.tick());
        }
        assert!(t.tick());
        assert!(t.is_locked());
        assert!(!t.pending());
    }

    #[test]
    fn successful_reset_rearms_full_window_not_failed_move() {
        let mut t = LockTimer::new();
        t.on_grounded();
        for _ in 0..25 {
            assert!(!t.tick());
        }
        // Successful shift: countdown restarts at a full 30 ticks.
        assert!(t.reset_on_success());
        for _ in 0..LOCK_DELAY_TICKS - 1 {
            assert!(!t.tick());
        }
        assert!(t.tick());
        assert_eq!(t.resets(), 1);
    }

    #[test]
    fn failed_move_does_not_reset() {
        let mut t = LockTimer::new();
        t.on_grounded();
        // 20 ticks in, a *failed* move happens: reset_on_success is not
        // called, so the original window still expires at tick 30.
        for _ in 0..20 {
            assert!(!t.tick());
        }
        for _ in 0..9 {
            assert!(!t.tick());
        }
        assert!(t.tick());
        assert_eq!(t.resets(), 0);
    }

    #[test]
    fn force_lock_after_15_resets() {
        let mut t = LockTimer::new();
        t.on_grounded();
        for _ in 0..MAX_RESETS {
            assert!(t.reset_on_success());
            assert!(!t.is_locked());
            assert!(!t.tick()); // reset window keeps it alive
        }
        // 16th successful manipulation: budget exhausted -> force lock.
        assert!(!t.reset_on_success());
        assert!(t.tick());
        assert!(t.is_locked());
        assert_eq!(t.resets(), MAX_RESETS);
    }

    #[test]
    fn airborne_manipulations_never_consume_or_force() {
        // Regression (playtest): 16 airborne moves/rotations used to
        // force-lock a falling piece in mid-air ("stuck in the air").
        let mut t = LockTimer::new();
        for _ in 0..64 {
            assert!(t.reset_on_success(), "airborne reset must always succeed");
            assert!(!t.tick());
        }
        assert_eq!(t.resets(), 0);
        assert!(!t.is_locked());
        // Full budget still available once grounded.
        t.on_grounded();
        for _ in 0..MAX_RESETS {
            assert!(t.reset_on_success());
            assert!(!t.tick());
        }
        assert_eq!(t.resets(), MAX_RESETS);
        t.on_ungrounded();
        assert!(t.reset_on_success());
        assert_eq!(t.resets(), MAX_RESETS);
        assert!(!t.tick());
    }

    #[test]
    fn ungrounded_cancels_countdown_then_regrounds_full_window() {
        let mut t = LockTimer::new();
        t.on_grounded();
        for _ in 0..29 {
            assert!(!t.tick());
        }
        t.on_ungrounded();
        assert!(!t.pending());
        for _ in 0..100 {
            assert!(!t.tick());
        }
        t.on_grounded();
        assert!(t.pending());
        for _ in 0..LOCK_DELAY_TICKS - 1 {
            assert!(!t.tick());
        }
        assert!(t.tick());
    }

    #[test]
    fn force_now_locks_next_tick() {
        let mut t = LockTimer::new();
        t.on_grounded();
        assert!(!t.tick());
        t.force_now();
        assert!(!t.pending());
        assert!(t.tick());
        assert!(t.is_locked());
    }

    #[test]
    fn grounded_is_idempotent_while_resting() {
        let mut t = LockTimer::new();
        t.on_grounded();
        for _ in 0..10 {
            assert!(!t.tick());
            t.on_grounded(); // repeatedly reporting grounded must not re-arm
        }
        for _ in 0..LOCK_DELAY_TICKS - 11 {
            assert!(!t.tick());
        }
        assert!(t.tick());
    }
}
