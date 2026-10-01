//! Per-mode records + play counters (T6).
//!
//! `best.json` used to hold exactly one `{score, level, lines}` object. It is
//! now a **per-mode records file** owned exclusively by this module:
//!
//! ```json
//! {
//!   "version": 1,
//!   "records": { "marathon": { "kind": "best_score", "score": 123, "level": 5, "lines": 40 } },
//!   "plays":   { "marathon": 3 }
//! }
//! ```
//!
//! Migration: a legacy file (top-level `score`/`level`/`lines`, no wrapper)
//! loads as the Marathon [`Record::BestScore`]; the next actual save rewrites
//! the new shape. A pure load never writes. Missing/corrupt ⇒ defaults with a
//! `warn!` — never a panic (same discipline as [`crate::settings_persist`],
//! whose `write_atomic`/`TETRIS_CONFIG_DIR` rules are reused). Unknown record
//! kinds or fields are tolerated (skipped/ignored) so future variants
//! (T14/T16/T17) written by newer builds do not break this reader.
//!
//! This module is the **only** writer of `best.json` (T6 single-writer fix):
//! `settings_persist` stopped writing it, and `PersistedBestScore` is a
//! view refreshed from the Marathon entry.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::settings_persist::{write_atomic, BEST_FILE, SAVE_DEBOUNCE_SECS};

/// Keys for [`Records`] lookups. T5's `ModeId` maps onto these; the strings
/// here are the on-disk keys and the API contract for T7/T9/T14/T16/T17.
pub type ModeKey = &'static str;

/// Marathon (endless, the legacy mode).
pub const MARATHON: ModeKey = "marathon";
/// Sprint (20 lines).
pub const SPRINT: ModeKey = "sprint";
/// Ultra (6-minute score attack).
pub const ULTRA: ModeKey = "ultra";
/// Dig (dig to the cheese).
pub const DIG: ModeKey = "dig";
/// Survival (tallest stack wins).
pub const SURVIVAL: ModeKey = "survival";
/// Zen (timed relaxation, lifetime lines tracker).
pub const ZEN: ModeKey = "zen";
/// Bot ladder (highest rung).
pub const BOT_LADDER: ModeKey = "bot_ladder";
/// Daily (date-stamped result).
pub const DAILY: ModeKey = "daily";
/// Dig duel (versus dig).
pub const DIG_DUEL: ModeKey = "dig_duel";
/// Switch (rotating ruleset).
pub const SWITCH: ModeKey = "switch";

/// All mode keys, for iteration (tests + future screens).
pub const ALL_MODE_KEYS: [ModeKey; 10] = [
    MARATHON, SPRINT, ULTRA, DIG, SURVIVAL, ZEN, BOT_LADDER, DAILY, DIG_DUEL, SWITCH,
];

/// On-disk schema version of the records wrapper.
const RECORDS_FILE_VERSION: u32 = 1;

/// One mode's record. Internally tagged on `kind` (snake_case) so unknown
/// kinds can be skipped by older readers instead of poisoning the whole file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// Lower `ticks` wins (Sprint/Dig/Survival).
    BestTime {
        /// Fixed-timestep ticks taken to finish.
        ticks: u64,
    },
    /// Higher `score` wins (Marathon/Ultra/…).
    BestScore {
        /// Final score.
        score: u64,
        /// Level reached.
        level: u32,
        /// Lines cleared.
        lines: u32,
    },
    /// Monotonic accumulator; `record_run` replaces only a strictly higher
    /// total, [`Records::add_lifetime_lines`] accumulates (saturating). Zen.
    LifetimeLines {
        /// Total lines cleared across all runs.
        total: u64,
    },
    /// Higher `rung` wins (bot ladder).
    HighestRung {
        /// Highest ladder rung cleared.
        rung: u32,
    },
    /// Latest submission wins: replaced whenever date or result differs.
    Daily {
        /// ISO date of the daily challenge.
        date: String,
        /// Result string for that date.
        result: String,
    },
}

impl Record {
    /// `true` when `incoming` should replace `existing` under this variant's
    /// improvement rule. Mismatched variants always replace (record-type
    /// change); [`Record::Daily`] keeps the latest content.
    fn is_improvement(existing: &Record, incoming: &Record) -> bool {
        match (existing, incoming) {
            (Record::BestTime { ticks: old }, Record::BestTime { ticks: new }) => new < old,
            (Record::BestScore { score: old, .. }, Record::BestScore { score: new, .. }) => {
                new > old
            }
            (Record::LifetimeLines { total: old }, Record::LifetimeLines { total: new }) => {
                new > old
            }
            (Record::HighestRung { rung: old }, Record::HighestRung { rung: new }) => new > old,
            (old, new) => old != new,
        }
    }
}

