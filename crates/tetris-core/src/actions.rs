//! `Action` enum — the discrete input schema the core consumes (PRD §13, T6).
//!
//! Pure data only: this module carries no execution logic. The [`game`]
//! facade (T8) interprets each variant against the board, [`lock`] timer and
//! [`hold`] slot. Keeping the schema here, small and stable, lets replays and
//! the input layer be built against a locked contract (PRD §13).

use serde::{Deserialize, Serialize};

/// A single discrete player input, as consumed by the core at 60 Hz.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    /// Shift the active piece one column left (no-op if blocked).
    MoveLeft,
    /// Shift the active piece one column right (no-op if blocked).
    MoveRight,
    /// Move the active piece down one row; faster gravity while held (T8).
    SoftDrop,
    /// Instantly drop to the ghost row and lock the same tick, bypassing
    /// the lock-delay countdown entirely ([`lock::LockTimer`]).
    HardDrop,
    /// Quarter turn clockwise with SRS kicks.
    RotateCw,
    /// Quarter turn counter-clockwise with SRS kicks.
    RotateCcw,
    /// Half turn with guideline 180° kicks.
    Rotate180,
    /// Swap the active piece with the hold slot ([`hold::HoldSlot`]);
    /// at most one press per piece.
    Hold,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_is_copy_eq_and_debug() {
        let a = Action::HardDrop;
        let b = a;
        assert_eq!(a, b);
        assert_eq!(format!("{:?}", Action::Hold), "Hold");
    }

    #[test]
    fn variants_are_distinct() {
        let all = [
            Action::MoveLeft,
            Action::MoveRight,
            Action::SoftDrop,
            Action::HardDrop,
            Action::RotateCw,
            Action::RotateCcw,
            Action::Rotate180,
            Action::Hold,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
    }
}
