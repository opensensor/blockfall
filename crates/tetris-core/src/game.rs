//! `Game` facade: `new(seed)` + `tick()`/`apply(Action)` + `snapshot()` (T8).
//!
//! Pure deterministic 60 Hz loop composing Board/Bag/gravity/SRS/lock/hold/
//! score/tspin. No I/O, no wall-clock: a game is fully reproducible from its
//! seed plus the action log. `tick()` advances one logical frame; `apply()`
//! injects one discrete player action in the same frame. Both return the
//! [`GameEvent`]s the transition produced (empty when nothing observable
//! happened). Block-out at spawn ends the game; afterwards every call is a
//! no-op returning no events — except under
//! [`BlockOutBehavior::WipeAndContinue`](crate::mode::BlockOutBehavior)
//! (Zen, T14), where a block-out wipes the stack and play continues.
//!
//! T-spin bookkeeping per T7: `last_action_was_rotation` is set by a
//! successful rotation and cleared by any successful player move/drop;
//! `last_kick_index` mirrors the latest successful [`RotationAttempt`] and
//! resets to 0 on move/drop. Gravity descent is not a player action and
//! clears neither.

use serde::{Deserialize, Serialize};

use crate::actions::Action;
use crate::bag::Bag;
use crate::board::{self, Board};
use crate::event::GameEvent;
use crate::gravity;
use crate::hold::HoldSlot;
use crate::lock::LockTimer;
use crate::mode::{BlockOutBehavior, FinishReason, GarbageFeed, Goal, ModeConfig};
use crate::piece::{spawn_state, Piece, PieceState};
use crate::prng::Rng;
use crate::score::ScoreState;
use crate::srs::{self, RotateDir};
use crate::tspin::{self, TSpinKind};
use crate::versus::{push_garbage_rows, MAX_GARBAGE_PER_LAND};

/// Number of upcoming pieces exposed by [`Game::snapshot`] (T3 keeps deeper
/// peeks available through [`Game::peek_next`], up to 6 for the app).
pub const NEXT_PREVIEW: usize = 5;

/// Seed-stream salt for the Survival feed's garbage hole columns (T12):
/// keeps the hole stream independent of both the 7-bag's `Rng::new(seed)`
/// stream and `StartBoard::build`'s buried-garbage stream (a different
/// salt constant, `mode::BURIED_GARBAGE_SALT`).
const FEED_HOLE_SALT: u64 = 0x51F0_7D3A_9C6B_4E21;

/// Survival garbage-feed runtime state (T12); `Some` only when
/// `config.garbage_feed` is `Some`. Queue events derive from game ticks
/// alone; rows touch the board only when they land on a lock.
struct FeedState {
    /// The feed rule set this game was configured with.
    rules: GarbageFeed,
    /// Absolute game tick on which the next garbage row queues.
    next_queue_tick: u64,
    /// Queued rows not yet landed on a lock (versus-style landing, cap
    /// [`MAX_GARBAGE_PER_LAND`] rows per lock, surplus trickles).
    pending: u32,
    /// Independent splitmix64 stream for hole columns: exactly one draw per
    /// *landing* batch (hole constant within a batch, like versus), never
    /// from the bag's stream — bag draws stay untouched.
    hole_rng: Rng,
}

/// Everything T11/T13/T18 need to render a full frame from scratch —
/// including right after a restart, where no events have been seen yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameSnapshot {
    /// Settled cells.
    pub board: Board,
    /// Active piece placement (`None` only after the game froze via
    /// `game_over` or `GoalReached`; a `TimeUp` freeze keeps the piece).
    pub active: Option<PieceState>,
    /// Ghost landing row for the active piece (`None` with no active piece).
    pub ghost_row: Option<i32>,
    /// Parked hold piece, if any.
    pub hold: Option<Piece>,
    /// True when the active piece already spent its one hold press.
    pub hold_used: bool,
    /// Next `NEXT_PREVIEW` pieces from the bag, deal order first.
    pub next: Vec<Piece>,
    /// Running score total.
    pub score: u64,
    /// Current level (≥ 1).
    pub level: u32,
    /// Total lines cleared this game.
    pub lines: u32,
    /// PRD combo count (0 = chain inactive).
    pub combo: u32,
    /// Back-to-back armed.
    pub b2b: bool,
    /// Block-out happened; the game is frozen.
    pub game_over: bool,
}

/// Deterministic game facade owning one full ruleset run. `Game` itself is
/// not `Clone`/`PartialEq` (the bag holds internal state); use
/// [`Game::snapshot`] for comparable, serializable state.
pub struct Game {
    board: Board,
    bag: Bag,
    active: Option<PieceState>,
    hold: HoldSlot,
    lock_timer: LockTimer,
    score: ScoreState,
    lines: u32,
    level: u32,
    gravity_elapsed: u32,
    pending_drop: u64,
    last_action_was_rotation: bool,
    last_kick_index: u8,
    game_over: bool,
    /// Active rule set (T2); `new` runs [`ModeConfig::default`] (Marathon).
    config: ModeConfig,
    /// Logical frames consumed since the game went live; incremented once per
    /// [`Game::tick`] while the game is live (never while frozen).
    ticks: u64,
    /// Terminal reason once the game froze via goal/clock; block-out sets it
    /// to `TopOut` alongside `game_over`. Kept out of `GameSnapshot` (wire
    /// stability); read through [`Game::finished_reason`].
    finished: Option<FinishReason>,
    /// Survival garbage feed (T12); `Some` only under a `garbage_feed`
    /// config. Not part of `GameSnapshot` (wire stability).
    feed: Option<FeedState>,
}

impl Game {
    /// Fresh Marathon game from `seed`: the first bag piece is already
    /// spawned. Identical to `with_config(seed, &ModeConfig::default())`.
    pub fn new(seed: u64) -> Self {
        Self::with_config(seed, &ModeConfig::default())
    }

    /// Fresh game from `seed` under `config`: the start board (if any) is
    /// installed, `level` starts at `start_level` (gravity applies from
    /// tick 0) and goal/clock/level-curve rules follow the config. The first
    /// piece is already spawned, exactly like [`Game::new`].
    pub fn with_config(seed: u64, config: &ModeConfig) -> Self {
        let board = match config.start_board {
            Some(start) => start.build(seed),
            None => Board::new(),
        };
        let feed = config.garbage_feed.map(|rules| FeedState {
            next_queue_tick: rules.interval_at(0),
            rules,
            pending: 0,
            hole_rng: Rng::new(seed ^ FEED_HOLE_SALT),
        });
        let mut g = Self {
            board,
            bag: Bag::new(seed),
            active: None,
            hold: HoldSlot::new(),
            lock_timer: LockTimer::new(),
            score: ScoreState::new(),
            lines: 0,
            level: config.start_level.max(1),
            gravity_elapsed: 0,
            pending_drop: 0,
            last_action_was_rotation: false,
            last_kick_index: 0,
            game_over: false,
            config: config.clone(),
            ticks: 0,
            finished: None,
            feed,
        };
        let first = g.bag.next();
        g.spawn(first, &mut Vec::new());
        g
    }

    /// Advance one logical frame (60 Hz): gravity, then lock-delay bookkeeping.
    /// Emits at most one lock (+ its events) per frame. Once the game is
    /// frozen (top-out, goal or clock) every call is a no-op returning no
    /// events, and the tick clock stops.
    pub fn tick(&mut self) -> Vec<GameEvent> {
        let mut ev = Vec::new();
        if self.finished.is_some() {
            return ev;
        }
        self.ticks += 1;
        // Survival feed (T12): one row queues whenever this live tick reaches
        // the scheduled queue tick; the next wait is the interval in force
        // *at* this tick (`interval_at`), i.e. decay applies from the first
        // queue event on/after each 1800-tick window boundary. Queued rows
        // only touch the board when they land on a lock.
        if let Some(feed) = self.feed.as_mut() {
            if self.ticks >= feed.next_queue_tick {
                feed.pending += 1;
                feed.next_queue_tick = self.ticks + feed.rules.interval_at(self.ticks);
            }
        }
        self.gravity_elapsed += 1;
        if self.gravity_elapsed >= gravity::interval_for(self.level) {
            self.gravity_elapsed = 0;
            if let Some(mut ps) = self.active {
                ps.row += 1;
                if !self.board.collides(&ps) {
                    self.active = Some(ps);
                }
            }
        }
        if let Some(ps) = self.active {
            if self.is_grounded(ps) {
                self.lock_timer.on_grounded();
            } else {
                self.lock_timer.on_ungrounded();
            }
        }
        if self.lock_timer.tick() {
            self.lock_and_spawn(&mut ev);
        }
        // Clock expiry wins only if this same frame did not already finish
        // the game (top-out or goal take precedence).
        if self.finished.is_none() {
            if let Some(limit) = self.config.clock_ticks {
                if self.ticks >= limit {
                    self.finished = Some(FinishReason::TimeUp);
                    ev.push(GameEvent::TimeUp { tick: self.ticks });
                }
            }
        }
        ev
    }

    /// Inject one discrete player action. Failed moves/rotations and
    /// rejected double holds are silent no-ops; hard drop locks in the same
    /// call, as does the lock-delay force-lock after 15 resets (T6).
    pub fn apply(&mut self, action: Action) -> Vec<GameEvent> {
        let mut ev = Vec::new();
        if self.finished.is_some() {
            return ev;
        }
        let Some(active) = self.active else {
            return ev;
        };
        match action {
            Action::MoveLeft | Action::MoveRight => {
                let mut target = active;
                target.col += if action == Action::MoveLeft { -1 } else { 1 };
                if self.board.collides(&target) {
                    return ev;
                }
                self.active = Some(target);
                self.last_action_was_rotation = false;
                self.last_kick_index = 0;
                if !self.lock_timer.reset_on_success() {
                    self.lock_and_spawn(&mut ev);
                }
            }
            Action::RotateCw | Action::RotateCcw | Action::Rotate180 => {
                let dir = match action {
                    Action::RotateCw => RotateDir::Clockwise,
                    Action::RotateCcw => RotateDir::CounterClockwise,
                    _ => RotateDir::HalfTurn,
                };
                let Some(attempt) = srs::try_rotate(&self.board, &active, dir) else {
                    return ev;
                };
                self.active = Some(attempt.state);
                self.last_action_was_rotation = true;
                self.last_kick_index = attempt.kick_index;
                if !self.lock_timer.reset_on_success() {
                    self.lock_and_spawn(&mut ev);
                }
            }
            Action::SoftDrop => {
                let mut target = active;
                target.row += 1;
                if self.board.collides(&target) {
                    return ev;
                }
                self.active = Some(target);
                self.pending_drop += 1;
                self.last_action_was_rotation = false;
                self.last_kick_index = 0;
            }
            Action::HardDrop => {
                let ghost = board::ghost_row(&self.board, &active);
                let cells = ghost.saturating_sub(active.row).max(0) as u64;
                let mut target = active;
                target.row = ghost;
                self.active = Some(target);
                self.pending_drop += 2 * cells;
                self.last_action_was_rotation = false;
                self.last_kick_index = 0;
                self.lock_and_spawn(&mut ev);
            }
            Action::Hold => {
                let bag_head = self.bag.peek(1)[0];
                let Some(swap) = self.hold.try_hold(active.piece, bag_head) else {
                    return ev;
                };
                if swap.consumed_bag {
                    self.bag.next();
                }
                ev.push(GameEvent::HoldPerformed {
                    stored: swap.stored,
                    incoming: swap.incoming,
                    from_bag: swap.consumed_bag,
                });
                self.spawn(swap.incoming, &mut ev);
            }
        }
        ev
    }

