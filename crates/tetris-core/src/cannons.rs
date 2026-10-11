//! Side-battery cannons for the CANNONS mutator: the [`CannonRules`] firing
//! schedule, the [`CannonShot`] in-flight record, and the shared `x8`
//! position convention. `Game` (game.rs) owns the runtime state and drives
//! one deterministic [`Rng`](crate::prng::Rng) stream from these rules.
//!
//! `x8` is the horizontal position of a cannonball quantized to 1/8 cell
//! units in board *column* coordinates: `x8 = 0` is the center of column 0,
//! `x8 = 80` the center of column 10 (one cell past the right edge). The
//! renderer maps `x8` to world space with
//! `x = origin.x + (x8 as f32 / 8.0 + 0.5) * cell` and never needs to know
//! the rules — [`CannonShot`] is the whole render contract.

use serde::{Deserialize, Serialize};

use crate::board::COLS;

/// Which side battery fired. `Left` shots fly left-to-right.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CannonSide {
    /// The battery left of the well; its shots travel toward larger columns.
    Left,
    /// The battery right of the well; its shots travel toward smaller columns.
    Right,
}

/// Firing schedule for the two side batteries (CANNONS mutator). One random
/// cannon fires every `min_gap_ticks..=max_gap_ticks` game ticks, starting
/// `first_shot_delay` ticks into the run. All randomness comes from the
/// game's independent cannon stream (see [`CANNON_SALT`]), so the 7-bag and
/// the Survival feed streams are untouched and a run is fully reproducible
/// from its seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CannonRules {
    /// Game ticks before the first cannon may fire (opening grace).
    pub first_shot_delay: u64,
    /// Lower bound of the random gap between successive shots.
    pub min_gap_ticks: u64,
    /// Upper bound of the random gap between successive shots. A bound
    /// below `min_gap_ticks` behaves as `min_gap_ticks`.
    pub max_gap_ticks: u64,
}

impl Default for CannonRules {
    /// Playtest defaults: first shot after 4 s, then one every 2.5–5.5 s.
    fn default() -> Self {
        Self {
            first_shot_delay: 240,
            min_gap_ticks: 150,
            max_gap_ticks: 330,
        }
    }
}

impl CannonRules {
    /// Gap in force after a shot fired on this rule set:
    /// `min_gap_ticks + draw`, where `draw` is a uniform draw from
    /// `0..=(max_gap_ticks - min_gap_ticks)` the *caller* makes from the
    /// cannon stream (one draw per shot, together with the cannon pick).
    pub fn gap_range(&self) -> (u64, u64) {
        let min = self.min_gap_ticks;
        let max = self.max_gap_ticks.max(min);
        (min, max)
    }
}

/// Board rows (0-based, visible field starts at
/// [`HIDDEN_ROWS`](crate::board::HIDDEN_ROWS)) of the three barrels on each
/// side: upper, mid and lower tier. A shot only ever crosses its own
/// cannon's row.
pub const CANNON_ROWS: [i32; 3] = [4, 9, 14];
/// Barrels per side battery; the six cannons are indexed `0..6` with the left
/// battery first (`i < 3` is Left, `i % 3` selects the row).
pub const CANNONS_PER_SIDE: u64 = 3;
/// Seed-stream salt for the cannon firing stream: keeps the side/row/gap
/// draws independent of both the 7-bag's `Rng::new(seed)` stream and the
/// Survival feed's `FEED_HOLE_SALT` stream.
pub const CANNON_SALT: u64 = 0xC0DE_C0DE_BC77_1E0F;

