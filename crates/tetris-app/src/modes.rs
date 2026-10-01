//! Mode catalogue (game-modes plan T5): the **single source of truth** for
//! all ten modes' `ModeConfig`s, display names, descriptions, pre-roll
//! budgets, shipped flags and records keys.
//!
//! All ten [`ModeId`] variants are pre-declared here so later mode tasks
//! (T13 Survival, T14 Zen, T16 Bot Ladder, T17 Daily, T20 Dig Duel, T21
//! Switch) only flip [`is_shipped`] and refine their config row — menus
//! (T7), HUD (T8) and result screens (T9) iterate the catalogue without
//! editing this file.
//!
//! Pre-roll counts are **app-bridge tick budgets**, never `ModeConfig`
//! fields: the core never reads a clock (plan constraint 3), and the bridge
//! gates stepping on them so core tick 0 == first playable frame (see
//! [`crate::core_bridge::Countdown`]).

use tetris_core::mode::{BlockOutBehavior, GarbageFeed, Goal, ModeConfig, StartBoard};

use crate::records::{self, ModeKey};

/// Fixed-step rate the tick budgets below are expressed in (matches
/// [`crate::core_bridge::SIM_HZ`]; a `u64` here for integer formatting).
const TICKS_PER_SECOND: u64 = 60;

/// Pre-roll length for Sprint/Dig, in fixed steps (3 s at 60 Hz).
pub const PRE_ROLL_TICKS: u32 = 180;

/// Ultra score-attack clock budget (6 minutes at 60 Hz).
pub const ULTRA_CLOCK_TICKS: u64 = 7200;

/// Sprint win condition: lines to clear.
pub const SPRINT_GOAL_LINES: u32 = 40;

/// Dig start board: buried garbage rows.
pub const DIG_GARBAGE_ROWS: usize = 10;

/// Every mode the game knows about — the full ten-mode catalogue (T5).
///
/// The four Release-1 solo modes are shipped; the rest carry placeholder or
/// best-effort configs that their own tasks refine (see each
/// [`mode_config`] arm). Versus campaigns/rules (BotLadder, DigLadder →
/// DigDuel, Switch) never start through the solo bridge; their configs are
/// unused there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModeId {
    /// Endless marathon with the classic level curve (the legacy mode).
    Marathon,
    /// Race 40 lines against the clock at fixed level 1.
    Sprint,
    /// 6-minute score attack with marathon progression.
    Ultra,
    /// Dig through 10 rows of buried garbage at fixed level 1.
    Dig,
    /// Outlast a rising garbage feed until it buries you (R2, T12+T13).
    Survival,
    /// Endless relaxation; a block-out wipes the stack (R2, T14).
    Zen,
    /// Eight-rung bot ladder campaign (R2, T16).
    BotLadder,
    /// One seeded mode per day, resolved at runtime (R2, T17).
    Daily,
    /// Versus: both sides dig the same buried board (R3, T20).
    DigDuel,
    /// Versus: whole game states swap on a timer (R3, T21).
    Switch,
}

impl ModeId {
    /// The full catalogue in display order.
    pub const ALL: [ModeId; 10] = [
        ModeId::Marathon,
        ModeId::Sprint,
        ModeId::Ultra,
        ModeId::Dig,
        ModeId::Survival,
        ModeId::Zen,
        ModeId::BotLadder,
        ModeId::Daily,
        ModeId::DigDuel,
        ModeId::Switch,
    ];
}

/// The rules a solo `Game` is built from for `id`
/// (`Game::with_config(seed, &mode_config(id))` via
/// [`crate::core_bridge::start_mode_run`]).
#[must_use]
pub fn mode_config(id: ModeId) -> ModeConfig {
    match id {
        ModeId::Marathon => ModeConfig::default(),
        ModeId::Sprint => ModeConfig {
            start_level: 1,
            levels_advance: false,
            goal: Some(Goal::Lines(SPRINT_GOAL_LINES)),
            clock_ticks: None,
            start_board: None,
            on_block_out: BlockOutBehavior::End,
            ..ModeConfig::default()
        },
        ModeId::Ultra => ModeConfig {
            clock_ticks: Some(ULTRA_CLOCK_TICKS),
            ..ModeConfig::default()
        },
        ModeId::Dig => ModeConfig {
            start_level: 1,
            levels_advance: false,
            goal: Some(Goal::GarbageCleared),
            clock_ticks: None,
            start_board: Some(StartBoard::BuriedGarbage {
                rows: DIG_GARBAGE_ROWS,
            }),
            on_block_out: BlockOutBehavior::End,
            ..ModeConfig::default()
        },
        // Survival (T12/T13 shipped): marathon progression, no goal/clock —
        // the core's timed garbage feed queues a rising row every interval
        // and lands queued rows on each lock until the stack buries you.
        ModeId::Survival => ModeConfig {
            garbage_feed: Some(GarbageFeed::default()),
            ..ModeConfig::default()
        },
        // T14 refines the wipe behavior (until then `WipeAndContinue`
        // behaves like `End` in the core) and adds the lifetime-lines HUD.
        ModeId::Zen => ModeConfig {
            start_level: 1,
            levels_advance: false,
            goal: None,
            clock_ticks: None,
            start_board: None,
            on_block_out: BlockOutBehavior::WipeAndContinue,
            ..ModeConfig::default()
        },
        // Versus campaign: runs through `start_versus` (T16), never the solo
        // bridge; the config is unused.
        ModeId::BotLadder => ModeConfig::default(),
        // Resolved at runtime by T17 to one of Sprint/Ultra/Dig (weekday
        // rotation + date seed) *before* starting; a bare Daily start uses
        // this placeholder.
        ModeId::Daily => ModeConfig::default(),
        // Versus rules (T20/T21) travel as `AttackRule` variants on
        // `MatchStart`, not as solo configs; unused by the solo bridge.
        ModeId::DigDuel => ModeConfig::default(),
        ModeId::Switch => ModeConfig::default(),
    }
}

