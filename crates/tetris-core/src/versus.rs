//! `Match` — pure, deterministic 1v1 versus logic over two [`Game`]s (T24,
//! PRD §15 1v1 Versus).
//!
//! A match owns two independent [`Game`] instances (left/right) plus the
//! attack bookkeeping between them: line clears queue *garbage* on the
//! opponent, and each side's next lock pushes that garbage up from the
//! bottom of its own stack — one full-except-one-hole row per queued line.
//! The `Game`/`GameEvent`/`Action` contract stays frozen; versus composes
//! two games from the outside and only needs one crate-internal hook
//! inside `Game` to install the shifted stack
//! ([`Game::install_board`](crate::game::Game::install_board)), a T24
//! contract exception documented there.
//!
//! Determinism: everything derives from the single match seed — the two
//! per-side game seeds (left first, then right) and one [`Rng`] stream
//! (splitmix64, the same generator `tests/soak.rs` inlines) for garbage
//! hole columns. Given the seed and the action sequence, a match replays
//! identically.
//!
//! Rules (PRD §15 1v1 Versus):
//! - [`AttackRule::Garbage`]: clearing N lines (1..=4) queues N garbage on
//!   the opponent; each consecutive clear beyond the first (the running
//!   combo count after this lock) adds +1 more (guideline simple form).
//!   A queued batch lands when the receiving side locks its next piece:
//!   the whole batch is pushed in at once, with the hole column drawn per
//!   batch (constant within a batch, random across batches). If the push
//!   moves occupied cells past the ceiling, or the garbage rows would land
//!   on the side's active piece, that side tops out.
//! - [`AttackRule::Race { target_lines }`]: no garbage. Reaching
//!   `target_lines` *finishes* a side instead of winning: its board
//!   freezes (its `apply`/`tick` become no-ops) while the opponent keeps
//!   playing. Once **all** sides have finished, the winner is the one with
//!   the higher score, then level, then lines; a perfect tie goes to the
//!   first side to the target. Racing fast is never punished, but a fast
//!   finish alone never decides the match — the other side always gets to
//!   play out its race.
//! - Any top-out (block-out in normal play, or garbage overflow) hands the
//!   win to the opponent, even if that opponent already finished. Once a
//!   winner is set, [`Match::apply`] is a no-op returning an empty batch,
//!   and so is `apply`/`tick` for a side that already finished a Race.
//!
//! Event emission order inside one `apply`: `PieceLocked`, then
//! `GarbageReceived` (+ `PlayerTopOut`/`WinnerCrowned` if the garbage was
//! fatal), then the top-out pair for a normal-play block-out, then
//! `RaceTargetReached`/`WinnerCrowned`, and finally `GarbageSent` when
//! this lock queued an attack and the match is still open.

use serde::{Deserialize, Serialize};

use crate::actions::Action;
use crate::board::{Board, COLS, ROWS};
use crate::event::GameEvent;
use crate::game::{Game, GameSnapshot};
use crate::piece::Piece;
use crate::prng::Rng;

/// Default sprint distance for [`AttackRule::Race`] (PRD §15).
pub const DEFAULT_RACE_LINES: u32 = 40;

/// Most garbage rows a single lock can land on the receiver; the surplus of
/// a bigger queued attack waits for the receiver's next locks (keeps even
/// a huge burst playable instead of instantly burying a side).
pub const MAX_GARBAGE_PER_LAND: u32 = 4;

/// Which side of the match an action or event belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    /// Left player.
    Left,
    /// Right player.
    Right,
}

impl Side {
    /// The other side of the match.
    pub fn other(self) -> Self {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }

    fn index(self) -> usize {
        match self {
            Side::Left => 0,
            Side::Right => 1,
        }
    }
}

/// How locks damage (or count toward victory for) the sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttackRule {
    /// Line clears send garbage; top-out decides the match.
    Garbage,
    /// No garbage. Reaching `target_lines` finishes (freezes) that side;
    /// the match is decided once all sides have finished, by score, then
    /// level, then lines (perfect tie: first to the target).
    Race {
        /// Cumulative cleared lines that win the match.
        target_lines: u32,
    },
}

impl Default for AttackRule {
    /// [`AttackRule::Race`] at the sprint distance of 40 lines.
    fn default() -> Self {
        AttackRule::Race {
            target_lines: DEFAULT_RACE_LINES,
        }
    }
}

/// A single observable versus transition produced by [`Match::apply`],
/// re-exposing the match-level view of the underlying [`GameEvent`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchEvent {
    /// `side` locked a piece, clearing `lines` rows on its own board.
    PieceLocked {
        /// Side that locked.
        side: Side,
        /// Rows cleared by that lock (0 when nothing cleared).
        lines: u32,
    },
    /// `side`'s lock queued `lines` garbage on the opponent.
    GarbageSent {
        /// Side that sent.
        side: Side,
        /// Lines queued onto the opponent.
        lines: u32,
    },
    /// `side` received a garbage batch of `lines` rows, now pushed in.
    GarbageReceived {
        /// Side that received.
        side: Side,
        /// Rows pushed into this side's stack.
        lines: u32,
    },
    /// `side` topped out (block-out in normal play, or garbage overflow).
    PlayerTopOut {
        /// Side that died.
        side: Side,
    },
    /// `side` reached the race target (Race rule only): its board is
    /// frozen from here on; the match stays open until every side finishes.
    RaceTargetReached {
        /// Side that finished the target.
        side: Side,
    },
    /// `side` won; the match is frozen from here on.
    WinnerCrowned {
        /// Winning side.
        side: Side,
    },
}

