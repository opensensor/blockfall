//! Mutator framework (T23): per-run modifiers toggled on the mode-select
//! screen before a run starts.
//!
//! [`Mutators`] is a hand-rolled `u8` bitset — no new dependency; the bits:
//!
//! | bit   | mutator           | wiring |
//! |-------|-------------------|--------|
//! | `1`   | [`Mutators::NO_HOLD`] | the bridge drops `Action::Hold` before the core sees it (`core_bridge_system`) |
//! | `2`   | [`Mutators::NO_GHOST`] | render skips ghost cells (`render::render_playfield`); the core keeps computing `ghost_row` |
//! | `4`   | [`Mutators::ONE_PREVIEW`] | HUD next queue forces a single preview (`hud::sync_hud_previews`) |
//! | `8`   | [`Mutators::TWENTY_G`] | `start_level = 20` override in `GameCore::start_mode` (R1 config, plan R7) |
//! | `16`  | [`Mutators::HORROR`] | night render: the active piece is a downward flashlight — everything outside its widening beam dims to a dark silhouette and the well grid mutes under night-shade quads (`render::render_playfield` light factor) |
//! | `128` | [`Mutators::INVISIBLE`] | locked cells fade out in the render (`render::render_playfield` per-cell lock ages, T24) |
//!
//! ## Lifecycle
//!
//! The *selection* lives on the mode-select screen
//! ([`GameCore::selected_mutators`](crate::core_bridge::GameCore));
//! [`GameCore::start_mode`](crate::core_bridge::GameCore::start_mode)
//! snapshots it onto [`ActiveMode::mutators`](crate::core_bridge::ActiveMode)
//! at run start, and every consumer reads **only** the active run's
//! snapshot — changing the selection mid-run never affects the live run.
//! Mutated runs never write best records (the
//! [`Records::record_run_mutated`](crate::records::Records::record_run_mutated)
//! gate — owner decision) but still bump per-mode play counters
//! (`start_mode_run`). Nothing here persists across app restarts by design.

/// Per-run mutator bitset. `Default` (and [`Mutators::empty`]) is a clean
/// run; [`Mutators::is_empty`] is the "no records suppressed" predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mutators(pub u8);

impl Mutators {
    /// `Action::Hold` never reaches the core.
    pub const NO_HOLD: Self = Self(1);
    /// Ghost cells are not drawn (core still computes `ghost_row`).
    pub const NO_GHOST: Self = Self(2);
    /// HUD next queue shows exactly one preview.
    pub const ONE_PREVIEW: Self = Self(4);
    /// The run starts at level 20 (20G gravity cap).
    pub const TWENTY_G: Self = Self(8);
    /// Night render (`render::render_playfield`): the active piece is a
    /// flashlight — a halo around it plus a shaft of light cast downward,
    /// widening with depth and dimmed off the edges, sweeping with the
    /// piece; everything the beam misses is a dark silhouette and the well
    /// grid only shows where the beam lights it. Active piece always fully
    /// lit; the ghost light follows its own beam position. Render-only:
    /// the snapshot, replay and all HUD elements are untouched.
    pub const HORROR: Self = Self(16);
    /// Locked cells fade out in the render (grace then linear fade over 60
    /// fixed steps, see `render::lock_fade_alpha`); active piece and ghost
    /// untouched. Wired in T24.
    pub const INVISIBLE: Self = Self(128);

    /// The mutators the mode-select screen exposes as toggles, in fixed
    /// left-to-right UI order (deterministic layout order).
    pub const SELECTABLE: [Self; 6] = [
        Self::NO_HOLD,
        Self::NO_GHOST,
        Self::ONE_PREVIEW,
        Self::TWENTY_G,
        Self::INVISIBLE,
        Self::HORROR,
    ];