    /// Full render state for a frame drawn from scratch.
    pub fn snapshot(&self) -> GameSnapshot {
        self.snapshot_with_next(NEXT_PREVIEW)
    }

    /// Snapshot variant exposing the requested number of upcoming pieces
    /// (clamped to 6, the queue depth the app contract covers).
    pub fn snapshot_with_next(&self, next_len: usize) -> GameSnapshot {
        GameSnapshot {
            board: self.board.clone(),
            active: self.active,
            ghost_row: self.active.map(|ps| board::ghost_row(&self.board, &ps)),
            hold: self.hold.slot(),
            hold_used: !self.hold.can_hold(),
            next: self.bag.peek(next_len.min(6)),
            score: self.score.total,
            level: self.level,
            lines: self.lines,
            combo: self.score.combo.saturating_sub(1),
            b2b: self.score.b2b,
            game_over: self.game_over,
        }
    }

    /// Upcoming queue without consuming (T3 `peek`, cap 6, deal order first).
    pub fn peek_next(&self, n: usize) -> Vec<Piece> {
        self.bag.peek(n.min(6))
    }

    /// Logical frames consumed while the game was live — one per effective
    /// [`Game::tick`] call (T2). Stays constant once the game is frozen.
    pub fn tick_count(&self) -> u64 {
        self.ticks
    }

    /// `Some` once the game is frozen, naming *why* (T2). `TopOut` always
    /// coincides with the snapshot's `game_over` flag; `GoalReached` and
    /// `TimeUp` freeze the game with `game_over == false` (wire stability:
    /// this getter, not the snapshot, carries the terminal reason).
    pub fn finished_reason(&self) -> Option<FinishReason> {
        self.finished
    }

    /// Rows of the settled stack that still contain at least one
    /// [`Piece::Garbage`] cell (T2; Dig metric, used by `Goal::GarbageCleared`
    /// and the T13 HUD).
    pub fn garbage_rows_left(&self) -> usize {
        (0..board::ROWS)
            .filter(|&r| (0..board::COLS).any(|c| self.board.get(r, c) == Some(Piece::Garbage)))
            .count()
    }

    /// Feed-queued garbage rows not yet landed on a lock (T12 Survival;
    /// always 0 without `garbage_feed`). Rows land on the player's next
    /// lock, at most [`crate::versus::MAX_GARBAGE_PER_LAND`] per lock with
    /// the surplus trickling over later locks.
    pub fn pending_garbage(&self) -> u32 {
        self.feed.as_ref().map_or(0, |feed| feed.pending)
    }

    /// Game ticks until the feed queues its *next* garbage row, or `None`
    /// without `garbage_feed` (T12 Survival). Semantics: the wait for the
    /// next queue event only — rows already queued and waiting for a lock
    /// live in [`Game::pending_garbage`] and do not affect it. While the
    /// game is frozen the tick clock stops, so this value freezes in place.
    pub fn ticks_to_next_row(&self) -> Option<u64> {
        self.feed
            .as_ref()
            .map(|feed| feed.next_queue_tick.saturating_sub(self.ticks))
    }

    /// T24 versus contract exception (crate-internal only; not part of the
    /// frozen public `Game` API): replace the settled stack with `board`
    /// after [`crate::versus`] has pushed garbage rows underneath it.
    /// `top_out` marks garbage that ran past the ceiling; overlap with the
    /// active piece is treated the same way. Either condition ends the game
    /// exactly like a block-out (`active` cleared, `game_over` set) and
    /// returns `false`; otherwise the board is installed and `true` is
    /// returned.
    pub(crate) fn install_board(&mut self, board: Board, top_out: bool) -> bool {
        let overlap = !top_out && self.active.is_some_and(|ps| board.collides(&ps));
        self.board = board;
        if top_out || overlap {
            self.active = None;
            self.game_over = true;
            self.finished = Some(FinishReason::TopOut);
            return false;
        }
        true
    }

    fn is_grounded(&self, ps: PieceState) -> bool {
        let mut down = ps;
        down.row += 1;
        self.board.collides(&down)
    }

    /// Place `piece` at its spawn state. Re-arms lock timer, gravity counter
    /// and T-spin history. The hold flag is *not* cleared here: a hold-swap
    /// keeps it armed for the swapped-in piece; only the lock path
    /// ([`Game::lock_and_spawn`]) re-arms it.
    ///
    /// On a spawn collision the config's [`BlockOutBehavior`] decides
    /// (T14 wired `WipeAndContinue`):
    /// - [`BlockOutBehavior::End`]: block-out — `active` cleared,
    ///   `game_over` set, `finished == TopOut`, `GameEvent::GameOver`
    ///   emitted (unchanged Marathon behavior).
    /// - [`BlockOutBehavior::WipeAndContinue`] (Zen): the **whole stack** is
    ///   wiped (owner decision — not just the rows above the piece's
    ///   clearance), any pending feed garbage is reset defensively (the Zen
    ///   catalogue carries no feed), and the colliding piece is re-spawned
    ///   at its spawn state — a spawn state always fits an empty board.
    ///   `GameEvent::StackWiped` is emitted immediately before the
    ///   re-spawn's `PieceSpawned`. `game_over`/`finished` stay unset and
    ///   score/lines/level **and** combo/B2B keep their values: the wipe is
    ///   not a lock, so it never scores, never clears (no `LineCleared` /
    ///   `PerfectClear`), and never resets chains.
    fn spawn(&mut self, piece: Piece, ev: &mut Vec<GameEvent>) {
        self.lock_timer = LockTimer::new();
        self.gravity_elapsed = 0;
        self.last_action_was_rotation = false;
        self.last_kick_index = 0;
        let st = spawn_state(piece);
        if self.board.collides(&st) && self.config.on_block_out == BlockOutBehavior::WipeAndContinue
        {
            // Zen wipe (T14): clear everything, keep every counter, and
            // re-spawn the blocked piece — it always fits a cleared board.
            self.board = Board::new();
            if let Some(feed) = self.feed.as_mut() {
                feed.pending = 0;
            }
            ev.push(GameEvent::StackWiped { tick: self.ticks });
            self.active = Some(st);
            ev.push(GameEvent::PieceSpawned { piece, state: st });
        } else if self.board.collides(&st) {
            self.active = None;
            self.game_over = true;
            self.finished = Some(FinishReason::TopOut);
            ev.push(GameEvent::GameOver);
        } else {
            self.active = Some(st);
            ev.push(GameEvent::PieceSpawned { piece, state: st });
        }
    }

