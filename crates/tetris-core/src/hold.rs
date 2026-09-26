//! Hold slot: one swap per piece, first press consumes the bag head, hold
//! cell persists across pieces (T6).
//!
//! [`HoldSlot`] owns the parked piece and the one-per-piece flag. The [`game`]
//! facade (T8) passes the piece about to be replaced plus the bag's next
//! piece, and spawns whatever [`HoldSwap::incoming`] names:
//!
//! - First press with an empty cell: `incoming` is the bag head
//!   (`consumed_bag == true`); the active piece parks in the cell and
//!   [`HoldSwap::stored`] reports the previous contents (`None`).
//! - Later presses: `incoming` is the parked piece; the active piece takes
//!   its place in the cell (`consumed_bag == false`).
//! - A second press on the same piece is rejected (`try_hold` → `None`)
//!   until [`HoldSlot::end_piece`] clears the flag.

use crate::piece::Piece;
use serde::{Deserialize, Serialize};

/// Result of an accepted hold press.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldSwap {
    /// Piece T8 must spawn now (bag head on first press, else the parked piece).
    pub incoming: Piece,
    /// What the hold cell contained *before* the swap; `None` on the first
    /// press, when the swap came from the bag instead.
    pub stored: Option<Piece>,
    /// True when the swap consumed the bag's next piece (first press on an
    /// empty cell) rather than the held piece.
    pub consumed_bag: bool,
}

/// The hold cell plus the one-swap-per-piece flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoldSlot {
    cell: Option<Piece>,
    used_this_piece: bool,
}

impl HoldSlot {
    /// Empty cell, unused flag — start of game.
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently parked piece, if any. Persists across pieces until replaced.
    pub fn slot(&self) -> Option<Piece> {
        self.cell
    }

    /// True while a hold press is still allowed for the active piece.
    pub fn can_hold(&self) -> bool {
        !self.used_this_piece
    }

    /// Attempt a hold press for `active` using `bag_next` as the bag head.
    ///
    /// Returns `None` (rejected) if this piece already used hold. Otherwise
    /// swaps per the module docs and marks the flag until the next
    /// [`HoldSlot::end_piece`].
    pub fn try_hold(&mut self, active: Piece, bag_next: Piece) -> Option<HoldSwap> {
        if self.used_this_piece {
            return None;
        }
        let (incoming, consumed_bag) = match self.cell {
            Some(parked) => (parked, false),
            None => (bag_next, true),
        };
        let stored = self.cell.replace(active);
        self.used_this_piece = true;
        Some(HoldSwap {
            incoming,
            stored,
            consumed_bag,
        })
    }

    /// A new piece is spawning: clear the one-press flag. The parked piece
    /// stays in the cell.
    pub fn end_piece(&mut self) {
        self.used_this_piece = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_hold_consumes_bag_head_not_empty_slot() {
        let mut h = HoldSlot::new();
        let s = h.try_hold(Piece::T, Piece::I).unwrap();
        assert_eq!(s.incoming, Piece::I);
        assert_eq!(s.stored, None);
        assert!(s.consumed_bag);
        assert_eq!(h.slot(), Some(Piece::T));
    }

    #[test]
    fn double_hold_rejected_until_end_piece() {
        let mut h = HoldSlot::new();
        assert!(h.try_hold(Piece::T, Piece::I).is_some());
        assert!(!h.can_hold());
        assert!(h.try_hold(Piece::I, Piece::O).is_none());
        // The rejection must not mutate the cell.
        assert_eq!(h.slot(), Some(Piece::T));
        h.end_piece();
        assert!(h.can_hold());
        assert!(h.try_hold(Piece::I, Piece::O).is_some());
    }

    #[test]
    fn second_press_swaps_parked_piece_not_bag() {
        let mut h = HoldSlot::new();
        h.try_hold(Piece::T, Piece::I).unwrap();
        h.end_piece();
        let s = h.try_hold(Piece::I, Piece::O).unwrap();
        assert_eq!(s.incoming, Piece::T);
        assert_eq!(s.stored, Some(Piece::T));
        assert!(!s.consumed_bag);
        assert_eq!(h.slot(), Some(Piece::I));
    }

    #[test]
    fn hold_persists_across_pieces_and_used_flag_resets() {
        let mut h = HoldSlot::new();
        h.try_hold(Piece::T, Piece::I).unwrap();
        // Two pieces pass without holding; the cell must survive untouched.
        h.end_piece();
        h.end_piece();
        assert_eq!(h.slot(), Some(Piece::T));
        assert!(h.can_hold());
        let s = h.try_hold(Piece::S, Piece::Z).unwrap();
        assert_eq!(s.incoming, Piece::T);
        assert_eq!(h.slot(), Some(Piece::S));
    }
}