/// Comparable, serializable match state (mirrors [`Game::snapshot`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchSnapshot {
    /// Left side's full game state.
    pub left: GameSnapshot,
    /// Right side's full game state.
    pub right: GameSnapshot,
    /// Queued garbage per side, `(left, right)`.
    pub pending: (u32, u32),
    /// Current winner (`None` while the match is open).
    pub winner: Option<Side>,
    /// Whether each side has finished a Race target (frozen; `(left,
    /// right)`, always `(false, false)` under Garbage).
    pub finished: (bool, bool),
    /// Active attack rule.
    pub rule: AttackRule,
}

/// Deterministic 1v1 match composing two independent [`Game`] instances.
pub struct Match {
    /// Left player's game.
    pub left: Game,
    /// Right player's game.
    pub right: Game,
    rule: AttackRule,
    winner: Option<Side>,
    pending: [u32; 2],
    /// Finish order per side (`None` = still racing): the 1-based index in
    /// which the side reached the Race target; always `None` under Garbage.
    finished: [Option<u32>; 2],
    /// Number of sides that have finished the Race target.
    finish_clock: u32,
    rng: Rng,
}

impl Match {
    /// Fresh match: per-side game seeds and the garbage-hole stream all
    /// derive from `seed` via one [`Rng`] stream (left game first, then
    /// right, then hole draws), so replays are exact.
    pub fn new(seed: u64, rule: AttackRule) -> Self {
        let mut rng = Rng::new(seed);
        let left_seed = rng.next_u64();
        let right_seed = rng.next_u64();
        Self {
            left: Game::new(left_seed),
            right: Game::new(right_seed),
            rule,
            winner: None,
            pending: [0, 0],
            finished: [None, None],
            finish_clock: 0,
            rng,
        }
    }

    /// The active attack rule.
    pub fn rule(&self) -> AttackRule {
        self.rule
    }

    /// Current winner, `None` while the match is open.
    pub fn winner(&self) -> Option<Side> {
        self.winner
    }

    /// Garbage lines currently queued on `side` (Garbage rule; always 0
    /// under the Race rule).
    pub fn pending_attack(&self, side: Side) -> u32 {
        self.pending[side.index()]
    }

    /// Apply one action for `side` to its game and run the versus rules:
    /// received garbage lands first on this side's lock (pushing its stack
    /// up), then this lock's attack (if any) queues on the opponent. After
    /// a winner is set — or once this side has finished a Race target —
    /// this is a no-op returning an empty batch.
    pub fn apply(&mut self, side: Side, action: Action) -> Vec<MatchEvent> {
        if self.winner.is_some() || self.finished[side.index()].is_some() {
            return Vec::new();
        }
        let events = match side {
            Side::Left => self.left.apply(action),
            Side::Right => self.right.apply(action),
        };
        self.settle(side, events)
    }

    /// Advance one logical frame (gravity + lock-delay bookkeeping) for
    /// `side` and run the same versus rules [`Match::apply`] runs after any
    /// lock: a lock that happens on the lock-delay timer clears lines,
    /// queues garbage and can top out exactly like an action-driven lock.
    /// No-op once a winner is set or this side has finished a Race target.
    pub fn tick(&mut self, side: Side) -> Vec<MatchEvent> {
        if self.winner.is_some() || self.finished[side.index()].is_some() {
            return Vec::new();
        }
        let events = match side {
            Side::Left => self.left.tick(),
            Side::Right => self.right.tick(),
        };
        self.settle(side, events)
    }