/// Per-mode records + play counters, loaded from / saved to `best.json`.
///
/// Improvement rules for [`Records::record_run`]:
/// - [`Record::BestTime`]: lower `ticks` wins.
/// - [`Record::BestScore`]: higher `score` wins (ties keep the old record).
/// - [`Record::LifetimeLines`] / [`Record::HighestRung`]: higher wins.
/// - [`Record::Daily`]: latest content wins (any differing submission).
/// - A differing variant stored under the same key always replaces.
#[derive(Debug, Default, Clone, PartialEq, Eq, Resource)]
pub struct Records {
    records: BTreeMap<String, Record>,
    plays: BTreeMap<String, u64>,
}

impl Records {
    /// Current record for `key`, if any.
    #[must_use]
    pub fn record_for(&self, key: ModeKey) -> Option<&Record> {
        self.records.get(key)
    }

    /// Fold a finished run into the records; `true` when it became (or
    /// replaced) the record under the variant's improvement rule.
    pub fn record_run(&mut self, key: ModeKey, record: Record) -> bool {
        match self.records.get_mut(key) {
            Some(existing) if !Record::is_improvement(existing, &record) => false,
            Some(existing) => {
                *existing = record;
                true
            }
            None => {
                self.records.insert(key.to_string(), record);
                true
            }
        }
    }

    /// Count one play for `key`; returns the new count.
    pub fn bump_plays(&mut self, key: ModeKey) -> u64 {
        let count = self.plays.entry(key.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }

    /// Plays recorded for `key` (0 when never played).
    #[must_use]
    pub fn plays(&self, key: ModeKey) -> u64 {
        self.plays.get(key).copied().unwrap_or(0)
    }

    /// Add `lines` to Zen's lifetime counter (saturating); returns the new
    /// total. Seeds [`Record::LifetimeLines`] when absent; replaces any other
    /// variant stored under Zen.
    pub fn add_lifetime_lines(&mut self, lines: u64) -> u64 {
        let entry = self
            .records
            .entry(ZEN.to_string())
            .or_insert(Record::LifetimeLines { total: 0 });
        if let Record::LifetimeLines { total } = entry {
            *total = total.saturating_add(lines);
            *total
        } else {
            *entry = Record::LifetimeLines { total: lines };
            lines
        }
    }
}

/// Legacy `best.json`: a bare `{score, level, lines}` object.
#[derive(Debug, Deserialize)]
struct LegacyBest {
    score: u64,
    level: u32,
    lines: u32,
}

/// New `best.json` wrapper. Records are kept as raw JSON on load so unknown
/// future kinds are skipped per-entry instead of failing the whole file.
#[derive(Debug, Serialize, Deserialize)]
struct RecordsFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    records: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    plays: BTreeMap<String, u64>,
}

fn default_version() -> u32 {
    RECORDS_FILE_VERSION
}

/// Parse `best.json` content: legacy shape → Marathon [`Record::BestScore`],
/// new shape → per-entry tolerant load. `None` ⇒ corrupt (caller warns).
fn parse_records(raw: &str) -> Option<Records> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object()?;
    if object.contains_key("score") {
        // Legacy shape (no wrapper): exactly {score, level, lines} → the
        // Marathon BestScore (PRD: "current best migrated to Marathon").
        let legacy: LegacyBest =
            serde_json::from_value(serde_json::Value::Object(object.clone())).ok()?;
        let mut records = BTreeMap::new();
        records.insert(
            MARATHON.to_string(),
            Record::BestScore {
                score: legacy.score,
                level: legacy.level,
                lines: legacy.lines,
            },
        );
        return Some(Records {
            records,
            plays: BTreeMap::new(),
        });
    }
    let file: RecordsFile =
        serde_json::from_value(serde_json::Value::Object(object.clone())).ok()?;
    let mut records = BTreeMap::new();
    for (key, raw_record) in file.records {
        match serde_json::from_value::<Record>(raw_record) {
            Ok(record) => {
                records.insert(key, record);
            }
            Err(err) => warn!("records: skipping entry {key:?} ({err})"),
        }
    }
    Some(Records {
        records,
        plays: file.plays,
    })
}

