//! Settings / best-score JSON persistence (T15).
//!
//! Storage layout — `<config_dir>/settings.json` + `<config_dir>/best.json`,
//! where the config dir is `dirs::config_dir()/tetris` (e.g.
//! `~/.config/tetris/`) unless the `TETRIS_CONFIG_DIR` environment variable
//! overrides it (tests and packaging). Both files are written atomically
//! (tmp file + rename) and a missing or corrupt file falls back to defaults
//! with a `warn!` — the loader never panics.
//!
//! Wire format:
//! - `settings.json` — `{ "settings": Settings, "bindings": BindingsFile }`.
//!   `Settings` is the T1 type serialized directly (it already derives serde);
//!   the T12 [`KeyBindings`] table is mirrored field-for-field with
//!   [`BindDto`]-encoded entries.
//! - `Bind` serialization (`BindDto`): externally tagged strings —
//!   `{"Key":"KeyA"}` (Bevy `KeyCode` variant name via its serde impl,
//!   enabled through the `bevy/serialize` feature) and plain `"WheelUp"` /
//!   `"WheelDown"`. Round-trip tested for every PRD §9 default binding.
//! - `best.json` — `{ "score": u64, "level": u32, "lines": u32 }`, the best
//!   finished run (highest score, [`record_final_run`] keeps the max).
//!
//! Public API for T16 (settings screen) and T17 (game-over screen):
//! [`SettingsPersistPlugin`], [`PersistedBestScore`] (read-only display
//! resource; saves happen automatically on change, on `CoreEvent(GameOver)`
//! and on app exit), plus the explicit [`load`]/[`save_once`] helpers and
//! their path-injectable [`load_from`]/[`save_to`] twins.

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use tetris_core::event::GameEvent;

use crate::core_bridge::{CoreEvent, GameCore};
use crate::input::{Bind, BindSlot, KeyBindings};
use crate::state::Settings;

/// App folder name under the platform config dir.
pub const APP_DIR_NAME: &str = "tetris";
/// Settings + key bindings file name.
pub const SETTINGS_FILE: &str = "settings.json";
/// Best-score file name.
pub const BEST_FILE: &str = "best.json";
/// Netplay profile file name (netplay-plan.md N5). Deliberately a **separate
/// file** with a separate resource: the T1 contract (`state.rs:1-3`) forbids
/// reshaping [`Settings`], so netplay persistence piggybacks on this module's
/// helpers instead of the settings wire format.
pub const NET_PROFILE_FILE: &str = "net_profile.json";
/// Environment variable that overrides the config dir (used by tests).
pub const CONFIG_DIR_ENV: &str = "TETRIS_CONFIG_DIR";

/// Seconds a settings/bindings change waits before hitting disk.
const SAVE_DEBOUNCE_SECS: f32 = 0.4;

/// Platform config directory for persisted files, honoring
/// [`CONFIG_DIR_ENV`]. Never fails; degrades to `./tetris`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR_NAME)
}

/// Best finished run, keyed on final score (PRD §7.4 "best").
/// T17 renders `score`/`level`/`lines` from this resource.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Resource)]
pub struct PersistedBestScore {
    /// Final score of the best run.
    pub score: u64,
    /// Level reached in the best run.
    pub level: u32,
    /// Lines cleared in the best run.
    pub lines: u32,
}

/// Netplay profile (netplay-plan.md N5): the UI state worth remembering
/// between runs for online play. A **separate resource + file**
/// ([`NET_PROFILE_FILE`]) from [`Settings`] — the T1 contract in `state.rs`
/// forbids reshaping `Settings`, so this struct lives here and round-trips on
/// its own. `SettingsPersistPlugin` loads it at `Startup` and rewrites it
/// (atomically, like every other file here) whenever a change settles.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, Resource)]
pub struct NetProfile {
    /// Last join address submitted by the player — prefilled into the IP
    /// entry widget on the Online → Join screen.
    pub last_join_addr: String,
}