    /// Merge the active piece, score the lock, then spawn the next piece.
    fn lock_and_spawn(&mut self, ev: &mut Vec<GameEvent>) {
        let Some(state) = self.active.take() else {
            return;
        };
        let tspin = tspin::detect_tspin(
            &self.board,
            &state,
            self.last_action_was_rotation,
            self.last_kick_index,
        );
        self.board.merge(&state);
        let lines = self.board.clear_full_rows();
        let empty_after = self.board.is_empty();
        let prev_combo = self.score.combo.saturating_sub(1);
        let delta = self
            .score
            .on_lock(lines, tspin, self.pending_drop, self.level, empty_after);
        self.pending_drop = 0;
        ev.push(GameEvent::PieceLocked {
            piece: state.piece,
            state,
        });
        if tspin != TSpinKind::None {
            ev.push(GameEvent::TSpinDetected { kind: tspin });
        }
        if lines > 0 {
            ev.push(GameEvent::LineCleared { lines });
        }
        if delta.points > 0 {
            ev.push(GameEvent::ScoreChanged {
                total: self.score.total,
                delta: delta.points,
            });
        }
        if delta.combo_now != prev_combo {
            ev.push(GameEvent::ComboChanged { n: delta.combo_now });
        }
        if empty_after {
            ev.push(GameEvent::PerfectClear);
        }
        self.lines += lines as u32;
        if self.config.levels_advance {
            let level = gravity::level_for(self.lines);
            if level > self.level {
                self.level = level;
                ev.push(GameEvent::LevelUp { level });
            }
        }
        // Survival feed landing (T12): this lock lands the queued garbage
        // batch — same settle point as versus's garbage (after this lock's
        // merge/clear/scoring). Here it runs *before* the next spawn: a
        // stack buried up into the spawn area then ends the game through
        // `spawn`'s own block-out (one `GameOver`), which is equivalent to
        // versus's post-spawn overlap top-out. At most
        // `MAX_GARBAGE_PER_LAND` rows land per lock (surplus trickles over
        // later locks); one hole column per landing batch, drawn from the
        // feed's independent stream. A push overflow tops out exactly like
        // a block-out and outranks the goal check below: the terminal is
        // always the single `GameOver`, `finished == TopOut`.
        if let Some(feed) = self.feed.as_mut() {
            if feed.pending > 0 {
                let rows = feed.pending.min(MAX_GARBAGE_PER_LAND);
                feed.pending -= rows;
                let hole = feed.hole_rng.next_below(board::COLS as u64) as usize;
                let (pushed, overflow) = push_garbage_rows(&self.board, rows as usize, hole);
                self.board = pushed;
                if overflow {
                    self.active = None;
                    self.game_over = true;
                    self.finished = Some(FinishReason::TopOut);
                    ev.push(GameEvent::GameOver);
                    return;
                }
            }
        }
        // Terminal goal: evaluated after this lock's line clear (T2/T3).
        // Freezes like game-over — no next piece spawns.
        if let Some(goal) = self.config.goal {
            let met = match goal {
                Goal::Lines(n) => self.lines >= n,
                Goal::GarbageCleared => self.garbage_rows_left() == 0,
            };
            if met {
                self.finished = Some(FinishReason::GoalReached);
                ev.push(GameEvent::GoalReached { tick: self.ticks });
                return;
            }
        }
        let next = self.bag.next();
        self.hold.end_piece();
        self.spawn(next, ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{COLS, ROWS};
    use crate::mode::BlockOutBehavior;
    use crate::piece::Rotation;

    fn state(piece: Piece, rot: Rotation, row: i32, col: i32) -> PieceState {
        PieceState {
            piece,
            rot,
            row,
            col,
        }
    }

    #[test]
    fn fresh_snapshot_is_complete() {
        let seed = 42;
        let g = Game::new(seed);
        let s = g.snapshot();
        assert_eq!(s.board, Board::new());
        assert!(s.active.is_some(), "first piece must already be active");
        assert_eq!(s.active.unwrap().rot, Rotation::Spawn);
        assert_eq!(s.active.unwrap().row, 0);
        // `new` already dealt the first piece; the peek queue follows it.
        let mut upcoming = Bag::new(seed).peek(NEXT_PREVIEW + 1);
        let first = upcoming.remove(0);
        assert_eq!(s.active.unwrap().piece, first);
        assert_eq!(s.next, upcoming);
        assert_eq!(
            s.ghost_row,
            Some(board::ghost_row(&s.board, s.active.as_ref().unwrap()))
        );
        assert_eq!(s.hold, None);
        assert!(!s.hold_used);
        assert_eq!(s.score, 0);
        assert_eq!(s.level, 1);
        assert_eq!(s.lines, 0);
        assert_eq!(s.combo, 0);
        assert!(!s.b2b);
        assert!(!s.game_over);
    }

    #[test]
    fn replay_determinism_same_seed_and_log() {
        let run = |seed: u64| {
            let mut g = Game::new(seed);
            let mut events = Vec::new();
            let mut snaps = Vec::new();
            for _ in 0..15 {
                events.extend(g.apply(Action::MoveLeft));
                events.extend(g.apply(Action::RotateCw));
                events.extend(g.apply(Action::SoftDrop));
                for _ in 0..2 {
                    events.extend(g.tick());
                }
                snaps.push(g.snapshot());
                events.extend(g.apply(Action::MoveRight));
                events.extend(g.apply(Action::RotateCcw));
                for _ in 0..2 {
                    events.extend(g.tick());
                }
                events.extend(g.apply(Action::HardDrop));
                events.extend(g.tick());
                snaps.push(g.snapshot());
            }
            (events, snaps)
        };
        let (e1, s1) = run(123);
        let (e2, s2) = run(123);
        assert_eq!(e1, e2, "event streams must match");
        assert_eq!(s1, s2, "snapshots must match");
        assert_eq!(s1.last().unwrap(), s2.last().unwrap());
        let (_, s3) = run(999);
        assert_ne!(
            s1.last().unwrap().board,
            s3.last().unwrap().board,
            "different seeds must diverge"
        );
    }

    /// Hand-computed marathon (all scoring at level 1; the 11th line raises
    /// the level only *after* that lock is scored; any line clear keeps the
    /// combo chain alive, only a no-clear lock resets it):
    /// 1. Tetris + PC, hard drop 18 cells: 800*1 + 36 + 3500 = 4336 (chain 1, b2b armed)
    /// 2. Single breaks B2B, chain 2 (combo n=1): 100*1 + 50*1*1 + 36 = 186 -> 4522
    ///    (board reset between steps keeps the score model step-local)
    /// 3. TSD via 180 into slot, lock-delay only, chain 3 (n=2): 1200*1 + 50*2*1 = 1300 -> 5822
    /// 4. Tetris under B2B, chain 4 (n=3): (800 + 400) + 50*3*1 + 36 = 1386 -> 7208 (LevelUp 2)
    /// 5. No-clear O lock resets combo: drop 40 -> 7248
    #[test]
    fn marathon_exact_score_tetris_tsd_combo() {
        let mut g = Game::new(7);
        let mut all: Vec<GameEvent> = Vec::new();

        // Step 1: Tetris + perfect clear.
        for r in 18..=21 {
            for c in 0..COLS - 1 {
                g.board.set(r, c, Some(Piece::Z));
            }
        }
        g.active = Some(state(Piece::I, Rotation::Cw, 0, 7));
        all.extend(g.apply(Action::HardDrop));
        assert_eq!(g.score.total, 4336);
        assert!(g.score.b2b);
        assert!(all.contains(&GameEvent::LineCleared { lines: 4 }));
        assert!(all.contains(&GameEvent::PerfectClear));

        // Step 2: plain single breaks B2B and continues the chain.
        g.board = Board::new();
        for c in 0..COLS - 1 {
            g.board.set(ROWS - 1, c, Some(Piece::Z));
        }
        g.active = Some(state(Piece::I, Rotation::Cw, 0, 7));
        all.extend(g.apply(Action::HardDrop));
        assert_eq!(g.score.total, 4522);
        assert!(!g.score.b2b);

        // Step 3: T-spin double, entered by 180 (last action = rotation).
        g.board = Board::new();
        g.board.set(19, 6, Some(Piece::J));
        for c in 0..4 {
            g.board.set(20, c, Some(Piece::J));
        }
        for c in 0..=4 {
            g.board.set(21, c, Some(Piece::J));
        }
        for c in 7..COLS {
            g.board.set(20, c, Some(Piece::J));
        }
        for c in 6..COLS {
            g.board.set(21, c, Some(Piece::J));
        }
        g.active = Some(state(Piece::T, Rotation::Spawn, 19, 4));
        let ev = g.apply(Action::Rotate180);
        assert!(ev.is_empty());
        assert!(g.last_action_was_rotation);
        for _ in 0..30 {
            all.extend(g.tick());
        }
        assert!(all.contains(&GameEvent::TSpinDetected {
            kind: TSpinKind::Full
        }));
        assert!(all.contains(&GameEvent::LineCleared { lines: 2 }));
        assert!(all.contains(&GameEvent::ComboChanged { n: 2 }));
        assert_eq!(g.score.total, 5822);
        assert!(g.score.b2b);
        assert_eq!(g.lines, 7);

        // Step 4: B2B Tetris, combo 3, 11th line raises the level.
        // One stray cell below the well keeps the board from going empty.
        g.board = Board::new();
        g.board.set(15, 0, Some(Piece::O));
        for r in 18..=21 {
            for c in 0..COLS - 1 {
                g.board.set(r, c, Some(Piece::Z));
            }
        }
        g.active = Some(state(Piece::I, Rotation::Cw, 0, 7));
        all.extend(g.apply(Action::HardDrop));
        assert!(all.contains(&GameEvent::ComboChanged { n: 3 }));
        assert!(all.contains(&GameEvent::LevelUp { level: 2 }));
        assert_eq!(g.score.total, 7208);
        assert_eq!(g.level, 2);
        assert_eq!(g.lines, 11);

        // Step 5: no-clear lock resets the combo chain.
        g.board = Board::new();
        g.active = Some(spawn_state(Piece::O));
        all.extend(g.apply(Action::HardDrop));
        assert!(all.contains(&GameEvent::ComboChanged { n: 0 }));
        assert_eq!(g.score.total, 7248);

        let s = g.snapshot();
        assert_eq!(s.score, 7248);
        assert_eq!(s.level, 2);
        assert_eq!(s.lines, 11);
        assert_eq!(s.combo, 0);
        assert!(s.b2b);
        assert!(!s.game_over);
        assert_eq!(s.board.get(ROWS - 1, 4), Some(Piece::O));
        assert_eq!(s.board.get(ROWS - 2, 5), Some(Piece::O));
        assert!(!all.contains(&GameEvent::GameOver));
    }

    #[test]
    fn blockout_spawns_gameover_then_no_ops() {
        let mut g = Game::new(3);
        // Column stack up to row 3 in the O spawn columns: an O hard-dropped
        // from spawn locks at rows 1..=2, so any next spawn collides.
        for r in 3..ROWS {
            g.board.set(r, 4, Some(Piece::S));
            g.board.set(r, 5, Some(Piece::S));
        }
        g.active = Some(spawn_state(Piece::O));
        let ev = g.apply(Action::HardDrop);
        assert!(ev.contains(&GameEvent::PieceLocked {
            piece: Piece::O,
            state: state(Piece::O, Rotation::Spawn, 1, 4),
        }));
        assert!(ev.contains(&GameEvent::GameOver));
        assert!(!ev.contains(&GameEvent::PieceSpawned {
            piece: Piece::O,
            state: state(Piece::O, Rotation::Spawn, 1, 4),
        }));
        assert!(g.game_over);

        assert!(g.tick().is_empty());
        assert!(g.apply(Action::MoveLeft).is_empty());
        assert!(g.apply(Action::HardDrop).is_empty());
        let s = g.snapshot();
        assert!(s.game_over);
        assert!(s.active.is_none());
    }

    #[test]
    fn hold_performs_swap_and_one_press_per_piece() {
        let mut g = Game::new(11);
        let bag_head = g.bag.peek(1)[0];
        let active = g.active.unwrap().piece;

        let ev = g.apply(Action::Hold);
        assert!(ev.contains(&GameEvent::HoldPerformed {
            stored: None,
            incoming: bag_head,
            from_bag: true,
        }));
        assert_eq!(g.active.unwrap().piece, bag_head);
        assert_eq!(g.hold.slot(), Some(active));
        assert!(!g.hold.can_hold());
        assert!(g.snapshot().hold_used);

        // Second press on the same piece is rejected silently.
        assert!(g.apply(Action::Hold).is_empty());

        // Locking the swapped piece re-arms hold for the next spawn.
        g.apply(Action::HardDrop);
        let ev = g.apply(Action::Hold);
        let stored = ev.iter().find_map(|e| match e {
            GameEvent::HoldPerformed { stored, .. } => Some(*stored),
            _ => None,
        });
        assert_eq!(stored, Some(Some(active)));
    }

    #[test]
    fn drop_points_accumulate_and_land_in_one_lock_event() {
        let mut g = Game::new(5);
        g.active = Some(spawn_state(Piece::O));
        for _ in 0..3 {
            assert!(g.apply(Action::SoftDrop).is_empty());
        }
        assert_eq!(g.active.unwrap().row, 3);
        let ev = g.apply(Action::HardDrop);
        // 3 soft rows (3 pts) + 17 hard rows (34 pts), merged same lock.
        let delta = ev.iter().find_map(|e| match e {
            GameEvent::ScoreChanged { delta, .. } => Some(*delta),
            _ => None,
        });
        assert_eq!(delta, Some(37));
        assert_eq!(g.score.total, 37);
    }

    #[test]
    fn mid_air_manipulation_spam_never_locks_falling_piece() {
        // Regression (playtest): 16+ moves/rotations before ever touching
        // the stack exhausted the 15-reset move-lockout and force-locked the
        // piece mid-air, leaving blocks stuck above empty columns.
        let mut g = Game::new(0xC0FFEE);
        let first = g.snapshot().active.expect("fresh spawn").piece;
        for round in 0..24 {
            for action in [
                Action::MoveLeft,
                Action::RotateCw,
                Action::MoveRight,
                Action::RotateCcw,
                Action::Rotate180,
                Action::MoveRight,
                Action::Rotate180,
            ] {
                assert!(
                    g.apply(action).is_empty(),
                    "round {round} action {action:?} must not lock an airborne piece"
                );
            }
        }
        let snap = g.snapshot();
        assert!(!snap.game_over);
        assert_eq!(snap.active.expect("piece still falling").piece, first);
        // Grounded locking still works: hard drop locks normally.
        let ev = g.apply(Action::HardDrop);
        assert!(ev
            .iter()
            .any(|e| matches!(e, GameEvent::PieceLocked { .. })));
    }

    // ------------------------------------------------------------------
    // T2: mode config, tick clock, terminal semantics
    // ------------------------------------------------------------------

    /// Scripted play log used by the config-comparison tests: exercises
    /// every action variant plus gravity ticks for many pieces.
    fn scripted_log(
        g: &mut Game,
        out_events: &mut Vec<GameEvent>,
        out_snaps: &mut Vec<GameSnapshot>,
    ) {
        for piece_idx in 0..12u64 {
            if piece_idx % 4 == 0 {
                out_events.extend(g.apply(Action::Hold));
            }
            out_events.extend(g.apply(Action::RotateCw));
            for _ in 0..(piece_idx % 5) {
                out_events.extend(g.apply(Action::MoveLeft));
            }
            out_events.extend(g.apply(Action::SoftDrop));
            out_events.extend(g.apply(Action::HardDrop));
            for _ in 0..7 {
                out_events.extend(g.tick());
            }
            out_snaps.push(g.snapshot());
        }
    }

    #[test]
    fn default_config_is_bit_identical_to_new() {
        for seed in [1u64, 31337, 20261001] {
            let mut a = Game::new(seed);
            let mut b = Game::with_config(seed, &ModeConfig::default());
            let (mut ea, mut eb) = (Vec::new(), Vec::new());
            let (mut sa, mut sb) = (Vec::new(), Vec::new());
            scripted_log(&mut a, &mut ea, &mut sa);
            scripted_log(&mut b, &mut eb, &mut sb);
            assert_eq!(ea, eb, "seed {seed}: default config event stream diverged");
            assert_eq!(sa, sb, "seed {seed}: default config snapshots diverged");
            // Same-seed default games serialize byte-identically.
            for (x, y) in sa.iter().zip(sb.iter()) {
                assert_eq!(
                    bincode::serialize(x).unwrap(),
                    bincode::serialize(y).unwrap()
                );
            }
            assert_eq!(a.tick_count(), b.tick_count());
            assert_eq!(a.finished_reason(), b.finished_reason());
        }
    }

    #[test]
    fn tick_count_counts_ticks() {
        let mut g = Game::new(1);
        assert_eq!(g.tick_count(), 0);
        for i in 1..=50 {
            g.tick();
            assert_eq!(g.tick_count(), i);
        }
    }

    #[test]
    fn fixed_level_config_never_emits_levelup() {
        let config = ModeConfig {
            levels_advance: false,
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(31337, &config);
        let mut events = Vec::new();
        // Three forced tetrises = 12 lines; Marathon would be at level 2+.
        for _ in 0..3 {
            g.board = Board::new();
            for r in 18..=21 {
                for c in 0..COLS - 1 {
                    g.board.set(r, c, Some(Piece::Z));
                }
            }
            g.active = Some(state(Piece::I, Rotation::Cw, 0, 7));
            events.extend(g.apply(Action::HardDrop));
        }
        assert_eq!(g.snapshot().lines, 12);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GameEvent::LevelUp { .. })),
            "levels_advance=false must never emit LevelUp"
        );
        assert_eq!(g.snapshot().level, 1);
    }