    /// Shared versus bookkeeping after one game transition (action or tick):
    /// map the [`GameEvent`]s to [`MatchEvent`]s, land queued garbage, crown
    /// winners. Same emission order as documented at the module level.
    fn settle(&mut self, side: Side, events: Vec<GameEvent>) -> Vec<MatchEvent> {
        let mut out = Vec::new();
        let mut locked = false;
        let mut cleared = 0u32;
        let mut top_out = false;
        for e in events {
            match e {
                GameEvent::PieceLocked { .. } => locked = true,
                GameEvent::LineCleared { lines } => cleared = lines as u32,
                GameEvent::GameOver => top_out = true,
                _ => {}
            }
        }
        if locked {
            out.push(MatchEvent::PieceLocked {
                side,
                lines: cleared,
            });
        }

        // Garbage rule: a queued batch lands first, before this lock's own
        // attack is computed — the receiving side may die on it. At most
        // [`MAX_GARBAGE_PER_LAND`] rows land per lock (guideline attack
        // cap): a big accumulated attack arrives as a steady trickle over
        // the receiver's next locks instead of burying them instantly, so
        // even a huge burst stays playable.
        let i = side.index();
        if self.rule == AttackRule::Garbage && locked && !top_out && self.pending[i] > 0 {
            let rows = self.pending[i].min(MAX_GARBAGE_PER_LAND);
            self.pending[i] -= rows;
            out.push(MatchEvent::GarbageReceived { side, lines: rows });
            let hole = self.rng.next_below(COLS as u64) as usize;
            let old = self.game(side).snapshot().board;
            let (board, overflow) = push_garbage_rows(&old, rows as usize, hole);
            if !self.game_mut(side).install_board(board, overflow) {
                top_out = true;
            }
        }

        if top_out {
            out.push(MatchEvent::PlayerTopOut { side });
            let winner = side.other();
            self.winner = Some(winner);
            out.push(MatchEvent::WinnerCrowned { side: winner });
            return out;
        }

        match self.rule {
            AttackRule::Garbage => {
                if locked && cleared > 0 {
                    // Attack table: N lines (1..=4) send N; every clear
                    // beyond the first in the chain adds +1 (the snapshot
                    // combo counter counts consecutive clears minus one).
                    let attack = cleared.saturating_add(self.game(side).snapshot().combo);
                    let j = side.other().index();
                    self.pending[j] = self.pending[j].saturating_add(attack);
                    out.push(MatchEvent::GarbageSent {
                        side,
                        lines: attack,
                    });
                }
            }
            AttackRule::Race { target_lines } => {
                // Reaching the target freezes this side; the match only
                // ends once every side has finished (see module docs).
                let i = side.index();
                if locked
                    && self.finished[i].is_none()
                    && self.game(side).snapshot().lines >= target_lines
                {
                    self.finish_clock += 1;
                    self.finished[i] = Some(self.finish_clock);
                    out.push(MatchEvent::RaceTargetReached { side });
                    if self.finished[side.other().index()].is_some() {
                        let winner = self.compare_finished();
                        self.winner = Some(winner);
                        out.push(MatchEvent::WinnerCrowned { side: winner });
                    }
                }
            }
        }
        out
    }

    /// Full comparable match state: both game snapshots, queued garbage
    /// `(left, right)`, winner and rule.
    pub fn snapshot(&self) -> MatchSnapshot {
        MatchSnapshot {
            left: self.left.snapshot(),
            right: self.right.snapshot(),
            pending: (self.pending[0], self.pending[1]),
            winner: self.winner,
            finished: (self.finished[0].is_some(), self.finished[1].is_some()),
            rule: self.rule,
        }
    }

    /// Whether `side` has finished a Race target (board frozen; always
    /// `false` under Garbage).
    pub fn finished(&self, side: Side) -> bool {
        self.finished[side.index()].is_some()
    }

    /// Rank the two finished sides: higher score, then level, then lines;
    /// a perfect tie rewards the first side to the target.
    fn compare_finished(&self) -> Side {
        let (left, right) = (self.left.snapshot(), self.right.snapshot());
        let key = |g: &GameSnapshot| (g.score, g.level, g.lines);
        if key(&left) != key(&right) {
            return if key(&left) > key(&right) {
                Side::Left
            } else {
                Side::Right
            };
        }
        if self.finished[0] < self.finished[1] {
            Side::Left
        } else {
            Side::Right
        }
    }

