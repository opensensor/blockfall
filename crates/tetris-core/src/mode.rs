//! Mode configuration (game-modes plan T2): the per-mode rule parameters a
//! `Game` is built from, plus the terminal-reason vocabulary modes report.
//!
//! [`ModeConfig::default`] reproduces today's Marathon exactly — `Game::new`
//! is `Game::with_config(seed, &ModeConfig::default())` and the T1 golden
//! test gates that equivalence. All times in a config are logical ticks:
//! the core never reads a clock (plan constraint 3).
//!
//! Design notes:
//! - `GameSnapshot`/`MatchSnapshot` gain **no** fields from modes (wire
//!   stability until T19); terminal state is exposed via
//!   [`crate::game::Game::finished_reason`].

use serde::{Deserialize, Serialize};

use crate::board::{Board, COLS, HIDDEN_ROWS, ROWS};
use crate::piece::Piece;
use crate::prng::Rng;

/// Win condition evaluated by the core (T3 wires `GarbageCleared` firing
/// checks beyond the initial wiring in T2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Goal {
    /// Reached when `lines_cleared >= n` at a lock (Sprint: `Lines(40)`).
    Lines(u32),
    /// Reached when [`crate::game::Game::garbage_rows_left`] hits 0 after a
    /// lock (Dig).
    ///
    /// Retire semantics: a settled row counts as a *garbage row* while it
    /// contains at least one [`Piece::Garbage`] cell — a single garbage cell
    /// on an otherwise empty row still counts. A garbage row retires the
    /// moment a line clear removes it (any garbage cell in the row retires
    /// the whole row, because clearing requires the row to complete), and
    /// the surviving garbage rows shift down with the stack, still counted.
    /// The goal is evaluated after this lock's line clear, so the winning
    /// lock's event batch emits `LineCleared` before `GoalReached`.
    ///
    /// A game that starts with zero garbage rows satisfies this on its first
    /// lock; modes using this goal must start with
    /// [`StartBoard::BuriedGarbage`] (or an equivalent hand-made garbage
    /// board).
    GarbageCleared,
}

/// Non-empty initial playfield assembled from the game seed at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartBoard {
    /// Bottom `rows` *visible-field* rows (board rows
    /// `ROWS - rows..ROWS`) are filled with [`Piece::Garbage`] except
    /// exactly one hole per row, and no two adjacent rows share a hole
    /// column. Hole columns are drawn from a splitmix64 stream derived
    /// from the game seed (salted so it never aliases the bag stream).
    /// `rows` is clamped to the visible field height
    /// (`ROWS - HIDDEN_ROWS = 20`) so hidden spawn rows stay empty and
    /// every spawn state fits.
    BuriedGarbage { rows: usize },
}

impl StartBoard {
    /// Materialize this start board for `seed`. Deterministic: same seed +
    /// same variant always yields the identical board.
    pub fn build(self, seed: u64) -> Board {
        match self {
            StartBoard::BuriedGarbage { rows } => buried_garbage_board(rows, seed),
        }
    }
}

/// Seed-stream salt for buried-garbage hole columns: keeps the hole stream
/// independent of the 7-bag's `Rng::new(seed)` stream.
const BURIED_GARBAGE_SALT: u64 = 0xFF51_AFD2_AFED_DAB9;

fn buried_garbage_board(rows: usize, seed: u64) -> Board {
    let mut board = Board::new();
    // Clamp to the visible field: hidden spawn rows must stay empty.
    let rows = rows.min(ROWS - HIDDEN_ROWS);
    let mut rng = Rng::new(seed ^ BURIED_GARBAGE_SALT);
    let mut prev_hole: Option<usize> = None;
    for r in (ROWS - rows)..ROWS {
        let mut hole = rng.next_below(COLS as u64) as usize;
        // Invariant: no two adjacent rows share a hole column.
        while Some(hole) == prev_hole {
            hole = rng.next_below(COLS as u64) as usize;
        }
        prev_hole = Some(hole);
        for c in 0..COLS {
            if c != hole {
                board.set(r, c, Some(Piece::Garbage));
            }
        }
    }
    board
}