    #[test]
    fn fixed_start_level_pins_gravity() {
        // levels_advance=false at level 5: gravity must be interval_for(5)
        // = 12 ticks per row, from the very first tick, and never change
        // even as lines clear.
        let config = ModeConfig {
            start_level: 5,
            levels_advance: false,
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(42, &config);
        assert_eq!(g.snapshot().level, 5);
        let interval = gravity::interval_for(5);
        let start_row = g.snapshot().active.unwrap().row;
        for _ in 0..interval - 1 {
            g.tick();
            assert_eq!(g.snapshot().active.unwrap().row, start_row);
        }
        g.tick();
        assert_eq!(g.snapshot().active.unwrap().row, start_row + 1);
        assert_eq!(g.snapshot().level, 5);
    }

    #[test]
    fn lines_goal_emits_goal_reached_once_and_freezes() {
        let config = ModeConfig {
            goal: Some(Goal::Lines(4)),
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(7, &config);
        // Build a 9-wide, 4-tall stack with the I-piece well open.
        for r in 18..=21 {
            for c in 0..COLS - 1 {
                g.board.set(r, c, Some(Piece::Z));
            }
        }
        g.active = Some(state(Piece::I, Rotation::Cw, 0, 7));
        let ev = g.apply(Action::HardDrop);
        assert!(ev.contains(&GameEvent::LineCleared { lines: 4 }));
        assert!(
            ev.contains(&GameEvent::GoalReached {
                tick: g.tick_count()
            }),
            "expected GoalReached in {ev:?}"
        );
        // Frozen: like game-over, every later call is a no-op.
        assert_eq!(g.finished_reason(), Some(FinishReason::GoalReached));
        let s = g.snapshot();
        assert!(!s.game_over, "goal finish is not a top-out");
        assert!(s.active.is_none());
        for _ in 0..50 {
            assert!(g.tick().is_empty());
            assert!(g.apply(Action::HardDrop).is_empty());
        }
        // Exactly once, even across the freeze.
        let mut count = 0;
        let mut h = Game::with_config(7, &config);
        for r in 18..=21 {
            for c in 0..COLS - 1 {
                h.board.set(r, c, Some(Piece::Z));
            }
        }
        h.active = Some(state(Piece::I, Rotation::Cw, 0, 7));
        count += h
            .apply(Action::HardDrop)
            .iter()
            .filter(|e| matches!(e, GameEvent::GoalReached { .. }))
            .count();
        for _ in 0..200 {
            count += h
                .tick()
                .iter()
                .filter(|e| matches!(e, GameEvent::GoalReached { .. }))
                .count();
        }
        assert_eq!(count, 1);
    }

    #[test]
    fn clock_timeup_fires_at_exact_tick_without_pieces() {
        let config = ModeConfig {
            clock_ticks: Some(100),
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(99, &config);
        let mut events = Vec::new();
        for _ in 0..99 {
            events.extend(g.tick());
        }
        assert!(
            !events.iter().any(|e| matches!(e, GameEvent::TimeUp { .. })),
            "TimeUp fired before tick 100"
        );
        assert_eq!(g.finished_reason(), None);
        events.extend(g.tick());
        assert!(
            events.contains(&GameEvent::TimeUp { tick: 100 }),
            "expected TimeUp at tick 100, got tail {events:?}"
        );
        assert_eq!(g.tick_count(), 100);
        assert_eq!(g.finished_reason(), Some(FinishReason::TimeUp));
        // Zero pieces played: nothing was ever locked or cleared.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GameEvent::PieceLocked { .. })),
            "clock test must not lock pieces"
        );
        // Frozen after TimeUp.
        assert!(!g.snapshot().game_over);
        for _ in 0..300 {
            assert!(g.tick().is_empty());
        }
        assert_eq!(g.tick_count(), 100);
    }

    #[test]
    fn topout_records_topout_finish_reason() {
        let mut g = Game::new(3);
        assert_eq!(g.finished_reason(), None);
        for r in 3..ROWS {
            g.board.set(r, 4, Some(Piece::S));
            g.board.set(r, 5, Some(Piece::S));
        }
        g.active = Some(spawn_state(Piece::O));
        let ev = g.apply(Action::HardDrop);
        assert!(ev.contains(&GameEvent::GameOver));
        assert_eq!(g.finished_reason(), Some(FinishReason::TopOut));
    }

    // ------------------------------------------------------------------
    // T14: Zen — wipe-and-continue block-out
    // ------------------------------------------------------------------

    /// Zen catalogue shape (the app's `modes::mode_config(Zen)` core twin):
    /// fixed level 1, no goal, no clock, block-outs wipe the stack.
    fn zen_config() -> ModeConfig {
        ModeConfig {
            start_level: 1,
            levels_advance: false,
            goal: None,
            clock_ticks: None,
            start_board: None,
            on_block_out: BlockOutBehavior::WipeAndContinue,
            garbage_feed: None,
        }
    }

    /// Arm a guaranteed spawn collision for the *next* piece: every spawn
    /// state includes cell `(1, 4)`, so a single settled cell there blocks
    /// any dealt piece.
    fn arm_spawn_collision(g: &mut Game) {
        g.board.set(1, 4, Some(Piece::Z));
    }

    #[test]
    fn t14_blockout_wipes_the_stack_and_play_continues() {
        // Every spawn state covers cell (1, 4): the test's arming trick.
        for piece in Piece::ALL {
            let mut probe = Board::new();
            probe.set(1, 4, Some(Piece::Z));
            assert!(
                probe.collides(&spawn_state(piece)),
                "{piece:?} must collide with the armed spawn cell"
            );
        }

        let mut g = Game::with_config(3, &zen_config());
        arm_spawn_collision(&mut g);
        let ev = g.apply(Action::HardDrop);
        assert!(
            !ev.contains(&GameEvent::GameOver),
            "WipeAndContinue must never end the game: {ev:?}"
        );
        assert!(
            !g.snapshot().game_over,
            "Zen must never set the game_over flag"
        );
        assert_eq!(g.finished_reason(), None, "a wipe is not a terminal");

        // Event order: StackWiped immediately before the re-spawn's
        // PieceSpawned, and never a line-clear/PF for the wipe itself.
        let wiped = ev
            .iter()
            .position(|e| matches!(e, GameEvent::StackWiped { .. }))
            .expect("StackWiped emitted");
        assert!(
            matches!(ev.get(wiped + 1), Some(GameEvent::PieceSpawned { .. })),
            "StackWiped directly precedes PieceSpawned: {ev:?}"
        );
        assert!(
            !ev.iter()
                .any(|e| matches!(e, GameEvent::LineCleared { .. } | GameEvent::PerfectClear)),
            "a wipe is not a line clear: {ev:?}"
        );

        let after = g.snapshot();
        assert!(
            after.board.is_empty(),
            "the whole stack is wiped: {:?}",
            after.board
        );
        assert!(after.active.is_some(), "the colliding piece re-spawns");
        assert_eq!(after.active.unwrap().row, 0);

        // Play continues: further actions and ticks behave normally.
        let mut later = Vec::new();
        for _ in 0..240 {
            later.extend(g.apply(Action::HardDrop));
            later.extend(g.tick());
        }
        assert!(!later.contains(&GameEvent::GameOver), "still no game over");
        assert_eq!(g.finished_reason(), None);
        assert!(g.snapshot().active.is_some());
    }

    #[test]
    fn t14_wipe_keeps_score_lines_level_and_chains() {
        // The hold path blocks the *next* spawn without any lock, so the
        // wipe's "keeps all counters" rule is observed unpolluted by lock
        // scoring. Combo/B2B decision (T14): the wipe keeps them as-is —
        // chain resets belong to lock rules only.
        let mut g = Game::with_config(3, &zen_config());
        g.score.total = 4321;
        g.score.b2b = true;
        g.score.combo = 4;
        g.lines = 37;
        arm_spawn_collision(&mut g);
        let ev = g.apply(Action::Hold);
        assert!(
            ev.iter()
                .any(|e| matches!(e, GameEvent::HoldPerformed { .. })),
            "hold swap happened: {ev:?}"
        );
        assert!(
            ev.iter().any(|e| matches!(e, GameEvent::StackWiped { .. })),
            "the swapped-in piece blocked out and wiped: {ev:?}"
        );
        assert!(!ev.contains(&GameEvent::GameOver));
        let s = g.snapshot();
        assert_eq!(s.score, 4321, "wipe never scores or clears score");
        assert_eq!(s.lines, 37, "wipe adds no lines");
        assert_eq!(s.level, 1);
        assert_eq!(s.combo, 3, "PRD combo = chain - 1, kept untouched");
        assert!(s.b2b, "back-to-back is kept across a wipe");
        assert!(s.board.is_empty());
        assert!(!s.game_over);
        assert_eq!(g.finished_reason(), None);
    }

    #[test]
    fn t14_multiple_wipes_keep_the_game_alive() {
        let mut g = Game::with_config(9, &zen_config());
        let mut wipes = 0;
        let mut events_all: Vec<GameEvent> = Vec::new();
        for _ in 0..3 {
            arm_spawn_collision(&mut g);
            let ev = g.apply(Action::HardDrop);
            wipes += ev
                .iter()
                .filter(|e| matches!(e, GameEvent::StackWiped { .. }))
                .count();
            assert!(!ev.contains(&GameEvent::GameOver));
            events_all.extend(ev);
            // Between wipes, normal play resumes: pieces lock again.
            for _ in 0..30 {
                events_all.extend(g.apply(Action::HardDrop));
                events_all.extend(g.tick());
            }
            assert!(!g.snapshot().board.is_empty(), "play refilled the board");
        }
        assert_eq!(wipes, 3, "repeated wipes all fired");
        assert!(!events_all.contains(&GameEvent::GameOver));
        assert!(g.snapshot().active.is_some());
        assert_eq!(g.finished_reason(), None);
    }

    #[test]
    fn t14_tick_count_keeps_counting_through_wipes() {
        let mut g = Game::with_config(5, &zen_config());
        for _ in 0..10 {
            assert_eq!(g.tick().len(), 0);
        }
        let before = g.tick_count();
        assert_eq!(before, 10);
        arm_spawn_collision(&mut g);
        g.apply(Action::HardDrop);
        // The wipe did not freeze anything: ticks keep advancing.
        for i in 1..=20 {
            g.tick();
            assert_eq!(g.tick_count(), before + i);
        }
        assert_eq!(g.finished_reason(), None);
    }

    #[test]
    fn t14_replay_determinism_across_wipes() {
        let run = |seed: u64| {
            let mut g = Game::with_config(seed, &zen_config());
            let mut events = Vec::new();
            let mut snaps = Vec::new();
            for cycle in 0..4 {
                for _ in 0..25 {
                    events.extend(g.apply(Action::MoveLeft));
                    events.extend(g.apply(Action::RotateCw));
                    events.extend(g.apply(Action::SoftDrop));
                    events.extend(g.tick());
                    snaps.push(g.snapshot());
                    events.extend(g.apply(Action::MoveRight));
                    events.extend(g.apply(Action::HardDrop));
                    snaps.push(g.snapshot());
                }
                if cycle % 2 == 1 {
                    arm_spawn_collision(&mut g);
                    events.extend(g.apply(Action::HardDrop));
                    snaps.push(g.snapshot());
                }
            }
            (events, snaps)
        };
        let (e1, s1) = run(4242);
        let (e2, s2) = run(4242);
        assert_eq!(e1, e2, "event streams must match across wipes");
        assert_eq!(s1, s2, "snapshots must match across wipes");
        assert!(
            e1.iter()
                .filter(|e| matches!(e, GameEvent::StackWiped { .. }))
                .count()
                >= 2,
            "the scripted replay must actually wipe (twice)"
        );
    }

    #[test]
    fn t14_end_behavior_parity_same_script_ends_the_game() {
        // The identical forced-block-out script under `End` (the Marathon
        // default) must still terminate exactly as before: one GameOver,
        // frozen game, board NOT wiped.
        let mut g = Game::with_config(3, &ModeConfig::default());
        arm_spawn_collision(&mut g);
        let ev = g.apply(Action::HardDrop);
        assert!(
            ev.contains(&GameEvent::GameOver),
            "End config keeps the block-out: {ev:?}"
        );
        assert!(
            !ev.iter().any(|e| matches!(e, GameEvent::StackWiped { .. })),
            "End never wipes: {ev:?}"
        );
        assert!(g.snapshot().game_over);
        assert_eq!(g.finished_reason(), Some(FinishReason::TopOut));
        assert!(!g.snapshot().board.is_empty(), "End keeps the stack");
        let ticks = g.tick_count();
        for _ in 0..60 {
            assert!(g.tick().is_empty());
            assert!(g.apply(Action::HardDrop).is_empty());
        }
        assert_eq!(g.tick_count(), ticks, "frozen after block-out (End)");
    }

    #[test]
    fn t14_wipe_resets_pending_feed_garbage_defensively() {
        // WipeAndContinue + a feed is not a shipped combination (Zen has no
        // feed); the wipe still clears the queue defensively. The hold path
        // blocks the spawn without a lock, so no feed landing confuses the
        // assertion.
        let config = ModeConfig {
            on_block_out: BlockOutBehavior::WipeAndContinue,
            garbage_feed: Some(GarbageFeed::default()),
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(11, &config);
        for _ in 0..300 {
            g.tick();
            pin_airborne(&mut g);
        }
        assert_eq!(g.pending_garbage(), 1, "one row queued, none landed");
        arm_spawn_collision(&mut g);
        let ev = g.apply(Action::Hold);
        assert!(
            ev.iter().any(|e| matches!(e, GameEvent::StackWiped { .. })),
            "hold re-spawn must block out and wipe: {ev:?}"
        );
        assert_eq!(g.pending_garbage(), 0, "the wipe reset the queue");
        assert!(g.snapshot().board.is_empty());
        assert!(!g.snapshot().game_over);
    }

    #[test]
    fn garbage_rows_left_counts_garbage_rows() {
        let mut g = Game::new(5);
        assert_eq!(g.garbage_rows_left(), 0);
        g.board.set(21, 0, Some(Piece::Garbage));
        g.board.set(21, 1, Some(Piece::Garbage));
        g.board.set(20, 4, Some(Piece::Garbage));
        g.board.set(19, 9, Some(Piece::Z)); // real piece, must not count
        assert_eq!(g.garbage_rows_left(), 2);
        g.board.set(20, 4, None);
        assert_eq!(g.garbage_rows_left(), 1);
    }

    #[test]
    fn garbage_cleared_goal_fires_when_last_garbage_row_goes() {
        let config = ModeConfig {
            goal: Some(Goal::GarbageCleared),
            ..ModeConfig::default()
        };
        // Not final: one garbage row survives the lock -> no goal yet.
        let mut g = Game::with_config(8, &config);
        g.board = Board::new();
        g.board.set(15, 3, Some(Piece::Garbage));
        g.board.set(21, 5, Some(Piece::Garbage));
        for c in [0usize, 1, 2, 3, 4, 6, 7, 8, 9] {
            g.board.set(21, c, Some(Piece::Z));
        }
        assert_eq!(g.garbage_rows_left(), 2);
        g.active = Some(state(Piece::I, Rotation::Cw, 0, 3)); // cells in col 5
        let ev = g.apply(Action::HardDrop);
        assert!(ev.contains(&GameEvent::LineCleared { lines: 1 }), "{ev:?}");
        assert_eq!(g.garbage_rows_left(), 1);
        assert!(
            !ev.iter()
                .any(|e| matches!(e, GameEvent::GoalReached { .. })),
            "goal must not fire while garbage remains: {ev:?}"
        );
        assert_eq!(g.finished_reason(), None);
        assert!(g.snapshot().active.is_some(), "game still playable");

        // Final: the last garbage row goes at this lock -> GoalReached.
        let mut h = Game::with_config(8, &config);
        h.board = Board::new();
        h.board.set(21, 5, Some(Piece::Garbage));
        for c in [0usize, 1, 2, 3, 4, 6, 7, 8, 9] {
            h.board.set(21, c, Some(Piece::Z));
        }
        h.active = Some(state(Piece::I, Rotation::Cw, 0, 3));
        let ev = h.apply(Action::HardDrop);
        assert!(
            ev.contains(&GameEvent::GoalReached {
                tick: h.tick_count()
            }),
            "expected GoalReached, got {ev:?}"
        );
        assert_eq!(h.garbage_rows_left(), 0);
        assert_eq!(h.finished_reason(), Some(FinishReason::GoalReached));
        assert!(!h.snapshot().game_over);
        for _ in 0..50 {
            assert!(h.tick().is_empty());
        }
    }

    #[test]
    fn buried_start_board_plus_garbage_goal_wires_dig_shape() {
        // Config-only smoke test of the Dig shape T3/T4 build on: a buried
        // board starts live with the goal armed and no early terminal state.
        let config = ModeConfig {
            start_level: 1,
            levels_advance: false,
            goal: Some(Goal::GarbageCleared),
            start_board: Some(crate::mode::StartBoard::BuriedGarbage { rows: 10 }),
            ..ModeConfig::default()
        };
        let g = Game::with_config(3, &config);
        assert_eq!(g.garbage_rows_left(), 10);
        assert!(g.snapshot().active.is_some());
        assert_eq!(g.finished_reason(), None);
    }

    #[test]
    fn snapshot_bytes_identical_for_same_seed_default_games() {
        let run = |seed: u64| {
            let mut g = Game::new(seed);
            let mut bytes = Vec::new();
            for _ in 0..60 {
                g.apply(Action::MoveLeft);
                g.apply(Action::RotateCw);
                g.apply(Action::HardDrop);
                for _ in 0..10 {
                    g.tick();
                }
                bytes.extend(bincode::serialize(&g.snapshot()).unwrap());
            }
            bytes
        };
        assert_eq!(run(12345), run(12345));
    }

    // ------------------------------------------------------------------
    // T12: Survival garbage feed
    // ------------------------------------------------------------------

    /// Feed config helper: Marathon rules plus a garbage feed.
    fn feed_config(feed: GarbageFeed) -> ModeConfig {
        ModeConfig {
            garbage_feed: Some(feed),
            ..ModeConfig::default()
        }
    }

    /// Count rows made entirely of [`Piece::Garbage`] cells except a single
    /// hole. Feeds (and only feeds) create such rows via
    /// [`crate::versus::push_garbage_rows`], so this detects landed feed
    /// garbage even after later clears/shifts.
    fn garbage_hole_row_count(board: &Board) -> usize {
        (0..ROWS)
            .filter(|&r| {
                let cells: Vec<Option<Piece>> =
                    (0..COLS).map(|c| board.get(r, c)).collect::<Vec<_>>();
                cells.iter().filter(|c| **c == Some(Piece::Garbage)).count() == COLS - 1
                    && cells.iter().filter(|c| c.is_none()).count() == 1
            })
            .count()
    }

    /// Hole column of a full-except-one-hole row (panics otherwise).
    fn hole_of_row(board: &Board, r: usize) -> usize {
        (0..COLS)
            .find(|&c| board.get(r, c).is_none())
            .expect("row with exactly one hole")
    }

    /// Keep the game live with an empty stack and a mid-air active piece
    /// (unit tests may poke private state): no locks, no game-overs. Used
    /// to observe the pure queue schedule.
    fn pin_airborne(g: &mut Game) {
        g.board = Board::new();
        g.active = Some(spawn_state(Piece::O));
    }

    #[test]
    fn feed_first_row_queues_at_interval_and_lands_on_next_lock() {
        let mut g = Game::with_config(7, &feed_config(GarbageFeed::default()));
        assert_eq!(g.pending_garbage(), 0);
        assert_eq!(g.ticks_to_next_row(), Some(300));

        for _ in 0..299 {
            g.tick();
        }
        assert_eq!(
            g.pending_garbage(),
            0,
            "row 1 must not queue before tick 300"
        );
        assert_eq!(g.ticks_to_next_row(), Some(1));
        g.tick(); // tick 300
        assert_eq!(g.pending_garbage(), 1, "first row queues at tick 300");
        assert_eq!(g.ticks_to_next_row(), Some(300));
        assert_eq!(
            g.garbage_rows_left(),
            0,
            "queued rows must not touch the board"
        );

        // No lock yet — keep ticking; the row stays pending.
        for _ in 0..50 {
            g.tick();
        }
        assert_eq!(g.pending_garbage(), 1);
        assert_eq!(g.garbage_rows_left(), 0);

        // The first lock after tick 300 lands it.
        let ev = g.apply(Action::HardDrop);
        assert!(
            ev.iter()
                .any(|e| matches!(e, GameEvent::PieceLocked { .. })),
            "{ev:?}"
        );
        assert_eq!(
            g.pending_garbage(),
            0,
            "the pending batch lands on the lock"
        );
        assert_eq!(g.garbage_rows_left(), 1);
        let board = g.snapshot().board;
        assert_eq!(garbage_hole_row_count(&board), 1);
        hole_of_row(&board, ROWS - 1);
        assert_eq!(g.finished_reason(), None);
    }

    #[test]
    fn feed_ticks_to_next_row_none_without_feed() {
        let g = Game::new(7);
        assert_eq!(g.ticks_to_next_row(), None);
        assert_eq!(g.pending_garbage(), 0);
    }

    #[test]
    fn feed_queue_schedule_decays_per_window_to_floor() {
        // Pure queue schedule (T12): rows queue every `interval` game ticks,
        // where the wait committed at tick `from` is the interval in force
        // *there*: `max(60, 300 - 15 * (from / 1800))`. Window 0's 300
        // divides 1800, so the first post-decay row lands exactly on the
        // tick-1800 boundary at 285 spacing; later windows keep the
        // previously committed wait across the boundary (decay applies at
        // each queue event, once per fully elapsed window).
        let mut g = Game::with_config(31337, &feed_config(GarbageFeed::default()));
        let mut queue_ticks: Vec<u64> = Vec::new();
        let mut prev_next = g.ticks_to_next_row().expect("feed present");
        for _ in 0..40_000 {
            g.tick();
            pin_airborne(&mut g);
            let next = g.ticks_to_next_row().expect("feed present");
            if next > prev_next {
                queue_ticks.push(g.tick_count());
            }
            prev_next = next;
        }
        assert_eq!(
            &queue_ticks[..14],
            &[300, 600, 900, 1200, 1500, 1800, 2085, 2370, 2655, 2940, 3225, 3510, 3795, 4065],
            "queue ticks around the first two decay windows"
        );
        // decay_by applies exactly once per 1800-tick window: every wait
        // equals the interval in force at the tick its row queued.
        for (i, &e) in queue_ticks.iter().enumerate() {
            let from = if i == 0 { 0 } else { queue_ticks[i - 1] };
            let expect = 300u64.saturating_sub(15 * (from / 1800)).max(60);
            assert_eq!(e - from, expect, "queue {} spacing from tick {}", i, from);
        }
        // Floor: once rows arrive at window-16 pace, they are exactly 60
        // ticks apart forever.
        let from_floor: Vec<u64> = queue_ticks
            .iter()
            .copied()
            .filter(|&t| t >= 28_860)
            .collect();
        assert!(
            from_floor.len() > 100,
            "expected many floor-interval queues, got {}",
            from_floor.len()
        );
        let first_floor = *queue_ticks
            .iter()
            .find(|&&t| t >= 28_800)
            .expect("a queue at floor pace by tick 28 860");
        assert!(
            first_floor < 28_800 + 75,
            "floor pace started late: {first_floor}"
        );
        for pair in from_floor.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                60,
                "floor spacing violated at {}",
                pair[0]
            );
        }
        // Rows were never landed (airborne pin) ⇒ all queues accumulate.
        assert_eq!(g.pending_garbage() as usize, queue_ticks.len());
    }

    #[test]
    fn feed_cap_four_per_lock_trickles_surplus() {
        let feed = GarbageFeed {
            interval_ticks: 100,
            decay_ticks: 1_000_000,
            decay_by: 0,
            floor_ticks: 0,
        };
        let mut g = Game::with_config(7, &feed_config(feed));
        // Queue 6 rows without ever locking.
        for _ in 0..599 {
            g.tick();
            pin_airborne(&mut g);
        }
        // ... the 600th tick queues row 6 — and would be followed by a lock.
        g.tick();
        pin_airborne(&mut g);
        assert_eq!(g.pending_garbage(), 6);

        g.active = Some(spawn_state(Piece::T));
        let ev = g.apply(Action::HardDrop);
        assert!(
            ev.iter()
                .any(|e| matches!(e, GameEvent::PieceLocked { .. })),
            "{ev:?}"
        );
        assert_eq!(g.pending_garbage(), 2, "cap 4 lands, 2 stay queued");
        let board = g.snapshot().board;
        let holes: Vec<usize> = (0..ROWS)
            .filter(|&r| {
                (0..COLS)
                    .map(|c| board.get(r, c))
                    .filter(|c| *c == Some(Piece::Garbage))
                    .count()
                    == COLS - 1
                    && (0..COLS).filter(|&c| board.get(r, c).is_none()).count() == 1
            })
            .map(|r| hole_of_row(&board, r))
            .collect();
        assert_eq!(holes.len(), MAX_GARBAGE_PER_LAND as usize);
        assert!(
            holes.iter().all(|&h| h == holes[0]),
            "one hole column per landing batch: {holes:?}"
        );

        // The surplus trickles on the next lock.
        g.apply(Action::HardDrop);
        assert_eq!(g.pending_garbage(), 0);
        assert!(g.garbage_rows_left() >= 6);
    }

    #[test]
    fn feed_overflow_tops_out_and_freezes() {
        let feed = GarbageFeed {
            interval_ticks: 30,
            decay_ticks: 1_000_000,
            decay_by: 0,
            floor_ticks: 0,
        };
        let mut g = Game::with_config(11, &feed_config(feed));
        // Queue one row.
        for _ in 0..29 {
            g.tick();
        }
        assert_eq!(g.pending_garbage(), 0);
        g.tick(); // tick 30 queues the first row
        assert_eq!(g.pending_garbage(), 1);

        // Stack from row 1 down (column 0 open, so no row is clearable):
        // locking the hidden-row O stamps cells into row 0, and the landing
        // push then runs those cells past the ceiling. The next spawn never
        // happens — the overflow alone ends the game.
        for r in 1..ROWS {
            for c in 1..COLS {
                g.board.set(r, c, Some(Piece::Z));
            }
        }
        g.active = Some(PieceState {
            piece: Piece::O,
            rot: Rotation::Spawn,
            row: -1,
            col: 4,
        });
        let ev = g.apply(Action::HardDrop);
        assert_eq!(g.snapshot().lines, 0, "the lock must not clear anything");
        assert_eq!(
            ev.iter().filter(|e| **e == GameEvent::GameOver).count(),
            1,
            "exactly one GameOver: {ev:?}"
        );
        assert!(!ev
            .iter()
            .any(|e| matches!(e, GameEvent::PieceSpawned { .. })));
        assert_eq!(g.finished_reason(), Some(FinishReason::TopOut));
        let s = g.snapshot();
        assert!(s.game_over);
        assert!(s.active.is_none());
        assert_eq!(g.pending_garbage(), 0);
        for _ in 0..10 {
            assert!(g.tick().is_empty());
            assert!(g.apply(Action::HardDrop).is_empty());
        }
        assert_eq!(g.tick_count(), 30, "the game froze without further ticks");
    }

    #[test]
    fn feed_landing_topout_wins_over_goal_on_the_same_lock() {
        // The lock clears one line (meeting `Goal::Lines(1)`), and the same
        // lock's landing push overflows the ceiling. A top-out must win:
        // exactly one coherent terminal, `GameOver` + `TopOut`, no
        // `GoalReached`.
        let config = ModeConfig {
            goal: Some(Goal::Lines(1)),
            garbage_feed: Some(GarbageFeed {
                interval_ticks: 30,
                decay_ticks: 1_000_000,
                decay_by: 0,
                floor_ticks: 0,
            }),
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(3, &config);
        for _ in 0..60 {
            g.tick();
        }
        assert_eq!(g.pending_garbage(), 2, "rows queued at ticks 30 and 60");

        // Row 0: full except the column-5 corridor. Rows 1..=20: full
        // except columns 5 (corridor) and 9 (keeps them from completing
        // under the I). Row 21: full except the column-5 hole the vertical
        // I completes — a one-line clear. The shift after that clear moves
        // the old row 0 into row 1, so the 2-row landing push overflows.
        for c in 0..COLS {
            if c != 5 {
                g.board.set(0, c, Some(Piece::Z));
            }
        }
        for r in 1..(ROWS - 1) {
            for c in 0..COLS {
                if c != 5 && c != 9 {
                    g.board.set(r, c, Some(Piece::Z));
                }
            }
        }
        for c in 0..COLS {
            if c != 5 {
                g.board.set(ROWS - 1, c, Some(Piece::Z));
            }
        }
        g.active = Some(PieceState {
            piece: Piece::I,
            rot: Rotation::Cw,
            row: -3,
            col: 3,
        });
        let ev = g.apply(Action::HardDrop);
        assert_eq!(g.snapshot().lines, 1, "the I completes exactly row 21");
        assert_eq!(
            ev.iter().filter(|e| **e == GameEvent::GameOver).count(),
            1,
            "exactly one GameOver: {ev:?}"
        );
        assert!(
            !ev.iter()
                .any(|e| matches!(e, GameEvent::GoalReached { .. })),
            "top-out wins over the goal on the same lock: {ev:?}"
        );
        assert!(ev.contains(&GameEvent::LineCleared { lines: 1 }), "{ev:?}");
        assert_eq!(g.finished_reason(), Some(FinishReason::TopOut));
        assert!(g.snapshot().game_over);
        for _ in 0..10 {
            assert!(g.tick().is_empty());
            assert!(g.apply(Action::HardDrop).is_empty());
        }
    }

    #[test]
    fn feed_replay_is_deterministic() {
        // A fast feed (one row every 60 ticks, no decay) queues and lands
        // rows well before the scripted pile tops out. Same seed ⇒ identical
        // event streams, snapshots and queue countdowns — hole columns and
        // landing ticks are observable only through those, so equality there
        // proves hole/landing determinism.
        fn replay(
            seed: u64,
        ) -> (
            Vec<GameEvent>,
            Vec<GameSnapshot>,
            Vec<u32>,
            Vec<Option<u64>>,
        ) {
            let feed = GarbageFeed {
                interval_ticks: 60,
                decay_ticks: 1_000_000,
                decay_by: 0,
                floor_ticks: 60,
            };
            let mut g = Game::with_config(seed, &feed_config(feed));
            let mut events = Vec::new();
            let mut snaps = Vec::new();
            let mut pendings = Vec::new();
            let mut nexts = Vec::new();
            for round in 0..60u64 {
                if round % 3 == 0 {
                    events.extend(g.apply(Action::Hold));
                }
                events.extend(g.apply(Action::RotateCw));
                for _ in 0..(round % 4) {
                    events.extend(g.apply(Action::MoveLeft));
                }
                events.extend(g.apply(Action::HardDrop));
                for _ in 0..20 {
                    events.extend(g.tick());
                    if g.finished_reason().is_some() {
                        break;
                    }
                }
                pendings.push(g.pending_garbage());
                nexts.push(g.ticks_to_next_row());
                snaps.push(g.snapshot());
            }
            (events, snaps, pendings, nexts)
        }
        let a = replay(4242);
        let b = replay(4242);
        assert_eq!(a.0, b.0, "same-seed feed event streams diverged");
        assert_eq!(a.1, b.1, "same-seed feed snapshots diverged");
        assert_eq!(a.2, b.2, "same-seed pending_garbage diverged");
        assert_eq!(a.3, b.3, "same-seed ticks_to_next_row diverged");
        // The scripted run actually saw feed garbage land (non-vacuous).
        assert!(a
            .1
            .iter()
            .any(|s| s.board.get(ROWS - 1, 0) == Some(Piece::Garbage)
                || s.board.get(ROWS - 1, 9) == Some(Piece::Garbage)));
        // A different seed must diverge (hole stream follows the seed).
        let c = replay(4243);
        assert_ne!(a.0, c.0, "different seeds must diverge");
    }

    #[test]
    fn feed_never_disturbs_bag_draws() {
        // Same seed, no feed vs an aggressive feed config: the first six
        // peek_next draws must be identical (the feed's hole stream is
        // independent of the bag).
        let plain = Game::with_config(31337, &ModeConfig::default());
        let fed = Game::with_config(
            31337,
            &feed_config(GarbageFeed {
                interval_ticks: 10,
                decay_ticks: 100,
                decay_by: 5,
                floor_ticks: 10,
            }),
        );
        assert_eq!(plain.peek_next(6), fed.peek_next(6));
    }

    #[test]
    fn feed_default_config_game_identical_to_new() {
        // `garbage_feed: None` (every existing config) reproduces `Game::new`
        // bit-for-bit, including the new getters.
        for seed in [1u64, 31337, 20261001] {
            let mut a = Game::new(seed);
            let mut b = Game::with_config(seed, &ModeConfig::default());
            let (mut ea, mut eb) = (Vec::new(), Vec::new());
            let (mut sa, mut sb) = (Vec::new(), Vec::new());
            scripted_log(&mut a, &mut ea, &mut sa);
            scripted_log(&mut b, &mut eb, &mut sb);
            assert_eq!(ea, eb, "seed {seed}: no-feed event stream diverged");
            assert_eq!(sa, sb, "seed {seed}: no-feed snapshots diverged");
            assert_eq!(a.pending_garbage(), b.pending_garbage());
            assert_eq!(a.ticks_to_next_row(), b.ticks_to_next_row());
            assert_eq!(a.ticks_to_next_row(), None);
        }
    }

    // ------------------------------------------------------------------
    // T3: Dig goal mechanics
    // ------------------------------------------------------------------

    /// The locked-in Dig rule set (plan §T3): garbage-clears goal, fixed
    /// level 1, buried 10-row start board.
    fn dig_config() -> ModeConfig {
        ModeConfig {
            start_level: 1,
            levels_advance: false,
            goal: Some(Goal::GarbageCleared),
            start_board: Some(crate::mode::StartBoard::BuriedGarbage { rows: 10 }),
            ..ModeConfig::default()
        }
    }

    /// Dig well columns. The scripted driver drops O pieces here; the dump
    /// policy must keep every other cell out of these columns so each O
    /// falls to the band bottom.
    const DIG_WELL: [usize; 2] = [4, 5];
    /// Dumps must never occupy rows `0..SPAWN_CLEAR_ROWS` (spawn/rotation/
    /// move corridor for the next piece).
    const SPAWN_CLEAR_ROWS: usize = 3;
    /// Hard budget for the scripted Dig clear ("~2000 steps").
    const DIG_ACTION_BUDGET: usize = 2000;

    /// Hand-made Dig endgame: the bottom 10 rows are a full garbage band
    /// except for the 2-wide well at columns 4-5, so
    /// `garbage_rows_left() == 10` — the same starting metric as
    /// `StartBoard::BuriedGarbage { rows: 10 }` — while remaining clearable
    /// by the scripted driver below in five O drops.
    ///
    /// Why not drive a real `BuriedGarbage` board: with exactly one hole per
    /// row (T2's generator), every row's hole is only reachable while that
    /// row is the band top, and each vertical-I dig leaves a 3-cell debris
    /// column directly above the new band top — permanently blocking that
    /// column for all deeper digs. Clearing the ten rows therefore needs a
    /// hole sequence with no column reuse *and* somewhere legal to place
    /// the ~25 non-I pieces the 7-bag deals meanwhile (nowhere: every
    /// column is a future descent column). Such boards are only winnable by
    /// an active dig solver (T10's job), not by any simple scripted driver;
    /// `dig_goal_dig_config_tops_out_before_garbage_cleared` below exercises
    /// the real buried board's terminal path instead.
    fn paired_buried_board() -> Board {
        let mut b = Board::new();
        for r in (ROWS - 10)..ROWS {
            for c in 0..COLS {
                if !DIG_WELL.contains(&c) {
                    b.set(r, c, Some(Piece::Garbage));
                }
            }
        }
        b
    }

    /// Dig game: full Dig config, then the paired board installed (the
    /// test-friendlier hand-made-board path).
    fn dig_game(seed: u64) -> Game {
        let mut g = Game::with_config(seed, &dig_config());
        g.board = paired_buried_board();
        assert_eq!(g.garbage_rows_left(), 10);
        assert_eq!(g.finished_reason(), None);
        g
    }

    /// Rotate the active piece to `target` with one rotation action
    /// (0 = already there). False if the rotation did not land on target.
    fn rotate_active_to(g: &mut Game, target: Rotation) -> bool {
        let Some(active) = g.snapshot().active else {
            return false;
        };
        match (target.index() + 4 - active.rot.index()) % 4 {
            0 => {}
            1 => {
                g.apply(Action::RotateCw);
            }
            2 => {
                g.apply(Action::Rotate180);
            }
            _ => {
                g.apply(Action::RotateCcw);
            }
        }
        g.snapshot().active.map(|a| a.rot) == Some(target)
    }

    /// Walk the active piece to box column `col` one move at a time.
    fn move_active_to(g: &mut Game, col: i32) -> bool {
        for _ in 0..2 * COLS + 2 {
            let Some(active) = g.snapshot().active else {
                return false;
            };
            if active.col == col {
                return true;
            }
            g.apply(if active.col < col {
                Action::MoveRight
            } else {
                Action::MoveLeft
            });
        }
        g.snapshot().active.map(|a| a.col) == Some(col)
    }

    /// Absolute cells of `ps`, or `None` if any cell is off the board.
    fn cells_abs(ps: &PieceState) -> Option<Vec<(i32, i32)>> {
        ps.cells()
            .iter()
            .copied()
            .map(|(r, c)| {
                if r < 0 || r >= ROWS as i32 || c < 0 || c >= COLS as i32 {
                    None
                } else {
                    Some((r, c))
                }
            })
            .collect()
    }

    /// Dump placement for a non-dig piece: the ghost landing must stay out
    /// of the well columns and at or below row `SPAWN_CLEAR_ROWS`, choosing
    /// the placement that keeps the tallest involved column as low as
    /// possible (towers must never grow into the spawn corridor). Ties
    /// break on leftmost box column — fully deterministic.
    fn dump_placement(board: &Board, piece: Piece) -> Option<(Rotation, i32)> {
        let mut heights = [ROWS; COLS];
        for r in 0..ROWS {
            for (c, top) in heights.iter_mut().enumerate() {
                if board.get(r, c).is_some() {
                    *top = (*top).min(r);
                }
            }
        }
        let mut best: Option<(usize, i32, Rotation)> = None;
        for rot in [Rotation::Spawn, Rotation::Cw, Rotation::R180, Rotation::Ccw] {
            for boxcol in -3i32..(COLS as i32 + 2) {
                let ps = PieceState {
                    piece,
                    rot,
                    row: 0,
                    col: boxcol,
                };
                if board.collides(&ps) {
                    continue;
                }
                let ghost = board::ghost_row(board, &ps);
                let landed = PieceState { row: ghost, ..ps };
                let Some(cells) = cells_abs(&landed) else {
                    continue;
                };
                if !cells.iter().all(|&(r, c)| {
                    r as usize >= SPAWN_CLEAR_ROWS && !DIG_WELL.contains(&(c as usize))
                }) {
                    continue;
                }
                let mut hh = heights;
                for &(r, c) in &cells {
                    hh[c as usize] = hh[c as usize].min(r as usize);
                }
                let cand = (hh.iter().copied().min().unwrap_or(ROWS), boxcol, rot);
                if best.is_none_or(|b| cand.0 > b.0 || (cand.0 == b.0 && cand.1 < b.1)) {
                    best = Some(cand);
                }
            }
        }
        best.map(|(_, col, rot)| (rot, col))
    }

    struct DigRun {
        goal_reached: bool,
        steps: usize,
        events: Vec<GameEvent>,
        snaps: Vec<GameSnapshot>,
    }

    /// Scripted Dig driver using only the public `Game` API: drop every O
    /// into the well (retiring the bottom two garbage rows per drop), dump
    /// everything else with the balanced dump policy, tick twice per piece.
    /// Terminates when the game freezes (goal or top-out) or the action
    /// budget is exceeded (driver failure, not a rules failure).
    fn drive_dig(g: &mut Game) -> DigRun {
        let mut events = Vec::new();
        let mut snaps = Vec::new();
        let mut steps = 0usize;
        loop {
            snaps.push(g.snapshot());
            if g.finished_reason().is_some() {
                return DigRun {
                    goal_reached: g.finished_reason() == Some(FinishReason::GoalReached),
                    steps,
                    events,
                    snaps,
                };
            }
            assert!(
                steps < DIG_ACTION_BUDGET,
                "dig driver exceeded the step budget"
            );
            let snap = g.snapshot();
            let active = snap.active.expect("a live game has an active piece");
            if active.piece == Piece::O {
                assert!(
                    move_active_to(g, DIG_WELL[0] as i32),
                    "O must always be walkable to the well"
                );
                let before = g.garbage_rows_left();
                events.extend(g.apply(Action::HardDrop));
                steps += 1;
                assert_eq!(
                    g.garbage_rows_left() + 2,
                    before,
                    "each well drop must retire exactly two garbage rows (step {steps})"
                );
            } else {
                let (rot, boxcol) = dump_placement(&snap.board, active.piece)
                    .expect("a balanced dump placement must exist");
                assert!(rotate_active_to(g, rot), "dump rotation must land");
                assert!(move_active_to(g, boxcol), "dump walk must land");
                events.extend(g.apply(Action::HardDrop));
                steps += 1;
            }
            for _ in 0..2 {
                events.extend(g.tick());
                steps += 1;
            }
        }
    }

    /// T3 AC1: the scripted Dig clear fires `GoalReached` exactly once,
    /// freezes the game, and reports `FinishReason::GoalReached`.
    #[test]
    fn dig_goal_scripted_clear_fires_once_and_freezes() {
        let mut g = dig_game(1);
        let run = drive_dig(&mut g);
        assert!(run.goal_reached, "scripted Dig clear must reach the goal");
        assert!(
            run.steps < 200,
            "the scripted clear must terminate quickly, took {} steps",
            run.steps
        );

        let goals: Vec<u64> = run
            .events
            .iter()
            .filter_map(|e| match e {
                GameEvent::GoalReached { tick } => Some(*tick),
                _ => None,
            })
            .collect();
        assert_eq!(goals.len(), 1, "exactly one GoalReached, got {goals:?}");
        assert_eq!(goals[0], g.tick_count());
        assert!(
            goals[0] > 0,
            "the tick clock advances during a scripted run"
        );
        assert!(
            !run.events.contains(&GameEvent::GameOver),
            "a completed Dig must not emit GameOver"
        );
        assert!(
            !run.events
                .iter()
                .any(|e| matches!(e, GameEvent::LevelUp { .. })),
            "levels_advance=false must never emit LevelUp"
        );
        assert_eq!(g.finished_reason(), Some(FinishReason::GoalReached));
        assert_eq!(g.garbage_rows_left(), 0);
        let frozen = g.snapshot();
        assert!(!frozen.game_over, "a goal finish is not a top-out");
        assert!(frozen.active.is_none());
        assert_eq!(frozen.level, 1);
        assert_eq!(frozen.lines, 10, "the paired band is exactly 10 rows");

        // Frozen: every later call is a silent no-op (like game-over).
        for _ in 0..100 {
            assert!(g.tick().is_empty());
            assert!(g.apply(Action::HardDrop).is_empty());
            assert!(g.apply(Action::MoveLeft).is_empty());
            assert!(g.apply(Action::Hold).is_empty());
        }
        assert_eq!(g.tick_count(), goals[0], "the tick clock stops when frozen");
        assert_eq!(g.snapshot(), frozen);
    }

    /// T3 AC2: a block-out under the Dig config (before the garbage is
    /// cleared) ends the game as `TopOut` and `GoalReached` never appears.
    #[test]
    fn dig_goal_dig_config_tops_out_before_garbage_cleared() {
        // Real buried start board, garbage fully intact.
        let mut g = Game::with_config(5, &dig_config());
        assert_eq!(g.garbage_rows_left(), 10);
        // Blocking stack under the O spawn columns (rows 3..11, stopping
        // above the buried band at row 12): the hard drop locks the O on
        // rows 1-2 and the next spawn collides (same pattern as
        // `blockout_spawns_gameover_then_no_ops`).
        for r in 3..12 {
            g.board.set(r, 4, Some(Piece::Z));
            g.board.set(r, 5, Some(Piece::Z));
        }
        g.active = Some(spawn_state(Piece::O));
        let ev = g.apply(Action::HardDrop);
        assert!(
            ev.contains(&GameEvent::GameOver),
            "expected GameOver: {ev:?}"
        );
        assert_eq!(g.finished_reason(), Some(FinishReason::TopOut));
        assert!(g.snapshot().game_over);
        // The goal never fires on the top-out path, and the garbage is
        // untouched.
        let mut all = ev;
        for _ in 0..50 {
            all.extend(g.tick());
            all.extend(g.apply(Action::HardDrop));
        }
        assert!(
            !all.iter()
                .any(|e| matches!(e, GameEvent::GoalReached { .. })),
            "GoalReached must never appear after a Dig top-out"
        );
        assert_eq!(g.garbage_rows_left(), 10);
        assert_eq!(g.finished_reason(), Some(FinishReason::TopOut));
    }

    /// T3 AC3: same seed + same scripted action log for a full Dig run gives
    /// identical event streams and snapshots (and diverges across seeds).
    #[test]
    fn dig_goal_replay_is_deterministic() {
        let run = |seed: u64| {
            let mut g = dig_game(seed);
            let r = drive_dig(&mut g);
            (r.goal_reached, r.events, r.snaps, g.snapshot())
        };
        let (a_goal, a_events, a_snaps, a_final) = run(1);
        let (b_goal, b_events, b_snaps, b_final) = run(1);
        assert!(a_goal && b_goal);
        assert_eq!(a_events, b_events, "replay event streams diverged");
        assert_eq!(a_snaps, b_snaps, "replay snapshots diverged");
        assert_eq!(
            bincode::serialize(&a_final).unwrap(),
            bincode::serialize(&b_final).unwrap()
        );
        // A different seed deals a different bag ⇒ the dig log diverges.
        let (c_goal, c_events, _, c_final) = run(2);
        assert!(c_goal, "seed 2 must also complete the scripted Dig");
        assert_ne!(a_final.next, c_final.next, "different seeds must diverge");
        assert_ne!(a_events, c_events);
    }

    /// T3 AC4: the goal is evaluated *after* this lock's line clear — the
    /// final lock's event batch carries `LineCleared` strictly before
    /// `GoalReached`, and `GoalReached` ends the batch.
    #[test]
    fn dig_goal_fires_after_line_clear_in_the_same_batch() {
        let config = ModeConfig {
            goal: Some(Goal::GarbageCleared),
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(8, &config);
        g.board = Board::new();
        for r in 20..ROWS {
            for c in 0..COLS {
                if !DIG_WELL.contains(&c) {
                    g.board.set(r, c, Some(Piece::Garbage));
                }
            }
        }
        assert_eq!(g.garbage_rows_left(), 2);
        g.active = Some(spawn_state(Piece::O));
        let ev = g.apply(Action::HardDrop);
        let cleared = ev
            .iter()
            .position(|e| *e == GameEvent::LineCleared { lines: 2 })
            .expect("the well drop clears two rows");
        let goal = ev
            .iter()
            .position(|e| matches!(e, GameEvent::GoalReached { .. }))
            .expect("clearing the last garbage rows reaches the goal");
        assert!(
            cleared < goal,
            "LineCleared must precede GoalReached: {ev:?}"
        );
        assert_eq!(goal, ev.len() - 1, "GoalReached ends the batch: {ev:?}");
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, GameEvent::GoalReached { .. }))
                .count(),
            1
        );
        assert_eq!(g.finished_reason(), Some(FinishReason::GoalReached));
    }

