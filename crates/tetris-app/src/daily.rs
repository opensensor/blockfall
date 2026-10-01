//! Daily Challenge (T17): one shared-seed run per UTC day — comparable
//! between friends with **no server**.
//!
//! The whole feature lives app-side: the **core never sees a date**. This is
//! the only module that turns wall-clock time into a calendar date (mirrors
//! the `wall_clock_seed` precedent in [`crate::core_bridge`]), and it derives
//! the date from `SystemTime` with a hand-rolled civil-date algorithm — no
//! chrono, no new dependencies.
//!
//! ## The recipe for "today"
//!
//! - [`utc_today`] → [`CivilDate`] (days-since-epoch split via Howard
//!   Hinnant's `civil_from_days`).
//! - [`daily_seed`] = splitmix64 over the packed date — stable per date,
//!   avalanche-distinct across dates.
//! - [`daily_mode`] picks the day's mode from [`DAILY_ROTATION`] indexed by
//!   `weekday(Mon = 0) % 3`: **Mon Sprint, Tue Ultra, Wed Dig, Thu Sprint,
//!   Fri Ultra, Sat Dig, Sun Sprint** — Sunday rolls back onto the head of
//!   the cycle, so the three modes stay evenly spread.
//! - A run started via the Daily banner carries a [`DailyAttempt`] marker on
//!   [`VersusFlow`](crate::screens_menu::VersusFlow) (T16's
//!   flow-resource-field precedent — zero new Bevy resources). Only that
//!   marker makes a run "daily"; the underlying mode plays normally from its
//!   own row and never records anything daily.
//!
//! ## First-run-wins-for-day
//!
//! T6's `record_run` treats [`Record::Daily`] as "latest content wins", so
//! the date gate lives in **this** layer: [`finish_daily_attempt`] ignores
//! any completed run once today's date is stored, and a new day replaces the
//! stored entry. The display string is `1:42.35` for the time modes (Sprint,
//! Dig) and the plain score for Ultra, per the share line:
//! `Blockfall Daily 2026-10-01 · Dig · 1:42.35` (U+00B7 separators). Bevy
//! 0.19 has no clipboard, so the line is display-only text (owner decision).

use std::cell::Cell;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use tetris_core::mode::FinishReason;

use crate::core_bridge::{start_mode_run_with_seed, Countdown, GameCore};
use crate::modes::{display_name, format_time_ticks, ModeId};
use crate::records::{Record, Records, DAILY};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Civil date (chrono-free)
// ---------------------------------------------------------------------------