/// What a block-out (spawn collision) does to the game.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockOutBehavior {
    /// Freeze the game and emit `GameEvent::GameOver` (Marathon et al.).
    #[default]
    End,
    /// Zen's wipe-and-continue behavior (wired in T14): the whole stack is
    /// cleared, the colliding piece re-spawns at its spawn state, and
    /// [`GameEvent::StackWiped`](crate::event::GameEvent::StackWiped) is
    /// emitted before the re-spawn's `PieceSpawned`. The game never freezes:
    /// `game_over` stays `false` and `finished_reason()` stays `None`;
    /// score/lines/level/combo/B2B keep their values.
    WipeAndContinue,
}

/// Default Survival feed: queue one garbage row every 5 s (at 60 Hz).
pub const FEED_INTERVAL_TICKS: u64 = 300;
/// Length of a decay window: every `decay_ticks` game ticks the queue
/// interval drops by `decay_by`.
pub const FEED_DECAY_TICKS: u64 = 1800;
/// Queue-interval reduction per elapsed decay window.
pub const FEED_DECAY_BY: u64 = 15;
/// Lower bound of the queue interval — the fastest the feed ever gets.
pub const FEED_FLOOR_TICKS: u64 = 60;

/// Timed garbage feed (T12, Survival): queues one garbage row every
/// `interval_ticks` game ticks. The queue interval decays by `decay_by`
/// ticks for every full `decay_ticks` decay window elapsed, never below
/// `floor_ticks` (a `decay_ticks` of 0 disables decay). Queued rows land on
/// the player's next lock through the versus garbage pusher (cap
/// [`crate::versus::MAX_GARBAGE_PER_LAND`] rows per lock, surplus trickles
/// over later locks, one hole column per landing batch); a push that runs
/// the stack past the ceiling tops out exactly like a block-out. The queue
/// events depend only on game ticks, never on locks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GarbageFeed {
    /// Starting queue interval, and the interval in force for decay
    /// window 0 (game ticks).
    pub interval_ticks: u64,
    /// Length of one decay window (game ticks); `decay_by` applies once
    /// per fully elapsed window.
    pub decay_ticks: u64,
    /// Interval reduction applied per elapsed decay window.
    pub decay_by: u64,
    /// Minimum interval the decay can shrink to (game ticks). A floor above
    /// `interval_ticks` wins: the feed runs at the floor from the start.
    pub floor_ticks: u64,
}

impl Default for GarbageFeed {
    /// Survival defaults: one row every 300 ticks, dropping 15 ticks per
    /// 1800-tick window down to a 60-tick floor.
    fn default() -> Self {
        Self {
            interval_ticks: FEED_INTERVAL_TICKS,
            decay_ticks: FEED_DECAY_TICKS,
            decay_by: FEED_DECAY_BY,
            floor_ticks: FEED_FLOOR_TICKS,
        }
    }
}

impl GarbageFeed {
    /// Queue interval in force at absolute game tick `t`:
    /// `interval_ticks - decay_by * (t / decay_ticks)`, clamped below by
    /// `floor_ticks` (windows are 0-based, so window `w` spans ticks
    /// `w * decay_ticks..(w + 1) * decay_ticks`).
    pub fn interval_at(&self, t: u64) -> u64 {
        // `checked_div` covers `decay_ticks == 0`: no windows elapsed, no
        // decay ever.
        let windows = t.checked_div(self.decay_ticks).unwrap_or(0);
        self.interval_ticks
            .saturating_sub(self.decay_by.saturating_mul(windows))
            .max(self.floor_ticks)
    }
}

/// Why a game froze. Exposed through [`crate::game::Game::finished_reason`]
/// (the snapshot carries no such field — wire stability).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    /// Block-out at spawn (or garbage pushed past the ceiling). The
    /// snapshot's `game_over` flag is `true` exactly in this case.
    TopOut,
    /// The [`Goal`] was met; the game froze without `game_over`.
    GoalReached,
    /// The `clock_ticks` budget expired; the game froze without `game_over`.
    TimeUp,
}