/// Wire mirror of [`Bind`]: `{"Key":"KeyA"}` / `"WheelUp"` / `"WheelDown"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum BindDto {
    /// A physical key, by `KeyCode` variant name.
    Key(KeyCode),
    /// Mouse-wheel notch up.
    WheelUp,
    /// Mouse-wheel notch down.
    WheelDown,
}

impl From<Bind> for BindDto {
    fn from(bind: Bind) -> Self {
        match bind {
            Bind::Key(key) => Self::Key(key),
            Bind::WheelUp => Self::WheelUp,
            Bind::WheelDown => Self::WheelDown,
        }
    }
}

impl From<BindDto> for Bind {
    fn from(dto: BindDto) -> Self {
        match dto {
            BindDto::Key(key) => Self::Key(key),
            BindDto::WheelUp => Self::WheelUp,
            BindDto::WheelDown => Self::WheelDown,
        }
    }
}

fn binds_to_dtos(binds: &[Bind]) -> Vec<BindDto> {
    binds.iter().copied().map(BindDto::from).collect()
}

fn binds_from_dtos(dtos: Vec<BindDto>) -> Vec<Bind> {
    dtos.into_iter().map(Bind::from).collect()
}

/// Field-for-field mirror of the private [`KeyBindings`] table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BindingsFile {
    move_left: Vec<BindDto>,
    move_right: Vec<BindDto>,
    soft_drop: Vec<BindDto>,
    hard_drop: Vec<BindDto>,
    rotate_cw: Vec<BindDto>,
    rotate_ccw: Vec<BindDto>,
    rotate_180: Vec<BindDto>,
    hold: Vec<BindDto>,
    pause: Vec<BindDto>,
}

impl From<&KeyBindings> for BindingsFile {
    fn from(bindings: &KeyBindings) -> Self {
        let slot = |id| binds_to_dtos(bindings.slot(id));
        Self {
            move_left: slot(BindSlot::MoveLeft),
            move_right: slot(BindSlot::MoveRight),
            soft_drop: slot(BindSlot::SoftDrop),
            hard_drop: slot(BindSlot::HardDrop),
            rotate_cw: slot(BindSlot::RotateCw),
            rotate_ccw: slot(BindSlot::RotateCcw),
            rotate_180: slot(BindSlot::Rotate180),
            hold: slot(BindSlot::Hold),
            pause: slot(BindSlot::Pause),
        }
    }
}

impl From<BindingsFile> for KeyBindings {
    fn from(file: BindingsFile) -> Self {
        let mut bindings = Self::default();
        bindings.set_slot(BindSlot::MoveLeft, binds_from_dtos(file.move_left));
        bindings.set_slot(BindSlot::MoveRight, binds_from_dtos(file.move_right));
        bindings.set_slot(BindSlot::SoftDrop, binds_from_dtos(file.soft_drop));
        bindings.set_slot(BindSlot::HardDrop, binds_from_dtos(file.hard_drop));
        bindings.set_slot(BindSlot::RotateCw, binds_from_dtos(file.rotate_cw));
        bindings.set_slot(BindSlot::RotateCcw, binds_from_dtos(file.rotate_ccw));
        bindings.set_slot(BindSlot::Rotate180, binds_from_dtos(file.rotate_180));
        bindings.set_slot(BindSlot::Hold, binds_from_dtos(file.hold));
        bindings.set_slot(BindSlot::Pause, binds_from_dtos(file.pause));
        bindings
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SettingsFile {
    settings: Settings,
    bindings: BindingsFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct BestFile {
    score: u64,
    level: u32,
    lines: u32,
}

impl From<PersistedBestScore> for BestFile {
    fn from(best: PersistedBestScore) -> Self {
        Self {
            score: best.score,
            level: best.level,
            lines: best.lines,
        }
    }
}

impl From<BestFile> for PersistedBestScore {
    fn from(file: BestFile) -> Self {
        Self {
            score: file.score,
            level: file.level,
            lines: file.lines,
        }
    }
}

/// Keep only the best run (highest final score) in `best`. Returns `true`
/// when the record was replaced.
pub fn record_final_run(best: &mut PersistedBestScore, score: u64, level: u32, lines: u32) -> bool {
    if score <= best.score {
        return false;
    }
    *best = PersistedBestScore {
        score,
        level,
        lines,
    };
    true
}

/// Read `path` as JSON; `None` for a missing file (silent, first run) or a
/// corrupt/unparseable one (logged with `warn!`). Never panics.
fn read_json_opt<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("{} unreadable ({err}); using defaults", path.display());
            return None;
        }
    };
    match serde_json::from_str::<T>(&raw) {
        Ok(value) => Some(value),
        Err(err) => {
            warn!("{} is corrupt ({err}); using defaults", path.display());
            None
        }
    }
}

