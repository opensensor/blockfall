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
    /// Active piece placement (`None` only after `game_over`).
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
}

impl Game {
    /// Fresh game from `seed`: the first bag piece is already spawned.
    pub fn new(seed: u64) -> Self {
        let mut g = Self {
            board: Board::new(),
            bag: Bag::new(seed),
            active: None,
            hold: HoldSlot::new(),
            lock_timer: LockTimer::new(),
            score: ScoreState::new(),
            lines: 0,
            level: 1,
            gravity_elapsed: 0,
            pending_drop: 0,
            last_action_was_rotation: false,
            last_kick_index: 0,
            game_over: false,
        };
        let first = g.bag.next();
        g.spawn(first, &mut Vec::new());
        g
    }

    /// Advance one logical frame (60 Hz): gravity, then lock-delay bookkeeping.
    /// Emits at most one lock (+ its events) per frame.
    pub fn tick(&mut self) -> Vec<GameEvent> {
        let mut ev = Vec::new();
        if self.game_over {
            return ev;
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
        ev
    }

    /// Inject one discrete player action. Failed moves/rotations and
    /// rejected double holds are silent no-ops; hard drop locks in the same
    /// call, as does the lock-delay force-lock after 15 resets (T6).
    pub fn apply(&mut self, action: Action) -> Vec<GameEvent> {
        let mut ev = Vec::new();
        if self.game_over {
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

    fn is_grounded(&self, ps: PieceState) -> bool {
        let mut down = ps;
        down.row += 1;
        self.board.collides(&down)
    }

    /// Place `piece` at its spawn state; block-out ends the game instead.
    /// Re-arms lock timer, gravity counter and T-spin history. The hold flag
    /// is *not* cleared here: a hold-swap keeps it armed for the swapped-in
    /// piece; only the lock path ([`Game::lock_and_spawn`]) re-arms it.
    fn spawn(&mut self, piece: Piece, ev: &mut Vec<GameEvent>) {
        self.lock_timer = LockTimer::new();
        self.gravity_elapsed = 0;
        self.last_action_was_rotation = false;
        self.last_kick_index = 0;
        let st = spawn_state(piece);
        if self.board.collides(&st) {
            self.active = None;
            self.game_over = true;
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
        let level = gravity::level_for(self.lines);
        if level > self.level {
            self.level = level;
            ev.push(GameEvent::LevelUp { level });
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
}