/// Menu display name for `id`.
#[must_use]
pub fn display_name(id: ModeId) -> &'static str {
    match id {
        ModeId::Marathon => "Marathon",
        ModeId::Sprint => "Sprint",
        ModeId::Ultra => "Ultra",
        ModeId::Dig => "Dig",
        ModeId::Survival => "Survival",
        ModeId::Zen => "Zen",
        ModeId::BotLadder => "Bot Ladder",
        ModeId::Daily => "Daily",
        ModeId::DigDuel => "Dig Duel",
        ModeId::Switch => "Switch",
    }
}

/// One-line menu description for `id`.
#[must_use]
pub fn description(id: ModeId) -> &'static str {
    match id {
        ModeId::Marathon => "Endless lines on the classic level curve.",
        ModeId::Sprint => "Clear 40 lines as fast as you can.",
        ModeId::Ultra => "Six minutes. Highest score wins.",
        ModeId::Dig => "Dig through ten rows of buried garbage.",
        ModeId::Survival => "Outlast the ever-rising garbage feed.",
        ModeId::Zen => "Relax: a top-out just wipes the stack.",
        ModeId::BotLadder => "Climb eight bots, each faster than the last.",
        ModeId::Daily => "One seeded mode per day. Everyone gets the same one.",
        ModeId::DigDuel => "Race a rival through the same garbage.",
        ModeId::Switch => "Entire boards swap on a timer.",
    }
}

/// `true` for the shipped modes (R1's four solo modes plus Survival, T13).
/// Later tasks flip this for their own mode — the **only** catalogue edit
/// they need, no new rows; the mode-select list filters `ModeId::ALL`
/// through this flag.
#[must_use]
pub fn is_shipped(id: ModeId) -> bool {
    matches!(
        id,
        ModeId::Marathon | ModeId::Sprint | ModeId::Ultra | ModeId::Dig | ModeId::Survival
    )
}

/// Solo-start pre-roll budget for `id`, in fixed steps: Sprint and Dig get a
/// 3 s 3-2-1 countdown (the bridge gates stepping until it drains; core tick
/// 0 == first playable frame), everything else starts immediately.
#[must_use]
pub fn pre_roll_ticks(id: ModeId) -> u32 {
    match id {
        ModeId::Sprint | ModeId::Dig => PRE_ROLL_TICKS,
        _ => 0,
    }
}

/// The [`records::Records`] key for `id` — menus, play counters and the
/// catalogue share this one mapping.
#[must_use]
pub fn mode_key(id: ModeId) -> ModeKey {
    match id {
        ModeId::Marathon => records::MARATHON,
        ModeId::Sprint => records::SPRINT,
        ModeId::Ultra => records::ULTRA,
        ModeId::Dig => records::DIG,
        ModeId::Survival => records::SURVIVAL,
        ModeId::Zen => records::ZEN,
        ModeId::BotLadder => records::BOT_LADDER,
        ModeId::Daily => records::DAILY,
        ModeId::DigDuel => records::DIG_DUEL,
        ModeId::Switch => records::SWITCH,
    }
}