/// Path-injectable loader: `(Settings, KeyBindings, best)` from `dir`,
/// falling back to defaults file-by-file on missing/corrupt input.
pub fn load_from(dir: &Path) -> (Settings, KeyBindings, PersistedBestScore) {
    let (settings, bindings) = match read_json_opt::<SettingsFile>(&dir.join(SETTINGS_FILE)) {
        Some(file) => (file.settings, file.bindings.into()),
        None => (Settings::default(), KeyBindings::default()),
    };
    let best = read_json_opt::<BestFile>(&dir.join(BEST_FILE))
        .map_or(PersistedBestScore::default(), PersistedBestScore::from);
    (settings, bindings, best)
}

/// Load from the platform [`config_dir`].
pub fn load() -> (Settings, KeyBindings, PersistedBestScore) {
    load_from(&config_dir())
}

/// Write `bytes` to `path` atomically: sibling tmp file, fsync, rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let result = (|| -> io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn to_bytes<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(value).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Write both files under `dir` (created if absent). One atomic swap per
/// file; a failed run leaves the previous files untouched.
pub fn save_to(
    dir: &Path,
    settings: &Settings,
    bindings: &KeyBindings,
    best: PersistedBestScore,
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let settings_bytes = to_bytes(&SettingsFile {
        settings: settings.clone(),
        bindings: BindingsFile::from(bindings),
    })?;
    write_atomic(&dir.join(SETTINGS_FILE), &settings_bytes)?;
    let best_bytes = to_bytes(&BestFile::from(best))?;
    write_atomic(&dir.join(BEST_FILE), &best_bytes)?;
    Ok(())
}

/// Save immediately to the platform [`config_dir`].
pub fn save_once(
    settings: &Settings,
    bindings: &KeyBindings,
    best: PersistedBestScore,
) -> io::Result<()> {
    save_to(&config_dir(), settings, bindings, best)
}

/// Path-injectable [`NetProfile`] loader: missing or corrupt
/// [`NET_PROFILE_FILE`] falls back to the default (empty prefill) exactly
/// like the other files here — never panics.
pub fn net_profile_load_from(dir: &Path) -> NetProfile {
    read_json_opt::<NetProfile>(&dir.join(NET_PROFILE_FILE)).unwrap_or_default()
}

/// Load the [`NetProfile`] from the platform [`config_dir`].
pub fn net_profile_load() -> NetProfile {
    net_profile_load_from(&config_dir())
}

/// Path-injectable [`NetProfile`] writer: one atomic swap (tmp + rename),
/// the dir created when absent, a failed write leaving the old file intact.
pub fn net_profile_save_to(dir: &Path, profile: &NetProfile) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    write_atomic(&dir.join(NET_PROFILE_FILE), &to_bytes(profile)?)
}

/// Save the [`NetProfile`] immediately to the platform [`config_dir`].
pub fn net_profile_save(profile: &NetProfile) -> io::Result<()> {
    net_profile_save_to(&config_dir(), profile)
}

/// Startup load of the live [`NetProfile`] resource from disk.
fn net_profile_load_system(mut profile: ResMut<NetProfile>) {
    let loaded = net_profile_load();
    if loaded != *profile {
        *profile = loaded;
    }
}

/// Private latch: `true` while a [`NetProfile`] change still needs the disk.
#[derive(Resource, Default)]
struct NetProfilePending(bool);

