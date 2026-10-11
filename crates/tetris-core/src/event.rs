//! `GameEvent` — the frozen core→app event contract (T8).
//!
//! One item per observable rules transition, emitted by [`crate::game::Game`]
//! from `tick()` and `apply()` in the order the rules happened. Events are
//! informational: the authoritative render state always comes from
//! [`crate::game::GameSnapshot`], so the app can ignore events it does not
//! need (T13 HUD uses score/combo/level, T18 audio uses lock/clear/spin,
//! T19 juice uses clears/spins/PC).

use serde::{Deserialize, Serialize};

use crate::cannons::CannonSide;
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
    /// The mode's goal ([`crate::mode::Goal`]) was met at `tick`: the game
    /// freezes exactly like a block-out, all later `tick`/`apply` calls are
    /// no-ops. Emitted exactly once per game. Unlike `GameOver` this is a
    /// *win*: the snapshot's `game_over` flag stays `false`; the terminal
    /// reason lives in `Game::finished_reason()`.
    GoalReached {
        /// Logical tick (`Game::tick_count()`) the game froze on.
        tick: u64,
    },
    /// The mode's tick clock budget was reached during the `tick()` that
    /// emitted this event: the game freezes exactly like a block-out and all
    /// later `tick`/`apply` calls are no-ops. Emitted exactly once per game;
    /// the snapshot's `game_over` flag stays `false`.
    TimeUp {
        /// Logical tick the clock expired on (`== clock_ticks`).
        tick: u64,
    },
    /// Zen block-out (T14): a spawn collision under
    /// [`BlockOutBehavior::WipeAndContinue`](crate::mode::BlockOutBehavior)
    /// wiped the **whole** settled stack and play continues — the colliding
    /// piece re-spawned in the same batch. Emitted immediately *before* the
    /// re-spawn's `PieceSpawned`; never accompanied by `GameOver`. Score,
    /// lines, level, combo and B2B are untouched by the wipe itself (the
    /// lock that led here follows the normal rules).
    StackWiped {
        /// Logical tick (`Game::tick_count()`) the wipe happened on.
        tick: u64,
    },
    /// CANNONS mutator: the battery on `side` fired the cannon mounted at
    /// board `row` at `tick`. A cannonball (see
    /// [`Game::cannon_shots`](crate::game::Game::cannon_shots)) now travels
    /// that row; the renderer uses this for the muzzle flash and audio for
    /// the boom. Appended last so older `GameEvent` streams stay decodable.
    CannonFired {
        /// Battery that fired.
        side: CannonSide,
        /// Board row of the barrel that fired (one of
        /// [`CANNON_ROWS`](crate::cannons::CANNON_ROWS)).
        row: i32,
        /// Logical tick the shot left the barrel on.
        tick: u64,
    },
    /// A cannonball met the falling piece on its row at board `(row, col)`.
    /// `chipped == true` took one table cell out of the piece's
    /// [`alive`](crate::piece::PieceState::alive) mask; `false` sparked off
    /// the piece's last remaining cell (a ball never annihilates a piece).
    /// Either way the ball is spent and leaves the shot list.
    CannonImpact {
        /// Battery that fired the ball.
        side: CannonSide,
        /// Board row of the impact.
        row: i32,
        /// Board column of the met cell at impact.
        col: i32,
        /// Logical tick of the impact.
        tick: u64,
        /// `true` when a cell was chipped out of the piece.
        chipped: bool,
    },
}
