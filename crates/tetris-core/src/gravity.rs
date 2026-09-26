//! Gravity table (Tetris Worlds curve, 20G cap) at the 60 Hz logical clock (T5).
//!
//! Tetris Worlds gives `1/level` seconds per row for levels 1–19, capped at
//! 20G from level 20 on. At the core's purely logical [`TICK_HZ`] clock that
//! is `60/level` ticks per row, converted to whole ticks by rounding to the
//! nearest tick (halves round up). No wall-clock is involved.

/// Logical clock rate of the core, in ticks per second.
pub const TICK_HZ: u32 = 60;

/// First level at which gravity is capped at 20G.
pub const GRAVITY_CAP_LEVEL: u32 = 20;

/// Whole ticks between forced rows at the 20G cap (`TICK_HZ / 20`).
pub const TICKS_20G: u32 = TICK_HZ / GRAVITY_CAP_LEVEL;

/// Ticks between forced one-row drops at `level` (1-based).
///
/// Rounding rule: `round(60 / level)` in whole ticks, halves rounding up —
/// e.g. level 1 = 60 ticks (exactly 1 s/row), level 8 = 7.5 → 8 ticks,
/// level 19 = 3.16 → 3 ticks (0.050 s/row), level 20 = 3 ticks exactly.
///
/// Clamping: `level == 0` is treated defensively as level 1; every
/// `level >= GRAVITY_CAP_LEVEL` clamps to [`TICKS_20G`], so arbitrarily
/// large levels still yield the same 20G interval and the division below
/// only ever sees `1..=19`. Speeds beyond 20G are out of scope.
pub fn interval_for(level: u32) -> u32 {
    let level = level.max(1);
    if level >= GRAVITY_CAP_LEVEL {
        TICKS_20G
    } else {
        (2 * TICK_HZ + level) / (2 * level)
    }
}

/// Current level for a total of `lines_cleared_total` cleared lines:
/// one level up every 10 lines, starting at level 1.
pub fn level_for(lines_cleared_total: u32) -> u32 {
    lines_cleared_total / 10 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_hz_is_60() {
        assert_eq!(TICK_HZ, 60);
    }

    #[test]
    fn level_1_is_one_row_per_second() {
        assert_eq!(interval_for(1), 60);
    }

    #[test]
    fn level_19_rounds_to_3_ticks() {
        // 60/19 = 3.158 ticks; round-to-nearest yields 3 ticks (0.0500 s/row).
        assert_eq!(interval_for(19), 3);
    }

    #[test]
    fn level_20_is_20g_cap() {
        assert_eq!(interval_for(20), 3);
        assert_eq!(TICKS_20G, 3);
    }

    #[test]
    fn level_25_clamps_to_cap() {
        assert_eq!(interval_for(25), interval_for(20));
    }

    #[test]
    fn huge_levels_clamp_to_cap() {
        assert_eq!(interval_for(u32::MAX), TICKS_20G);
    }

    #[test]
    fn level_0_is_treated_as_level_1() {
        assert_eq!(interval_for(0), interval_for(1));
    }

    #[test]
    fn exact_halves_and_thirds() {
        assert_eq!(interval_for(2), 30);
        assert_eq!(interval_for(3), 20);
        assert_eq!(interval_for(4), 15);
    }

    #[test]
    fn half_rounds_up() {
        // 60/8 = 7.5 -> 8 ticks with round-half-up.
        assert_eq!(interval_for(8), 8);
    }

    #[test]
    fn level_for_line_boundaries() {
        assert_eq!(level_for(9), 1);
        assert_eq!(level_for(10), 2);
        assert_eq!(level_for(19), 2);
        assert_eq!(level_for(20), 3);
    }

    #[test]
    fn level_for_zero_lines_is_level_1() {
        assert_eq!(level_for(0), 1);
    }

    #[test]
    fn level_for_huge_line_counts_does_not_overflow() {
        assert_eq!(level_for(u32::MAX), u32::MAX / 10 + 1);
    }

    #[test]
    fn interval_is_monotonic_non_increasing_over_levels_1_to_30() {
        for level in 1..30u32 {
            assert!(
                interval_for(level + 1) <= interval_for(level),
                "level {} ({}) faster than level {} ({})",
                level + 1,
                interval_for(level + 1),
                level,
                interval_for(level)
            );
        }
    }

    #[test]
    fn level_advances_once_per_ten_lines() {
        for lines in 0..100u32 {
            let expected = 1 + lines / 10;
            assert_eq!(level_for(lines), expected);
        }
    }
}