/// A proleptic-Gregorian civil date (UTC). Built from days-since-epoch so
/// the whole calendar math is integer-only and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CivilDate {
    /// Proleptic Gregorian year (e.g. 2026).
    pub year: i64,
    /// Month `1..=12`.
    pub month: u32,
    /// Day of month `1..=31`.
    pub day: u32,
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's
/// `days_from_civil`).
const fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // year of era [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // day of year [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // day of era [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// Inverse of [`days_from_civil`]: split days-since-epoch into
/// `(year, month, day)`.
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl CivilDate {
    /// Assemble from calendar parts (no validation — inputs come from
    /// [`Self::from_epoch_days`] or tests).
    #[must_use]
    pub const fn from_ymd(year: i64, month: u32, day: u32) -> Self {
        Self { year, month, day }
    }

    /// Epoch day (days since 1970-01-01).
    #[must_use]
    pub const fn epoch_days(self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    /// The date `days` epoch days after 1970-01-01.
    #[must_use]
    pub const fn from_epoch_days(days: i64) -> Self {
        let (year, month, day) = civil_from_days(days);
        Self { year, month, day }
    }

    /// Weekday with **Monday = 0 … Sunday = 6** (1970-01-01 was a Thursday,
    /// hence the +3).
    #[must_use]
    pub const fn weekday_mon0(self) -> u32 {
        // `rem_euclid` is not `const`; epoch days are >= 0 for any sane
        // clock, and the +3 keeps the arithmetic in the non-negative range.
        let days = self.epoch_days();
        if days + 3 >= 0 {
            ((days + 3) % 7) as u32
        } else {
            (((days + 3) % 7) + 7) as u32
        }
    }
}

impl fmt::Display for CivilDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Today's **UTC** civil date from the system clock (whole days since the
/// epoch — floor division, so times before the epoch stay sane).
#[must_use]
pub fn utc_today() -> CivilDate {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    CivilDate::from_epoch_days(secs.div_euclid(86_400))
}

thread_local! {
    /// Test seam: forces [`today`] (headless tests pin "today" so the
    /// rotation/seed assertions are deterministic). Same pattern as
    /// [`crate::render::set_portrait_override`]: a thread-local the tests
    /// set before driving `App::update`, which runs on the same thread.
    static TODAY_OVERRIDE: Cell<Option<CivilDate>> = const { Cell::new(None) };
}

/// Install (or clear with `None`) a fixed "today" for the current thread.
/// Tests that assert on today's rotation/seed/label must set it for their
/// whole duration and clear it afterwards.
pub fn set_today_override(date: Option<CivilDate>) {
    TODAY_OVERRIDE.with(|slot| slot.set(date));
}

/// The app's notion of "today": the override when one is installed, else
/// [`utc_today`].
#[must_use]
pub fn today() -> CivilDate {
    TODAY_OVERRIDE
        .with(|slot| slot.get())
        .unwrap_or_else(utc_today)
}

// ---------------------------------------------------------------------------
// Rotation + seed
// ---------------------------------------------------------------------------

/// The day's mode rotates Sprint → Ultra → Dig across the weekdays, indexed
/// by `weekday(Mon = 0) % 3`: **Mon Sprint, Tue Ultra, Wed Dig, Thu Sprint,
/// Fri Ultra, Sat Dig, Sun Sprint**. Sunday rolls back onto the head of the
/// cycle (6 % 3 == 0), so every mode lands on two weekdays and none on
/// three.
pub const DAILY_ROTATION: [ModeId; 3] = [ModeId::Sprint, ModeId::Ultra, ModeId::Dig];

/// Today's daily-challenge mode for `date` ([`DAILY_ROTATION`] indexed by
/// weekday-0 modulo 3).
#[must_use]
pub fn daily_mode(date: CivilDate) -> ModeId {
    DAILY_ROTATION[(date.weekday_mon0() % 3) as usize]
}

/// splitmix64 finalizer (the canonical one: golden-ratio bump + two
/// xorshift-multiply rounds).
#[must_use]
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The day's shared seed: splitmix64 over the packed `(year, month, day)`.
/// Deterministic forever (date-based, never wall-clock-at-press), distinct
/// across neighbouring dates, and avalanche-scrambled so a shared seed
/// reveals nothing exploitable about the layout directly.
#[must_use]
pub fn daily_seed(date: CivilDate) -> u64 {
    let packed = ((date.year as u64) << 16) | ((date.month as u64) << 8) | u64::from(date.day);
    splitmix64(packed)
}

// ---------------------------------------------------------------------------
// Result strings + share line
// ---------------------------------------------------------------------------

/// The daily run's **display result** under the finished mode's rules, or
/// `None` when the terminal earns nothing: Sprint/Dig produce a centisecond
/// time only on `GoalReached` (top-out gives no result), Ultra's score
/// stands on `TimeUp` *and* `TopOut` (PRD).
#[must_use]
pub fn daily_result_string(
    mode: ModeId,
    reason: FinishReason,
    ticks: u64,
    score: u64,
) -> Option<String> {
    match (mode, reason) {
        (ModeId::Sprint | ModeId::Dig, FinishReason::GoalReached) => Some(format_time_ticks(ticks)),
        (ModeId::Ultra, FinishReason::TimeUp | FinishReason::TopOut) => Some(score.to_string()),
        _ => None,
    }
}

/// The share line, exact format: `Blockfall Daily 2026-10-01 · Dig · 1:42.35`
/// — date `YYYY-MM-DD`, U+00B7 middle-dot separators. Display-only text
/// (Bevy 0.19 has no clipboard — owner decision).
#[must_use]
pub fn share_line(date: CivilDate, mode: ModeId, result: &str) -> String {
    format!(
        "Blockfall Daily {} \u{b7} {} \u{b7} {}",
        date,
        display_name(mode),
        result
    )
}

// ---------------------------------------------------------------------------
// Daily attempt marker (lives on the existing VersusFlow resource)
// ---------------------------------------------------------------------------

/// Marks a live run as started via the Daily banner, carrying the date it
/// started on (the seed's date — so a run across the UTC midnight boundary
/// still records for the day it was played). [`VersusFlow::daily`] (T16's
/// `LadderOrigin` precedent) holds it; **zero new Bevy resources**.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DailyAttempt {
    /// No daily run is live (every normal start ends here).
    #[default]
    Idle,
    /// A daily run is live, started on `date`.
    Active {
        /// The date whose seed/mode the run was started from.
        date: CivilDate,
    },
}