/// Persist [`NetProfile`] whenever it settles: writes fire the frame *after*
/// a change (first frame is skipped), so `to_bytes` never runs inside a
/// `ResMut` borrow. The change itself already dirtied the resource, so a
/// failed write keeps the latch set and retries next frame; [`AppExit`] gets
/// one final flush.
fn net_profile_flush_system(
    profile: Res<NetProfile>,
    mut primed: Local<bool>,
    mut pending: ResMut<NetProfilePending>,
) {
    if !*primed {
        *primed = true;
        return;
    }
    if profile.is_changed() {
        pending.0 = true;
    }
    if !pending.0 {
        return;
    }
    if net_profile_save(&profile).is_ok() {
        pending.0 = false;
    } else {
        warn!("could not persist net profile");
    }
}

/// Last-chance [`NetProfile`] flush on app exit.
fn net_profile_exit_system(
    mut exits: MessageReader<AppExit>,
    profile: Res<NetProfile>,
    pending: Res<NetProfilePending>,
) {
    if exits.read().next().is_none() || !pending.0 {
        return;
    }
    if let Err(err) = net_profile_save(&profile) {
        warn!("could not persist net profile on exit: {err}");
    }
}

/// Last-seen settings/bindings fingerprint; the first frame after load only
/// primes it, so loading never self-dirties.
#[derive(Debug, Default, Resource)]
struct SettingsFingerprint {
    hash: u64,
    primed: bool,
}

#[derive(Resource)]
struct SaveQueue {
    pending: bool,
    force: bool,
    timer: Timer,
}

impl Default for SaveQueue {
    fn default() -> Self {
        Self {
            pending: false,
            force: false,
            timer: Timer::from_seconds(SAVE_DEBOUNCE_SECS, TimerMode::Once),
        }
    }
}

fn settings_hash(settings: &Settings, bindings: &KeyBindings) -> u64 {
    let mut hasher = DefaultHasher::new();
    format!("{settings:?}|{bindings:?}").hash(&mut hasher);
    hasher.finish()
}

/// Replace the live resources with what is on disk (defaults when absent or
/// corrupt — see [`load`]).
fn load_system(
    mut settings: ResMut<Settings>,
    mut bindings: ResMut<KeyBindings>,
    mut best: ResMut<PersistedBestScore>,
) {
    let (loaded, loaded_bindings, loaded_best) = load();
    *settings = loaded;
    *bindings = loaded_bindings;
    *best = loaded_best;
}

/// Mark a debounced save whenever the settings/bindings fingerprint moves.
fn change_detection_system(
    settings: Res<Settings>,
    bindings: Res<KeyBindings>,
    mut seen: ResMut<SettingsFingerprint>,
    mut queue: ResMut<SaveQueue>,
) {
    let hash = settings_hash(&settings, &bindings);
    if hash == seen.hash {
        return;
    }
    seen.hash = hash;
    if !seen.primed {
        seen.primed = true;
        return;
    }
    queue.pending = true;
    queue.timer.reset();
}

/// On `CoreEvent(GameEvent::GameOver)` fold the finished run's final
/// score/level/lines (read from `GameCore`) into the best record and force
/// an immediate flush. Exclusive (direct `&mut World`) because `GameCore`
/// is a non-send resource; no-ops in apps without the core bridge.
fn best_score_system(world: &mut World) {
    let game_over = world
        .get_resource::<Messages<CoreEvent>>()
        .is_some_and(|messages| {
            let mut cursor = messages.get_cursor();
            cursor
                .read(messages)
                .any(|event| event.0 == GameEvent::GameOver)
        });
    if !game_over {
        return;
    }
    let Some(core) = world.get_non_send::<GameCore>() else {
        return;
    };
    let snapshot = core.game.snapshot();
    let mut best = world.resource_mut::<PersistedBestScore>();
    if record_final_run(&mut best, snapshot.score, snapshot.level, snapshot.lines) {
        let mut queue = world.resource_mut::<SaveQueue>();
        queue.pending = true;
        queue.force = true;
    }
}

