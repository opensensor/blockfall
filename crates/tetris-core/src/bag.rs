//! 7-bag randomizer over the seedable PRNG (PRD §6.3, T3).
//!
//! Pieces are dealt in bags of seven: each bag is a fresh Fisher–Yates
//! shuffle of [`Piece::ALL`] driven by [`Rng`], so every aligned group of
//! seven consecutive deals is a permutation of all seven tetrominoes. The
//! whole sequence is a pure function of the seed.

use std::cell::RefCell;
use std::collections::VecDeque;

use crate::piece::Piece;
use crate::prng::Rng;

#[derive(Debug)]
struct BagState {
    rng: Rng,
    /// Upcoming pieces; `queue[0]` is dealt by the next [`Bag::next`].
    queue: VecDeque<Piece>,
}

/// Deterministic 7-bag randomizer.
#[derive(Debug)]
pub struct Bag {
    state: RefCell<BagState>,
}

impl Bag {
    /// New bag dealing from `seed`. The first bag is shuffled immediately,
    /// so even `peek` sees post-shuffle pieces.
    pub fn new(seed: u64) -> Self {
        let bag = Bag {
            state: RefCell::new(BagState {
                rng: Rng::new(seed),
                queue: VecDeque::new(),
            }),
        };
        bag.refill();
        bag
    }

    /// Deal the next piece, appending a freshly shuffled bag when the
    /// queue runs dry.
    // Deliberately an inherent API (`Iterator::next` would return `Option`,
    // but dealing never fails).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Piece {
        let mut state = self.state.borrow_mut();
        if state.queue.is_empty() {
            push_shuffled_bag(&mut state);
        }
        state.queue.pop_front().expect("queue refilled above")
    }

    /// Preview the next `n` pieces without consuming them. The queue is
    /// transparently refilled across bag boundaries as needed, so the
    /// returned `Vec` always has exactly `n` elements; repeated peeks
    /// never advance the deal sequence.
    pub fn peek(&self, n: usize) -> Vec<Piece> {
        let mut state = self.state.borrow_mut();
        while state.queue.len() < n {
            push_shuffled_bag(&mut state);
        }
        state.queue.iter().copied().take(n).collect()
    }

    fn refill(&self) {
        push_shuffled_bag(&mut self.state.borrow_mut());
    }
}

/// Append one Fisher–Yates shuffled copy of `Piece::ALL` to the queue.
fn push_shuffled_bag(state: &mut BagState) {
    let mut bag = Piece::ALL;
    for i in (1..bag.len()).rev() {
        let j = state.rng.next_below((i + 1) as u64) as usize;
        bag.swap(i, j);
    }
    state.queue.extend(bag);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece::Piece;

    fn is_permutation(group: &[Piece]) -> bool {
        group.len() == 7 && Piece::ALL.iter().all(|p| group.contains(p))
    }

    #[test]
    fn every_group_of_seven_is_a_permutation() {
        let mut bag = Bag::new(0xC0FFEE);
        for _ in 0..1000 {
            let group: Vec<Piece> = (0..7).map(|_| bag.next()).collect();
            assert!(is_permutation(&group), "not a permutation: {group:?}");
        }
    }

    #[test]
    fn consecutive_bags_are_not_repeats() {
        let mut bag = Bag::new(31337);
        let mut prev: Vec<Piece> = (0..7).map(|_| bag.next()).collect();
        assert!(is_permutation(&prev));
        let mut distinct_adjacency = 0;
        for _ in 0..999 {
            let group: Vec<Piece> = (0..7).map(|_| bag.next()).collect();
            assert!(is_permutation(&group));
            if group != prev {
                distinct_adjacency += 1;
            }
            prev = group;
        }
        assert!(distinct_adjacency > 900, "shuffle looks degenerate");
    }

    #[test]
    fn same_seed_identical_long_sequence() {
        let mut a = Bag::new(0x5EED);
        let mut b = Bag::new(0x5EED);
        for _ in 0..700 {
            assert_eq!(a.next(), b.next());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Bag::new(1);
        let mut b = Bag::new(2);
        let seq_a: Vec<Piece> = (0..70).map(|_| a.next()).collect();
        let seq_b: Vec<Piece> = (0..70).map(|_| b.next()).collect();
        assert_ne!(seq_a, seq_b);
    }

    #[test]
    fn peek_does_not_consume_and_matches_next() {
        let mut bag = Bag::new(0xABCD);
        for i in 0..100 {
            let n = i % 21;
            let peeked = bag.peek(n);
            assert_eq!(peeked.len(), n);
            assert_eq!(bag.peek(n), peeked, "repeat peek must be stable");
            for expected in peeked {
                assert_eq!(bag.next(), expected);
            }
        }
    }

    #[test]
    fn peek_beyond_current_refill_auto_refills() {
        let peeker = Bag::new(0xB00C);
        let mut drawer = Bag::new(0xB00C);
        let upcoming = peeker.peek(50);
        assert_eq!(upcoming.len(), 50);
        let drawn: Vec<Piece> = (0..50).map(|_| drawer.next()).collect();
        assert_eq!(upcoming, drawn);
        assert!(is_permutation(&upcoming[0..7]));
        assert!(is_permutation(&upcoming[42..49]));
    }

    #[test]
    fn peek_zero_returns_empty() {
        assert!(Bag::new(0).peek(0).is_empty());
    }

    #[test]
    fn all_pieces_appear_across_seeded_first_deals() {
        let mut seen = Vec::new();
        for seed in 0..200u64 {
            let first = Bag::new(seed).next();
            if !seen.contains(&first) {
                seen.push(first);
            }
        }
        assert_eq!(seen.len(), 7, "not all pieces appeared as first deal");
    }
}
