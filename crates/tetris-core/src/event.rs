//! `GameEvent` — the frozen core→app event contract (T8).
//!
//! One item per observable rules transition, emitted by [`crate::game::Game`]
//! from `tick()` and `apply()` in the order the rules happened. Events are
//! informational: the authoritative render state always comes from
//! [`crate::game::GameSnapshot`], so the app can ignore events it does not
//! need (T13 HUD uses score/combo/level, T18 audio uses lock/clear/spin,
//! T19 juice uses clears/spins/PC).

use serde::{Deserialize, Serialize};

use crate::piece::{Piece, PieceState};
use crate::tspin::TSpinKind;

/// A single observable rules transition produced by the [`crate::game::Game`]
/// facade. Clone/Debug/PartialEq/Eq/serde: replays and snapshots are diffable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameEvent {
    /// A new active piece entered the field at `state`.
    PieceSpawned {
        /// Tetromino that spawned.
        piece: Piece,
        /// Spawn placement (guideline box position, `rot = Spawn`).
        state: PieceState,
    },
    /// The active piece merged into the stack at `state`.
    PieceLocked {
        /// Tetromino that locked.
        piece: Piece,
        /// Final placement at lock time.
        state: PieceState,
    },
    /// `lines` full rows were cleared by the just-merged piece.
    LineCleared {
        /// Number of rows cleared by this lock (1..=4).
        lines: usize,
    },
    /// Score total changed: `total` is the new sum, `delta` this award.
    ScoreChanged {
        /// Running score total after the award.
        total: u64,
        /// Points awarded by this event (never 0).
        delta: u64,
    },
    /// Level advanced to `level` (line-count threshold crossed).
    LevelUp {
        /// New level (≥ 2).
        level: u32,
    },
    /// The lock that just happened was classified as a T-spin of `kind`.
    TSpinDetected {
        /// Full or mini classification (never `None`).
        kind: TSpinKind,
    },
    /// A hold press was accepted: `incoming` is now the active piece.
    HoldPerformed {
        /// Piece that was parked before the swap (`None` on the first press).
        stored: Option<Piece>,
        /// Piece that became active from the swap.
        incoming: Piece,
        /// True when the swap consumed the bag head (first press on an
        /// empty cell) rather than the previously parked piece.
        from_bag: bool,
    },
    /// Board is completely empty after a merge + line clear (perfect clear).
    PerfectClear,
    /// Combo count changed to `n` (0 = the combo chain just broke).
    ComboChanged {
        /// PRD combo count after the lock (consecutive clears − 1).
        n: u32,
    },
    /// Block-out at spawn: the game is over; all further `tick`/`apply`
    /// calls are no-ops. Emitted exactly once per game.
    GameOver,
}