    /// No mutators (clean run).
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// `true` when no bit is set — a clean run whose records may be written.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// `true` when every bit of `other` is also set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Flip a single bit (the mode-select toggles call this).
    pub fn toggle(&mut self, bit: Self) {
        self.0 ^= bit.0;
    }

    /// Raw bit value (tests/debug).
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Short toggle label on the mode-select screen (`NO HOLD`, `20G`, …).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NO_HOLD => "NO HOLD",
            Self::NO_GHOST => "NO GHOST",
            Self::ONE_PREVIEW => "1 PREVIEW",
            Self::TWENTY_G => "20G",
            Self::INVISIBLE => "INVISIBLE",
            Self::HORROR => "HORROR",
            _ => "?",
        }
    }
}

impl std::ops::BitOr for Mutators {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_layout_is_disjoint_and_matches_the_plan() {
        assert_eq!(Mutators::NO_HOLD.bits(), 1);
        assert_eq!(Mutators::NO_GHOST.bits(), 2);
        assert_eq!(Mutators::ONE_PREVIEW.bits(), 4);
        assert_eq!(Mutators::TWENTY_G.bits(), 8);
        assert_eq!(Mutators::HORROR.bits(), 16);
        assert_eq!(Mutators::INVISIBLE.bits(), 128);
        let all = Mutators::SELECTABLE
            .into_iter()
            .fold(0u8, |acc, m| acc | m.bits());
        for m in Mutators::SELECTABLE {
            assert_eq!(all & m.bits(), m.bits(), "{m:?} is a distinct single bit");
        }
        // INVISIBLE uses the sign-free top bit, clear of everything else.
        assert_eq!(all, 1 | 2 | 4 | 8 | 16 | 128);
    }

    #[test]
    fn empty_contains_is_empty_toggle_semantics() {
        assert!(Mutators::empty().is_empty());
        assert!(Mutators::default().is_empty());
        assert!(!Mutators::empty().contains(Mutators::NO_HOLD));

        let mut m = Mutators::empty();
        m.toggle(Mutators::NO_HOLD);
        assert!(m.contains(Mutators::NO_HOLD));
        assert!(!m.contains(Mutators::NO_GHOST));
        assert!(!m.is_empty());
        assert_eq!(m, Mutators::NO_HOLD);

        m.toggle(Mutators::NO_GHOST);
        assert!(m.contains(Mutators::NO_HOLD | Mutators::NO_GHOST));
        m.toggle(Mutators::NO_HOLD);
        assert_eq!(m, Mutators::NO_GHOST);
        m.toggle(Mutators::NO_GHOST);
        assert!(m.is_empty());
    }

    /// T24 landed: INVISIBLE is selectable (the fade is rendered); the
    /// Horror night render joined after it, still last in the fixed
    /// left-to-right toggle order.
    #[test]
    fn selectable_runs_in_fixed_left_to_right_order() {
        assert!(Mutators::SELECTABLE.contains(&Mutators::INVISIBLE));
        assert!(Mutators::SELECTABLE.contains(&Mutators::HORROR));
        assert_eq!(
            Mutators::SELECTABLE,
            [
                Mutators::NO_HOLD,
                Mutators::NO_GHOST,
                Mutators::ONE_PREVIEW,
                Mutators::TWENTY_G,
                Mutators::INVISIBLE,
                Mutators::HORROR,
            ],
            "fixed left-to-right UI order"
        );
        // A set that carries the bits still behaves as a bitset.
        let m = Mutators(Mutators::NO_GHOST.bits() | Mutators::HORROR.bits());
        assert!(m.contains(Mutators::HORROR));
        assert!(m.contains(Mutators::NO_GHOST));
        assert!(!m.contains(Mutators::NO_HOLD));
        assert!(!m.contains(Mutators::INVISIBLE));
    }

    #[test]
    fn every_selectable_bit_has_a_label() {
        for m in Mutators::SELECTABLE {
            assert!(!m.label().is_empty() && m.label() != "?", "{m:?}");
        }
    }
}