/// Perform the pending save once the debounce window has elapsed (or was
/// forced by a game-over record).
fn flush_system(
    time: Res<Time>,
    mut queue: ResMut<SaveQueue>,
    settings: Res<Settings>,
    bindings: Res<KeyBindings>,
    best: Res<PersistedBestScore>,
) {
    if !queue.pending {
        return;
    }
    queue.timer.tick(time.delta());
    if !(queue.force || queue.timer.is_finished()) {
        return;
    }
    queue.pending = false;
    queue.force = false;
    if let Err(err) = save_to(&config_dir(), &settings, &bindings, *best) {
        warn!("could not persist settings/best score: {err}");
    }
}

/// Last-chance flush: a still-pending change is written when the app exits.
fn exit_flush_system(
    mut exits: MessageReader<AppExit>,
    queue: Res<SaveQueue>,
    settings: Res<Settings>,
    bindings: Res<KeyBindings>,
    best: Res<PersistedBestScore>,
) {
    if exits.read().next().is_none() || !queue.pending {
        return;
    }
    if let Err(err) = save_to(&config_dir(), &settings, &bindings, *best) {
        warn!("could not persist settings/best score on exit: {err}");
    }
}

/// Loads `Settings` + best score at startup and saves them atomically.
///
/// Registers [`PersistedBestScore`] for T17 and auto-persists: debounced on
/// any [`Settings`]/[`KeyBindings`] change (T16 edits need no extra call),
/// immediately on `CoreEvent(GameEvent::GameOver)`, and once more on
/// [`AppExit`] if a debounced save is still in flight. N5 adds the separate
/// [`NetProfile`] resource on top: loaded at `Startup`, rewritten (atomic,
/// same discipline) one frame after a change and once more on [`AppExit`].
pub struct SettingsPersistPlugin;

impl Plugin for SettingsPersistPlugin {
    fn build(&self, app: &mut App) {
        // Defensive inits: the plugin is self-sufficient even when added
        // before `main.rs`/`InputPlugin` inserted these resources.
        app.init_resource::<Settings>()
            .init_resource::<KeyBindings>()
            .init_resource::<PersistedBestScore>()
            .init_resource::<SettingsFingerprint>()
            .init_resource::<SaveQueue>()
            .init_resource::<NetProfile>()
            .init_resource::<NetProfilePending>()
            .add_systems(Startup, (load_system, net_profile_load_system))
            .add_systems(
                Update,
                (
                    best_score_system,
                    change_detection_system,
                    flush_system,
                    net_profile_flush_system,
                )
                    .chain(),
            )
            .add_systems(Last, (exit_flush_system, net_profile_exit_system));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::core_bridge::CoreBridgePlugin;
    use crate::input::ALL_BIND_SLOTS;
    use crate::state::{AppState, EffectsQuality};

    /// Serializes every test that mutates the process-wide
    /// `TETRIS_CONFIG_DIR`, so parallel cargo threads can never observe a
    /// half-set override.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Unique temp dir under the system temp dir. Tests that go through the
    /// plugin point `TETRIS_CONFIG_DIR` at these dirs, so the real config
    /// dir is never read or written.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("tetris-t15-{label}-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("temp dir created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn default_trio() -> (Settings, KeyBindings, PersistedBestScore) {
        (
            Settings::default(),
            KeyBindings::default(),
            PersistedBestScore::default(),
        )
    }

    #[test]
    fn defaults_round_trip_through_temp_dir() {
        let dir = TempDir::new("roundtrip");
        let (settings, bindings, best) = default_trio();
        save_to(dir.path(), &settings, &bindings, best).expect("save defaults");
        let loaded = load_from(dir.path());
        assert_eq!(loaded, (settings, bindings, best));
    }

