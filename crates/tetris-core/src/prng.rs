//! Dependency-free seedable PRNG (splitmix64/xorshift) (T3).
//!
//! SplitMix64: 16 bytes of state, no dependencies, fully deterministic —
//! the same seed always yields the same stream. Used by [`crate::bag`] for
//! reproducible piece sequences; no std RNG and no wall-clock is involved.

/// SplitMix64 generator (Steele et al., "Fast Splittable Pseudorandom
/// Number Generators", OCPL 2014) — the standard `splitmix64` variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rng {
    state: u64,
}

/// Golden-ratio increment of the 2^64 additive sequence.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

impl Rng {
    /// Create a generator from `seed`. Every seed, including `0`, produces
    /// a well-mixed stream.
    pub fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    /// Advance the state and return the next 64-bit output.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GOLDEN);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..n` via unbiased multiply-shift (Lemire).
    ///
    /// Panics in debug builds if `n == 0`.
    pub fn next_below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0, "next_below requires n > 0");
        let wide = (self.next_u64() as u128).wrapping_mul(n as u128);
        (wide >> 64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_produces_identical_stream() {
        let mut a = Rng::new(0x1234_5678_9ABC_DEF0);
        let mut b = Rng::new(0x1234_5678_9ABC_DEF0);
        for _ in 0..10_000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let a: Vec<u64> = (0..64)
            .scan(Rng::new(1), |r, _| Some(r.next_u64()))
            .collect();
        let b: Vec<u64> = (0..64)
            .scan(Rng::new(2), |r, _| Some(r.next_u64()))
            .collect();
        assert_ne!(a, b);
    }

    #[test]
    fn degenerate_seeds_still_produce_output() {
        for seed in [0, 1, u64::MAX] {
            let mut r = Rng::new(seed);
            let mut prev = r.next_u64();
            for _ in 0..100 {
                let x = r.next_u64();
                assert_ne!(x, prev, "constant stream for seed {seed}");
                prev = x;
            }
        }
    }

    #[test]
    fn outputs_span_the_full_u64_range() {
        let mut r = Rng::new(0xDEAD_BEEF);
        let has_high = (0..1000).any(|_| r.next_u64() >= (1 << 63));
        let has_low = (0..1000).any(|_| r.next_u64() < (1 << 63));
        assert!(has_high && has_low);
    }

    #[test]
    fn next_below_stays_in_range() {
        let mut r = Rng::new(42);
        for _ in 0..10_000 {
            assert!(r.next_below(7) < 7);
            assert_eq!(r.next_below(1), 0);
            assert!(r.next_below(u64::MAX) < u64::MAX);
        }
    }

    #[test]
    fn next_below_is_deterministic_and_uses_high_bits() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let mut saw_max_bucket = false;
        for _ in 0..10_000 {
            let x = a.next_below(1_000_000);
            assert_eq!(x, b.next_below(1_000_000));
            saw_max_bucket |= x >= 900_000;
        }
        assert!(saw_max_bucket, "next_below never reaches high buckets");
    }
}
