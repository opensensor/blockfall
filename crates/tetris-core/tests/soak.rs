//! T9 wall-clock soak: PRD §12 "1 h crash-free" run of random play.
//!
//! Gated with `#[ignore]` so plain `cargo test` skips it; executed only by
//! the dedicated nightly CI job (T20), never per-push. Run manually with:
//!
//! ```text
//! cargo test -p tetris-core -- --ignored
//! ```
//!
//! The loop is driven by a deterministic LCG (no extra dev-deps), restarts
//! games on `game_over`, and spot-checks core invariants after every step:
//! no active/board overlap, no uncleared full rows, monotonic score, next
//! queue depth 1..=6. Any panic fails the test.

use std::time::{Duration, Instant};

use tetris_core::actions::Action;
use tetris_core::game::Game;

const ALL_ACTIONS: [Action; 8] = [
    Action::MoveLeft,
    Action::MoveRight,
    Action::SoftDrop,
    Action::HardDrop,
    Action::RotateCw,
    Action::RotateCcw,
    Action::Rotate180,
    Action::Hold,
];

/// SplitMix64 — tiny deterministic PRNG, no dependency needed.
struct Splitmix64(u64);

impl Splitmix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

fn spot_check(game: &Game, prev_score: u64, step: u64) -> u64 {
    let s = game.snapshot();
    assert!(
        s.score >= prev_score,
        "soak step {step}: score regressed {prev_score} -> {}",
        s.score
    );
    if let Some(ps) = s.active {
        assert!(
            !s.board.collides(&ps),
            "soak step {step}: active piece overlaps board: {ps:?}"
        );
    }
    assert!(
        s.board.full_rows().is_empty(),
        "soak step {step}: uncleared full row on board"
    );
    assert!(
        (1..=6).contains(&s.next.len()),
        "soak step {step}: bad next queue len {}",
        s.next.len()
    );
    s.score
}

#[test]
#[ignore = "1 h crash-free soak; run: cargo test -p tetris-core -- --ignored"]
fn crash_free_soak_1h() {
    let budget = Duration::from_secs(3600);
    let start = Instant::now();
    let mut rng = Splitmix64(0x5EED_2026_0926);
    let mut games = 0u64;
    let mut steps = 0u64;

    while start.elapsed() < budget {
        let mut game = Game::new(rng.next_u64());
        let mut score = game.snapshot().score;
        games += 1;
        loop {
            let action = ALL_ACTIONS[rng.below(ALL_ACTIONS.len() as u64) as usize];
            game.apply(action);
            let ticks = rng.below(5);
            for _ in 0..ticks {
                game.tick();
            }
            steps += 1;
            score = spot_check(&game, score, steps);
            if game.snapshot().game_over {
                break;
            }
        }
    }

    assert!(games > 0, "soak completed no games");
    println!(
        "soak: {} games, {} steps in {:?}",
        games,
        steps,
        start.elapsed()
    );
}