/// Format a fixed-step tick count as `m:ss.hh` centiseconds, e.g. 9 835
/// ticks → `2:43.91`. 60 Hz floor division; the centisecond remainder is
/// **truncated** (never rounded up), so the display never shows time the
/// run has not actually taken.
#[must_use]
pub fn format_time_ticks(ticks: u64) -> String {
    let total_centis = ticks * 100 / TICKS_PER_SECOND;
    let centis = total_centis % 100;
    let total_secs = total_centis / 100;
    let secs = total_secs % 60;
    let mins = total_secs / 60;
    format!("{mins}:{secs:02}.{centis:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_time_ticks_truncates_at_sixty_hz() {
        assert_eq!(format_time_ticks(0), "0:00.00");
        assert_eq!(format_time_ticks(1), "0:00.01", "1 tick = 1.66cs → floor 1");
        assert_eq!(format_time_ticks(9835), "2:43.91");
        // The plan's example reads "10 235 → 2:43.91"; at 60 Hz floor
        // division 10 235 ticks is 170.58 s — 2:43.91 is *9 835* ticks
        // (plan typo, see T5 log). Both correct values are pinned here.
        assert_eq!(format_time_ticks(10235), "2:50.58");
    }

    #[test]
    fn format_time_ticks_minute_rollover_boundaries() {
        assert_eq!(format_time_ticks(3599), "0:59.98", "just under a minute");
        assert_eq!(format_time_ticks(3600), "1:00.00", "exactly a minute");
        assert_eq!(format_time_ticks(3601), "1:00.01");
        assert_eq!(format_time_ticks(215_999), "59:59.98");
        assert_eq!(format_time_ticks(216_000), "60:00.00", "no hour rollover");
    }

    #[test]
    fn pre_roll_budgets_match_the_ship_list() {
        assert_eq!(pre_roll_ticks(ModeId::Sprint), 180);
        assert_eq!(pre_roll_ticks(ModeId::Dig), 180);
        assert_eq!(pre_roll_ticks(ModeId::Marathon), 0);
        assert_eq!(pre_roll_ticks(ModeId::Ultra), 0);
    }

    /// The shipped set as of R2: the four Release-1 solo modes plus Survival
    /// (T13). The mode-select screen (T7) filters `ModeId::ALL` through this
    /// flag — shipping a mode is the **only** catalogue edit needed; the row
    /// list stays data-driven (`screens_modes::rows_are_data_driven…`).
    #[test]
    fn is_shipped_matches_the_shipped_set() {
        let shipped: Vec<ModeId> = ModeId::ALL
            .iter()
            .copied()
            .filter(|id| is_shipped(*id))
            .collect();
        assert_eq!(
            shipped,
            vec![
                ModeId::Marathon,
                ModeId::Sprint,
                ModeId::Ultra,
                ModeId::Dig,
                ModeId::Survival,
            ]
        );
        for id in [
            ModeId::Zen,
            ModeId::BotLadder,
            ModeId::Daily,
            ModeId::DigDuel,
            ModeId::Switch,
        ] {
            assert!(!is_shipped(id), "{id:?} is not shipped yet");
        }
    }

    #[test]
    fn mode_key_mapping_is_bijection_onto_all_record_keys() {
        let keys: Vec<ModeKey> = ModeId::ALL.iter().copied().map(mode_key).collect();
        for record_key in records::ALL_MODE_KEYS {
            assert!(
                keys.contains(&record_key),
                "{record_key} missing from the ModeId mapping"
            );
        }
        assert_eq!(mode_key(ModeId::Marathon), records::MARATHON);
        assert_eq!(mode_key(ModeId::BotLadder), records::BOT_LADDER);
        assert_eq!(mode_key(ModeId::DigDuel), records::DIG_DUEL);
    }

    #[test]
    fn catalogue_configs_match_the_locked_owner_decisions() {
        let sprint = mode_config(ModeId::Sprint);
        assert_eq!(sprint.goal, Some(Goal::Lines(40)));
        assert_eq!(sprint.start_level, 1);
        assert!(!sprint.levels_advance, "Sprint gravity fixed at level 1");

        let ultra = mode_config(ModeId::Ultra);
        assert_eq!(ultra.clock_ticks, Some(7200));
        assert!(ultra.levels_advance, "Ultra uses marathon progression");
        assert_eq!(ultra.goal, None);

        let dig = mode_config(ModeId::Dig);
        assert_eq!(dig.goal, Some(Goal::GarbageCleared));
        assert_eq!(
            dig.start_board,
            Some(StartBoard::BuriedGarbage { rows: 10 })
        );
        assert!(!dig.levels_advance);

        // Survival (T13): marathon progression + the T12 garbage feed,
        // no goal and no clock.
        let survival = mode_config(ModeId::Survival);
        assert_eq!(survival.garbage_feed, Some(GarbageFeed::default()));
        assert_eq!(survival.goal, None);
        assert_eq!(survival.clock_ticks, None);
        assert!(
            survival.levels_advance,
            "Survival uses marathon progression"
        );

        assert_eq!(mode_config(ModeId::Marathon), ModeConfig::default());
    }
}