    #[test]
    fn edited_settings_and_bindings_round_trip() {
        let dir = TempDir::new("edited");
        let settings = Settings {
            das_ms: 88,
            arr_ms: 0,
            master_volume: 0.5,
            sfx_volume: 0.25,
            music_volume: 0.0,
            next_queue_size: 6,
            soft_drop_multiplier: 30,
            effects: EffectsQuality::High,
        };
        let mut bindings = KeyBindings::default();
        bindings.set_slot(
            BindSlot::HardDrop,
            vec![
                Bind::Key(KeyCode::Enter),
                Bind::WheelUp,
                Bind::Key(KeyCode::NumpadEnter),
            ],
        );
        bindings.set_slot(BindSlot::Rotate180, vec![Bind::WheelDown]);
        bindings.set_slot(BindSlot::Pause, Vec::new());
        let best = PersistedBestScore {
            score: 123_456,
            level: 9,
            lines: 140,
        };

        save_to(dir.path(), &settings, &bindings, best).expect("save edited");
        assert_eq!(load_from(dir.path()), (settings, bindings, best));
    }

    #[test]
    fn missing_files_load_defaults_without_warn_path() {
        let dir = TempDir::new("missing");
        assert_eq!(load_from(dir.path()), default_trio());
    }

    #[test]
    fn corrupt_files_load_defaults_without_panicking() {
        let dir = TempDir::new("corrupt");
        fs::write(dir.path().join(SETTINGS_FILE), b"{ this is not ] json").unwrap();
        fs::write(dir.path().join(BEST_FILE), b"\x00\x01garbage").unwrap();
        assert_eq!(load_from(dir.path()), default_trio());
    }

