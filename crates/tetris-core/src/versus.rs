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
//! - [`AttackRule::Dig`] (T20, Dig Duel): no garbage ever travels between
//!   the sides. Both games start from **one shared side seed** on the same
//!   10-row buried garbage board (identical hole columns, identical piece
//!   sequence) under the solo-Dig [`ModeConfig`] (level pinned to 1,
//!   `Goal::GarbageCleared` — see [`dig_side_config`]). The first side whose
//!   `garbage_rows_left()` hits 0 on a lock is crowned immediately; a
//!   top-out loses on the spot (opponent crowned). If both sides would zero
//!   on the same bridge frame, the first-settled side wins (fixed
//!   Left-then-Right settle order ⇒ symmetric on both peers).
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
use crate::mode::{Goal, ModeConfig, StartBoard};
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
    /// Dig Duel (T20): both sides start on the **identical** 10-row buried
    /// garbage board (same hole columns — both side games share one seed,
    /// so the piece sequence is identical too) and no garbage ever travels
    /// between them. First side whose `garbage_rows_left()` reaches 0 wins;
    /// a top-out loses immediately (opponent crowned). A side game runs the
    /// solo-Dig [`ModeConfig`] (see [`dig_side_config`]).
    ///
    /// Tie rule: crowning is synchronous with the settle that zeros a side,
    /// so both sides can only "tie" inside one bridge frame — the side that
    /// settles first there wins. The bridge order (Left settles before
    /// Right per frame) is fixed bridge code, identical on both peers, so
    /// the outcome is fully deterministic; see
    /// `Match::new` and [`Match::settle`].
    Dig,
    /// Switch scaffold (rule behavior lands in T21): garbage attacks plus
    /// full board swaps every `swap_interval_ticks` of the match clock, with
    /// [`MatchEvent::SwapWarning`] `warning_ticks` before each swap. Until
    /// T21 implements it, this behaves like no-op/no-attack (see
    /// [`Match::settle`]).
    Switch {
        /// Match ticks between board swaps (T21; default 1 800).
        swap_interval_ticks: u32,
        /// Match ticks of warning ahead of a swap (T21; default 180).
        warning_ticks: u32,
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

/// Buried garbage rows on both boards of a Dig Duel (T20). Mirrors
/// `modes::DIG_GARBAGE_ROWS` on the app side; the shared hole-column
/// derivation is `mode::StartBoard::BuriedGarbage::build` — versus and
/// solo Dig started from the same seed produce the same buried board
/// (regression-tested).
pub const DIG_DUEL_GARBAGE_ROWS: usize = 10;

/// The [`ModeConfig`] each Dig Duel side game runs: the exact solo-Dig rule
/// set (`crates/tetris-app/src/modes.rs::mode_config(ModeId::Dig)` —
/// `start_level: 1` with `levels_advance: false`, so gravity is fixed at
/// level 1 for the whole duel, and `Goal::GarbageCleared` freezes a side
/// game the moment its buried rows are gone, in lockstep with the match
/// crowning it). The buried board itself derives from the side game's seed
/// through the shared `mode::StartBoard` generator.
pub(crate) fn dig_side_config() -> ModeConfig {
    ModeConfig {
        start_level: 1,
        levels_advance: false,
        goal: Some(Goal::GarbageCleared),
        clock_ticks: None,
        start_board: Some(StartBoard::BuriedGarbage {
            rows: DIG_DUEL_GARBAGE_ROWS,
        }),
        ..ModeConfig::default()
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
    /// A Switch board swap is scheduled at match tick `at_tick` (T21
    /// scaffold: emitted `warning_ticks` before [`MatchEvent::BoardSwapped`]).
    SwapWarning {
        /// Match tick at which the swap will happen.
        at_tick: u64,
    },
    /// The two sides' entire game states swapped at match tick `tick`
    /// (T21 scaffold; Switch rule only).
    BoardSwapped {
        /// Match tick the swap executed on.
        tick: u64,
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
    /// Match-level tick counter, mirrored from [`Match::match_ticks`]: one
    /// increment per fully-observed frame, advanced through
    /// [`Match::advance_match_clock`] (the Switch rule's swap schedule is
    /// expressed against this clock).
    pub match_ticks: u64,
    /// Number of Switch board swaps already executed (T21 swap bookkeeping;
    /// always 0 until T21 implements the Switch rule).
    pub swaps_done: u32,
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
    /// Match-level clock: one increment per fully-observed frame, driven
    /// exclusively by [`Match::advance_match_clock`] (see there for the
    /// exact stepping contract both peers must follow).
    match_ticks: u64,
    /// Number of Switch board swaps executed so far (T21 scaffold; stays 0
    /// until T21 implements the swap logic).
    swaps_done: u32,
    rng: Rng,
}

impl Match {
    /// Fresh match: per-side game seeds and the garbage-hole stream all
    /// derive from `seed` via one [`Rng`] stream (left game first, then
    /// right, then hole draws), so replays are exact.
    ///
    /// Dig Duel exception ([`AttackRule::Dig`]): **both** side games share
    /// the first draw (same seed ⇒ same buried board and same piece
    /// sequence), and each is built with [`dig_side_config`] — the solo-Dig
    /// rule set (10 buried rows from that shared seed, level pinned to 1,
    /// `Goal::GarbageCleared`). The hole-draw stream is never consumed
    /// under Dig (no garbage is ever pushed).
    pub fn new(seed: u64, rule: AttackRule) -> Self {
        let mut rng = Rng::new(seed);
        let left_seed = rng.next_u64();
        let (left, right) = if rule == AttackRule::Dig {
            let config = dig_side_config();
            (
                Game::with_config(left_seed, &config),
                Game::with_config(left_seed, &config),
            )
        } else {
            let right_seed = rng.next_u64();
            (Game::new(left_seed), Game::new(right_seed))
        };
        Self {
            left,
            right,
            rule,
            winner: None,
            pending: [0, 0],
            finished: [None, None],
            finish_clock: 0,
            match_ticks: 0,
            swaps_done: 0,
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

    /// Match-level clock: number of fully-observed frames advanced through
    /// [`Match::advance_match_clock`] since the match was created.
    pub fn match_ticks(&self) -> u64 {
        self.match_ticks
    }

    /// Advance the match clock by one frame. The bridge calls this **exactly
    /// once per fixed step, after both sides have ticked** (local path:
    /// `versus_bridge_system`; netplay path: `lockstep::apply_batch` — both
    /// on both peers), never from anywhere else. The rule is pure
    /// call-sequence logic: both peers run identical bridge code and step
    /// the mirror in lockstep, so `match_ticks` is the same value on both
    /// ends at every observable point and every Swap schedule derived from
    /// it is deterministic. Returns match-level events emitted on this tick
    /// boundary (always empty until T21 emits `SwapWarning`/`BoardSwapped`).
    /// Like the rest of `Match`, a crowned match is frozen: after a winner
    /// the clock stops advancing (both peers crown on the identical lockstep
    /// step, so the gate is symmetric and the frozen match snapshot stays
    /// byte-stable).
    pub fn advance_match_clock(&mut self) -> Vec<MatchEvent> {
        if self.winner.is_some() {
            return Vec::new();
        }
        self.match_ticks += 1;
        Vec::new()
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
            AttackRule::Dig => {
                // Dig Duel: garbage never moves between sides; the win is
                // read off this side's own board on every settle. `Game`
                // only ever empties garbage rows on a lock (line clears),
                // so checking on `locked` is exhaustive — and the top-out
                // above already returned, making "zeroed and topped out on
                // the same lock" a top-out loss. The winner-crowning path
                // is the same one the Garbage rule uses (set `winner`,
                // emit `WinnerCrowned`): the match freezes from here, so
                // the first settled zero wins even if the other side also
                // zeroed later in the same frame (its settle is inert) —
                // see the tie note on [`AttackRule::Dig`].
                if locked && self.game(side).garbage_rows_left() == 0 {
                    self.winner = Some(side);
                    out.push(MatchEvent::WinnerCrowned { side });
                }
            }
            // Switch: rule behavior lands in T21. Until then a documented
            // no-attack placeholder: locks clear and top out normally
            // (handled above), but no swap bookkeeping happens here.
            AttackRule::Switch { .. } => {}
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
            match_ticks: self.match_ticks,
            swaps_done: self.swaps_done,
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

    /// T19 append-order canary: bincode pins the variant index in the
    /// leading fixint u32 — Garbage/Race stay at 0/1 forever, the appended
    /// variants occupy exactly 2/3.
    #[test]
    fn attack_rule_variant_indices_are_append_only() {
        let index = |rule: &AttackRule| -> u32 {
            let bytes = bincode::serialize(rule).expect("rule encodes");
            u32::from_le_bytes(bytes[..4].try_into().unwrap())
        };
        assert_eq!(index(&AttackRule::Garbage), 0);
        assert_eq!(index(&AttackRule::Race { target_lines: 40 }), 1);
        assert_eq!(index(&AttackRule::Dig), 2);
        assert_eq!(
            index(&AttackRule::Switch {
                swap_interval_ticks: 1_800,
                warning_ticks: 180,
            }),
            3
        );
    }

    #[test]
    fn new_variants_round_trip() {
        let cases: Vec<(AttackRule, MatchEvent)> = vec![
            (AttackRule::Dig, MatchEvent::SwapWarning { at_tick: 1_620 }),
            (
                AttackRule::Switch {
                    swap_interval_ticks: 1_800,
                    warning_ticks: 180,
                },
                MatchEvent::BoardSwapped { tick: 1_800 },
            ),
        ];
        for (rule, event) in cases {
            let rule_back: AttackRule = bincode::deserialize(&bincode::serialize(&rule).unwrap())
                .expect("rule round-trips");
            assert_eq!(rule_back, rule);
            let event_back: MatchEvent = bincode::deserialize(&bincode::serialize(&event).unwrap())
                .expect("event round-trips");
            assert_eq!(event_back, event);
        }
    }

    /// The match clock moves *only* through `advance_match_clock` — per-side
    /// `tick` calls alone never touch it — and the snapshot mirrors it.
    #[test]
    fn match_clock_advances_only_via_advance_match_clock() {
        let mut m = Match::new(1, AttackRule::Garbage);
        assert_eq!(m.match_ticks(), 0);
        assert_eq!(m.snapshot().match_ticks, 0);
        assert_eq!(m.snapshot().swaps_done, 0);
        for _ in 0..5 {
            m.tick(Side::Left);
            m.tick(Side::Right);
        }
        assert_eq!(m.match_ticks(), 0, "tick() must not move the clock");
        m.advance_match_clock();
        m.advance_match_clock();
        assert_eq!(m.match_ticks(), 2);
        let snap = m.snapshot();
        assert_eq!(snap.match_ticks, 2);
        assert_eq!(snap.swaps_done, 0, "no swap logic before T21");
    }

    /// T19 scaffolding (Switch half): Switch is still a documented
    /// no-attack placeholder — locks clear normally, but nothing queues,
    /// swaps or crowns beyond the standard top-out path. (The Dig half
    /// became the real Dig Duel rule in T20; see the dig tests below.)
    #[test]
    fn switch_rule_is_a_no_attack_placeholder() {
        let seed = find_match_seed(Some(&[Piece::I]), Some(&[Piece::I]));
        let rule = AttackRule::Switch {
            swap_interval_ticks: 1_800,
            warning_ticks: 180,
        };
        let mut m = Match::new(seed, rule);
        setup_flat_gap(&mut m, Side::Left);
        let ev = m.apply(Side::Left, Action::HardDrop);
        assert_eq!(
            ev,
            vec![MatchEvent::PieceLocked {
                side: Side::Left,
                lines: 1
            }],
            "{rule:?} must neither attack nor swap before T21"
        );
        assert_eq!(m.pending_attack(Side::Right), 0);
        assert_eq!(m.winner(), None);
        let mut ev = m.advance_match_clock();
        ev.extend(m.advance_match_clock());
        assert!(ev.is_empty(), "{rule:?} emits no clock events yet");
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

    // ----------------------------------------------------------------
    // T20: Dig Duel
    // ----------------------------------------------------------------

    /// Replace the side's board with a single clearable bottom garbage row
    /// (full except columns 4-5): a hard-dropped O finishes that row and
    /// takes `garbage_rows_left()` to zero.
    fn setup_o_clearable_garbage(m: &mut Match, side: Side) {
        let mut board = Board::new();
        for c in 0..COLS {
            if !(4..=5).contains(&c) {
                board.set(ROWS - 1, c, Some(Piece::Garbage));
            }
        }
        assert!(m.game_mut(side).install_board(board, false));
    }

    /// The first per-side game seed `Match::new` draws from `seed` (both
    /// sides under Dig — one seed for both games).
    fn dig_side_seed(match_seed: u64) -> u64 {
        Rng::new(match_seed).next_u64()
    }

    #[test]
    fn dig_duel_starts_with_identical_buried_boards_and_sequences() {
        let m = Match::new(42, AttackRule::Dig);
        let (ls, rs) = (m.left.snapshot(), m.right.snapshot());
        assert_eq!(ls.board, rs.board, "same seed ⇒ same buried board");
        let left_seq: Vec<_> = ls.active.map(|p| p.piece).into_iter().collect();
        assert_eq!(ls.active.map(|p| p.piece), rs.active.map(|p| p.piece));
        assert!(!left_seq.is_empty());
        assert_eq!(m.left.peek_next(6), m.right.peek_next(6));
        assert_eq!(m.left.garbage_rows_left(), 10, "solo-Dig depth");
        assert_eq!(m.right.garbage_rows_left(), 10);
        assert_eq!(ls.level, 1, "Dig gravity is fixed at level 1");
        assert_eq!(rs.level, 1);
        // Single source of truth: the duel board is exactly the solo-Dig
        // buried board built from the same side seed.
        let solo = Game::with_config(
            dig_side_seed(42),
            &ModeConfig {
                start_level: 1,
                levels_advance: false,
                goal: Some(Goal::GarbageCleared),
                start_board: Some(StartBoard::BuriedGarbage { rows: 10 }),
                ..ModeConfig::default()
            },
        );
        assert_eq!(
            ls.board,
            solo.snapshot().board,
            "versus and solo Dig must agree on hole columns"
        );
        // Every buried row: exactly one hole, rest garbage.
        for r in (ROWS - 10)..ROWS {
            let holes = (0..COLS).filter(|&c| ls.board.get(r, c).is_none()).count();
            assert_eq!(holes, 1, "row {r}");
        }
    }

    /// The side-game config is the documented mirror of solo Dig's: level
    /// pinned to 1, `Goal::GarbageCleared`, 10 buried rows, no clock/feed.
    #[test]
    fn dig_side_config_mirrors_solo_dig() {
        assert_eq!(DIG_DUEL_GARBAGE_ROWS, 10, "same depth as solo Dig");
        let c = dig_side_config();
        assert_eq!(c.start_level, 1);
        assert!(!c.levels_advance, "Dig gravity fixed at level 1");
        assert_eq!(c.goal, Some(Goal::GarbageCleared));
        assert_eq!(c.clock_ticks, None);
        assert_eq!(
            c.start_board,
            Some(StartBoard::BuriedGarbage {
                rows: DIG_DUEL_GARBAGE_ROWS
            })
        );
        assert_eq!(c.on_block_out, crate::mode::BlockOutBehavior::End);
        assert_eq!(c.garbage_feed, None);
    }

    #[test]
    fn dig_duel_never_sends_or_lands_garbage() {
        let mut m = Match::new(7, AttackRule::Dig);
        let mut log = Vec::new();
        for i in 0..20 {
            if m.winner().is_some() {
                break;
            }
            log.extend(m.apply(Side::Left, Action::HardDrop));
            if i % 3 == 0 {
                log.extend(m.apply(Side::Left, Action::MoveLeft));
            }
            log.extend(m.apply(Side::Right, Action::HardDrop));
            log.extend(m.tick(Side::Left));
            log.extend(m.tick(Side::Right));
        }
        assert!(
            !log.iter().any(|e| matches!(
                e,
                MatchEvent::GarbageSent { .. } | MatchEvent::GarbageReceived { .. }
            )),
            "Dig Duel must never attack: {log:?}"
        );
        assert_eq!(m.pending_attack(Side::Left), 0);
        assert_eq!(m.pending_attack(Side::Right), 0);
    }

    #[test]
    fn dig_clearing_the_last_row_crowns_that_side_immediately() {
        let seed = find_match_seed(Some(&[Piece::O]), None);
        let mut m = Match::new(seed, AttackRule::Dig);
        setup_o_clearable_garbage(&mut m, Side::Left);
        setup_o_clearable_garbage(&mut m, Side::Right);
        assert_eq!(m.left.garbage_rows_left(), 1);
        assert_eq!(m.winner(), None, "buried rows left ⇒ match open");

        let ev = m.apply(Side::Left, Action::HardDrop);
        assert_eq!(
            ev,
            vec![
                MatchEvent::PieceLocked {
                    side: Side::Left,
                    lines: 1
                },
                MatchEvent::WinnerCrowned { side: Side::Left },
            ],
            "zero buried rows crowns on the spot"
        );
        assert_eq!(m.left.garbage_rows_left(), 0);
        assert_eq!(m.winner(), Some(Side::Left));

        // Frozen: the opponent never gets to answer.
        let before = m.snapshot();
        assert!(m.apply(Side::Right, Action::HardDrop).is_empty());
        assert!(m.tick(Side::Right).is_empty());
        assert_eq!(m.snapshot(), before, "match frozen after crowning");
    }

    #[test]
    fn dig_top_out_hands_the_win_to_the_opponent() {
        let seed = find_match_seed(Some(&[Piece::O]), None);
        let mut m = Match::new(seed, AttackRule::Dig);
        // Spawn-column wall added **on top of** the buried board (garbage
        // rows stay ⇒ no zero-clear shortcut): Right's O locks on the wall,
        // the next spawn block-outs.
        let mut board = m.right.snapshot().board;
        for r in 2..ROWS {
            board.set(r, 4, Some(Piece::Z));
            board.set(r, 5, Some(Piece::Z));
        }
        assert!(m.game_mut(Side::Right).install_board(board, false));
        assert!(m.right.garbage_rows_left() > 0);

        let mut log = Vec::new();
        for _ in 0..40 {
            log.extend(m.apply(Side::Right, Action::HardDrop));
            if m.winner().is_some() {
                break;
            }
        }
        assert!(
            log.contains(&MatchEvent::PlayerTopOut { side: Side::Right }),
            "{log:?}"
        );
        assert!(log.contains(&MatchEvent::WinnerCrowned { side: Side::Left }));
        assert_eq!(m.winner(), Some(Side::Left));
    }

    /// Tie rule: crowning is synchronous with the settle that zeros a side,
    /// so within one bridge frame (Left settles before Right, fixed order on
    /// both peers) the first-settled zero wins even when both sides would
    /// clear their last row on that frame.
    #[test]
    fn dig_same_frame_double_zero_goes_to_the_first_settled_side() {
        let seed = find_match_seed(Some(&[Piece::O]), None);
        for first in [Side::Left, Side::Right] {
            let mut m = Match::new(seed, AttackRule::Dig);
            setup_o_clearable_garbage(&mut m, Side::Left);
            setup_o_clearable_garbage(&mut m, Side::Right);
            // One settle zeroes `first`; the other side's matching lock lands
            // later within the same frame and must be inert.
            let ev = m.apply(first, Action::HardDrop);
            assert!(
                ev.contains(&MatchEvent::WinnerCrowned { side: first }),
                "first-settled zero must crown on its own settle: {ev:?}"
            );
            assert_eq!(m.winner(), Some(first));
            assert!(m.apply(first.other(), Action::HardDrop).is_empty());
            assert_eq!(m.winner(), Some(first), "crown is not re-decided");
        }
    }

    #[test]
    fn dig_replay_is_deterministic() {
        fn replay() -> (Vec<MatchEvent>, MatchSnapshot) {
            let seed = find_match_seed(Some(&[Piece::I, Piece::O]), None);
            let mut m = Match::new(seed, AttackRule::Dig);
            let mut log = Vec::new();
            for i in 0..25 {
                if m.winner().is_some() {
                    break;
                }
                log.extend(m.apply(Side::Left, Action::HardDrop));
                if i % 2 == 0 {
                    log.extend(m.apply(Side::Right, Action::RotateCw));
                }
                log.extend(m.apply(Side::Right, Action::HardDrop));
                for _ in 0..5 {
                    log.extend(m.tick(Side::Left));
                    log.extend(m.tick(Side::Right));
                }
                log.extend(m.advance_match_clock());
            }
            (log, m.snapshot())
        }
        let (log_a, snap_a) = replay();
        let (log_b, snap_b) = replay();
        assert!(!log_a.is_empty());
        assert_eq!(log_a, log_b);
        assert_eq!(snap_a, snap_b);
        // Fresh duel boards start identical (divergence below is purely
        // input-driven; the shared-seed property itself is pinned in
        // `dig_duel_starts_with_identical_buried_boards_and_sequences`).
        let fresh = Match::new(
            find_match_seed(Some(&[Piece::I, Piece::O]), None),
            AttackRule::Dig,
        )
        .snapshot();
        assert_eq!(fresh.left.board, fresh.right.board);
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