/// Everything that distinguishes one solo mode's rules from another. A mode
/// is pure data: `Game::with_config(seed, &config)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeConfig {
    /// Level the game starts at; sets the gravity from tick 0. Values
    /// below 1 behave as level 1 (matches `gravity::interval_for`
    /// clamping).
    pub start_level: u32,
    /// `true` (Marathon): every 10 lines raise the level and emit
    /// `GameEvent::LevelUp`. `false` (Sprint/Dig): the level and gravity
    /// interval are pinned to `start_level` and no `LevelUp` is ever
    /// emitted.
    pub levels_advance: bool,
    /// Optional win condition; checked at each lock.
    pub goal: Option<Goal>,
    /// Optional logical-tick budget; `tick()` emits
    /// `GameEvent::TimeUp { tick }` and freezes once `ticks >= n`
    /// (Ultra: `Some(21_600)` = 6 min). `None` = no clock.
    pub clock_ticks: Option<u64>,
    /// Optional non-empty initial playfield.
    pub start_board: Option<StartBoard>,
    /// What a block-out does.
    pub on_block_out: BlockOutBehavior,
    /// Optional timed garbage feed (T12, Survival). `None` (the default)
    /// means no feed: every pre-T12 config behaves exactly as before.
    pub garbage_feed: Option<GarbageFeed>,
}