    #[test]
    fn half_corrupt_settings_file_drops_whole_file_to_defaults() {
        // Both fields are required: a settings.json missing its bindings
        // section counts as corrupt rather than silently unbinding everything.
        let dir = TempDir::new("halfcorrupt");
        fs::write(
            dir.path().join(SETTINGS_FILE),
            br#"{"settings":{"das_ms":99}}"#,
        )
        .unwrap();
        let (settings, _, _) = load_from(dir.path());
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn bind_dto_uses_stable_string_names() {
        let encode = |bind| serde_json::to_string(&BindDto::from(bind)).unwrap();
        let decode = |text: &str| serde_json::from_str::<BindDto>(text).unwrap();

        assert_eq!(encode(Bind::Key(KeyCode::KeyA)), r#"{"Key":"KeyA"}"#);
        assert_eq!(encode(Bind::Key(KeyCode::ArrowUp)), r#"{"Key":"ArrowUp"}"#);
        assert_eq!(
            encode(Bind::Key(KeyCode::ShiftLeft)),
            r#"{"Key":"ShiftLeft"}"#
        );
        assert_eq!(encode(Bind::WheelUp), r#""WheelUp""#);
        assert_eq!(encode(Bind::WheelDown), r#""WheelDown""#);

        for text in [
            r#"{"Key":"KeyA"}"#,
            r#"{"Key":"F7"}"#,
            r#""WheelUp""#,
            r#""WheelDown""#,
        ] {
            let bind: Bind = decode(text).into();
            assert_eq!(decode(&encode(bind)), decode(text), "round trip {text}");
        }
    }

    #[test]
    fn all_default_bindings_round_trip_through_bindings_file() {
        let bindings = KeyBindings::default();
        let file = BindingsFile::from(&bindings);
        assert_eq!(KeyBindings::from(file), bindings);
        // And slot-by-slot through the DTO encoder, wheel variants included.
        for slot in ALL_BIND_SLOTS {
            let dtos = binds_to_dtos(bindings.slot(slot));
            assert_eq!(binds_from_dtos(dtos), *bindings.slot(slot), "slot {slot:?}");
        }
    }

    #[test]
    fn record_final_run_keeps_max_score() {
        let mut best = PersistedBestScore::default();
        assert!(record_final_run(&mut best, 500, 3, 40));
        assert!(
            !record_final_run(&mut best, 499, 99, 999),
            "lower run ignored"
        );
        assert!(!record_final_run(&mut best, 500, 9, 99), "tie keeps first");
        assert!(record_final_run(&mut best, 900, 7, 120));
        assert_eq!(
            best,
            PersistedBestScore {
                score: 900,
                level: 7,
                lines: 120
            }
        );
    }

    #[test]
    fn atomic_save_leaves_only_valid_json_behind() {
        let dir = TempDir::new("atomic");
        let (settings, bindings, best) = default_trio();
        save_to(dir.path(), &settings, &bindings, best).unwrap();

        for name in [SETTINGS_FILE, BEST_FILE] {
            let raw = fs::read_to_string(dir.path().join(name)).unwrap();
            serde_json::from_str::<serde_json::Value>(&raw)
                .unwrap_or_else(|err| panic!("{name} is not valid JSON: {err}"));
        }
        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp file left behind: {leftovers:?}");

        // A second save atomically replaces both files and stays readable.
        let better = PersistedBestScore {
            score: 7,
            level: 1,
            lines: 2,
        };
        save_to(dir.path(), &settings, &bindings, better).unwrap();
        assert_eq!(load_from(dir.path()).2, better);
    }

    #[test]
    fn settings_change_arms_debounced_save() {
        let _env = ENV_LOCK.lock().unwrap();
        // One frame of wall-clock Time cannot elapse the 0.4 s debounce, so
        // the change must arm the queue without writing yet.
        let dir = TempDir::new("debounce");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());
        let mut app = minimal_persist_app(&dir);
        app.update();
        assert!(
            !dir.path().join(SETTINGS_FILE).exists(),
            "first frame only primes the fingerprint"
        );

        app.world_mut().resource_mut::<Settings>().das_ms = 77;
        app.update();
        assert!(app.world().resource::<SaveQueue>().pending);
        assert!(
            !dir.path().join(SETTINGS_FILE).exists(),
            "save must wait out the debounce window"
        );
        std::env::remove_var(CONFIG_DIR_ENV);
    }

    #[test]
    fn exit_flush_writes_pending_change() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("exitsave");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());
        let mut app = minimal_persist_app(&dir);
        app.update();
        app.world_mut().resource_mut::<Settings>().das_ms = 111;
        app.update();
        assert!(app.world().resource::<SaveQueue>().pending);

        app.world_mut().write_message(AppExit::Success);
        app.update();
        let (settings, _, _) = load_from(dir.path());
        assert_eq!(settings.das_ms, 111);
        std::env::remove_var(CONFIG_DIR_ENV);
    }

    /// Full integration path: boot the persistence plugin (plus the real
    /// core bridge) against a temp dir, play to game over through the fixed
    /// schedule and assert the best score reached disk.
    #[test]
    fn app_boots_and_game_over_persists_best_score() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("integration");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((CoreBridgePlugin, SettingsPersistPlugin));
        app.insert_non_send(GameCore::new(0xB005));
        app.init_resource::<AppState>();
        app.update();

        // Startup load against an empty dir → PRD defaults, nothing written.
        assert_eq!(app.world().resource::<Settings>().das_ms, 150);
        assert_eq!(
            *app.world().resource::<PersistedBestScore>(),
            PersistedBestScore::default()
        );
        assert!(!dir.path().join(SETTINGS_FILE).exists());

        // Pile pieces up until the core declares game over.
        for _ in 0..1000 {
            app.world_mut()
                .resource_mut::<crate::core_bridge::PendingActions>()
                .push(tetris_core::actions::Action::HardDrop);
            app.world_mut().run_schedule(FixedUpdate);
            if *app.world().resource::<AppState>() == AppState::GameOver {
                break;
            }
        }
        assert_eq!(*app.world().resource::<AppState>(), AppState::GameOver);

        let expected = app.world().non_send::<GameCore>().game.snapshot();
        assert!(expected.score > 0, "precondition: the run scored");

        // Two Update frames: capture the GameOver event, then flush it.
        app.update();
        assert_eq!(
            *app.world().resource::<PersistedBestScore>(),
            PersistedBestScore {
                score: expected.score,
                level: expected.level,
                lines: expected.lines,
            }
        );
        let (_, _, best) = load_from(dir.path());
        assert_eq!(best.score, expected.score);
        // The forced save rewrites settings.json too — still valid defaults.
        let (settings, bindings, _) = load_from(dir.path());
        assert_eq!(settings, Settings::default());
        assert_eq!(bindings, KeyBindings::default());

        std::env::remove_var(CONFIG_DIR_ENV);
    }

    fn minimal_persist_app(dir: &TempDir) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(SettingsPersistPlugin);
        // Startup load must find nothing even if the machine has real
        // settings — point this instance at the empty temp dir.
        assert!(!dir.path().join(SETTINGS_FILE).exists());
        app
    }