/// Path-injectable loader: missing file ⇒ defaults (silent, first run);
/// corrupt ⇒ defaults + `warn!`. Never writes, never panics.
#[must_use]
pub fn load_from(dir: &Path) -> Records {
    let path = dir.join(BEST_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Records::default(),
        Err(err) => {
            warn!("{} unreadable ({err}); using defaults", path.display());
            return Records::default();
        }
    };
    parse_records(&raw).unwrap_or_else(|| {
        warn!("{} is corrupt; using defaults", path.display());
        Records::default()
    })
}

/// Load from the platform config dir ([`crate::settings_persist::CONFIG_DIR_ENV`]
/// honored).
#[must_use]
pub fn load() -> Records {
    load_from(&crate::settings_persist::config_dir())
}

/// Serialize to the new wrapper shape.
fn to_file(records: &Records) -> io::Result<RecordsFile> {
    let mut out = BTreeMap::new();
    for (key, record) in &records.records {
        let value = serde_json::to_value(record)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        out.insert(key.clone(), value);
    }
    Ok(RecordsFile {
        version: RECORDS_FILE_VERSION,
        records: out,
        plays: records.plays.clone(),
    })
}

/// Path-injectable writer: atomic (tmp + rename), dir created when absent,
/// a failed write leaves the previous file intact. This is the **only** code
/// path that writes `best.json`.
pub fn save_to(dir: &Path, records: &Records) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let file = to_file(records)?;
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write_atomic(&dir.join(BEST_FILE), &bytes)
}

/// Save to the platform config dir.
pub fn save(records: &Records) -> io::Result<()> {
    save_to(&crate::settings_persist::config_dir(), records)
}

/// Debounce/force latch for the records writer (same shape as
/// `settings_persist::SaveQueue`; `force` bypasses the debounce window).
#[derive(Resource)]
pub(crate) struct RecordsSaveQueue {
    pub(crate) pending: bool,
    pub(crate) force: bool,
    timer: Timer,
}

impl Default for RecordsSaveQueue {
    fn default() -> Self {
        Self {
            pending: false,
            force: false,
            timer: Timer::from_seconds(SAVE_DEBOUNCE_SECS, TimerMode::Once),
        }
    }
}

/// Startup load of the live [`Records`] resource (never writes).
fn records_load_system(mut records: ResMut<Records>) {
    let loaded = load();
    if loaded != *records {
        *records = loaded;
    }
}

/// Arm the debounce on any [`Records`] change and write once it settles (or
/// immediately when forced, e.g. by a new Marathon record). The first frame
/// is skipped so the `Startup` load never self-dirties into a save — a pure
/// load must not rewrite a legacy file (downgrade safety).
pub(crate) fn records_flush_system(
    time: Res<Time>,
    records: Res<Records>,
    mut queue: ResMut<RecordsSaveQueue>,
    mut primed: Local<bool>,
) {
    if !*primed {
        *primed = true;
        return;
    }
    if records.is_changed() {
        queue.pending = true;
        queue.timer.reset();
    }
    if !queue.pending {
        return;
    }
    queue.timer.tick(time.delta());
    if !(queue.force || queue.timer.is_finished()) {
        return;
    }
    queue.pending = false;
    queue.force = false;
    if let Err(err) = save(&records) {
        warn!("could not persist records: {err}");
    }
}

/// Last-chance [`Records`] flush on app exit.
pub(crate) fn records_exit_flush_system(
    mut exits: MessageReader<AppExit>,
    records: Res<Records>,
    queue: Res<RecordsSaveQueue>,
) {
    if exits.read().next().is_none() || !(queue.pending || records.is_changed()) {
        return;
    }
    if let Err(err) = save(&records) {
        warn!("could not persist records on exit: {err}");
    }
}

/// Loads [`Records`] at startup, auto-saves them (debounced, forced on
/// demand via [`RecordsSaveQueue`]) and flushes on [`AppExit`]. Mounted by
/// `SettingsPersistPlugin`; add it standalone only for headless tests that
/// need records without the settings screen.
pub struct RecordsPlugin;