/// `true` while `attempt` marks a live daily run.
#[must_use]
pub fn is_daily_run_active(attempt: DailyAttempt) -> bool {
    matches!(attempt, DailyAttempt::Active { .. })
}

// ---------------------------------------------------------------------------
// Start + record (the flow entry points)
// ---------------------------------------------------------------------------

/// Daily banner press: start **today's** mode with **today's** seed through
/// the single shared start path — [`start_mode_run_with_seed`] is the
/// bookkeeping home (pre-roll re-arm, play bump); the forced seed beats
/// `TETRIS_SEED` because the whole point is that everyone plays the same
/// board. Returns `(date, mode, seed)`; the caller stores
/// [`DailyAttempt::Active { date }`] on the flow marker.
pub fn start_daily(
    core: &mut GameCore,
    countdown: &mut Countdown,
    app_state: &mut AppState,
    records: Option<&mut Records>,
) -> (CivilDate, ModeId, u64) {
    let date = today();
    let mode = daily_mode(date);
    let seed = start_mode_run_with_seed(
        mode,
        core,
        countdown,
        app_state,
        records,
        Some(daily_seed(date)),
    );
    (date, mode, seed)
}

/// Fold a daily attempt's terminal into [`Records`] under the
/// **first-run-wins-for-day** rule. T6's `record_run` replaces
/// [`Record::Daily`] content on any difference, so the date gate lives here:
///
/// - mode rule earns nothing (Sprint/Dig top-out) ⇒ `None`, nothing written;
/// - nothing stored for `date` yet ⇒ write `Daily { date, result }`
///   (`true` ⇒ caller force-flushes the save queue);
/// - `date` already stored ⇒ change nothing — the returned share line quotes
///   the **stored** (first) result.
///
/// The return is `Some((share_line, wrote))` for a completed daily run.
#[must_use]
pub fn finish_daily_attempt(
    records: &mut Records,
    date: CivilDate,
    mode: ModeId,
    reason: FinishReason,
    ticks: u64,
    score: u64,
) -> Option<(String, bool)> {
    let result = daily_result_string(mode, reason, ticks, score)?;
    let date_str = date.to_string();
    let (shown, wrote) = match records.record_for(DAILY) {
        Some(Record::Daily {
            date: stored,
            result: stored_result,
        }) if *stored == date_str => (stored_result.clone(), false),
        _ => {
            records.record_run(
                DAILY,
                Record::Daily {
                    date: date_str,
                    result: result.clone(),
                },
            );
            (result, true)
        }
    };
    Some((share_line(date, mode, &shown), wrote))
}