    fn game(&self, side: Side) -> &Game {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    fn game_mut(&mut self, side: Side) -> &mut Game {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }
}

/// Piece stamped into incoming garbage cells. `Piece::Garbage` exists so
/// the board can carry garbage while the app renders it neutral instead of
/// piece-colored.
const GARBAGE_MARK: Piece = Piece::Garbage;

/// Push `rows` garbage rows up from the bottom of `board`: every existing
/// cell shifts up by `rows`, the `rows` new bottom rows come in full except
/// `hole_col`, and the result reports `true` when the push moved occupied
/// cells past the top of the playfield (overflow ⇒ block-out). Built purely
/// from the public [`Board`] API (`new`/`get`/`set`) as required by the T24
/// contract — `Board` needs no push helper of its own. `rows` saturates at
/// the board height; `rows == 0` returns the board unchanged.
pub(crate) fn push_garbage_rows(board: &Board, rows: usize, hole_col: usize) -> (Board, bool) {
    let rows = rows.min(ROWS);
    let mut pushed = Board::new();
    let mut overflow = false;
    for r in 0..ROWS {
        if r < rows {
            if (0..COLS).any(|c| board.get(r, c).is_some()) {
                overflow = true;
            }
        } else {
            for c in 0..COLS {
                if let Some(piece) = board.get(r, c) {
                    pushed.set(r - rows, c, Some(piece));
                }
            }
        }
    }
    for r in (ROWS - rows)..ROWS {
        for c in 0..COLS {
            if c != hole_col {
                pushed.set(r, c, Some(GARBAGE_MARK));
            }
        }
    }
    (pushed, overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True when `g`'s active piece is `pieces[0]` followed (deal order) by
    /// `pieces[1..]`.
    fn starts_with(g: &Game, pieces: &[Piece]) -> bool {
        g.snapshot().active.map(|ps| ps.piece) == Some(pieces[0])
            && g.peek_next(pieces.len() - 1) == pieces[1..]
    }

    /// Match seed whose left/right game deal orders start as given
    /// (`None` = unconstrained). NOTE: 7-bag ⇒ the first seven pieces of a
    /// side are all distinct, so repeated pieces must be spread out.
    fn find_match_seed(left: Option<&[Piece]>, right: Option<&[Piece]>) -> u64 {
        (0u64..)
            .find(|&s| {
                let mut r = Rng::new(s);
                let l = r.next_u64();
                let r_seed = r.next_u64();
                left.is_none_or(|p| starts_with(&Game::new(l), p))
                    && right.is_none_or(|p| starts_with(&Game::new(r_seed), p))
            })
            .expect("match seed with requested piece sequences")
    }

    /// Plain *game* seed (not match seed) starting with `pieces`.
    fn find_seed_with(pieces: &[Piece]) -> u64 {
        (0u64..)
            .find(|&s| starts_with(&Game::new(s), pieces))
            .expect("seed with requested piece sequence")
    }

    /// Install `filled` full bottom rows except column `gap`, so a
    /// vertical I dropped into `gap` clears exactly `filled` lines.
    fn setup_vertical_gap(m: &mut Match, side: Side, filled: usize, gap: usize) {
        let mut board = Board::new();
        for r in (ROWS - filled)..ROWS {
            for c in 0..COLS {
                if c != gap {
                    board.set(r, c, Some(Piece::Z));
                }
            }
        }
        assert!(m.game_mut(side).install_board(board, false));
    }

    /// Bottom row full except columns 3..=6: the flat spawn I clears one.
    fn setup_flat_gap(m: &mut Match, side: Side) {
        let mut board = Board::new();
        for c in 0..COLS {
            if !(3..=6).contains(&c) {
                board.set(ROWS - 1, c, Some(Piece::Z));
            }
        }
        assert!(m.game_mut(side).install_board(board, false));
    }

    /// Bottom row full except O's spawn columns 4-5: a hard-dropped O
    /// clears exactly one line (its top row gets only two cells).
    fn setup_o_single(m: &mut Match, side: Side) {
        let mut board = Board::new();
        for c in 0..COLS {
            if !(4..=5).contains(&c) {
                board.set(ROWS - 1, c, Some(Piece::Z));
            }
        }
        assert!(m.game_mut(side).install_board(board, false));
    }

    /// Two bottom rows full except columns 4-5: a hard-dropped O clears
    /// exactly two lines.
    fn setup_o_double(m: &mut Match, side: Side) {
        let mut board = Board::new();
        for r in (ROWS - 2)..ROWS {
            for c in 0..COLS {
                if !(4..=5).contains(&c) {
                    board.set(r, c, Some(Piece::Z));
                }
            }
        }
        assert!(m.game_mut(side).install_board(board, false));
    }

    /// Stack so a T rotated Cw and slid one right (cells: column 5 rows
    /// 19-21, nub column 6 row 20) clears exactly rows 20-21; row 19 keeps
    /// permanent holes at columns 6-7 so it never completes and the nub
    /// passes while falling.
    fn setup_t_double(m: &mut Match, side: Side) {
        let mut board = Board::new();
        for c in 0..COLS {
            if c != 5 && c != 6 && c != 7 {
                board.set(ROWS - 3, c, Some(Piece::Z));
            }
            if c != 5 && c != 6 {
                board.set(ROWS - 2, c, Some(Piece::Z));
            }
            if c != 5 {
                board.set(ROWS - 1, c, Some(Piece::Z));
            }
        }
        assert!(m.game_mut(side).install_board(board, false));
    }

    /// Rotate the spawn I vertical, aim at column `gap`, drop.
    fn drop_vertical_i(m: &mut Match, side: Side, gap: usize) -> Vec<MatchEvent> {
        assert!(m.apply(side, Action::RotateCw).is_empty());
        let drift = gap as i32 - 5; // spawn box col 3 + Cw cell offset 2
        for _ in 0..drift.abs() {
            let slide = if drift > 0 {
                Action::MoveRight
            } else {
                Action::MoveLeft
            };
            assert!(m.apply(side, slide).is_empty());
        }
        m.apply(side, Action::HardDrop)
    }

    /// Rotate the spawn T Cw, slide one right (cells col 5 rows r..r+2,
    /// nub col 6), drop: clears exactly two lines over `setup_t_double`.
    fn drop_t_cw(m: &mut Match, side: Side) -> Vec<MatchEvent> {
        assert!(m.apply(side, Action::RotateCw).is_empty());
        assert!(m.apply(side, Action::MoveRight).is_empty());
        m.apply(side, Action::HardDrop)
    }

    fn sent(ev: &[MatchEvent]) -> Option<u32> {
        ev.iter().find_map(|e| match e {
            MatchEvent::GarbageSent { lines, .. } => Some(*lines),
            _ => None,
        })
    }

    fn received(ev: &[MatchEvent]) -> Option<u32> {
        ev.iter().find_map(|e| match e {
            MatchEvent::GarbageReceived { lines, .. } => Some(*lines),
            _ => None,
        })
    }

    /// Hole column of row `r`, requiring exactly one hole in that row.
    fn hole_in_row(board: &Board, r: usize) -> usize {
        let holes: Vec<usize> = (0..COLS).filter(|&c| board.get(r, c).is_none()).collect();
        assert_eq!(holes.len(), 1, "row {r} must have exactly one hole");
        holes[0]
    }

    #[test]
    fn fresh_match_is_open_and_deterministic() {
        let m = Match::new(42, AttackRule::Garbage);
        assert_eq!(m.winner(), None);
        assert_eq!(m.pending_attack(Side::Left), 0);
        assert_eq!(m.pending_attack(Side::Right), 0);
        assert_eq!(m.rule(), AttackRule::Garbage);
        assert_eq!(
            m.snapshot(),
            Match::new(42, AttackRule::Garbage).snapshot(),
            "same seed must derive identical games (and hence snapshots)"
        );
    }

    #[test]
    fn default_rule_is_race_at_forty() {
        assert_eq!(AttackRule::default(), AttackRule::Race { target_lines: 40 });
        assert_eq!(DEFAULT_RACE_LINES, 40);
    }

    #[test]
    fn attack_table_single_double_triple_tetris() {
        let seed = find_match_seed(Some(&[Piece::I]), None);
        for lines in 1..=4usize {
            let mut m = Match::new(seed, AttackRule::Garbage);
            setup_vertical_gap(&mut m, Side::Left, lines, 5);
            let ev = drop_vertical_i(&mut m, Side::Left, 5);
            assert_eq!(
                ev,
                vec![
                    MatchEvent::PieceLocked {
                        side: Side::Left,
                        lines: lines as u32
                    },
                    MatchEvent::GarbageSent {
                        side: Side::Left,
                        lines: lines as u32
                    },
                ],
                "{lines}-line clear must queue exactly {lines} garbage"
            );
            assert_eq!(m.pending_attack(Side::Right), lines as u32);
            assert!(m.right.snapshot().board.is_empty());
        }
    }

    #[test]
    fn combo_chain_adds_plus_one_per_extra_clear() {
        let seed = find_match_seed(Some(&[Piece::I, Piece::O]), None);
        let mut m = Match::new(seed, AttackRule::Garbage);
        setup_flat_gap(&mut m, Side::Left);
        let ev = m.apply(Side::Left, Action::HardDrop);
        assert_eq!(sent(&ev), Some(1), "first clear sends its own count");
        setup_o_single(&mut m, Side::Left);
        let ev = m.apply(Side::Left, Action::HardDrop);
        assert_eq!(
            sent(&ev),
            Some(2),
            "second consecutive clear adds +1 (1 line + combo 1)"
        );
        assert_eq!(m.pending_attack(Side::Right), 3);
    }

    #[test]
    fn garbage_lands_on_the_receivers_next_lock() {
        let seed = find_match_seed(Some(&[Piece::I]), Some(&[Piece::I]));
        let mut m = Match::new(seed, AttackRule::Garbage);
        setup_flat_gap(&mut m, Side::Left);
        m.apply(Side::Left, Action::HardDrop);
        assert_eq!(m.pending_attack(Side::Right), 1);
        assert!(
            m.right.snapshot().board.is_empty(),
            "pending garbage must not touch the board before the lock"
        );

        let ev = m.apply(Side::Right, Action::HardDrop);
        assert_eq!(
            ev,
            vec![
                MatchEvent::PieceLocked {
                    side: Side::Right,
                    lines: 0
                },
                MatchEvent::GarbageReceived {
                    side: Side::Right,
                    lines: 1
                },
            ]
        );
        assert_eq!(m.pending_attack(Side::Right), 0);
        let board = m.right.snapshot().board;
        hole_in_row(&board, ROWS - 1);
        // The locked I row was pushed up from the floor by the garbage row.
        assert!(board.get(ROWS - 2, 3).is_some());
    }

    #[test]
    fn garbage_batch_hole_is_constant_within_batch() {
        let seed = find_match_seed(Some(&[Piece::I]), Some(&[Piece::I]));
        let mut m = Match::new(seed, AttackRule::Garbage);
        setup_vertical_gap(&mut m, Side::Left, 2, 5);
        drop_vertical_i(&mut m, Side::Left, 5);
        assert_eq!(m.pending_attack(Side::Right), 2);
        let ev = m.apply(Side::Right, Action::HardDrop);
        assert_eq!(received(&ev), Some(2));
        let board = m.right.snapshot().board;
        let hole = hole_in_row(&board, ROWS - 1);
        assert_eq!(hole_in_row(&board, ROWS - 2), hole, "one hole per batch");
    }

    #[test]
    fn garbage_overflow_tops_out_the_receiver() {
        let seed = find_match_seed(None, Some(&[Piece::I]));
        let mut m = Match::new(seed, AttackRule::Garbage);
        setup_vertical_gap(&mut m, Side::Right, 1, 5);
        drop_vertical_i(&mut m, Side::Right, 5);
        assert_eq!(m.pending_attack(Side::Left), 1);

        // Left has a settled cell on row 0: any push runs it off the top.
        let mut board = Board::new();
        board.set(0, 0, Some(Piece::J));
        assert!(m.game_mut(Side::Left).install_board(board, false));

        let ev = m.apply(Side::Left, Action::HardDrop);
        assert_eq!(
            ev,
            vec![
                MatchEvent::PieceLocked {
                    side: Side::Left,
                    lines: 0
                },
                MatchEvent::GarbageReceived {
                    side: Side::Left,
                    lines: 1
                },
                MatchEvent::PlayerTopOut { side: Side::Left },
                MatchEvent::WinnerCrowned { side: Side::Right },
            ]
        );
        assert_eq!(m.winner(), Some(Side::Right));
        assert!(m.left.snapshot().game_over);
        assert!(m.apply(Side::Right, Action::HardDrop).is_empty());
    }

    #[test]
    fn normal_play_blockout_hands_win_to_opponent() {
        // J wall in O's spawn columns up to row 3: the hard-dropped O locks
        // on rows 1-2, then every next piece block-outs at spawn.
        let seed = find_match_seed(Some(&[Piece::O]), None);
        let mut m = Match::new(seed, AttackRule::Race { target_lines: 1000 });
        let mut board = Board::new();
        for r in 3..ROWS {
            board.set(r, 4, Some(Piece::J));
            board.set(r, 5, Some(Piece::J));
        }
        assert!(m.game_mut(Side::Left).install_board(board, false));

        let ev = m.apply(Side::Left, Action::HardDrop);
        assert_eq!(
            ev,
            vec![
                MatchEvent::PieceLocked {
                    side: Side::Left,
                    lines: 0
                },
                MatchEvent::PlayerTopOut { side: Side::Left },
                MatchEvent::WinnerCrowned { side: Side::Right },
            ],
            "race rule must not emit garbage events"
        );
        assert_eq!(m.winner(), Some(Side::Right));
    }

    #[test]
    fn race_freezes_finishing_side_until_both_finish() {
        let seed = find_match_seed(Some(&[Piece::I]), Some(&[Piece::I]));
        let mut m = Match::new(seed, AttackRule::Race { target_lines: 1 });
        setup_flat_gap(&mut m, Side::Left);
        let ev = m.apply(Side::Left, Action::HardDrop);
        assert_eq!(
            ev,
            vec![
                MatchEvent::PieceLocked {
                    side: Side::Left,
                    lines: 1
                },
                MatchEvent::RaceTargetReached { side: Side::Left },
            ]
        );
        assert_eq!(
            m.winner(),
            None,
            "finishing must not crown while the opponent still races"
        );
        assert!(m.finished(Side::Left));
        assert!(!m.finished(Side::Right));

        // The finished side's board is frozen; the opponent keeps racing.
        let frozen = m.snapshot();
        assert!(m.apply(Side::Left, Action::HardDrop).is_empty());
        assert!(m.tick(Side::Left).is_empty());
        assert_eq!(m.snapshot(), frozen, "finished side's board is frozen");

        // Right clears its target too: equal one-single scores decide by
        // finish order, so the first finisher wins.
        setup_flat_gap(&mut m, Side::Right);
        let ev = m.apply(Side::Right, Action::HardDrop);
        assert_eq!(
            ev,
            vec![
                MatchEvent::PieceLocked {
                    side: Side::Right,
                    lines: 1
                },
                MatchEvent::RaceTargetReached { side: Side::Right },
                MatchEvent::WinnerCrowned { side: Side::Left },
            ]
        );
        assert_eq!(m.winner(), Some(Side::Left));
        assert_eq!(m.pending_attack(Side::Right), 0, "race never queues");
        let before = m.snapshot();
        assert!(m.apply(Side::Right, Action::HardDrop).is_empty());
        assert_eq!(m.snapshot(), before, "match frozen after crowning");
    }

    #[test]
    fn race_better_score_wins_when_both_finish() {
        // Left finishes with one single, Right answers with a 4-line
        // tetris: the *later* finisher wins on score, not on speed.
        let seed = find_match_seed(Some(&[Piece::I]), Some(&[Piece::I]));
        let mut m = Match::new(seed, AttackRule::Race { target_lines: 1 });
        setup_flat_gap(&mut m, Side::Left);
        let ev = m.apply(Side::Left, Action::HardDrop);
        assert!(ev.contains(&MatchEvent::RaceTargetReached { side: Side::Left }));
        assert_eq!(m.winner(), None);

        setup_vertical_gap(&mut m, Side::Right, 4, 5);
        let ev = drop_vertical_i(&mut m, Side::Right, 5);
        assert!(
            ev.contains(&MatchEvent::RaceTargetReached { side: Side::Right }),
            "{ev:?}"
        );
        assert_eq!(m.winner(), Some(Side::Right), "tetris outscores a single");
    }

    #[test]
    fn race_top_out_hands_win_to_finished_opponent() {
        let seed = find_match_seed(Some(&[Piece::I]), Some(&[Piece::O]));
        let mut m = Match::new(seed, AttackRule::Race { target_lines: 1 });
        setup_flat_gap(&mut m, Side::Left);
        let ev = m.apply(Side::Left, Action::HardDrop);
        assert!(ev.contains(&MatchEvent::RaceTargetReached { side: Side::Left }));
        assert_eq!(m.winner(), None);

        // Same spawn-wall setup as the tick top-out test: Right's O locks
        // on it, then every next piece block-outs at spawn.
        let mut board = Board::new();
        for r in 2..ROWS {
            board.set(r, 4, Some(Piece::Z));
            board.set(r, 5, Some(Piece::Z));
        }
        assert!(m.game_mut(Side::Right).install_board(board, false));
        let mut log = Vec::new();
        for _ in 0..40 {
            log.extend(m.tick(Side::Right));
            if m.winner().is_some() {
                break;
            }
        }
        assert!(
            log.contains(&MatchEvent::PlayerTopOut { side: Side::Right }),
            "{log:?}"
        );
        assert_eq!(m.winner(), Some(Side::Left));
    }

    #[test]
    fn garbage_lands_at_most_the_cap_per_lock_and_drips_the_surplus() {
        let seed = find_match_seed(Some(&[Piece::O]), Some(&[Piece::O]));
        let mut m = Match::new(seed, AttackRule::Garbage);
        m.pending[Side::Right.index()] = MAX_GARBAGE_PER_LAND + 2;

        let ev = m.apply(Side::Right, Action::HardDrop);
        assert!(
            ev.contains(&MatchEvent::GarbageReceived {
                side: Side::Right,
                lines: MAX_GARBAGE_PER_LAND
            }),
            "first lock lands exactly the cap: {ev:?}"
        );
        assert_eq!(m.pending_attack(Side::Right), 2);

        let ev = m.apply(Side::Right, Action::HardDrop);
        assert!(
            ev.contains(&MatchEvent::GarbageReceived {
                side: Side::Right,
                lines: 2
            }),
            "the surplus arrives on the next lock: {ev:?}"
        );
        assert_eq!(m.pending_attack(Side::Right), 0);
    }

    #[test]
    fn pending_attacks_accumulate_until_lock() {
        let seed = find_match_seed(Some(&[Piece::I, Piece::O, Piece::T]), None);
        let mut m = Match::new(seed, AttackRule::Garbage);
        setup_vertical_gap(&mut m, Side::Left, 1, 5);
        drop_vertical_i(&mut m, Side::Left, 5);
        assert_eq!(m.pending_attack(Side::Right), 1);

        // A no-clear lock sends nothing and never spends pending counts.
        assert_eq!(
            m.apply(Side::Left, Action::HardDrop),
            vec![MatchEvent::PieceLocked {
                side: Side::Left,
                lines: 0
            }]
        );
        assert_eq!(m.pending_attack(Side::Right), 1);

        setup_t_double(&mut m, Side::Left);
        let ev = drop_t_cw(&mut m, Side::Left);
        assert_eq!(sent(&ev), Some(2), "chain broken: plain 2-line attack");
        assert_eq!(sent(&ev), Some(2), "chain broken: plain 2-line attack");
        assert_eq!(m.pending_attack(Side::Right), 3);
    }

    #[test]
    fn replay_is_deterministic_and_hole_columns_follow_the_stream() {
        fn replay() -> (Vec<MatchEvent>, MatchSnapshot, usize, usize) {
            let seed = find_match_seed(Some(&[Piece::I, Piece::O]), Some(&[Piece::I]));
            let mut m = Match::new(seed, AttackRule::Garbage);
            let mut log = Vec::new();
            setup_vertical_gap(&mut m, Side::Left, 4, 5);
            log.extend(drop_vertical_i(&mut m, Side::Left, 5));
            log.extend(m.apply(Side::Right, Action::HardDrop));
            let board = m.right.snapshot().board;
            let hole1 = hole_in_row(&board, ROWS - 1);
            for r in (ROWS - 4)..ROWS {
                assert_eq!(hole_in_row(&board, r), hole1);
            }
            setup_o_double(&mut m, Side::Left);
            log.extend(m.apply(Side::Left, Action::HardDrop));
            log.extend(m.apply(Side::Right, Action::HardDrop));
            let board = m.right.snapshot().board;
            let hole2 = hole_in_row(&board, ROWS - 1);
            for r in (ROWS - 3)..ROWS {
                assert_eq!(hole_in_row(&board, r), hole2);
            }
            (log, m.snapshot(), hole1, hole2)
        }
        let (log_a, snap_a, h1_a, h2_a) = replay();
        let (log_b, snap_b, h1_b, h2_b) = replay();
        assert_eq!(log_a, log_b);
        assert_eq!(snap_a, snap_b);
        assert_eq!((h1_a, h2_a), (h1_b, h2_b), "same seed => same holes");
        assert_ne!(
            h1_a, h2_b,
            "consecutive batches draw different holes from the stream"
        );
    }

    #[test]
    fn hole_column_varies_across_seeds() {
        let mut holes = std::collections::BTreeSet::new();
        for seed in 0..32u64 {
            let mut m = Match::new(seed, AttackRule::Garbage);
            m.pending[Side::Right.index()] = 1;
            let ev = m.apply(Side::Right, Action::HardDrop);
            assert_eq!(received(&ev), Some(1));
            holes.insert(hole_in_row(&m.right.snapshot().board, ROWS - 1));
        }
        assert!(holes.len() > 1, "hole column never varies: {holes:?}");
    }

    #[test]
    fn push_garbage_rows_shifts_stack_and_reports_overflow() {
        let mut board = Board::new();
        board.set(ROWS - 3, 2, Some(Piece::S));
        let (pushed, overflow) = push_garbage_rows(&board, 2, 7);
        assert!(!overflow);
        assert_eq!(pushed.get(ROWS - 5, 2), Some(Piece::S));
        assert_eq!(pushed.get(ROWS - 3, 2), None);
        for r in [ROWS - 2, ROWS - 1] {
            assert_eq!(hole_in_row(&pushed, r), 7);
        }
        // Overflow: a cell inside the top `rows` band runs off the ceiling.
        let mut tall = Board::new();
        tall.set(1, 0, Some(Piece::L));
        let (_, overflow) = push_garbage_rows(&tall, 2, 3);
        assert!(overflow);
        let (_, overflow) = push_garbage_rows(&tall, 1, 3);
        assert!(!overflow);
        // Identity for zero rows.
        let (same, overflow) = push_garbage_rows(&board, 0, 4);
        assert!(!overflow);
        assert_eq!(same, board);
    }

    #[test]
    fn push_garbage_rows_every_row_exactly_one_hole_at_chosen_column() {
        let board = Board::new();
        for hole in 0..COLS {
            let (pushed, overflow) = push_garbage_rows(&board, 3, hole);
            assert!(!overflow);
            for r in (ROWS - 3)..ROWS {
                assert_eq!(hole_in_row(&pushed, r), hole);
                assert_eq!(
                    (0..COLS)
                        .filter(|&c| pushed.get(r, c) == Some(GARBAGE_MARK))
                        .count(),
                    COLS - 1
                );
            }
            assert!((0..(ROWS - 3)).all(|r| (0..COLS).all(|c| pushed.get(r, c).is_none())));
        }
    }

    #[test]
    fn install_board_blocks_out_on_overlap_and_top_out() {
        let seed = find_seed_with(&[Piece::I]); // spawn I: cells row 1, cols 3-6
        let mut overlap_board = Board::new();
        overlap_board.set(1, 4, Some(Piece::Z));

        let mut g = Game::new(seed);
        assert!(!g.install_board(overlap_board, false));
        assert!(g.snapshot().game_over, "overlap must end the game");
        assert!(g.snapshot().active.is_none());

        let mut g = Game::new(seed);
        assert!(!g.install_board(Board::new(), true), "top_out must end it");
        assert!(g.snapshot().game_over);

        let mut g = Game::new(seed);
        assert!(g.install_board(Board::new(), false));
        assert!(!g.snapshot().game_over);
        assert_eq!(g.snapshot().board, Board::new());
    }

    #[test]
    fn tick_locks_a_grounded_piece_and_its_clear_sends_garbage() {
        let seed = find_match_seed(Some(&[Piece::O]), None);
        let mut m = Match::new(seed, AttackRule::Garbage);
        // Spawn O (cells rows 0-1, cols 4-5) grounded on (2, 4), with rows
        // 0-1 otherwise full: the lock-delay lock completes both rows.
        let mut board = Board::new();
        for r in 0..2 {
            for c in 0..COLS {
                if c != 4 && c != 5 {
                    board.set(r, c, Some(Piece::Z));
                }
            }
        }
        board.set(2, 4, Some(Piece::Z));
        assert!(m.game_mut(Side::Left).install_board(board, false));

        let mut log = Vec::new();
        for _ in 0..31 {
            log.extend(m.tick(Side::Left));
        }
        let locked = log
            .iter()
            .find(|e| matches!(e, MatchEvent::PieceLocked { side, .. } if *side == Side::Left));
        assert_eq!(
            locked,
            Some(&MatchEvent::PieceLocked {
                side: Side::Left,
                lines: 2
            }),
            "lock-delay lock must clear the two completed rows: {log:?}"
        );
        assert!(log.contains(&MatchEvent::GarbageSent {
            side: Side::Left,
            lines: 2
        }));
        assert_eq!(m.pending_attack(Side::Right), 2);
        assert_eq!(m.winner(), None);
    }

    #[test]
    fn tick_top_out_crowns_the_opponent_and_freezes_the_match() {
        let seed = find_match_seed(Some(&[Piece::O]), None);
        let mut m = Match::new(seed, AttackRule::Garbage);
        // Columns 4-5 filled from row 2 down: the spawn O lock-delays onto
        // them, and every next piece block-outs at spawn — all via tick().
        let mut board = Board::new();
        for r in 2..ROWS {
            board.set(r, 4, Some(Piece::Z));
            board.set(r, 5, Some(Piece::Z));
        }
        assert!(m.game_mut(Side::Left).install_board(board, false));

        let mut log = Vec::new();
        for _ in 0..40 {
            log.extend(m.tick(Side::Left));
        }
        assert!(
            log.contains(&MatchEvent::PieceLocked {
                side: Side::Left,
                lines: 0
            }),
            "the grounded O must lock on the delay: {log:?}"
        );
        assert!(log.contains(&MatchEvent::PlayerTopOut { side: Side::Left }));
        assert!(log.contains(&MatchEvent::WinnerCrowned { side: Side::Right }));
        assert_eq!(m.winner(), Some(Side::Right));

        let before = m.snapshot();
        assert!(m.tick(Side::Right).is_empty(), "frozen after crowning");
        assert!(m.apply(Side::Right, Action::HardDrop).is_empty());
        assert_eq!(m.snapshot(), before);
    }
}