impl Plugin for RecordsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Records>()
            .init_resource::<RecordsSaveQueue>()
            .add_systems(Startup, records_load_system)
            .add_systems(Update, records_flush_system)
            .add_systems(Last, records_exit_flush_system);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::settings_persist::{CONFIG_DIR_ENV, ENV_LOCK};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("tetris-t6-{label}-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn best_score(score: u64) -> Record {
        Record::BestScore {
            score,
            level: 3,
            lines: 40,
        }
    }

    // ---- migration ----

    #[test]
    fn legacy_file_migrates_to_marathon_best_score() {
        let dir = TempDir::new("migrate");
        std::fs::write(
            dir.path().join(BEST_FILE),
            br#"{"score":500,"level":3,"lines":40}"#,
        )
        .unwrap();
        let records = load_from(dir.path());
        assert_eq!(
            records.record_for(MARATHON),
            Some(&Record::BestScore {
                score: 500,
                level: 3,
                lines: 40
            })
        );
        assert_eq!(records.plays(MARATHON), 0);

        // A pure load never rewrites the legacy file (downgrade safety).
        let raw = std::fs::read_to_string(dir.path().join(BEST_FILE)).unwrap();
        assert_eq!(raw, r#"{"score":500,"level":3,"lines":40}"#);

        // The first actual save writes the new shape; it still loads.
        save_to(dir.path(), &records).expect("save migrated shape");
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(BEST_FILE)).unwrap())
                .unwrap();
        assert_eq!(raw["version"], serde_json::json!(1));
        assert_eq!(raw["records"]["marathon"]["kind"], "best_score");
        assert!(raw.get("score").is_none(), "legacy shape must be gone");
        assert_eq!(load_from(dir.path()), records);
    }

    #[test]
    fn corrupt_file_loads_defaults_without_panicking() {
        let dir = TempDir::new("corrupt");
        std::fs::write(dir.path().join(BEST_FILE), b"\x00garbage{]").unwrap();
        assert_eq!(load_from(dir.path()), Records::default());
        assert!(load_from(dir.path()).record_for(MARATHON).is_none());
    }

    #[test]
    fn missing_file_loads_defaults() {
        let dir = TempDir::new("missing");
        assert_eq!(load_from(dir.path()), Records::default());
    }

    #[test]
    fn unknown_kinds_and_keys_are_tolerated() {
        let dir = TempDir::new("unknown");
        std::fs::write(
            dir.path().join(BEST_FILE),
            br#"{
                "version": 1,
                "records": {
                    "marathon": {"kind":"best_score","score":7,"level":1,"lines":2,"future_field":true},
                    "sprint": {"kind":"from_the_year_3000","whatever":1}
                },
                "plays": {"marathon": 3},
                "top_level_future_key": [1,2,3]
            }"#,
        )
        .unwrap();
        let records = load_from(dir.path());
        assert_eq!(
            records.record_for(MARATHON),
            Some(&Record::BestScore {
                score: 7,
                level: 1,
                lines: 2
            })
        );
        assert!(
            records.record_for(SPRINT).is_none(),
            "unknown kind is skipped, not fatal"
        );
        assert_eq!(records.plays(MARATHON), 3);
    }

    #[test]
    fn new_shape_round_trips_every_variant() {
        let dir = TempDir::new("roundtrip");
        let mut records = Records::default();
        records.record_run(MARATHON, best_score(900));
        records.record_run(SPRINT, Record::BestTime { ticks: 1234 });
        records.record_run(ZEN, Record::LifetimeLines { total: 5000 });
        records.record_run(BOT_LADDER, Record::HighestRung { rung: 7 });
        records.record_run(
            DAILY,
            Record::Daily {
                date: "2026-10-01".to_string(),
                result: "1234".to_string(),
            },
        );
        records.bump_plays(MARATHON);
        records.bump_plays(MARATHON);
        records.bump_plays(SPRINT);
        save_to(dir.path(), &records).expect("save");
        assert_eq!(load_from(dir.path()), records);
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = TempDir::new("atomic");
        let mut records = Records::default();
        records.bump_plays(MARATHON);
        save_to(dir.path(), &records).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp leftovers: {leftovers:?}");
    }

    // ---- improvement rules ----

    #[test]
    fn best_time_lower_wins() {
        let mut records = Records::default();
        assert!(records.record_run(SPRINT, Record::BestTime { ticks: 100 }));
        assert!(
            !records.record_run(SPRINT, Record::BestTime { ticks: 150 }),
            "slower run ignored"
        );
        assert!(
            !records.record_run(SPRINT, Record::BestTime { ticks: 100 }),
            "tie keeps first"
        );
        assert!(records.record_run(SPRINT, Record::BestTime { ticks: 50 }));
        assert_eq!(
            records.record_for(SPRINT),
            Some(&Record::BestTime { ticks: 50 })
        );
    }

    #[test]
    fn best_score_higher_wins() {
        let mut records = Records::default();
        assert!(records.record_run(MARATHON, best_score(500)));
        assert!(!records.record_run(MARATHON, best_score(499)));
        assert!(!records.record_run(MARATHON, best_score(500)));
        assert!(records.record_run(MARATHON, best_score(900)));
        assert_eq!(records.record_for(MARATHON), Some(&best_score(900)));
    }

    #[test]
    fn highest_rung_higher_wins() {
        let mut records = Records::default();
        assert!(records.record_run(BOT_LADDER, Record::HighestRung { rung: 3 }));
        assert!(!records.record_run(BOT_LADDER, Record::HighestRung { rung: 3 }));
        assert!(!records.record_run(BOT_LADDER, Record::HighestRung { rung: 2 }));
        assert!(records.record_run(BOT_LADDER, Record::HighestRung { rung: 4 }));
        assert_eq!(
            records.record_for(BOT_LADDER),
            Some(&Record::HighestRung { rung: 4 })
        );
    }

    #[test]
    fn lifetime_lines_rule_and_saturating_accumulation() {
        let mut records = Records::default();
        assert!(records.record_run(ZEN, Record::LifetimeLines { total: 100 }));
        assert!(!records.record_run(ZEN, Record::LifetimeLines { total: 90 }));
        assert!(records.record_run(ZEN, Record::LifetimeLines { total: 110 }));
        assert_eq!(records.add_lifetime_lines(40), 150);
        assert_eq!(records.add_lifetime_lines(u64::MAX - 100), u64::MAX);
        assert_eq!(records.add_lifetime_lines(u64::MAX), u64::MAX);
        assert_eq!(
            records.record_for(ZEN),
            Some(&Record::LifetimeLines { total: u64::MAX })
        );
    }

    #[test]
    fn daily_latest_content_wins() {
        let mut records = Records::default();
        assert!(records.record_run(
            DAILY,
            Record::Daily {
                date: "2026-10-01".to_string(),
                result: "100".to_string(),
            }
        ));
        assert!(
            !records.record_run(
                DAILY,
                Record::Daily {
                    date: "2026-10-01".to_string(),
                    result: "100".to_string(),
                }
            ),
            "identical submission is not an improvement"
        );
        assert!(records.record_run(
            DAILY,
            Record::Daily {
                date: "2026-10-01".to_string(),
                result: "90".to_string(),
            }
        ));
        assert!(records.record_run(
            DAILY,
            Record::Daily {
                date: "2026-10-02".to_string(),
                result: "0".to_string(),
            }
        ));
        assert_eq!(
            records.record_for(DAILY),
            Some(&Record::Daily {
                date: "2026-10-02".to_string(),
                result: "0".to_string(),
            })
        );
    }

    #[test]
    fn mismatched_variant_replaces_and_counts_as_improvement() {
        let mut records = Records::default();
        records.record_run(ULTRA, Record::BestTime { ticks: 1 });
        assert!(records.record_run(ULTRA, best_score(10)));
        assert_eq!(records.record_for(ULTRA), Some(&best_score(10)));
    }

    // ---- play counters ----

    #[test]
    fn plays_increment_per_mode() {
        let mut records = Records::default();
        assert_eq!(records.plays(MARATHON), 0);
        assert_eq!(records.bump_plays(MARATHON), 1);
        assert_eq!(records.bump_plays(MARATHON), 2);
        assert_eq!(records.bump_plays(SPRINT), 1);
        assert_eq!(records.plays(MARATHON), 2);
        assert_eq!(records.plays(SPRINT), 1);
        assert_eq!(records.plays(DIG), 0);
    }

    // ---- plugin wiring ----

    #[test]
    fn read_only_boot_writes_nothing() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("readonlyboot");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(RecordsPlugin);
        for _ in 0..3 {
            app.update();
        }
        assert!(!dir.path().join(BEST_FILE).exists());
        std::env::remove_var(CONFIG_DIR_ENV);
    }

    #[test]
    fn change_is_saved_on_exit_flush() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("exitsave");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(RecordsPlugin);
        app.update();

        app.world_mut()
            .resource_mut::<Records>()
            .bump_plays(MARATHON);
        app.world_mut().write_message(AppExit::Success);
        app.update();
        let loaded = load_from(dir.path());
        assert_eq!(loaded.plays(MARATHON), 1);
        std::env::remove_var(CONFIG_DIR_ENV);
    }
}