/// One-line banner text for the mode-select screen: `Daily · Dig — Not yet`
/// before the first completion, `Daily · Dig — 1:42.35` (the stored result)
/// once today's run is done. Stale records (another date) read "Not yet".
#[must_use]
pub fn banner_text(record: Option<&Record>) -> String {
    let date = today();
    let status = match record {
        Some(Record::Daily {
            date: stored,
            result,
        }) if *stored == date.to_string() => result.as_str(),
        _ => "Not yet",
    };
    format!(
        "Daily \u{b7} {} \u{2014} {status}",
        display_name(daily_mode(date))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::Record;
    use std::collections::HashSet;

    fn date(y: i64, m: u32, d: u32) -> CivilDate {
        CivilDate::from_ymd(y, m, d)
    }

    // ---- civil date math ----

    #[test]
    fn civil_date_math_round_trips_known_dates() {
        assert_eq!(date(1970, 1, 1).epoch_days(), 0);
        assert_eq!(date(2026, 10, 1).epoch_days(), 20_727);
        assert_eq!(CivilDate::from_epoch_days(0), date(1970, 1, 1));
        assert_eq!(CivilDate::from_epoch_days(-1), date(1969, 12, 31));
        assert_eq!(CivilDate::from_epoch_days(20_727), date(2026, 10, 1));
        // Leap-day sanity.
        assert_eq!(
            date(2024, 3, 1).epoch_days() - date(2024, 2, 28).epoch_days(),
            2
        );
        for offset in [0, 1, 400, 20_727, 50_000] {
            let d = CivilDate::from_epoch_days(offset);
            assert_eq!(d.epoch_days(), offset, "round trip at {offset}");
        }
    }

    #[test]
    fn weekdays_are_monday_zero_and_2026_10_01_is_thursday() {
        // 1970-01-01 was a Thursday.
        assert_eq!(date(1970, 1, 1).weekday_mon0(), 3);
        // The plan's "today" — Thursday.
        assert_eq!(date(2026, 10, 1).weekday_mon0(), 3);
        assert_eq!(
            date(2026, 9, 28).weekday_mon0(),
            0,
            "2026-09-28 is a Monday"
        );
        assert_eq!(date(2026, 10, 4).weekday_mon0(), 6, "Sunday");
    }

    #[test]
    fn display_is_zero_padded_iso() {
        assert_eq!(date(2026, 10, 1).to_string(), "2026-10-01");
        assert_eq!(date(1999, 1, 9).to_string(), "1999-01-09");
    }

    #[test]
    fn utc_today_sane_without_override() {
        set_today_override(None);
        let d = utc_today();
        assert!(d.year >= 2026 && d.year < 2100, "clock sanity: {d}");
        assert_eq!(
            d.epoch_days(),
            CivilDate::from_epoch_days(d.epoch_days()).epoch_days()
        );
    }

    #[test]
    fn today_follows_the_thread_local_override() {
        set_today_override(Some(date(2026, 10, 1)));
        assert_eq!(today(), date(2026, 10, 1));
        set_today_override(None);
        assert_eq!(today(), utc_today());
    }

    // ---- seed + rotation ----

    #[test]
    fn date_seed_stable_and_distinct_across_ten_dates() {
        let dates = [
            date(2026, 9, 28),
            date(2026, 9, 29),
            date(2026, 9, 30),
            date(2026, 10, 1),
            date(2026, 10, 2),
            date(2026, 11, 1),
            date(2026, 12, 31),
            date(2027, 1, 1),
            date(2000, 2, 29),
            date(1999, 12, 31),
        ];
        let mut seen = HashSet::new();
        for d in dates {
            assert_eq!(daily_seed(d), daily_seed(d), "stable across calls");
            assert!(seen.insert(daily_seed(d)), "seeds distinct for {d}");
        }
        // Neighbours differ (avalanche).
        assert_ne!(daily_seed(date(2026, 10, 1)), daily_seed(date(2026, 10, 2)));
    }

    #[test]
    fn rotation_maps_the_seven_weekdays_in_order() {
        // 2026-09-28 is a Monday; the 7-day cycle from it is pinned.
        let monday = date(2026, 9, 28).epoch_days();
        let week: Vec<ModeId> = (0..7)
            .map(|i| daily_mode(CivilDate::from_epoch_days(monday + i)))
            .collect();
        assert_eq!(
            week,
            vec![
                ModeId::Sprint, // Mon
                ModeId::Ultra,  // Tue
                ModeId::Dig,    // Wed
                ModeId::Sprint, // Thu
                ModeId::Ultra,  // Fri
                ModeId::Dig,    // Sat
                ModeId::Sprint, // Sun rolls back onto the cycle head
            ]
        );
        assert_eq!(DAILY_ROTATION, [ModeId::Sprint, ModeId::Ultra, ModeId::Dig]);
    }

    // ---- share line ----

    #[test]
    fn share_line_exact_format() {
        // 6 141 ticks at 60 Hz floor = 102.35 s → "1:42.35" (zero-padded).
        assert_eq!(format_time_ticks(6_141), "1:42.35");
        assert_eq!(
            share_line(date(2026, 10, 1), ModeId::Dig, "1:42.35"),
            "Blockfall Daily 2026-10-01 \u{b7} Dig \u{b7} 1:42.35"
        );
        assert_eq!(
            share_line(date(2026, 9, 1), ModeId::Ultra, "12 345"),
            "Blockfall Daily 2026-09-01 \u{b7} Ultra \u{b7} 12 345"
        );
    }

    #[test]
    fn daily_result_strings_follow_the_mode_rules() {
        assert_eq!(
            daily_result_string(ModeId::Sprint, FinishReason::GoalReached, 6_141, 99),
            Some("1:42.35".to_string())
        );
        assert_eq!(
            daily_result_string(ModeId::Dig, FinishReason::GoalReached, 3_600, 0),
            Some("1:00.00".to_string())
        );
        assert_eq!(
            daily_result_string(ModeId::Ultra, FinishReason::TimeUp, 7_200, 12_345),
            Some("12345".to_string())
        );
        assert_eq!(
            daily_result_string(ModeId::Ultra, FinishReason::TopOut, 500, 40),
            Some("40".to_string()),
            "Ultra's score stands on a top-out"
        );
        assert_eq!(
            daily_result_string(ModeId::Sprint, FinishReason::TopOut, 500, 40),
            None,
            "a Sprint daily top-out earns nothing"
        );
        assert_eq!(
            daily_result_string(ModeId::Dig, FinishReason::TopOut, 500, 40),
            None
        );
    }

    // ---- first-run-wins gate ----

    #[test]
    fn first_completed_run_wins_for_the_day_and_a_new_day_replaces() {
        let mut records = Records::default();
        let monday = date(2026, 9, 28); // Sprint day

        // First completed run: written, share line quotes our result.
        let (line, wrote) = finish_daily_attempt(
            &mut records,
            monday,
            ModeId::Sprint,
            FinishReason::GoalReached,
            6_141,
            0,
        )
        .expect("goal finish earns a result");
        assert!(wrote);
        assert_eq!(
            line,
            "Blockfall Daily 2026-09-28 \u{b7} Sprint \u{b7} 1:42.35"
        );
        assert_eq!(
            records.record_for(DAILY),
            Some(&Record::Daily {
                date: "2026-09-28".to_string(),
                result: "1:42.35".to_string(),
            })
        );

        // Second completed run the same day — even faster — changes nothing,
        // and the share line quotes the stored (first) result.
        let (line2, wrote2) = finish_daily_attempt(
            &mut records,
            monday,
            ModeId::Sprint,
            FinishReason::GoalReached,
            3_000,
            0,
        )
        .expect("goal finish earns a result");
        assert!(!wrote2, "first-run-wins: later same-day runs are ignored");
        assert_eq!(line2, line);
        assert_eq!(
            records.record_for(DAILY),
            Some(&Record::Daily {
                date: "2026-09-28".to_string(),
                result: "1:42.35".to_string(),
            })
        );

        // A top-out the same day earns nothing and writes nothing.
        assert!(finish_daily_attempt(
            &mut records,
            monday,
            ModeId::Sprint,
            FinishReason::TopOut,
            900,
            10
        )
        .is_none());

        // A new day replaces the stored entry outright.
        let tuesday = date(2026, 9, 29); // Ultra day
        let (line3, wrote3) = finish_daily_attempt(
            &mut records,
            tuesday,
            ModeId::Ultra,
            FinishReason::TimeUp,
            7_200,
            4242,
        )
        .expect("Ultra TimeUp earns a result");
        assert!(wrote3, "a new day may replace the stored day");
        assert_eq!(line3, "Blockfall Daily 2026-09-29 \u{b7} Ultra \u{b7} 4242");
        assert_eq!(
            records.record_for(DAILY),
            Some(&Record::Daily {
                date: "2026-09-29".to_string(),
                result: "4242".to_string(),
            })
        );
    }

    #[test]
    fn attempt_marker_is_active_only_when_started() {
        assert!(!is_daily_run_active(DailyAttempt::Idle));
        assert_eq!(DailyAttempt::default(), DailyAttempt::Idle);
        let attempt = DailyAttempt::Active {
            date: date(2026, 10, 1),
        };
        assert!(is_daily_run_active(attempt));
    }

    #[test]
    fn banner_text_shows_mode_then_stored_result() {
        set_today_override(Some(date(2026, 10, 1))); // Thursday → Sprint
        assert_eq!(banner_text(None), "Daily \u{b7} Sprint \u{2014} Not yet");
        let done = Record::Daily {
            date: "2026-10-01".to_string(),
            result: "1:42.35".to_string(),
        };
        assert_eq!(
            banner_text(Some(&done)),
            "Daily \u{b7} Sprint \u{2014} 1:42.35"
        );
        // A record from another date is stale → "Not yet".
        let stale = Record::Daily {
            date: "2026-09-30".to_string(),
            result: "2:00.00".to_string(),
        };
        assert_eq!(
            banner_text(Some(&stale)),
            "Daily \u{b7} Sprint \u{2014} Not yet"
        );
        set_today_override(None);
    }
}