    /// T3: retire semantics — a row counts as garbage while it holds at
    /// least one `Piece::Garbage` cell (a lone garbage cell on an otherwise
    /// empty row counts); completing the line that contains it retires it,
    /// and the surviving garbage rows shift down with the stack, still
    /// counted.
    #[test]
    fn garbage_rows_retire_on_clear_and_shift_with_the_stack() {
        let mut g = Game::new(9);
        g.board = Board::new();
        // Row 19: a lone garbage cell — a garbage row despite nine empties.
        g.board.set(19, 3, Some(Piece::Garbage));
        // Row 20: nine garbage cells with column 9 open. Row 21: nine plain
        // Z cells (not garbage) with column 9 open.
        for c in 0..COLS - 1 {
            g.board.set(20, c, Some(Piece::Garbage));
            g.board.set(21, c, Some(Piece::Z));
        }
        assert_eq!(g.garbage_rows_left(), 2);
        // A vertical I in column 9 drops to the floor and completes rows
        // 20-21 at once; the garbage row retires with its line clear.
        g.active = Some(state(Piece::I, Rotation::Ccw, 0, 8));
        let ev = g.apply(Action::HardDrop);
        assert!(ev.contains(&GameEvent::LineCleared { lines: 2 }), "{ev:?}");
        assert_eq!(g.garbage_rows_left(), 1);
        // The lone garbage cell shifted down two rows with the stack and is
        // still counted (row 19 -> row 21).
        assert_eq!(g.board.get(21, 3), Some(Piece::Garbage));
        assert_eq!(g.board.get(19, 3), None);
        assert_eq!(g.garbage_rows_left(), 1);
    }

