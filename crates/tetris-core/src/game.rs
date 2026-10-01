//! `Game` facade: `new(seed)` + `tick()`/`apply(Action)` + `snapshot()` (T8).
//!
//! Pure deterministic 60 Hz loop composing Board/Bag/gravity/SRS/lock/hold/
//! score/tspin. No I/O, no wall-clock: a game is fully reproducible from its
//! seed plus the action log. `tick()` advances one logical frame; `apply()`
//! injects one discrete player action in the same frame. Both return the
//! [`GameEvent`]s the transition produced (empty when nothing observable
//! happened). Block-out at spawn ends the game; afterwards every call is a
//! no-op returning no events.
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
use crate::mode::{FinishReason, Goal, ModeConfig};
use crate::piece::{spawn_state, Piece, PieceState};
use crate::score::ScoreState;
use crate::srs::{self, RotateDir};
use crate::tspin::{self, TSpinKind};

/// Number of upcoming pieces exposed by [`Game::snapshot`] (T3 keeps deeper
/// peeks available through [`Game::peek_next`], up to 6 for the app).
pub const NEXT_PREVIEW: usize = 5;

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

    /// Place `piece` at its spawn state; block-out ends the game instead.
    /// Re-arms lock timer, gravity counter and T-spin history. The hold flag
    /// is *not* cleared here: a hold-swap keeps it armed for the swapped-in
    /// piece; only the lock path ([`Game::lock_and_spawn`]) re-arms it.
    ///
    /// `BlockOutBehavior::WipeAndContinue` is plumbed through `config` but
    /// behaves like `End` until T14 wires the Zen wipe here.
    fn spawn(&mut self, piece: Piece, ev: &mut Vec<GameEvent>) {
        self.lock_timer = LockTimer::new();
        self.gravity_elapsed = 0;
        self.last_action_was_rotation = false;
        self.last_kick_index = 0;
        let st = spawn_state(piece);
        if self.board.collides(&st) {
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

    #[test]
    fn wipe_and_continue_plumbs_as_end_for_now() {
        // T14 wires the actual wipe; until then the variant behaves like
        // End so the plumbing is exercised here.
        let config = ModeConfig {
            on_block_out: BlockOutBehavior::WipeAndContinue,
            ..ModeConfig::default()
        };
        let mut g = Game::with_config(3, &config);
        for r in 3..ROWS {
            g.board.set(r, 4, Some(Piece::S));
            g.board.set(r, 5, Some(Piece::S));
        }
        g.active = Some(spawn_state(Piece::O));
        let ev = g.apply(Action::HardDrop);
        assert!(ev.contains(&GameEvent::GameOver));
        assert!(g.snapshot().game_over);
        assert_eq!(g.finished_reason(), Some(FinishReason::TopOut));
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