impl Default for ModeConfig {
    /// Today's Marathon exactly: start at level 1 with the line-count
    /// level curve, no goal, no clock, empty board, end on block-out.
    fn default() -> Self {
        Self {
            start_level: 1,
            levels_advance: true,
            goal: None,
            clock_ticks: None,
            start_board: None,
            on_block_out: BlockOutBehavior::End,
            garbage_feed: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Game;

    #[test]
    fn default_config_is_marathon() {
        let c = ModeConfig::default();
        assert_eq!(c.start_level, 1);
        assert!(c.levels_advance);
        assert_eq!(c.goal, None);
        assert_eq!(c.clock_ticks, None);
        assert_eq!(c.start_board, None);
        assert_eq!(c.on_block_out, BlockOutBehavior::End);
        assert_eq!(c.garbage_feed, None);
    }

    #[test]
    fn garbage_feed_defaults_are_survival_constants() {
        let f = GarbageFeed::default();
        assert_eq!(f.interval_ticks, FEED_INTERVAL_TICKS);
        assert_eq!(f.decay_ticks, FEED_DECAY_TICKS);
        assert_eq!(f.decay_by, FEED_DECAY_BY);
        assert_eq!(f.floor_ticks, FEED_FLOOR_TICKS);
        assert_eq!(
            f,
            GarbageFeed {
                interval_ticks: 300,
                decay_ticks: 1800,
                decay_by: 15,
                floor_ticks: 60,
            }
        );
    }

    #[test]
    fn feed_interval_at_decays_once_per_window_to_floor() {
        let f = GarbageFeed::default();
        // Window 0 spans ticks 0..1800 at the starting interval.
        assert_eq!(f.interval_at(0), 300);
        assert_eq!(f.interval_at(1799), 300);
        // decay_by applies exactly once per elapsed 1800-tick window…
        for w in 1..17u64 {
            assert_eq!(
                f.interval_at(w * 1800),
                300 - 15 * w,
                "first tick of window {w}"
            );
            assert_eq!(
                f.interval_at(w * 1800 - 1),
                300 - 15 * (w - 1),
                "last tick of window {}",
                w - 1
            );
        }
        // …and floors at 60 from window 16 (tick 28 800) onward.
        assert_eq!(f.interval_at(15 * 1800), 75);
        assert_eq!(f.interval_at(16 * 1800), 60);
        assert_eq!(f.interval_at(100_000_000), 60);
    }

    #[test]
    fn feed_interval_at_degenerate_configs() {
        // decay_ticks == 0 disables decay; a floor above the start wins;
        // huge windows saturate instead of overflowing or wrapping.
        let no_decay = GarbageFeed {
            interval_ticks: 100,
            decay_ticks: 0,
            decay_by: 15,
            floor_ticks: 10,
        };
        assert_eq!(no_decay.interval_at(u64::MAX), 100);
        let floored = GarbageFeed {
            interval_ticks: 10,
            decay_ticks: 1,
            decay_by: 1,
            floor_ticks: 120,
        };
        assert_eq!(floored.interval_at(0), 120);
        let saturating = GarbageFeed {
            interval_ticks: 300,
            decay_ticks: 1,
            decay_by: u64::MAX,
            floor_ticks: 0,
        };
        assert_eq!(saturating.interval_at(u64::MAX), 0);
    }

    /// Buried-garbage start-board invariants over 100 seeds:
    /// every buried row has exactly one hole, no two adjacent rows share
    /// a hole column, and at 10 buried rows every possible first piece
    /// still spawns without collision.
    #[test]
    fn buried_garbage_invariants_over_100_seeds() {
        for seed in 0..100u64 {
            let board = StartBoard::BuriedGarbage { rows: 10 }.build(seed);
            let mut prev_hole: Option<usize> = None;
            for r in (ROWS - 10)..ROWS {
                let holes: Vec<usize> = (0..COLS).filter(|&c| board.get(r, c).is_none()).collect();
                assert_eq!(holes.len(), 1, "seed {seed} row {r}: {holes:?} holes");
                let hole = holes[0];
                assert_ne!(
                    Some(hole),
                    prev_hole,
                    "seed {seed}: rows {} and {r} share hole column {hole}",
                    r - 1
                );
                prev_hole = Some(hole);
                // All other cells are garbage (never a real piece mark).
                for c in 0..COLS {
                    if c != hole {
                        assert_eq!(board.get(r, c), Some(Piece::Garbage));
                    }
                }
            }
            // Rows above the buried band are empty, including hidden rows.
            for r in 0..(ROWS - 10) {
                for c in 0..COLS {
                    assert_eq!(board.get(r, c), None, "seed {seed} cell {r},{c}");
                }
            }
            // The first piece must spawn without collision (all 7 pieces,
            // whatever the bag deals — checked for every piece here).
            for piece in Piece::ALL {
                let st = crate::piece::spawn_state(piece);
                assert!(
                    !board.collides(&st),
                    "seed {seed}: {piece:?} cannot spawn over 10 buried rows"
                );
            }
        }
    }

    #[test]
    fn buried_garbage_first_spawn_fits_for_100_seeds() {
        // Same invariant through the real Game path: the dealt first piece
        // (not just every piece) spawns live.
        let config = ModeConfig {
            start_board: Some(StartBoard::BuriedGarbage { rows: 10 }),
            ..ModeConfig::default()
        };
        for seed in 0..100u64 {
            let g = Game::with_config(seed, &config);
            let s = g.snapshot();
            assert!(
                s.active.is_some() && !s.game_over,
                "seed {seed}: first piece must spawn over 10 buried rows"
            );
            assert_eq!(g.garbage_rows_left(), 10);
        }
    }

    #[test]
    fn buried_garbage_is_seed_derivated_deterministic() {
        let a = StartBoard::BuriedGarbage { rows: 10 }.build(7);
        let b = StartBoard::BuriedGarbage { rows: 10 }.build(7);
        assert_eq!(a.get(12, 0), b.get(12, 0));
        let mut same = true;
        for r in 0..ROWS {
            for c in 0..COLS {
                same &= a.get(r, c) == b.get(r, c);
            }
        }
        assert!(same, "same seed must build identical buried boards");
    }

    #[test]
    fn buried_garbage_clamps_rows_to_visible_field() {
        let board = StartBoard::BuriedGarbage { rows: 25 }.build(1);
        // Hidden spawn rows stay empty even when over-requested.
        for r in 0..HIDDEN_ROWS {
            for c in 0..COLS {
                assert_eq!(board.get(r, c), None);
            }
        }
        assert_eq!(
            (0..COLS)
                .filter(|&c| board.get(ROWS - HIDDEN_ROWS, c).is_none())
                .count(),
            1
        );
    }
}