/// Cannonball speed in 1/8-cell units per game tick (0.5 cells/tick).
pub const SHOT_SPEED_X8: i32 = 4;
/// Spawn position of a Left shot: one full cell outside the left edge.
pub const SHOT_SPAWN_LEFT_X8: i32 = -8;
/// Spawn position of a Right shot: one full cell outside the right edge
/// (column `COLS` center).
pub const SHOT_SPAWN_RIGHT_X8: i32 = COLS as i32 * 8;
/// A shot is gone once it drifts past these bounds (one margin cell beyond
/// each edge) without meeting the piece.
pub const SHOT_EXIT_BELOW_X8: i32 = -16;
/// See [`SHOT_EXIT_BELOW_X8`].
pub const SHOT_EXIT_ABOVE_X8: i32 = (COLS as i32 + 2) * 8;

/// One cannonball in flight. Emitted by the firing schedule
/// ([`Game::cannon_shots`](crate::game::Game::cannon_shots) is the render
/// view — in-flight state is not part of `GameSnapshot`, mirroring the
/// Survival feed's treatment for wire stability).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CannonShot {
    /// Battery that fired.
    pub side: CannonSide,
    /// Board row the ball travels along (one of [`CANNON_ROWS`]).
    pub row: i32,
    /// Horizontal position in 1/8-cell column units (module docs).
    pub x8: i32,
}

impl CannonShot {
    /// Horizontal advance per tick in `x8` units.
    #[must_use]
    pub const fn velocity_x8(&self) -> i32 {
        match self.side {
            CannonSide::Left => SHOT_SPEED_X8,
            CannonSide::Right => -SHOT_SPEED_X8,
        }
    }

    /// Board column whose cell this shot covers on the tick ending at its
    /// current `x8` (checked against the active piece's alive cells). A
    /// margin position returns a column outside `0..COLS`.
    #[must_use]
    pub const fn covered_col(&self) -> i32 {
        match self.side {
            CannonSide::Left => (self.x8 + SHOT_SPEED_X8).div_euclid(8),
            CannonSide::Right => (self.x8 - SHOT_SPEED_X8).div_euclid(8),
        }
    }

    /// True once the ball is past both exit bounds and can be despawned.
    #[must_use]
    pub const fn off_field(&self) -> bool {
        self.x8 < SHOT_EXIT_BELOW_X8 || self.x8 > SHOT_EXIT_ABOVE_X8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rules_are_the_playtest_schedule() {
        let r = CannonRules::default();
        assert_eq!(r.first_shot_delay, 240);
        assert_eq!(r.min_gap_ticks, 150);
        assert_eq!(r.max_gap_ticks, 330);
    }

    #[test]
    fn gap_range_clamps_inverted_bounds() {
        let r = CannonRules {
            min_gap_ticks: 10,
            max_gap_ticks: 3,
            ..CannonRules::default()
        };
        assert_eq!(r.gap_range(), (10, 10));
    }

    #[test]
    fn shot_geometry_is_mirror_symmetric() {
        let left = CannonShot {
            side: CannonSide::Left,
            row: 4,
            x8: SHOT_SPAWN_LEFT_X8 + SHOT_SPEED_X8,
        };
        let right = CannonShot {
            side: CannonSide::Right,
            row: 4,
            x8: SHOT_SPAWN_RIGHT_X8 - SHOT_SPEED_X8,
        };
        // First covered column after spawning: col 0 and col COLS-1.
        assert_eq!(left.covered_col(), 0);
        assert_eq!(right.covered_col(), COLS as i32 - 1);
        assert_eq!(left.velocity_x8(), SHOT_SPEED_X8);
        assert_eq!(right.velocity_x8(), -SHOT_SPEED_X8);
        // Spawn positions sit inside the exit bounds; exits clear them.
        assert!(!left.off_field() && !right.off_field());
        let gone_left = CannonShot {
            x8: SHOT_EXIT_ABOVE_X8 + SHOT_SPEED_X8,
            ..left
        };
        let gone_right = CannonShot {
            x8: SHOT_EXIT_BELOW_X8 - SHOT_SPEED_X8,
            ..right
        };
        assert!(gone_left.off_field() && gone_right.off_field());
    }
}