    /// T3: the Sprint config (`Goal::Lines(40)`, fixed level 1) needs no
    /// further core work — the goal fires once at exactly 40 lines, with
    /// level pinned at 1 and no `LevelUp`, and freezes the game.
    #[test]
    fn sprint_config_completes_at_40_lines_fixed_level() {
        let config = ModeConfig {
            start_level: 1,
            levels_advance: false,
            goal: Some(Goal::Lines(40)),
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(7, &config);
        let mut events = Vec::new();
        for i in 0..10 {
            g.board = Board::new();
            for r in 18..=21 {
                for c in 0..COLS - 1 {
                    g.board.set(r, c, Some(Piece::Z));
                }
            }
            g.active = Some(state(Piece::I, Rotation::Cw, 0, 7));
            events.extend(g.apply(Action::HardDrop));
            let lines = g.snapshot().lines;
            assert_eq!(lines, 4 * (i + 1));
            let goals = events
                .iter()
                .filter(|e| matches!(e, GameEvent::GoalReached { .. }))
                .count();
            if i < 9 {
                assert_eq!(goals, 0, "goal fired early at {lines} lines");
            }
        }
        assert_eq!(g.snapshot().lines, 40);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, GameEvent::GoalReached { .. }))
                .count(),
            1
        );
        assert_eq!(g.finished_reason(), Some(FinishReason::GoalReached));
        assert_eq!(g.snapshot().level, 1);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GameEvent::LevelUp { .. })),
            "fixed-level Sprint must never emit LevelUp"
        );
        for _ in 0..50 {
            assert!(g.tick().is_empty());
            assert!(g.apply(Action::HardDrop).is_empty());
        }
    }
}