    #[test]
    fn config_dir_honors_env_override() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("envdir");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());
        assert_eq!(config_dir(), dir.path());
        std::env::remove_var(CONFIG_DIR_ENV);
        assert!(config_dir().ends_with(APP_DIR_NAME));
    }

    // ---- N5: separate net_profile.json persistence ----

    #[test]
    fn net_profile_round_trips_through_temp_dir() {
        let dir = TempDir::new("netprofile");
        let profile = NetProfile {
            last_join_addr: "192.168.1.42:27015".to_string(),
        };
        net_profile_save_to(dir.path(), &profile).expect("save net profile");
        assert_eq!(net_profile_load_from(dir.path()), profile);

        // Overwrite round-trips too (a later join replaces the prefill).
        let updated = NetProfile {
            last_join_addr: "10.0.0.9:40000".to_string(),
        };
        net_profile_save_to(dir.path(), &updated).expect("re-save net profile");
        assert_eq!(net_profile_load_from(dir.path()), updated);
    }

    #[test]
    fn net_profile_missing_file_loads_default() {
        let dir = TempDir::new("netprofile-missing");
        assert_eq!(
            net_profile_load_from(dir.path()),
            NetProfile {
                last_join_addr: String::new()
            }
        );
    }

    #[test]
    fn net_profile_corrupt_file_loads_default_without_panicking() {
        let dir = TempDir::new("netprofile-corrupt");
        fs::write(dir.path().join(NET_PROFILE_FILE), b"{ nope ]").unwrap();
        assert_eq!(net_profile_load_from(dir.path()), NetProfile::default());
    }

    #[test]
    fn net_profile_atomic_save_leaves_no_tmp_file() {
        let dir = TempDir::new("netprofile-atomic");
        net_profile_save_to(dir.path(), &NetProfile::default()).unwrap();
        let raw = fs::read_to_string(dir.path().join(NET_PROFILE_FILE)).unwrap();
        serde_json::from_str::<serde_json::Value>(&raw).expect("valid json");
        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp file left behind: {leftovers:?}");
    }

    #[test]
    fn net_profile_is_its_own_file_not_mixed_into_settings() {
        // The T1 contract: Settings is never reshaped — saving a net profile
        // must not touch settings.json / best.json at all.
        let dir = TempDir::new("netprofile-isolated");
        let (settings, bindings, best) = default_trio();
        save_to(dir.path(), &settings, &bindings, best).unwrap();
        net_profile_save_to(
            dir.path(),
            &NetProfile {
                last_join_addr: "127.0.0.1:27015".to_string(),
            },
        )
        .unwrap();
        assert_eq!(load_from(dir.path()), (settings, bindings, best));
        assert!(dir.path().join(NET_PROFILE_FILE).exists());
    }

    /// Full plugin path: `TETRIS_CONFIG_DIR` override, boot, change the
    /// resource, and the separate file lands on disk (with the exit flush as
    /// the safety net).
    #[test]
    fn net_profile_plugin_round_trip() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("netprofile-plugin");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(SettingsPersistPlugin);
        app.update();

        // Startup load against an empty dir → empty prefill, nothing written.
        assert_eq!(*app.world().resource::<NetProfile>(), NetProfile::default());
        assert!(!dir.path().join(NET_PROFILE_FILE).exists());

        app.world_mut().resource_mut::<NetProfile>().last_join_addr =
            "192.168.0.7:27015".to_string();
        app.update();
        let loaded = net_profile_load_from(dir.path());
        assert_eq!(loaded.last_join_addr, "192.168.0.7:27015");

        std::env::remove_var(CONFIG_DIR_ENV);
    }
}
