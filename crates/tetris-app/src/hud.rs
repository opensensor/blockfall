//! Snapshot-driven side-panel HUD (T13).
//!
//! Reads **only** [`GameSnapshot`] data (score/level/lines/combo/b2b/next/
//! hold/hold_used) — no rules are recomputed here. Panels are anchored to
//! the letterboxed playfield every `Update` via [`crate::render::letterbox`],
//! so they track window resizes for free.
//!
//! Layout: the left panel holds the hold box (top) plus the `SCORE` /
//! `LEVEL` / `LINES` stat texts, the conditional `COMBO` / `B2B` indicators
//! and the pause chord hint; the right panel holds the next queue
//! (spawn-rotation mini pieces, `Settings::next_queue_size` clamped to
//! `1..=6` slots, never more than `snapshot.next.len()`).
//!
//! While a [`VersusMatch`] is active (T26) the solo panels are hidden and a
//! compact per-side panel is rendered under a single [`VersusHudRoot`] from
//! the match snapshot instead: score / lines / level, a `+N` incoming
//! garbage indicator, two next previews and a small hold box. The root is
//! toggled by versus activity rather than [`AppState`](crate::state::AppState)
//! (versus plays *inside* `Playing`), so it deliberately does **not** join
//! the screens-menu root visibility filter.
//!
//! Public API reusable by later tasks (T17 menus/overlays, T19 juice):
//! - [`HudAnchor`] / [`hud_anchor`] — window size → playfield + panel anchors.
//! - [`hud_text_center`] — per-slot text anchor inside the left panel.
//! - [`hold_center`] / [`next_center`] — preview slot anchors.
//! - [`versus_panel_anchors`] and the `versus_*_center` helpers — compact
//!   versus side-panel anchors (T26), derived from
//!   [`crate::render::versus_layouts`].
//! - Markers [`NextPreview`], [`HoldPreview`], [`HudText`]/[`HudTextSlot`],
//!   [`HudMiniCell`] for queries and styling, plus [`VersusHudRoot`],
//!   [`VersusHudText`], [`VersusPreview`], [`VersusMiniCell`] for the
//!   versus HUD.
//! - [`HudFixture`] — test/QA hook forcing the HUD to render from a chosen
//!   snapshot instead of the live core.
//!
//! Entities are updated in place each frame; spawn/despawn only happens on
//! count changes (queue size, conditional indicators, hold contents).

use bevy::prelude::*;
use bevy::window::Window;

use tetris_core::board::{COLS, ROWS};
use tetris_core::game::GameSnapshot;
use tetris_core::piece::{Piece, Rotation};
use tetris_core::versus::{AttackRule, Side, DIG_DUEL_GARBAGE_ROWS};

use crate::art::ArtAssets;
use crate::core_bridge::{CoreEvent, GameCore, ModeHudInfo, VersusMatch};
use crate::input::{Bind, BindSlot, KeyBindings};
use crate::modes;
use crate::records::{Record, Records};
use crate::render::{self, VISIBLE_ROWS};
use crate::screens_ladder::{ladder_badge_text, LadderOrigin};
use crate::screens_menu::VersusFlow;
use crate::state::Settings;
use tetris_core::event::GameEvent;

/// Panel gap between the playfield edge and the panel, in cell units.
const PANEL_GAP: f32 = 0.75;
/// Panel half-width (preview box half-size), in cell units.
const PANEL_HALF: f32 = 2.0;
/// Mini-preview cell size, in playfield cell units.
const MINI_FACTOR: f32 = 0.5;
/// Hard cap on rendered next previews (PRD settings slider max).
const MAX_NEXT: usize = 6;
/// Fallback next-queue size when no `Settings` resource exists.
const DEFAULT_QUEUE_SIZE: usize = 5;

/// Playfield-anchored coordinates for HUD/menu placement. Derived from
/// [`crate::render::letterbox`] so every consumer shares one layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HudAnchor {
    /// Square cell edge length in world units.
    pub cell: f32,
    /// World x of the playfield left edge.
    pub field_left: f32,
    /// World x of the playfield right edge.
    pub field_right: f32,
    /// World y of the playfield top edge.
    pub field_top: f32,
    /// World y of the playfield bottom edge.
    pub field_bottom: f32,
    /// World x of the left panel center line (landscape) / hold column
    /// (portrait top strip).
    pub left_panel_x: f32,
    /// World x of the right panel center line (landscape; unused in
    /// portrait).
    pub right_panel_x: f32,
    /// `true` for the portrait-native layout (Android / `TETRIS_PORTRAIT=1`
    /// with a taller-than-wide window): HUD slots form a strip above the
    /// field instead of side panels.
    pub portrait: bool,
    /// Full window width in world units.
    pub window_w: f32,
    /// Full window height in world units.
    pub window_h: f32,
}

/// Compute the current HUD anchor set for a window size (resize aware).
pub fn hud_anchor(window_w: f32, window_h: f32) -> HudAnchor {
    let portrait = render::portrait_layout(window_w, window_h);
    let (view_w, view_h, center_y) = render::playfield_view(window_w, window_h);
    let (cell, _, _) = render::letterbox(view_w, view_h);
    let half_w = cell * COLS as f32 * 0.5;
    let half_h = cell * VISIBLE_ROWS as f32 * 0.5;
    let panel = half_w + (PANEL_GAP + PANEL_HALF) * cell;
    HudAnchor {
        cell,
        field_left: -half_w,
        field_right: half_w,
        field_top: center_y + half_h,
        field_bottom: center_y - half_h,
        left_panel_x: if portrait {
            -window_w * 0.5 + 1.9 * cell
        } else {
            -panel
        },
        right_panel_x: if portrait {
            window_w * 0.5 - 2.3 * cell
        } else {
            panel
        },
        portrait,
        window_w,
        window_h,
    }
}

/// Which HUD text a [`HudText`] entity renders.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudTextSlot {
    /// `SCORE` label + value.
    Score,
    /// `LEVEL` label + value.
    Level,
    /// `LINES` label + value.
    Lines,
    /// Combo chain indicator, present only while `snapshot.combo > 0`.
    Combo,
    /// Back-to-back indicator, present only while `snapshot.b2b`.
    B2B,
    /// Pause chord hint reflecting the current [`KeyBindings`] pause slot.
    PauseHint,
    /// Mode clock (`TIME` + `m:ss.hh` from [`crate::modes::format_time_ticks`],
    /// count-down for clock-budgeted modes like Ultra). Present only while
    /// [`ModeHudInfo::show_hud`] is set and the pre-roll countdown is done.
    Clock,
    /// Mode goal counter — `Lines left: N` (Sprint, plus the pieces-placed
    /// tally) or `Garbage: N` (Dig). Same gate as [`Self::Clock`].
    Goal,
    /// Survival garbage meter (T13): `GARBAGE +N` queued rows + the seconds
    /// until the next feed row queues — the same `+N` idiom as the versus
    /// incoming-garbage text (same garbage color), driven by
    /// [`ModeHudInfo::feed_pending`] / [`ModeHudInfo::feed_next_row_in`].
    /// Present only while a feed mode's [`Self::Clock`] is.
    Feed,
    /// Zen lifetime lines (T14): `LIFETIME` + the all-time line total
    /// (`Records::LifetimeLines`, updated on every Zen `LineCleared` by
    /// [`zen_lifetime_lines_system`]; session lines are the normal `LINES`
    /// stat). Present only while a Zen run is live (no goal/clock/feed, so
    /// the slot shares the Survival feed meter's position — the two are
    /// never visible together).
    Lifetime,
    /// Big pre-roll `3`/`2`/`1` (= `ceil(countdown / 60)`), centered over
    /// the field. Present only while [`ModeHudInfo::countdown`] is nonzero;
    /// the clock and goal rows hide meanwhile.
    Countdown,
}

/// World-space center of a text slot: left panel column (landscape) or the
/// top strip / bottom combo row (portrait-native — `LEVEL | SCORE | LINES`
/// across the strip, `COMBO`/`B2B` in a row just under the field).
pub fn hud_text_center(anchor: &HudAnchor, slot: HudTextSlot) -> Vec2 {
    let c = anchor.cell;
    if anchor.portrait {
        let hw = anchor.window_w * 0.5;
        let strip_y = anchor.field_top + 3.6 * c;
        let deck_y = anchor.field_bottom - 1.0 * c;
        return match slot {
            HudTextSlot::Score => Vec2::new(0.0, strip_y),
            HudTextSlot::Level => Vec2::new(-hw + 2.3 * c, strip_y),
            // `LINES` sits left of the pause disc's top-right corner zone.
            HudTextSlot::Lines => Vec2::new(hw - 5.2 * c, strip_y),
            HudTextSlot::Combo => Vec2::new(-3.4 * c, deck_y),
            HudTextSlot::B2B => Vec2::new(3.4 * c, deck_y),
            HudTextSlot::PauseHint => Vec2::new(0.0, deck_y - 2.6 * c),
            // T8: clock/goal join the deck row at the outer margins (the
            // pause hint it would share is never shown in portrait); the
            // 3-2-1 sits over the empty field center, clear of both.
            HudTextSlot::Clock => Vec2::new(-hw + 2.4 * c, deck_y),
            HudTextSlot::Goal => Vec2::new(hw - 2.4 * c, deck_y),
            // T13: the feed meter takes the pause-hint line (never shown in
            // portrait), below the clock/goal deck row and clear of the
            // field-centered 3-2-1. T14: the Zen lifetime counter takes the
            // same line — the feed is never visible in Zen.
            HudTextSlot::Feed | HudTextSlot::Lifetime => Vec2::new(0.0, deck_y - 2.6 * c),
            HudTextSlot::Countdown => {
                Vec2::new(0.0, (anchor.field_top + anchor.field_bottom) * 0.5)
            }
        };
    }
    let y = match slot {
        HudTextSlot::Score => anchor.field_top - 4.0 * c,
        HudTextSlot::Level => anchor.field_top - 7.0 * c,
        HudTextSlot::Lines => anchor.field_top - 10.0 * c,
        HudTextSlot::Combo => anchor.field_top - 13.0 * c,
        HudTextSlot::B2B => anchor.field_top - 15.0 * c,
        HudTextSlot::PauseHint => anchor.field_top - 18.0 * c,
        // T8: clock/goal stack in the right panel below the next queue
        // (below even a 6-slot queue, above the window bottom); the 3-2-1
        // centers over the field. T13: the Survival feed meter stacks one
        // row further down. T14: the Zen lifetime counter shares that row —
        // Zen has no clock/goal/feed, so they never coexist.
        HudTextSlot::Clock => anchor.field_top - 18.5 * c,
        HudTextSlot::Goal => anchor.field_top - 21.0 * c,
        HudTextSlot::Feed | HudTextSlot::Lifetime => anchor.field_top - 23.5 * c,
        HudTextSlot::Countdown => (anchor.field_top + anchor.field_bottom) * 0.5,
    };
    let x = match slot {
        HudTextSlot::Clock | HudTextSlot::Goal | HudTextSlot::Feed | HudTextSlot::Lifetime => {
            anchor.right_panel_x
        }
        HudTextSlot::Countdown => 0.0,
        _ => anchor.left_panel_x,
    };
    Vec2::new(x, y)
}

/// World-space center of the hold box.
pub fn hold_center(anchor: &HudAnchor) -> Vec2 {
    let c = anchor.cell;
    if anchor.portrait {
        Vec2::new(anchor.left_panel_x, anchor.field_top + 1.9 * c)
    } else {
        Vec2::new(anchor.left_panel_x, anchor.field_top - 1.25 * c)
    }
}

/// World-space center of next-queue slot `index` (0 = first preview).
/// Landscape stacks down the right panel; portrait-native lays the queue
/// out horizontally to the right of the hold box inside the top strip.
pub fn next_center(anchor: &HudAnchor, index: usize) -> Vec2 {
    let c = anchor.cell;
    if anchor.portrait {
        let hw = anchor.window_w * 0.5;
        let x = (-hw + 4.7 * c + 2.25 * c * index as f32).min(hw - 1.2 * c);
        Vec2::new(x, anchor.field_top + 1.9 * c)
    } else {
        Vec2::new(
            anchor.right_panel_x,
            anchor.field_top - 1.25 * c - 3.0 * c * index as f32,
        )
    }
}

/// Normalized spawn-rotation footprint of a piece: cells shifted so the
/// top-left occupied cell is `(0, 0)`, plus the `(w, h)` box size.
pub fn spawn_footprint(piece: Piece) -> ([(i32, i32); 4], f32, f32) {
    let raw = piece.cells(Rotation::Spawn);
    let min_r = raw.iter().map(|(r, _)| *r).min().unwrap_or(0);
    let min_c = raw.iter().map(|(_, c)| *c).min().unwrap_or(0);
    let norm: Vec<(i32, i32)> = raw.iter().map(|(r, c)| (r - min_r, c - min_c)).collect();
    let w = norm.iter().map(|(_, c)| *c).max().unwrap_or(0) + 1;
    let h = norm.iter().map(|(r, _)| *r).max().unwrap_or(0) + 1;
    let mut out = [(0, 0); 4];
    out.copy_from_slice(&norm);
    (out, w as f32, h as f32)
}

/// Child transform offsets (centered box) for a mini preview with cell size
/// `s`, relative to the preview root center.
pub fn mini_offsets(piece: Piece, s: f32) -> [Vec2; 4] {
    let (cells, w, h) = spawn_footprint(piece);
    let mut out = [Vec2::ZERO; 4];
    for (i, (r, c)) in cells.iter().enumerate() {
        out[i] = Vec2::new(
            (*c as f32 + 0.5 - w * 0.5) * s,
            (h * 0.5 - *r as f32 - 0.5) * s,
        );
    }
    out
}

/// Marker component for mini-preview cell sprites (next queue and hold box).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HudMiniCell;

/// Root marker of one next-queue mini preview.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct NextPreview {
    /// Piece currently previewed in this slot.
    pub piece: Piece,
    /// Queue slot index (`0..count`).
    pub index: usize,
}

/// Root marker of the hold box preview.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldPreview {
    /// Parked piece (`None` while hold is empty).
    pub piece: Option<Piece>,
    /// `true` while `snapshot.hold_used` — previews draw dimmed then.
    pub used: bool,
}

/// Marker component on HUD [`Text2d`] entities.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HudText {
    /// Which slot this text renders.
    pub slot: HudTextSlot,
}

/// Test/QA hook: when set, the HUD renders from this snapshot instead of the
/// live [`GameCore`]. Still snapshot-driven — only the source changes.
#[derive(Resource, Default, Clone)]
pub struct HudFixture(pub Option<GameSnapshot>);

/// Pooled text entities (combo/b2b come and go with snapshot flags; the T8
/// clock/goal/countdown texts, the T13 feed meter and the T14 Zen lifetime
/// counter come and go with `ModeHudInfo`).
#[derive(Resource, Default)]
pub struct HudTextEntities {
    score: Option<Entity>,
    level: Option<Entity>,
    lines: Option<Entity>,
    combo: Option<Entity>,
    b2b: Option<Entity>,
    pause_hint: Option<Entity>,
    clock: Option<Entity>,
    goal: Option<Entity>,
    feed: Option<Entity>,
    lifetime: Option<Entity>,
    countdown: Option<Entity>,
}

/// Pooled preview root entities.
#[derive(Resource, Default)]
pub struct HudPreviewEntities {
    hold_root: Option<Entity>,
    next_roots: Vec<Entity>,
}

#[allow(clippy::type_complexity)]
type TextQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Text2d,
        &'static mut TextFont,
        &'static mut Transform,
    ),
    With<HudText>,
>;

type CellsQuery<'w, 's> = Query<
    'w,
    's,
    (&'static mut Sprite, &'static mut Transform),
    (
        With<HudMiniCell>,
        Without<NextPreview>,
        Without<HoldPreview>,
    ),
>;

/// Resolve the snapshot to render: fixture override wins over the live core.
fn hud_snapshot(fixture: Option<&HudFixture>, core: Option<&GameCore>) -> Option<GameSnapshot> {
    if let Some(snapshot) = fixture.and_then(|f| f.0.clone()) {
        return Some(snapshot);
    }
    core.map(|core| core.game.snapshot())
}

fn pick_window_size(windows: &Query<&Window>) -> Option<Vec2> {
    let window = windows.iter().next()?;
    let size = window.resolution.size();
    if size.x <= 0.0 || size.y <= 0.0 {
        return None;
    }
    Some(size)
}

fn slot_entity(entities: &mut HudTextEntities, slot: HudTextSlot) -> &mut Option<Entity> {
    match slot {
        HudTextSlot::Score => &mut entities.score,
        HudTextSlot::Level => &mut entities.level,
        HudTextSlot::Lines => &mut entities.lines,
        HudTextSlot::Combo => &mut entities.combo,
        HudTextSlot::B2B => &mut entities.b2b,
        HudTextSlot::PauseHint => &mut entities.pause_hint,
        HudTextSlot::Clock => &mut entities.clock,
        HudTextSlot::Goal => &mut entities.goal,
        HudTextSlot::Feed => &mut entities.feed,
        HudTextSlot::Lifetime => &mut entities.lifetime,
        HudTextSlot::Countdown => &mut entities.countdown,
    }
}

/// Desired text for a slot this frame (`None` = slot must be absent).
fn slot_text(slot: HudTextSlot, snapshot: &GameSnapshot) -> Option<String> {
    let text = match slot {
        HudTextSlot::Score => format!("SCORE\n{}", snapshot.score),
        HudTextSlot::Level => format!("LEVEL\n{}", snapshot.level),
        HudTextSlot::Lines => format!("LINES\n{}", snapshot.lines),
        HudTextSlot::Combo if snapshot.combo > 0 => format!("COMBO x{}", snapshot.combo),
        HudTextSlot::B2B if snapshot.b2b => "B2B".to_string(),
        // Mode slots are driven by `ModeHudInfo`, not the snapshot.
        HudTextSlot::Combo
        | HudTextSlot::B2B
        | HudTextSlot::PauseHint
        | HudTextSlot::Clock
        | HudTextSlot::Goal
        | HudTextSlot::Feed
        | HudTextSlot::Lifetime
        | HudTextSlot::Countdown => return None,
    };
    Some(text)
}

/// Human-readable label for one bound input.
pub fn bind_label(bind: &Bind) -> String {
    match bind {
        Bind::Key(KeyCode::Escape) => "Esc".to_string(),
        Bind::Key(key) => format!("{key:?}").trim_start_matches("Key").to_string(),
        Bind::WheelUp => "WheelUp".to_string(),
        Bind::WheelDown => "WheelDown".to_string(),
    }
}

/// `PAUSE  Esc / P` from the pause chord currently in the bindings.
pub fn pause_hint_text(binds: &[Bind]) -> String {
    if binds.is_empty() {
        return "PAUSE  (unbound)".to_string();
    }
    format!(
        "PAUSE  {}",
        binds.iter().map(bind_label).collect::<Vec<_>>().join(" / ")
    )
}

fn spawn_text(
    commands: &mut Commands,
    slot: HudTextSlot,
    text: String,
    center: Vec2,
    font_size: f32,
) -> Entity {
    commands
        .spawn((
            HudText { slot },
            Text2d::new(text),
            TextFont {
                font_size: bevy::text::FontSize::Px(font_size),
                ..default()
            },
            // T13: the feed meter borrows the versus pending-garbage color.
            TextColor(match slot {
                HudTextSlot::Feed => GARBAGE_COLOR,
                _ => Color::WHITE,
            }),
            Transform::from_xyz(center.x, center.y, 0.5),
        ))
        .id()
}

/// Insert-or-update one pooled text entity; despawn when `want` is `None`.
fn sync_text_slot(
    commands: &mut Commands,
    slot: HudTextSlot,
    want: Option<String>,
    slot_ref: &mut Option<Entity>,
    anchor: &HudAnchor,
    texts: &mut TextQuery,
) {
    let center = hud_text_center(anchor, slot);
    let font_size = match slot {
        HudTextSlot::Score | HudTextSlot::Level | HudTextSlot::Lines => anchor.cell * 0.45,
        HudTextSlot::Countdown => anchor.cell * 2.2,
        _ => anchor.cell * 0.36,
    };
    let font_size = font_size.max(8.0);
    match (slot_ref.take(), want) {
        (Some(entity), Some(text)) => match texts.get_mut(entity) {
            Ok((mut content, mut font, mut transform)) => {
                if content.0 != text {
                    content.0 = text;
                }
                font.font_size = bevy::text::FontSize::Px(font_size);
                transform.translation = center.extend(0.5);
                *slot_ref = Some(entity);
            }
            Err(_) => {
                *slot_ref = Some(spawn_text(commands, slot, text, center, font_size));
            }
        },
        (None, Some(text)) => {
            *slot_ref = Some(spawn_text(commands, slot, text, center, font_size));
        }
        (Some(entity), None) => {
            commands.entity(entity).despawn();
        }
        (None, None) => {}
    }
}

/// Resolve the four `ModeHudInfo`-driven slot texts for this frame
/// (`None` = slot absent): pre-roll countdown wins (clock/goal/feed hide),
/// otherwise a mode with a goal/clock/feed shows `TIME` (count-down when the
/// mode has a clock budget), its goal row, and — for feed modes — the
/// queued-garbage meter (`+N` rows + next-row seconds, T13).
fn mode_slot_texts(
    info: &ModeHudInfo,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    if info.countdown > 0 {
        return (
            None,
            None,
            Some(info.countdown.div_ceil(60).to_string()),
            None,
        );
    }
    if !info.show_hud {
        return (None, None, None, None);
    }
    let clock = info.clock_limit.map_or_else(
        || modes::format_time_ticks(info.clock_ticks),
        |limit| modes::format_time_ticks(limit.saturating_sub(info.clock_ticks)),
    );
    let goal = match (info.lines_left, info.garbage_left) {
        (Some(lines), _) => Some(format!(
            "Lines left: {lines}\nPieces: {}",
            info.pieces_placed
        )),
        (None, Some(rows)) => Some(format!("Garbage: {rows}")),
        (None, None) => None,
    };
    let feed = info.feed_pending.map(|pending| {
        let next_secs = info.feed_next_row_in.unwrap_or(0) as f32 / 60.0;
        format!("GARBAGE +{pending}\nNEXT {next_secs:.1}s")
    });
    (Some(format!("TIME\n{clock}")), goal, None, feed)
}

/// Score/level/lines always present; combo/b2b only while active; pause
/// hint reflects the live pause chord; the mode clock/goal/countdown rows
/// follow [`ModeHudInfo`] (hidden for modes with no goal, clock or feed),
/// and the Zen run adds the T14 lifetime-lines row from [`Records`].
#[allow(clippy::too_many_arguments)]
fn sync_hud_texts(
    mut commands: Commands,
    core: Option<NonSend<GameCore>>,
    fixture: Option<Res<HudFixture>>,
    mode: Option<Res<ModeHudInfo>>,
    bindings: Option<Res<KeyBindings>>,
    records: Option<Res<Records>>,
    windows: Query<&Window>,
    mut entities: ResMut<HudTextEntities>,
    mut texts: TextQuery,
    versus: Option<NonSend<VersusMatch>>,
) {
    // Defensive self-heal: re-assert the solo-HUD hide every frame while a
    // versus match runs, so no code path can leave solo panels visible on
    // top of the versus fields (playtest leak: the next queue showed up
    // over the right field mid-match).
    let hud_wanted = if versus.is_some_and(|versus| versus.active) {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    };
    for entity in [
        entities.score,
        entities.level,
        entities.lines,
        entities.combo,
        entities.b2b,
        entities.pause_hint,
        entities.clock,
        entities.goal,
        entities.feed,
        entities.lifetime,
        entities.countdown,
    ]
    .into_iter()
    .flatten()
    {
        commands.entity(entity).insert(hud_wanted);
    }
    let Some(snapshot) = hud_snapshot(fixture.as_deref(), core.as_deref()) else {
        return;
    };
    let Some(size) = pick_window_size(&windows) else {
        return;
    };
    let anchor = hud_anchor(size.x, size.y);

    let pause_binds = bindings
        .as_deref()
        .map(|b| b.slot(BindSlot::Pause).clone())
        .unwrap_or_else(|| KeyBindings::default_slot(BindSlot::Pause));

    for slot in [
        HudTextSlot::Score,
        HudTextSlot::Level,
        HudTextSlot::Lines,
        HudTextSlot::Combo,
        HudTextSlot::B2B,
    ] {
        let want = slot_text(slot, &snapshot);
        let slot_ref = slot_entity(&mut entities, slot);
        sync_text_slot(&mut commands, slot, want, slot_ref, &anchor, &mut texts);
    }
    // Portrait-native has a visible pause button; the keyboard-chord hint
    // would just add clutter to the touch deck.
    let pause_hint = if anchor.portrait {
        None
    } else {
        Some(pause_hint_text(&pause_binds))
    };
    let slot_ref = slot_entity(&mut entities, HudTextSlot::PauseHint);
    sync_text_slot(
        &mut commands,
        HudTextSlot::PauseHint,
        pause_hint,
        slot_ref,
        &anchor,
        &mut texts,
    );

    // T8 mode feed: bridge-written resource; apps without the bridge fall
    // back to the hidden-marathon default (read-only here, never mutated).
    let mode = mode.map(|info| *info).unwrap_or_default();
    let (clock, goal, countdown, feed) = mode_slot_texts(&mode);
    // T14 Zen: session lines are the regular `LINES` stat; the lifetime
    // total comes from `Records` (missing entry ⇒ 0). Zen has no goal/clock
    // so the slot never clashes with clock/goal/feed.
    let lifetime = if mode.mode_id == modes::ModeId::Zen && mode.countdown == 0 {
        let total = records
            .as_deref()
            .and_then(|records| records.record_for(crate::records::ZEN))
            .and_then(|record| match record {
                Record::LifetimeLines { total } => Some(*total),
                _ => None,
            })
            .unwrap_or(0);
        Some(format!("LIFETIME\n{total}"))
    } else {
        None
    };
    for (slot, want) in [
        (HudTextSlot::Clock, clock),
        (HudTextSlot::Goal, goal),
        (HudTextSlot::Countdown, countdown),
        (HudTextSlot::Feed, feed),
        (HudTextSlot::Lifetime, lifetime),
    ] {
        let slot_ref = slot_entity(&mut entities, slot);
        sync_text_slot(&mut commands, slot, want, slot_ref, &anchor, &mut texts);
    }
}

/// Zen lifetime-lines bookkeeping (T14): every `LineCleared` seen during a
/// Zen run folds into [`Records::add_lifetime_lines`] (saturating, keyed
/// `records::ZEN`). Mutating [`Records`] dirties it for the existing
/// debounced `records_flush_system` — no per-line disk writes, and the
/// exit-flush catches a quit mid-debounce. Apps without `Records` or the
/// bridge stay untouched; the reader is *drained in every mode* so its
/// cursor never lags and never folds foreign clears into the Zen total.
fn zen_lifetime_lines_system(
    core: Option<NonSend<GameCore>>,
    mut records: Option<ResMut<Records>>,
    mut events: MessageReader<CoreEvent>,
) {
    let zen = core.is_some_and(|core| core.active_mode.id == modes::ModeId::Zen);
    let mut lifetime = records.as_deref_mut();
    for CoreEvent(event) in events.read() {
        let GameEvent::LineCleared { lines } = event else {
            continue;
        };
        if let (true, Some(lifetime)) = (zen, lifetime.as_deref_mut()) {
            lifetime.add_lifetime_lines(*lines as u64);
        }
    }
}

/// Spawn one next-queue preview root with its four mini cells.
fn spawn_next_preview(
    commands: &mut Commands,
    piece: Piece,
    index: usize,
    center: Vec2,
    s: f32,
) -> Entity {
    let root = commands
        .spawn((
            NextPreview { piece, index },
            Transform::from_xyz(center.x, center.y, 0.5),
        ))
        .id();
    for offset in mini_offsets(piece, s) {
        let cell = commands
            .spawn((
                HudMiniCell,
                Sprite {
                    color: render::piece_color(piece),
                    custom_size: Some(Vec2::splat(s)),
                    ..default()
                },
                Transform::from_translation(offset.extend(0.0)),
            ))
            .id();
        commands.entity(root).add_child(cell);
    }
    root
}

/// Re-arm one existing preview root: anchor, palette, footprint layout.
type NextQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut NextPreview,
        &'static mut Transform,
        &'static Children,
    ),
    (
        With<NextPreview>,
        Without<HoldPreview>,
        Without<HudMiniCell>,
    ),
>;

type HoldQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut HoldPreview,
        &'static mut Transform,
        Option<&'static Children>,
    ),
    (
        With<HoldPreview>,
        Without<NextPreview>,
        Without<HudMiniCell>,
    ),
>;

/// Placement for one mini preview: root center plus cell edge length.
struct MiniLayout {
    center: Vec2,
    s: f32,
}

fn refresh_next_preview(
    preview: &mut NextPreview,
    transform: &mut Transform,
    cell_entities: &[Entity],
    piece: Piece,
    index: usize,
    layout: &MiniLayout,
    cells: &mut CellsQuery,
) {
    transform.translation = layout.center.extend(0.5);
    let relaid = preview.piece != piece;
    preview.piece = piece;
    preview.index = index;
    let offsets = mini_offsets(piece, layout.s);
    let color = render::piece_color(piece);
    for (i, child) in cell_entities.iter().enumerate() {
        if let Ok((mut sprite, mut sprite_transform)) = cells.get_mut(*child) {
            sprite.color = color;
            sprite.custom_size = Some(Vec2::splat(layout.s));
            if relaid {
                let offset = offsets.get(i).copied().unwrap_or(Vec2::ZERO);
                sprite_transform.translation = offset.extend(0.0);
            }
        }
    }
}

/// Alpha of hold-preview cells while `hold_used` blocks the swap (M5: split
/// out of `GHOST_ALPHA`, which now also drives the ghost *outline* and moved
/// to a higher value with it).
pub const HOLD_USED_ALPHA: f32 = 0.3;

fn hold_cell_color(piece: Piece, used: bool) -> Color {
    let color = render::piece_color(piece);
    if used {
        color.with_alpha(HOLD_USED_ALPHA)
    } else {
        color
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_hud_previews(
    mut commands: Commands,
    core: Option<NonSend<GameCore>>,
    fixture: Option<Res<HudFixture>>,
    settings: Option<Res<Settings>>,
    windows: Query<&Window>,
    mut entities: ResMut<HudPreviewEntities>,
    mut next_q: NextQuery,
    mut hold_q: HoldQuery,
    mut cells: CellsQuery,
    versus: Option<NonSend<VersusMatch>>,
) {
    // Self-heal the versus hide every frame (see `sync_hud_texts`).
    let hud_wanted = if versus.is_some_and(|versus| versus.active) {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    };
    for entity in entities
        .hold_root
        .into_iter()
        .chain(entities.next_roots.iter().copied())
    {
        commands.entity(entity).insert(hud_wanted);
    }
    let Some(snapshot) = hud_snapshot(fixture.as_deref(), core.as_deref()) else {
        return;
    };
    let Some(size) = pick_window_size(&windows) else {
        return;
    };
    let anchor = hud_anchor(size.x, size.y);
    let mini = anchor.cell * MINI_FACTOR;

    // Next queue: pool roots to min(queue_size clamped 1..=6, next.len()).
    let configured = settings.as_deref().map_or(DEFAULT_QUEUE_SIZE, |s| {
        s.next_queue_size.clamp(1, MAX_NEXT as u8) as usize
    });
    // T23 **One Preview**: the run's mutators beat the global setting while
    // active (fixture-only HUDs have no `GameCore`, so fixtures are
    // unaffected; versus panels render through `sync_versus_hud` instead).
    let configured = if core.as_deref().is_some_and(|core| {
        core.active_mode
            .mutators
            .contains(crate::mutators::Mutators::ONE_PREVIEW)
    }) {
        1
    } else {
        configured
    };
    let wanted = configured.min(snapshot.next.len());
    while entities.next_roots.len() > wanted {
        let entity = entities.next_roots.pop().expect("length checked");
        commands.entity(entity).despawn();
    }
    while entities.next_roots.len() < wanted {
        let index = entities.next_roots.len();
        let entity = spawn_next_preview(
            &mut commands,
            snapshot.next[index],
            index,
            next_center(&anchor, index),
            mini,
        );
        entities.next_roots.push(entity);
    }
    for (index, entity) in entities.next_roots.iter().copied().enumerate() {
        let Ok((mut preview, mut transform, children)) = next_q.get_mut(entity) else {
            continue;
        };
        let layout = MiniLayout {
            center: next_center(&anchor, index),
            s: mini,
        };
        refresh_next_preview(
            &mut preview,
            &mut transform,
            &children[..],
            snapshot.next[index],
            index,
            &layout,
            &mut cells,
        );
    }

    // Hold box: root always present, 0 or 4 mini cells.
    let hold_root = match entities.hold_root {
        Some(entity) => entity,
        None => {
            let entity = commands
                .spawn((
                    HoldPreview {
                        piece: None,
                        used: false,
                    },
                    Transform::from_translation(hold_center(&anchor).extend(0.5)),
                ))
                .id();
            entities.hold_root = Some(entity);
            return;
        }
    };
    let Ok((mut hold, mut root_transform, children)) = hold_q.get_mut(hold_root) else {
        return;
    };
    root_transform.translation = hold_center(&anchor).extend(0.5);
    let changed = hold.piece != snapshot.hold || hold.used != snapshot.hold_used;
    hold.piece = snapshot.hold;
    hold.used = snapshot.hold_used;
    let wanted_cells = if snapshot.hold.is_some() { 4 } else { 0 };
    let children: Vec<Entity> = children.map(|c| c.iter().collect()).unwrap_or_default();
    for extra in children.iter().skip(wanted_cells) {
        commands.entity(*extra).despawn();
    }
    if changed {
        if let Some(piece) = snapshot.hold {
            let color = hold_cell_color(piece, snapshot.hold_used);
            let offsets = mini_offsets(piece, mini);
            for (i, child) in children.iter().take(wanted_cells).enumerate() {
                if let Ok((mut sprite, mut sprite_transform)) = cells.get_mut(*child) {
                    sprite.color = color;
                    sprite.custom_size = Some(Vec2::splat(mini));
                    if let Some(offset) = offsets.get(i) {
                        sprite_transform.translation = offset.extend(0.0);
                    }
                }
            }
            for offset in &offsets[children.len()..wanted_cells] {
                let cell = commands
                    .spawn((
                        HudMiniCell,
                        Sprite {
                            color,
                            custom_size: Some(Vec2::splat(mini)),
                            ..default()
                        },
                        Transform::from_translation(offset.extend(0.0)),
                    ))
                    .id();
                commands.entity(hold_root).add_child(cell);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// T26: versus (1v1) HUD
// ---------------------------------------------------------------------------

/// Mini cell size of the versus next previews, in versus cell units.
const VERSUS_NEXT_MINI: f32 = 0.45;
/// Mini cell size of the (small) versus hold box, in versus cell units.
const VERSUS_HOLD_MINI: f32 = 0.35;
/// Next previews rendered per versus side (PRD §6.3 preview, compact form).
const VERSUS_NEXT_SLOTS: usize = 2;
/// Incoming-garbage `+N` indicator color.
const GARBAGE_COLOR: Color = Color::srgb(1.0, 0.45, 0.25);
const FINISHED_COLOR: Color = Color::srgb(1.0, 0.81, 0.43);

/// Compact-panel anchors for one side of an active versus match (T26),
/// derived from [`crate::render::versus_layouts`] so HUD and playfield share
/// one layout. `panel_x` sits in the outer margin of the side's half-window,
/// between the field edge and the window edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VersusPanelAnchor {
    /// Shared versus cell edge length.
    pub cell: f32,
    /// World x of the side panel center line.
    pub panel_x: f32,
    /// World y of the side's field top edge.
    pub field_top: f32,
}

/// `[left, right]` versus panel anchors for a window size. Landscape puts
/// the compact panel in the outer margin of each half; portrait-native has
/// no side margin, so the panel centers over each field (the stack starts
/// just below the top of the spawn buffer and stays readable over the
/// normally-empty rows).
pub fn versus_panel_anchors(window_w: f32, window_h: f32) -> [VersusPanelAnchor; 2] {
    let [left, right] = render::versus_layouts(window_w, window_h);
    let cell = left.cell;
    if render::portrait_layout(window_w, window_h) {
        return [
            VersusPanelAnchor {
                cell,
                panel_x: left.origin.x + cell * COLS as f32 * 0.5,
                field_top: left.origin.y,
            },
            VersusPanelAnchor {
                cell,
                panel_x: right.origin.x + cell * COLS as f32 * 0.5,
                field_top: right.origin.y,
            },
        ];
    }
    let gap = (PANEL_GAP + PANEL_HALF) * cell;
    [
        VersusPanelAnchor {
            cell,
            panel_x: left.origin.x - gap,
            field_top: left.origin.y,
        },
        VersusPanelAnchor {
            cell,
            panel_x: right.origin.x + cell * COLS as f32 + gap,
            field_top: right.origin.y,
        },
    ]
}

/// Which versus stat a [`VersusHudText`] renders.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersusHudSlot {
    /// `SCORE` + value.
    Score,
    /// `LINES` + value.
    Lines,
    /// `LEVEL` + value.
    Level,
    /// `+N` incoming-garbage indicator (empty text when nothing pending).
    /// Under the Dig rule (T20) it doubles as the buried-board `DUG n/10`
    /// progress counter instead.
    Pending,
    /// `FINISHED` badge for a side that completed a Race target (empty
    /// text while it still races or tops out). Under the Switch rule (T21)
    /// it doubles as the match-wide `SWAP n` countdown — under Switch no
    /// side ever finishes a Race, so the badge is structurally dead there.
    /// Inside the swap warning window (`warning_ticks` before a boundary)
    /// the text turns to the garbage-orange warning color.
    Status,
}

/// Marker on a versus stat [`Text2d`], tagged with the side it mirrors.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersusHudText {
    /// Match side this value belongs to.
    pub side: Side,
    /// Which stat this text renders.
    pub slot: VersusHudSlot,
}

/// Which preview slot a [`VersusPreview`] occupies.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersusPreviewRole {
    /// Next-queue slot `index` (`0..VERSUS_NEXT_SLOTS`).
    Next(usize),
    /// The (small) hold box.
    Hold,
}

/// Root marker of one versus mini preview (2 next slots + hold per side).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersusPreview {
    /// Match side this preview belongs to.
    pub side: Side,
    /// Which slot this preview fills.
    pub role: VersusPreviewRole,
}

/// Marker on versus mini-preview cell sprites.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersusMiniCell;

/// Root marker of one side's versus HUD block (all versus entities are its
/// children, so a single root toggle shows/hides the whole panel). Unlike
/// the menu roots it is toggled by [`VersusMatch::active`], not by an
/// [`AppState`](crate::state::AppState) variant — versus plays *inside*
/// `Playing`. See `screens_menu::sync_root_visibility`.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersusHudRoot {
    /// Match side this root carries.
    pub side: Side,
}

/// Test/QA hook mirroring [`HudFixture`]: when set, the versus HUD renders
/// from this match snapshot instead of the live [`VersusMatch`]. Still
/// snapshot-driven — only the source changes.
#[derive(Resource, Default, Clone)]
pub struct VersusHudFixture(pub Option<tetris_core::versus::MatchSnapshot>);

/// `+N` incoming-garbage label (empty string when nothing pending, so the
/// pooled text simply renders nothing).
fn versus_pending_text(pending: u32) -> String {
    if pending > 0 {
        format!("+{pending}")
    } else {
        String::new()
    }
}

/// Rows of the snapshot's board that still contain a [`Piece::Garbage`]
/// cell — the HUD-side mirror of the core's `Game::garbage_rows_left()`
/// (`GameSnapshot` exposes the board, not the getter).
fn buried_rows_left(game: &GameSnapshot) -> usize {
    (0..ROWS)
        .filter(|&r| (0..COLS).any(|c| game.board.get(r, c) == Some(Piece::Garbage)))
        .count()
}

/// Dig Duel progress label (T20): rows dug of the shared 10-row buried
/// board. Reuses the Pending slot — under Dig no garbage ever travels, so
/// the `+N` meter is structurally dead there.
fn versus_dug_text(rows_left: usize) -> String {
    let dug = DIG_DUEL_GARBAGE_ROWS.saturating_sub(rows_left);
    format!("DUG {dug}/{DIG_DUEL_GARBAGE_ROWS}")
}

/// `FINISHED` badge text for a side that completed a Race target (empty
/// while it still races).
fn versus_status_text(finished: bool) -> String {
    if finished {
        "FINISHED".to_string()
    } else {
        String::new()
    }
}

/// Switch-rule swap countdown (T21), derived purely from the snapshot
/// (fixture-driven like the rest of the versus HUD): whole seconds until
/// the next swap (rounded up) and whether the match is already inside the
/// `warning_ticks` window before that boundary. `None` for every other
/// rule (and for the degenerate zero-interval config the core keeps inert).
fn versus_swap_info(snapshot: &tetris_core::versus::MatchSnapshot) -> Option<(u32, bool)> {
    let AttackRule::Switch {
        swap_interval_ticks,
        warning_ticks,
    } = snapshot.rule
    else {
        return None;
    };
    if swap_interval_ticks == 0 {
        return None;
    }
    let interval = swap_interval_ticks as u64;
    let boundary = (snapshot.swaps_done as u64 + 1) * interval;
    let remaining = boundary.saturating_sub(snapshot.match_ticks);
    let warning = snapshot.match_ticks.saturating_add(warning_ticks as u64) >= boundary;
    Some((remaining.div_ceil(60) as u32, warning))
}

/// World-space center of a versus stat text.
pub fn versus_text_center(anchor: &VersusPanelAnchor, slot: VersusHudSlot) -> Vec2 {
    let c = anchor.cell;
    let y = match slot {
        VersusHudSlot::Score => anchor.field_top - 1.5 * c,
        VersusHudSlot::Lines => anchor.field_top - 4.2 * c,
        VersusHudSlot::Level => anchor.field_top - 6.9 * c,
        VersusHudSlot::Pending => anchor.field_top - 9.2 * c,
        VersusHudSlot::Status => anchor.field_top - 10.5 * c,
    };
    Vec2::new(anchor.panel_x, y)
}

/// World-space center of the versus hold box.
pub fn versus_hold_center(anchor: &VersusPanelAnchor) -> Vec2 {
    Vec2::new(anchor.panel_x, anchor.field_top - 11.5 * anchor.cell)
}

/// World-space center of versus next-preview slot `index`.
pub fn versus_next_center(anchor: &VersusPanelAnchor, index: usize) -> Vec2 {
    Vec2::new(
        anchor.panel_x,
        anchor.field_top - (14.0 + 2.8 * index as f32) * anchor.cell,
    )
}

/// Spawn the (hidden) versus HUD tree for one side once on `Startup`.
fn spawn_versus_side(commands: &mut Commands, side: Side) {
    commands
        .spawn((
            VersusHudRoot { side },
            Visibility::Hidden,
            Transform::default(),
        ))
        .with_children(|root| {
            for slot in [
                VersusHudSlot::Score,
                VersusHudSlot::Lines,
                VersusHudSlot::Level,
                VersusHudSlot::Pending,
                VersusHudSlot::Status,
            ] {
                root.spawn((
                    VersusHudText { side, slot },
                    Text2d::new(String::new()),
                    TextFont {
                        font_size: bevy::text::FontSize::Px(12.0),
                        ..default()
                    },
                    TextColor(match slot {
                        VersusHudSlot::Pending => GARBAGE_COLOR,
                        VersusHudSlot::Status => FINISHED_COLOR,
                        _ => Color::WHITE,
                    }),
                    Transform::from_xyz(0.0, 0.0, 0.5),
                ));
            }
            let roles = [
                VersusPreviewRole::Hold,
                VersusPreviewRole::Next(0),
                VersusPreviewRole::Next(1),
            ];
            for role in roles {
                root.spawn((
                    VersusPreview { side, role },
                    Visibility::Hidden,
                    Transform::from_xyz(0.0, 0.0, 0.5),
                ))
                .with_children(|preview| {
                    for _ in 0..4 {
                        preview.spawn((
                            VersusMiniCell,
                            // Explicit zero-size placeholder: a bare
                            // `Sprite::default()` is a 100x100 white quad
                            // that would flash giant if ever drawn before
                            // the sync pass sizes it.
                            Sprite {
                                color: Color::NONE,
                                custom_size: Some(Vec2::ZERO),
                                ..default()
                            },
                            Transform::from_translation(Vec3::ZERO),
                        ));
                    }
                });
            }
        });
}

/// Spawn both versus HUD roots.
fn spawn_versus_hud(mut commands: Commands) {
    spawn_versus_side(&mut commands, Side::Left);
    spawn_versus_side(&mut commands, Side::Right);
}

#[allow(clippy::type_complexity)]
type VersusTextQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static VersusHudText,
        &'static mut Text2d,
        &'static mut TextFont,
        &'static mut Transform,
        &'static mut TextColor,
    ),
    (
        With<VersusHudText>,
        Without<VersusPreview>,
        Without<VersusMiniCell>,
    ),
>;

#[allow(clippy::type_complexity)]
type VersusPreviewQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static VersusPreview,
        &'static mut Visibility,
        &'static mut Transform,
        &'static Children,
    ),
    (
        With<VersusPreview>,
        Without<VersusHudText>,
        Without<VersusMiniCell>,
    ),
>;

#[allow(clippy::type_complexity)]
type VersusMiniQuery<'w, 's> = Query<
    'w,
    's,
    (&'static mut Sprite, &'static mut Transform),
    (
        With<VersusMiniCell>,
        Without<VersusHudText>,
        Without<VersusPreview>,
    ),
>;

/// Reposition and refill the versus HUD from the match snapshot every
/// `Update` (root visibility itself is owned by the screens-menu sync).
/// A ladder-origin match (T16) re-targets the Status slot to the
/// `RUNG n/8` badge — while a Garbage-rule ladder runs, the slot's
/// original `FINISHED` race badge can never be live anyway.
#[allow(clippy::too_many_arguments)]
fn sync_versus_hud(
    versus: Option<NonSend<VersusMatch>>,
    fixture: Option<Res<VersusHudFixture>>,
    flow: Option<Res<VersusFlow>>,
    windows: Query<&Window>,
    mut texts: VersusTextQuery,
    mut previews: VersusPreviewQuery,
    mut minis: VersusMiniQuery,
) {
    let snapshot = fixture
        .and_then(|fixture| fixture.0.clone())
        .or_else(|| versus.map(|versus| versus.match_.snapshot()));
    let Some(snapshot) = snapshot else {
        return;
    };
    let Some(size) = pick_window_size(&windows) else {
        return;
    };
    let [left, right] = versus_panel_anchors(size.x, size.y);
    // Ladder badge (T16): rung of the live match, if any (the flow marker
    // is app-side; VersusMatch knows nothing about the campaign).
    let ladder_rung = flow.and_then(|flow| match flow.ladder {
        LadderOrigin::Match { rung } => Some(rung),
        _ => None,
    });
    // Switch swap countdown (T21): match-wide, shown identically on both
    // side panels; the ladder badge (Garbage-only flows) outranks it.
    let swap = versus_swap_info(&snapshot);

    for (meta, mut text, mut font, mut transform, mut color) in texts.iter_mut() {
        let anchor = if meta.side == Side::Left { left } else { right };
        let game = if meta.side == Side::Left {
            &snapshot.left
        } else {
            &snapshot.right
        };
        let pending = if meta.side == Side::Left {
            snapshot.pending.0
        } else {
            snapshot.pending.1
        };
        let finished = if meta.side == Side::Left {
            snapshot.finished.0
        } else {
            snapshot.finished.1
        };
        font.font_size = bevy::text::FontSize::Px((anchor.cell * 0.45).max(8.0));
        transform.translation = versus_text_center(&anchor, meta.slot).extend(0.5);
        let content = match meta.slot {
            VersusHudSlot::Score => format!("SCORE\n{}", game.score),
            VersusHudSlot::Lines => format!("LINES\n{}", game.lines),
            VersusHudSlot::Level => format!("LEVEL\n{}", game.level),
            VersusHudSlot::Pending => match snapshot.rule {
                // Dig Duel: the pending meter doubles as the shared buried
                // board's dug-rows counter (no garbage ever travels under
                // Dig). Garbage/Race keep the `+N` incoming indicator.
                AttackRule::Dig => versus_dug_text(buried_rows_left(game)),
                _ => versus_pending_text(pending),
            },
            VersusHudSlot::Status => {
                // Badge precedence: ladder rung > Switch swap countdown >
                // race `FINISHED`. The color follows (garbage-orange inside
                // the swap warning window, white during the plain countdown,
                // the gold badge color otherwise — restored every frame so
                // a fixture-driven rule switch can never leave a stale
                // warning color behind).
                let (content, wanted) = match ladder_rung {
                    Some(rung) => (ladder_badge_text(rung), FINISHED_COLOR),
                    None => match swap {
                        Some((secs, warning)) => (
                            format!("SWAP {secs}"),
                            if warning { GARBAGE_COLOR } else { Color::WHITE },
                        ),
                        None => (versus_status_text(finished), FINISHED_COLOR),
                    },
                };
                if color.0 != wanted {
                    color.0 = wanted;
                }
                content
            }
        };
        if text.0 != content {
            text.0 = content;
        }
    }

    for (preview, mut visibility, mut transform, children) in previews.iter_mut() {
        let anchor = if preview.side == Side::Left {
            left
        } else {
            right
        };
        let game = if preview.side == Side::Left {
            &snapshot.left
        } else {
            &snapshot.right
        };
        let (piece, s) = match preview.role {
            VersusPreviewRole::Next(index) => {
                transform.translation = versus_next_center(&anchor, index).extend(0.5);
                (
                    game.next.get(index).copied(),
                    VERSUS_NEXT_MINI * anchor.cell,
                )
            }
            VersusPreviewRole::Hold => {
                transform.translation = versus_hold_center(&anchor).extend(0.5);
                (game.hold, VERSUS_HOLD_MINI * anchor.cell)
            }
        };
        let wanted = if piece.is_some() {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *visibility != wanted {
            *visibility = wanted;
        }
        let Some(piece) = piece else { continue };
        let color = render::piece_color(piece);
        let offsets = mini_offsets(piece, s);
        for (index, child) in children.iter().enumerate() {
            if let Ok((mut sprite, mut cell_transform)) = minis.get_mut(child) {
                sprite.color = color;
                sprite.custom_size = Some(Vec2::splat(s));
                if let Some(offset) = offsets.get(index) {
                    cell_transform.translation = offset.extend(0.0);
                }
            }
        }
    }
}

/// Hide the solo HUD panels (stat texts, next queue, hold box) for the whole
/// duration of a versus match; restore them untouched on `end_versus`. The
/// mini cells hang under the preview roots and inherit their visibility, so
/// toggling the roots suffices. Preview roots are plain entities (no render
/// component ⇒ no required `Visibility`), so the flag is inserted on demand
/// while versus runs and reset to `Inherited` afterwards. The Or-filter
/// keeps foreign entities (playfield sprites, camera) out of the query —
/// same discipline as the menu root sync.
#[allow(clippy::type_complexity)]
type SoloHudVisibility<'w, 's> = Query<
    'w,
    's,
    (Entity, Option<&'static mut Visibility>),
    Or<(With<HudText>, With<NextPreview>, With<HoldPreview>)>,
>;

fn sync_solo_hud_visibility(
    versus: Option<NonSend<VersusMatch>>,
    mut commands: Commands,
    mut solo: SoloHudVisibility,
) {
    let hidden = versus.is_some_and(|versus| versus.active);
    let wanted = if hidden {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    };
    for (entity, visibility) in solo.iter_mut() {
        match visibility {
            Some(mut current) => {
                if *current != wanted {
                    *current = wanted;
                }
            }
            // Roots without any render component carry no `Visibility` at
            // all; only versus needs one (insert), never solo-off.
            None => {
                if hidden {
                    commands.entity(entity).insert(wanted);
                }
            }
        }
    }
}

/// Snapshot-driven HUD: side panels anchored to the letterboxed playfield
/// (solo) plus the compact per-side versus panels (T26).
pub struct HudPlugin;

/// M5: dress every HUD mini-preview cell (next queue, hold, versus boxes)
/// with the same beveled tile texture the playfield uses, so previews read
/// as blocks rather than flat chips. Colors/sizes stay owned by the sync
/// systems — this only ever swaps the texture, and runs headless-safe
/// (without [`ArtAssets`] nothing happens).
type MiniCellStyleQuery<'w, 's> =
    Query<'w, 's, &'static mut Sprite, Or<(With<HudMiniCell>, With<VersusMiniCell>)>>;

fn style_mini_cells(art: Option<Res<ArtAssets>>, mut cells: MiniCellStyleQuery) {
    let Some(art) = art else { return };
    for mut sprite in cells.iter_mut() {
        if sprite.image.id() != art.tile.id() {
            sprite.image = art.tile.clone();
        }
    }
}

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HudFixture>()
            .init_resource::<VersusHudFixture>()
            .init_resource::<HudTextEntities>()
            .init_resource::<HudPreviewEntities>()
            .add_systems(Startup, spawn_versus_hud)
            .add_systems(
                Update,
                (
                    (
                        zen_lifetime_lines_system,
                        (sync_hud_texts, sync_hud_previews).chain(),
                    )
                        .chain(),
                    sync_versus_hud,
                    style_mini_cells,
                    sync_solo_hud_visibility,
                )
                    .chain(),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_bridge::{CoreBridgePlugin, PendingActions};
    use crate::state::AppState;

    use bevy::app::FixedUpdate;
    use bevy::window::WindowPlugin;
    use tetris_core::actions::Action;

    const EPS: f32 = 1e-4;

    fn hud_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris T13 hud smoke".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((CoreBridgePlugin, HudPlugin));
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app.init_resource::<Settings>();
        app
    }

    fn frame(app: &mut App, actions: &[Action]) {
        {
            let mut pending = app.world_mut().resource_mut::<PendingActions>();
            for action in actions {
                pending.push(*action);
            }
        }
        app.world_mut().run_schedule(FixedUpdate);
        let _ = app.world_mut().try_run_schedule(Update);
    }

    fn snapshot(app: &App) -> GameSnapshot {
        app.world().non_send::<GameCore>().game.snapshot()
    }

    fn text_of(app: &mut App, slot: HudTextSlot) -> Option<String> {
        let mut query = app.world_mut().query::<(&HudText, &Text2d)>();
        let items: Vec<(HudTextSlot, String)> = query
            .iter(app.world())
            .map(|(text, content)| (text.slot, content.0.clone()))
            .collect();
        items
            .into_iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, content)| content)
    }

    fn next_previews(app: &mut App) -> Vec<(usize, Piece)> {
        let mut query = app.world_mut().query::<&NextPreview>();
        let mut previews: Vec<(usize, Piece)> = query
            .iter(app.world())
            .map(|preview| (preview.index, preview.piece))
            .collect();
        previews.sort_by_key(|(index, _)| *index);
        previews
    }

    fn hold_state(app: &mut App) -> (HoldPreview, Vec<Color>) {
        let mut query = app.world_mut().query::<(&HoldPreview, &Children)>();
        let (hold, children) = {
            let (hold, children) = query.single(app.world()).unwrap();
            (*hold, children.iter().collect::<Vec<_>>())
        };
        drop(query);
        let colors = children
            .iter()
            .map(|child| {
                app.world()
                    .get::<Sprite>(*child)
                    .expect("hold mini cell")
                    .color
            })
            .collect();
        (hold, colors)
    }

    #[test]
    fn stat_texts_mirror_the_snapshot() {
        let mut app = hud_app(0xC0FFEE);
        frame(&mut app, &[Action::HardDrop]);
        let snapshot = snapshot(&app);
        assert!(snapshot.score > 0, "precondition: hard drop scored");
        assert_eq!(
            text_of(&mut app, HudTextSlot::Score),
            Some(format!("SCORE\n{}", snapshot.score))
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Level),
            Some(format!("LEVEL\n{}", snapshot.level))
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Lines),
            Some(format!("LINES\n{}", snapshot.lines))
        );
    }

    #[test]
    fn next_queue_size_bounds_preview_count() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        assert_eq!(next_previews(&mut app).len(), 5, "default size 5");

        app.world_mut().resource_mut::<Settings>().next_queue_size = 3;
        let _ = app.world_mut().try_run_schedule(Update);
        let previews = next_previews(&mut app);
        assert_eq!(previews.len(), 3);
        assert_eq!(
            previews.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        app.world_mut().resource_mut::<Settings>().next_queue_size = 6;
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            next_previews(&mut app).len(),
            5,
            "clamped to snapshot.next.len()"
        );
        let expected: Vec<(usize, Piece)> =
            snapshot(&app).next.iter().copied().enumerate().collect();
        assert_eq!(
            next_previews(&mut app),
            expected,
            "pieces match snapshot.next"
        );
    }

    #[test]
    fn next_previews_match_palette_and_spawn_footprint() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        let anchor = hud_anchor(1280.0, 720.0);
        let mini = anchor.cell * MINI_FACTOR;
        let collected: Vec<(NextPreview, Vec<(Vec2, Color)>)> = {
            let mut query = app.world_mut().query::<(&NextPreview, &Children)>();
            let items: Vec<(NextPreview, Vec<Entity>)> = query
                .iter(app.world())
                .map(|(preview, children)| (*preview, children.iter().collect()))
                .collect();
            drop(query);
            items
                .into_iter()
                .map(|(preview, children)| {
                    let cells = children
                        .iter()
                        .map(|child| {
                            let sprite = app.world().get::<Sprite>(*child).unwrap();
                            let transform = app.world().get::<Transform>(*child).unwrap();
                            (transform.translation.xy(), sprite.color)
                        })
                        .collect();
                    (preview, cells)
                })
                .collect()
        };
        assert_eq!(collected.len(), 5);
        for (preview, cells) in &collected {
            assert_eq!(cells.len(), 4, "four mini cells per preview");
            assert_eq!(next_center(&anchor, preview.index).x, anchor.right_panel_x);
            for (offset, color) in cells {
                let on_footprint = mini_offsets(preview.piece, mini)
                    .into_iter()
                    .any(|expected| (expected - *offset).length() < EPS);
                assert!(on_footprint, "mini cell {offset:?} off spawn footprint");
                assert_eq!(
                    *color,
                    render::piece_color(preview.piece),
                    "preview palette via piece_color"
                );
            }
        }
    }

    /// Portrait-native anchors: hold + horizontal next queue + stat row
    /// inside the reserved top strip, combo/B2B in a deck row under the
    /// field, nothing off-screen.
    #[test]
    fn portrait_anchors_form_top_strip_and_deck_row() {
        render::set_portrait_override(Some(true));
        let (w, h) = (1080.0, 2404.0);
        let anchor = hud_anchor(w, h);
        assert!(anchor.portrait);
        let c = anchor.cell;

        let hold = hold_center(&anchor);
        let next0 = next_center(&anchor, 0);
        let next5 = next_center(&anchor, 5);
        let score = hud_text_center(&anchor, HudTextSlot::Score);
        let level = hud_text_center(&anchor, HudTextSlot::Level);
        let lines = hud_text_center(&anchor, HudTextSlot::Lines);
        let combo = hud_text_center(&anchor, HudTextSlot::Combo);
        let b2b = hud_text_center(&anchor, HudTextSlot::B2B);

        // Top strip (above the field), score row highest, then hold/next.
        assert!(hold.y > anchor.field_top && score.y > hold.y);
        assert!(next0.x > hold.x && next5.x > next0.x);
        assert!(level.x < score.x && lines.x > score.x);
        assert!(
            score.y + 0.8 * c < h * 0.5 - 50.0 + EPS,
            "score row intrudes on the status/cutout zone"
        );
        assert!(
            hold.y - 1.15 * c >= anchor.field_top - EPS,
            "hold box sinks into the field"
        );
        // Queue stays fully on screen even at 6 slots.
        assert!(next5.x + 1.15 * c <= w * 0.5 + EPS);
        assert!(next_center(&anchor, 11).x + 1.15 * c <= w * 0.5 + EPS);
        // Deck row: under the field, above the button deck.
        assert!(combo.y < anchor.field_bottom && b2b.y < anchor.field_bottom);
        assert!(combo.y > anchor.field_bottom - render::PORTRAIT_BOTTOM_FRAC * h * 0.5);
        assert!(b2b.x > combo.x);

        // Landscape windows keep the classic side-panel layout.
        let landscape = hud_anchor(1280.0, 720.0);
        assert!(!landscape.portrait);
        assert_eq!(landscape.field_top, landscape.window_h * 0.5);
    }

    #[test]
    fn hold_dims_while_hold_used() {
        let mut app = hud_app(1);
        // First press: parks the active piece, hold_used -> true.
        frame(&mut app, &[Action::Hold]);
        // One more HUD tick so the freshly spawned hold root gets populated.
        let _ = app.world_mut().try_run_schedule(Update);
        let after_hold = snapshot(&app);
        assert!(after_hold.hold.is_some());
        assert!(after_hold.hold_used, "precondition: hold just used");
        let (hold, colors) = hold_state(&mut app);
        assert_eq!(hold.piece, after_hold.hold);
        assert!(hold.used);
        assert_eq!(colors.len(), 4);
        for color in &colors {
            assert_eq!(color.to_srgba().alpha, HOLD_USED_ALPHA, "dimmed preview");
            assert_eq!(
                color.with_alpha(1.0),
                render::piece_color(after_hold.hold.expect("hold piece")),
                "dim keeps the piece hue"
            );
        }

        // Locking the swapped-in piece resets hold_used; hold stays populated.
        frame(&mut app, &[Action::HardDrop]);
        let settled = snapshot(&app);
        assert!(settled.hold.is_some());
        assert!(!settled.hold_used);
        let (hold, colors) = hold_state(&mut app);
        assert!(!hold.used);
        assert_eq!(colors.len(), 4);
        for color in &colors {
            assert_eq!(color.to_srgba().alpha, 1.0, "undimmed preview");
        }
    }

    #[test]
    fn resize_reanchors_panels() {
        let mut app = hud_app(1);
        frame(&mut app, &[Action::Hold]);
        let before: Vec<Vec2> = {
            let mut query = app.world_mut().query::<&Transform>();
            let mut positions: Vec<Vec2> = query
                .iter(app.world())
                .map(|t| t.translation.xy())
                .collect();
            positions.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap().then(a.y.total_cmp(&b.y)));
            positions
        };
        {
            let mut windows = app.world_mut().query::<&mut Window>();
            windows
                .single_mut(app.world_mut())
                .unwrap()
                .resolution
                .set_physical_resolution(1600, 900);
        }
        let _ = app.world_mut().try_run_schedule(Update);
        let after: Vec<Vec2> = {
            let mut query = app.world_mut().query::<&Transform>();
            let mut positions: Vec<Vec2> = query
                .iter(app.world())
                .map(|t| t.translation.xy())
                .collect();
            positions.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap().then(a.y.total_cmp(&b.y)));
            positions
        };
        assert_ne!(before, after, "HUD must reflow on resize");

        // Anchors follow hud_anchor/letterbox for the new size exactly.
        let anchor = hud_anchor(1600.0, 900.0);
        assert!((anchor.cell - render::letterbox(1600.0, 900.0).0).abs() < EPS);
        let (hold, _) = hold_state(&mut app);
        assert!(hold.used);
        let hold_pos = {
            let mut query = app.world_mut().query::<(&HoldPreview, &Transform)>();
            let (_, transform) = query.single(app.world()).unwrap();
            transform.translation.xy()
        };
        assert!((hold_pos - hold_center(&anchor)).length() < EPS);
        let next_pos = {
            let mut query = app.world_mut().query::<(&NextPreview, &Transform)>();
            query
                .iter(app.world())
                .find(|(preview, _)| preview.index == 4)
                .map(|(_, transform)| transform.translation.xy())
                .expect("fifth preview exists")
        };
        assert!((next_pos - next_center(&anchor, 4)).length() < EPS);
    }

    #[test]
    fn combo_and_b2b_indicators_follow_the_snapshot() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Combo),
            None,
            "combo 0 hidden"
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::B2B),
            None,
            "b2b false hidden"
        );

        app.world_mut().resource_mut::<HudFixture>().0 = Some(GameSnapshot {
            combo: 3,
            b2b: true,
            ..snapshot(&app)
        });
        let _ = app.world_mut().try_run_schedule(Update);
        let combo = text_of(&mut app, HudTextSlot::Combo).expect("combo>0 shows indicator");
        assert!(combo.contains("COMBO") && combo.contains('3'), "{combo}");
        let b2b = text_of(&mut app, HudTextSlot::B2B).expect("b2b shows indicator");
        assert!(b2b.contains("B2B"), "{b2b}");

        app.world_mut().resource_mut::<HudFixture>().0 = None;
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Combo),
            None,
            "despawned again"
        );
        assert_eq!(text_of(&mut app, HudTextSlot::B2B), None, "despawned again");
    }

    #[test]
    fn pause_hint_reflects_the_bound_pause_chord() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        let hint = text_of(&mut app, HudTextSlot::PauseHint).expect("hint present");
        assert!(
            hint.contains("PAUSE") && hint.contains("Esc") && hint.contains("P"),
            "{hint}"
        );

        let mut bindings = KeyBindings::default();
        bindings.set_slot(BindSlot::Pause, vec![Bind::Key(KeyCode::KeyF)]);
        app.insert_resource(bindings);
        let _ = app.world_mut().try_run_schedule(Update);
        let hint = text_of(&mut app, HudTextSlot::PauseHint).expect("hint present");
        assert_eq!(hint, "PAUSE  F", "rebound chord picked up");
    }

    #[test]
    fn no_entity_growth_across_frames() {
        let mut app = hud_app(1);
        frame(&mut app, &[Action::Hold]);
        for _ in 0..5 {
            let _ = app.world_mut().try_run_schedule(Update);
        }
        let entities_before = app.world().entities().len();
        let mut mini_query = app.world_mut().query::<&HudMiniCell>();
        let mini_before = mini_query.iter(app.world()).count();
        assert_eq!(mini_before, 24, "5 next previews + 4 hold cells");

        for _ in 0..20 {
            let _ = app.world_mut().try_run_schedule(Update);
        }
        let mut mini_query = app.world_mut().query::<&HudMiniCell>();
        assert_eq!(mini_query.iter(app.world()).count(), mini_before);
        assert_eq!(app.world().entities().len(), entities_before, "no leaks");
    }

    // ------------------------------------------------------------------
    // T26: versus HUD
    // ------------------------------------------------------------------

    use crate::core_bridge::{end_versus, start_versus, Controller, VersusMatch, VersusWinner};
    use tetris_core::versus::{AttackRule, Match, MatchSnapshot, Side};

    /// App whose `Startup` ran once (spawns the versus HUD roots) plus a
    /// scripted solo frame.
    fn versus_hud_app(seed: u64) -> App {
        let mut app = hud_app(seed);
        app.update();
        app
    }

    fn fixture_match(rule: AttackRule) -> MatchSnapshot {
        let mut snapshot = Match::new(0xBEEF, rule).snapshot();
        snapshot.left.score = 1234;
        snapshot.left.lines = 12;
        snapshot.left.level = 3;
        snapshot.right.score = 5678;
        snapshot.right.lines = 24;
        snapshot.right.level = 5;
        snapshot.pending = (0, 3);
        snapshot
    }

    fn versus_text_of(app: &mut App, side: Side, slot: VersusHudSlot) -> String {
        let mut query = app.world_mut().query::<(&VersusHudText, &Text2d)>();
        query
            .iter(app.world())
            .find(|(meta, _)| meta.side == side && meta.slot == slot)
            .map(|(_, text)| text.0.clone())
            .expect("versus stat text exists")
    }

    fn versus_previews(app: &mut App, side: Side) -> Vec<(VersusPreviewRole, Visibility)> {
        let mut query = app.world_mut().query::<(&VersusPreview, &Visibility)>();
        query
            .iter(app.world())
            .filter(|(preview, _)| preview.side == side)
            .map(|(preview, visibility)| (preview.role, *visibility))
            .collect()
    }

    fn vis_of_solo_score(app: &mut App) -> Visibility {
        let mut query = app.world_mut().query::<(&HudText, &Visibility)>();
        *query
            .iter(app.world())
            .find(|(text, _)| text.slot == HudTextSlot::Score)
            .map(|(_, visibility)| visibility)
            .expect("solo score text exists")
    }

    #[test]
    fn versus_panel_anchors_track_shared_versus_layout() {
        for (w, h) in [(1280.0f32, 720.0f32), (1920.0, 1080.0), (900.0, 600.0)] {
            let [left, right] = versus_panel_anchors(w, h);
            let [field_left, field_right] = render::versus_layouts(w, h);
            assert_eq!(left.cell, field_left.cell, "one shared versus cell size");
            assert_eq!(left.field_top, field_left.origin.y);
            let field_w = left.cell * COLS as f32;
            // Panels sit in the outer margins of their halves, inside the
            // window at these (representative) sizes.
            assert!(left.panel_x < field_left.origin.x && left.panel_x > -w * 0.5);
            assert!(right.panel_x > field_right.origin.x + field_w && right.panel_x < w * 0.5);
        }
    }

    #[test]
    fn versus_hud_mirrors_match_snapshot_and_pending_garbage() {
        let mut app = versus_hud_app(1);
        let snapshot = fixture_match(AttackRule::Garbage);
        app.world_mut().resource_mut::<VersusHudFixture>().0 = Some(snapshot.clone());
        let _ = app.world_mut().try_run_schedule(Update);

        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Score),
            format!("SCORE\n{}", snapshot.left.score)
        );
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Lines),
            format!("LINES\n{}", snapshot.left.lines)
        );
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Level),
            format!("LEVEL\n{}", snapshot.left.level)
        );
        assert_eq!(
            versus_text_of(&mut app, Side::Right, VersusHudSlot::Score),
            format!("SCORE\n{}", snapshot.right.score)
        );
        // `+N` pending-garbage indicator on the side with queued garbage,
        // silent on the other.
        assert_eq!(
            versus_text_of(&mut app, Side::Right, VersusHudSlot::Pending),
            "+3"
        );
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Pending),
            ""
        );

        // Two next previews per side mirror `next[..2]`; the hold box is
        // hidden while empty.
        let left_previews = versus_previews(&mut app, Side::Left);
        let mut next_visible = 0;
        for (role, visibility) in &left_previews {
            match role {
                VersusPreviewRole::Next(_) => {
                    assert_eq!(*visibility, Visibility::Inherited, "next slots filled");
                    next_visible += 1;
                }
                VersusPreviewRole::Hold => {
                    assert_eq!(*visibility, Visibility::Hidden, "empty hold hidden");
                }
            }
        }
        assert_eq!(next_visible, 2, "exactly two next previews per side");
    }

    /// T20: under the Dig rule the pending slot flips to the buried-board
    /// `DUG n/10` counter — the `+N` queue read is suppressed (Dig never
    /// sends), and the counter follows each side's own board.
    #[test]
    fn versus_hud_shows_dug_rows_under_dig_rule() {
        let mut app = versus_hud_app(1);
        let mut snapshot = fixture_match(AttackRule::Dig);
        snapshot.pending = (0, 3); // never shown under Dig
        app.world_mut().resource_mut::<VersusHudFixture>().0 = Some(snapshot.clone());
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Pending),
            "DUG 0/10"
        );
        assert_eq!(
            versus_text_of(&mut app, Side::Right, VersusHudSlot::Pending),
            "DUG 0/10"
        );

        // Clear the left board's bottom buried row: only that side's
        // counter moves.
        let mut dug = snapshot;
        for c in 0..COLS {
            dug.left.board.set(ROWS - 1, c, None);
        }
        app.world_mut().resource_mut::<VersusHudFixture>().0 = Some(dug);
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Pending),
            "DUG 1/10"
        );
        assert_eq!(
            versus_text_of(&mut app, Side::Right, VersusHudSlot::Pending),
            "DUG 0/10"
        );
    }

    #[test]
    fn versus_hud_renders_live_match_cells() {
        let mut app = versus_hud_app(2);
        app.world_mut()
            .resource_scope::<crate::state::AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Race {
                            target_lines: 1_000_000,
                        },
                        Controller::Human,
                        Controller::Bot,
                    );
                });
            });
        let snapshot = app.world().non_send::<VersusMatch>().match_.snapshot();
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Score),
            format!("SCORE\n{}", snapshot.left.score)
        );
        // No fixture needed: the live match drives the panels; race rule
        // never queues garbage.
        assert_eq!(
            versus_text_of(&mut app, Side::Right, VersusHudSlot::Pending),
            ""
        );
    }

    #[test]
    fn solo_panels_hide_for_versus_and_restore_after() {
        let mut app = versus_hud_app(3);
        frame(&mut app, &[Action::Hold]);
        assert_ne!(
            vis_of_solo_score(&mut app),
            Visibility::Hidden,
            "solo HUD visible pre-versus"
        );

        app.world_mut()
            .resource_scope::<crate::state::AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Garbage,
                        Controller::Human,
                        Controller::Human,
                    );
                });
            });
        let _ = app.world_mut().try_run_schedule(Update);
        let mut hud_roots = app.world_mut().query::<(&Visibility, &HudText)>();
        for (visibility, _) in hud_roots.iter(app.world()) {
            assert_eq!(*visibility, Visibility::Hidden, "solo HUD hidden in versus");
        }
        let mut previews = app.world_mut().query::<(&Visibility, &NextPreview)>();
        assert!(previews.iter(app.world()).count() > 0);
        for (visibility, _) in previews.iter(app.world()) {
            assert_eq!(
                *visibility,
                Visibility::Hidden,
                "next queue hidden in versus"
            );
        }

        app.world_mut()
            .resource_scope::<crate::state::AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    end_versus(versus.into_inner(), winner.into_inner(), state.into_inner());
                });
            });
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            vis_of_solo_score(&mut app),
            Visibility::Inherited,
            "restored"
        );
        let mut previews = app.world_mut().query::<(&Visibility, &NextPreview)>();
        for (visibility, _) in previews.iter(app.world()) {
            assert_eq!(*visibility, Visibility::Inherited, "restored");
        }
    }

    #[test]
    fn versus_hud_tree_is_stable_across_frames() {
        let mut app = versus_hud_app(4);
        let snapshot = fixture_match(AttackRule::Garbage);
        app.world_mut().resource_mut::<VersusHudFixture>().0 = Some(snapshot);
        for _ in 0..5 {
            let _ = app.world_mut().try_run_schedule(Update);
        }
        let before = app.world().entities().len();
        for _ in 0..20 {
            let _ = app.world_mut().try_run_schedule(Update);
        }
        assert_eq!(app.world().entities().len(), before, "no versus HUD leaks");
    }

    /// T16: a ladder-origin match re-targets the Status slot to the
    /// `RUNG n/8` badge on both sides; without the flow marker (or without
    /// the resource at all, as every pre-T16 tree) the slot stays silent /
    /// race-driven exactly as before.
    #[test]
    fn versus_hud_status_shows_ladder_rung_badge() {
        let mut app = versus_hud_app(5);
        app.insert_resource(VersusFlow {
            ladder: LadderOrigin::Match { rung: 3 },
            ..Default::default()
        });
        app.world_mut()
            .resource_scope::<crate::state::AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Garbage,
                        Controller::Human,
                        Controller::Bot,
                    );
                });
            });
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Status),
            "RUNG 3/8"
        );
        assert_eq!(
            versus_text_of(&mut app, Side::Right, VersusHudSlot::Status),
            "RUNG 3/8"
        );

        // Leaving the ladder flow (match still live) restores the plain
        // Status slot (empty while a Garbage match runs).
        app.world_mut().resource_mut::<VersusFlow>().ladder = LadderOrigin::Closed;
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Status),
            ""
        );
    }

    /// T16: `FINISHED` race badges keep working when no ladder flow is
    /// marked (the badge override is strictly ladder-gated).
    #[test]
    fn versus_hud_finished_badge_survives_the_ladder_badge() {
        let mut app = versus_hud_app(6);
        let mut snapshot = fixture_match(AttackRule::Race { target_lines: 4 });
        snapshot.finished = (true, false);
        app.world_mut().resource_mut::<VersusHudFixture>().0 = Some(snapshot);
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Status),
            "FINISHED"
        );
        assert_eq!(
            versus_text_of(&mut app, Side::Right, VersusHudSlot::Status),
            ""
        );
    }

    // ------------------------------------------------------------------
    // T21: Switch swap countdown (fixture-driven like the T8/T13 meters)
    // ------------------------------------------------------------------

    fn versus_color_of(app: &mut App, side: Side, slot: VersusHudSlot) -> Color {
        let mut query = app.world_mut().query::<(&VersusHudText, &TextColor)>();
        query
            .iter(app.world())
            .find(|(meta, _)| meta.side == side && meta.slot == slot)
            .map(|(_, color)| color.0)
            .expect("versus stat text exists")
    }

    fn switch_fixture(app: &mut App, match_ticks: u64, swaps_done: u32) {
        let rule = AttackRule::Switch {
            swap_interval_ticks: 1_800,
            warning_ticks: 180,
        };
        let mut snapshot = Match::new(0xBEEF, rule).snapshot();
        snapshot.match_ticks = match_ticks;
        snapshot.swaps_done = swaps_done;
        app.world_mut().resource_mut::<VersusHudFixture>().0 = Some(snapshot);
        let _ = app.world_mut().try_run_schedule(Update);
    }

    /// The Status slot counts the next swap down in whole seconds (rounded
    /// up) on both panels, turns garbage-orange inside the 3 s warning
    /// window, and re-arms after a swap — all from the snapshot alone.
    #[test]
    fn versus_hud_swaps_status_to_swap_countdown_under_switch() {
        let mut app = versus_hud_app(7);
        for (ticks, expected, warning) in [
            (0u64, "SWAP 30", false),
            (900, "SWAP 15", false),
            // 1 619 ticks in: 181 ticks left (rounds up to 4 s), one tick
            // outside the 180-tick warning window.
            (1_619, "SWAP 4", false),
            // 1 620: exactly warning_ticks before the boundary ⇒ warning.
            (1_620, "SWAP 3", true),
            // One tick before the swap the countdown reads 1 s, warning.
            (1_799, "SWAP 1", true),
            // After the swap (swaps_done advanced) it re-arms at 30.
            (1_800, "SWAP 30", false),
        ] {
            switch_fixture(&mut app, ticks, if ticks >= 1_800 { 1 } else { 0 });
            for side in [Side::Left, Side::Right] {
                assert_eq!(
                    versus_text_of(&mut app, side, VersusHudSlot::Status),
                    expected,
                    "match_ticks {ticks}"
                );
                assert_eq!(
                    versus_color_of(&mut app, side, VersusHudSlot::Status),
                    if warning { GARBAGE_COLOR } else { Color::WHITE },
                    "warning state at match_ticks {ticks}"
                );
            }
        }
    }

    /// Other rules keep the plain Status slot (empty under Garbage/Dig,
    /// gold-badge colored), and the color from a previous Switch fixture is
    /// never left behind on a rule switch.
    #[test]
    fn versus_hud_swap_countdown_is_hidden_for_other_rules() {
        let mut app = versus_hud_app(8);
        switch_fixture(&mut app, 1_700, 0);
        assert_eq!(
            versus_text_of(&mut app, Side::Left, VersusHudSlot::Status),
            "SWAP 2"
        );
        for rule in [
            AttackRule::Garbage,
            AttackRule::Race { target_lines: 40 },
            AttackRule::Dig,
            AttackRule::Switch {
                swap_interval_ticks: 0,
                warning_ticks: 0,
            },
        ] {
            let snapshot = fixture_match(rule);
            app.world_mut().resource_mut::<VersusHudFixture>().0 = Some(snapshot);
            let _ = app.world_mut().try_run_schedule(Update);
            for side in [Side::Left, Side::Right] {
                assert_eq!(
                    versus_text_of(&mut app, side, VersusHudSlot::Status),
                    "",
                    "{:?} rule must show no swap countdown",
                    app.world()
                        .resource::<VersusHudFixture>()
                        .0
                        .as_ref()
                        .unwrap()
                        .rule
                );
                assert_eq!(
                    versus_color_of(&mut app, side, VersusHudSlot::Status),
                    FINISHED_COLOR,
                    "status color restored"
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // T8: mode HUD — clock, goal counter, pre-roll 3-2-1
    // ------------------------------------------------------------------

    use crate::core_bridge::{start_mode_run, Countdown, ModeHudInfo};
    use crate::modes::{self, ModeId};

    /// Fixture injection (extended `HudFixture` pattern, T8): write the
    /// bridge-owned [`ModeHudInfo`] directly and re-render — no fixed step,
    /// so the live-bridge refresh never clobbers the injected values.
    fn inject_mode(app: &mut App, info: ModeHudInfo) {
        *app.world_mut().resource_mut::<ModeHudInfo>() = info;
        let _ = app.world_mut().try_run_schedule(Update);
    }

    fn sprint_fixture(app: &mut App, clock_ticks: u64) {
        inject_mode(
            app,
            ModeHudInfo {
                mode_id: ModeId::Sprint,
                clock_ticks,
                show_hud: true,
                lines_left: Some(37),
                pieces_placed: 5,
                ..ModeHudInfo::default()
            },
        );
    }

    fn ultra_fixture(app: &mut App, clock_ticks: u64) -> ModeHudInfo {
        let info = ModeHudInfo {
            mode_id: ModeId::Ultra,
            clock_ticks,
            clock_limit: Some(modes::ULTRA_CLOCK_TICKS),
            show_hud: true,
            ..ModeHudInfo::default()
        };
        inject_mode(app, info);
        info
    }

    #[test]
    fn marathon_shows_no_clock_or_goal_rows() {
        let mut app = hud_app(1);
        frame(&mut app, &[Action::HardDrop]);
        assert_eq!(text_of(&mut app, HudTextSlot::Clock), None);
        assert_eq!(text_of(&mut app, HudTextSlot::Goal), None);
        assert_eq!(text_of(&mut app, HudTextSlot::Countdown), None);

        // Even an explicitly hidden fixture keeps the slots absent.
        inject_mode(&mut app, ModeHudInfo::default());
        assert_eq!(text_of(&mut app, HudTextSlot::Clock), None);
        assert_eq!(text_of(&mut app, HudTextSlot::Goal), None);
    }

    #[test]
    fn sprint_clock_delegates_to_format_time_ticks() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        // Pin the plan's examples through the label wiring (the truncation
        // math itself is covered by `modes::tests`).
        for (ticks, want) in [(0_u64, "0:00.00"), (9_835, "2:43.91"), (10_235, "2:50.58")] {
            sprint_fixture(&mut app, ticks);
            assert_eq!(
                text_of(&mut app, HudTextSlot::Clock),
                Some(format!("TIME\n{want}")),
                "elapsed clock at {ticks} ticks"
            );
        }
    }

    #[test]
    fn sprint_fixture_shows_lines_left_and_pieces() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        sprint_fixture(&mut app, 6_600);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Clock),
            Some("TIME\n1:50.00".to_string())
        );
        let goal = text_of(&mut app, HudTextSlot::Goal).expect("sprint goal row");
        assert!(goal.contains("Lines left: 37"), "{goal}");
        assert!(goal.contains("Pieces: 5"), "{goal}");
    }

    #[test]
    fn ultra_fixture_counts_down_to_the_tenth() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        ultra_fixture(&mut app, 6_600);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Clock),
            Some("TIME\n0:10.00".to_string()),
            "6 600 elapsed of 7 200 → 10 s remaining"
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Goal),
            None,
            "Ultra shows no goal row (score already shown)"
        );
        ultra_fixture(&mut app, 7_199);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Clock),
            Some("TIME\n0:00.01".to_string()),
            "7 199 elapsed → 1 tick remaining, truncated"
        );
    }

    #[test]
    fn dig_fixture_shows_garbage_rows_left() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        inject_mode(
            &mut app,
            ModeHudInfo {
                mode_id: ModeId::Dig,
                clock_ticks: 1_500,
                show_hud: true,
                garbage_left: Some(3),
                ..ModeHudInfo::default()
            },
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Goal),
            Some("Garbage: 3".into())
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Clock),
            Some(format!("TIME\n{}", modes::format_time_ticks(1_500)))
        );
    }

    /// Survival fixture (T13): count-up clock (like Sprint) plus the
    /// queued-garbage meter — the versus `+N` idiom — with the next-row
    /// seconds, fed from `ModeHudInfo.feed_pending` / `feed_next_row_in`.
    #[test]
    fn survival_fixture_shows_feed_meter() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        inject_mode(
            &mut app,
            ModeHudInfo {
                mode_id: ModeId::Survival,
                clock_ticks: 660,
                show_hud: true,
                feed_pending: Some(2),
                feed_next_row_in: Some(120),
                ..ModeHudInfo::default()
            },
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Clock),
            Some(format!("TIME\n{}", modes::format_time_ticks(660))),
            "Survival counts up like Sprint (no clock budget)"
        );
        let feed = text_of(&mut app, HudTextSlot::Feed).expect("survival feed meter");
        assert!(feed.contains("+2"), "queued count: {feed}");
        assert!(feed.contains("2.0s"), "next-row countdown: {feed}");
        assert_eq!(
            text_of(&mut app, HudTextSlot::Goal),
            None,
            "Survival has no goal row (no goal)"
        );

        inject_mode(
            &mut app,
            ModeHudInfo {
                mode_id: ModeId::Survival,
                clock_ticks: 300,
                show_hud: true,
                feed_pending: Some(0),
                feed_next_row_in: Some(300),
                ..ModeHudInfo::default()
            },
        );
        let idle = text_of(&mut app, HudTextSlot::Feed).expect("feed meter stays up");
        assert!(idle.contains("+0"), "quiet queue still reads +0: {idle}");
        assert!(idle.contains("5.0s"), "300 ticks = 5.0 s: {idle}");
    }

    /// The feed slot follows the gate: marathon (no feed, hidden HUD) never
    /// renders it, and the pre-roll hides it just like clock/goal.
    #[test]
    fn feed_slot_follows_the_feed_gate() {
        let mut app = hud_app(1);
        frame(&mut app, &[Action::HardDrop]);
        assert_eq!(text_of(&mut app, HudTextSlot::Feed), None);
        assert_eq!(text_of(&mut app, HudTextSlot::Clock), None);

        inject_mode(
            &mut app,
            ModeHudInfo {
                mode_id: ModeId::Survival,
                show_hud: true,
                countdown: 90,
                feed_pending: Some(3),
                feed_next_row_in: Some(60),
                ..ModeHudInfo::default()
            },
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Countdown).as_deref(),
            Some("2")
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Feed),
            None,
            "pre-roll hides the feed meter"
        );
    }

    #[test]
    fn pre_roll_shows_big_countdown_and_hides_clock() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        inject_mode(
            &mut app,
            ModeHudInfo {
                mode_id: ModeId::Sprint,
                show_hud: true,
                lines_left: Some(modes::SPRINT_GOAL_LINES),
                countdown: 180,
                ..ModeHudInfo::default()
            },
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Countdown).as_deref(),
            Some("3"),
            "180 pre-roll steps → 3"
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Clock),
            None,
            "clock hidden during pre-roll"
        );
        assert_eq!(text_of(&mut app, HudTextSlot::Goal), None);
        for (count, want) in [(120_u32, "2"), (121, "3"), (61, "2"), (60, "1"), (1, "1")] {
            inject_mode(
                &mut app,
                ModeHudInfo {
                    mode_id: ModeId::Sprint,
                    show_hud: true,
                    countdown: count,
                    ..ModeHudInfo::default()
                },
            );
            assert_eq!(
                text_of(&mut app, HudTextSlot::Countdown).as_deref(),
                Some(want),
                "ceil({count}/60)"
            );
        }
        inject_mode(&mut app, ModeHudInfo::default());
        assert_eq!(
            text_of(&mut app, HudTextSlot::Countdown),
            None,
            "0 → normal HUD"
        );
    }

    #[test]
    fn live_sprint_refresh_tracks_core_and_pre_roll() {
        let mut app = hud_app(0xA17);
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, mut state| {
                let mut countdown = world.remove_resource::<Countdown>().unwrap();
                {
                    let mut core = world.non_send_mut::<GameCore>();
                    start_mode_run(
                        ModeId::Sprint,
                        core.as_mut(),
                        &mut countdown,
                        state.as_mut(),
                        None,
                    );
                }
                world.insert_resource(countdown);
            });
        // The first fixed step burns pre-roll 180 → 179 (core frozen).
        frame(&mut app, &[]);
        {
            let hud = app.world().resource::<ModeHudInfo>();
            assert_eq!(hud.mode_id, ModeId::Sprint);
            assert_eq!(hud.countdown, 179);
            assert_eq!(hud.lines_left, Some(modes::SPRINT_GOAL_LINES));
            assert!(hud.show_hud);
            assert_eq!(hud.clock_ticks, 0, "core frozen during the pre-roll");
            assert_eq!(hud.feed_pending, None);
            assert_eq!(hud.feed_next_row_in, None);
            assert_eq!(hud.swap_in, None);
        }
        assert_eq!(
            text_of(&mut app, HudTextSlot::Countdown).as_deref(),
            Some("3")
        );

        // 179 more steps drain the pre-roll: the 180th step is the first
        // playable frame (core tick 0 → steps 1 == clock_ticks 1).
        for _ in 0..179 {
            frame(&mut app, &[]);
        }
        {
            let hud = app.world().resource::<ModeHudInfo>();
            let core = app.world().non_send::<GameCore>();
            assert_eq!(hud.countdown, 0);
            assert_eq!(
                hud.clock_ticks, core.steps,
                "clock_ticks == core steps after pre-roll"
            );
            assert_eq!(core.steps, 1);
        }
        assert_eq!(text_of(&mut app, HudTextSlot::Countdown), None);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Clock),
            Some("TIME\n0:00.01".to_string()),
            "core tick 1 renders as 0:00.01 (truncated)"
        );

        frame(&mut app, &[Action::HardDrop]);
        {
            let hud = app.world().resource::<ModeHudInfo>();
            assert_eq!(hud.pieces_placed, 1, "PieceLocked events counted");
            assert_eq!(hud.clock_ticks, app.world().non_send::<GameCore>().steps);
        }
    }

    #[test]
    fn mode_slots_avoid_the_countdown_center() {
        render::set_portrait_override(Some(true));
        let anchor = hud_anchor(1080.0, 2404.0);
        let clock = hud_text_center(&anchor, HudTextSlot::Clock);
        let goal = hud_text_center(&anchor, HudTextSlot::Goal);
        let big = hud_text_center(&anchor, HudTextSlot::Countdown);
        assert!(anchor.portrait);
        assert!(clock.x < goal.x);
        assert!((clock.y - goal.y).abs() < EPS, "same deck row");
        assert!(
            (clock.y - hud_text_center(&anchor, HudTextSlot::Combo).y).abs() < EPS,
            "clock/goal share the combo/deck row"
        );
        assert!(
            big.y < anchor.field_top && big.y > anchor.field_bottom,
            "3-2-1 centered over the field, clear of the deck row"
        );
        render::set_portrait_override(None);

        let anchor = hud_anchor(1280.0, 720.0);
        let clock = hud_text_center(&anchor, HudTextSlot::Clock);
        let goal = hud_text_center(&anchor, HudTextSlot::Goal);
        let big = hud_text_center(&anchor, HudTextSlot::Countdown);
        assert_eq!(clock.x, anchor.right_panel_x, "right panel below the queue");
        assert_eq!(goal.x, anchor.right_panel_x);
        // Below the tallest possible next queue (6 slots) …
        assert!(clock.y < next_center(&anchor, 5).y);
        // … and above the window bottom.
        assert!(
            goal.y > anchor.field_bottom,
            "{goal:?} vs {:?}",
            anchor.field_bottom
        );
        assert!((big.x).abs() < EPS, "landscape 3-2-1 centered");
        assert!(big.y < anchor.field_top && big.y > anchor.field_bottom);
    }

    /// T13: the feed meter stacks below the goal row (landscape) / under the
    /// deck row (portrait), above the window bottom and clear of the
    /// field-centered 3-2-1.
    #[test]
    fn feed_slot_layout_clears_the_field_bounds() {
        render::set_portrait_override(Some(true));
        let anchor = hud_anchor(1080.0, 2404.0);
        let feed = hud_text_center(&anchor, HudTextSlot::Feed);
        let deck = hud_text_center(&anchor, HudTextSlot::Combo);
        let big = hud_text_center(&anchor, HudTextSlot::Countdown);
        assert!(anchor.portrait);
        assert!(feed.y < deck.y, "feed line below the deck row");
        assert!(feed.y < big.y, "clear of the field-centered 3-2-1");
        render::set_portrait_override(None);

        let anchor = hud_anchor(1280.0, 720.0);
        let feed = hud_text_center(&anchor, HudTextSlot::Feed);
        let goal = hud_text_center(&anchor, HudTextSlot::Goal);
        assert_eq!(feed.x, anchor.right_panel_x, "right panel column");
        assert!(feed.y < goal.y, "feed stacks below the goal row");
        assert!(
            feed.y > anchor.field_bottom,
            "{feed:?} above the window bottom ({})",
            anchor.field_bottom
        );
    }

    /// Deterministic Zen line-clear driver: alternate hard drops into the
    /// left and right walls. Seed 7 clears 4 lines within 240 drops without
    /// ever ending the run (Zen wipes keep it alive).
    fn drive_zen_clears(app: &mut App, seed: u64, frames: u32) {
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.start_mode(seed, modes::ModeId::Zen);
        }
        for i in 0..frames {
            let actions: &[Action] = if i % 2 == 0 {
                &[
                    Action::MoveLeft,
                    Action::MoveLeft,
                    Action::MoveLeft,
                    Action::MoveLeft,
                    Action::HardDrop,
                ]
            } else {
                &[
                    Action::MoveRight,
                    Action::MoveRight,
                    Action::MoveRight,
                    Action::MoveRight,
                    Action::HardDrop,
                ]
            };
            frame(app, actions);
        }
    }

    fn lifetime_total(app: &App) -> Option<u64> {
        app.world()
            .resource::<Records>()
            .record_for(crate::records::ZEN)
            .and_then(|record| match record {
                Record::LifetimeLines { total } => Some(*total),
                _ => None,
            })
    }

    #[test]
    fn zen_line_clears_accumulate_lifetime_and_hud_shows_both() {
        let mut app = hud_app(0xC0FFEE);
        app.init_resource::<Records>();
        drive_zen_clears(&mut app, 7, 240);
        let snapshot = snapshot(&app);

        // Zen never ends: the run that cleared lines is still live.
        assert_eq!(snapshot.lines, 4, "deterministic scripted clears");
        assert!(!snapshot.game_over, "Zen must never game over");
        assert_eq!(
            *app.world().resource::<AppState>(),
            AppState::Playing,
            "the bridge never flips a wiping Zen run"
        );

        // Lifetime folded exactly once per cleared line.
        assert_eq!(lifetime_total(&app), Some(4));

        // HUD: session lines (left panel) + lifetime total (right panel).
        assert_eq!(
            text_of(&mut app, HudTextSlot::Lines),
            Some(format!("LINES\n{}", snapshot.lines))
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Lifetime),
            Some("LIFETIME\n4".to_string())
        );
    }

    #[test]
    fn marathon_clears_never_touch_the_zen_lifetime() {
        let mut app = hud_app(0xC0FFEE);
        app.init_resource::<Records>();
        // Same seed/driver but the default (Marathon) config: its line
        // clears must not fold into the Zen counter.
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.restart_with(7);
        }
        for i in 0..240u32 {
            let actions: &[Action] = if i % 2 == 0 {
                &[
                    Action::MoveLeft,
                    Action::MoveLeft,
                    Action::MoveLeft,
                    Action::MoveLeft,
                    Action::HardDrop,
                ]
            } else {
                &[
                    Action::MoveRight,
                    Action::MoveRight,
                    Action::MoveRight,
                    Action::MoveRight,
                    Action::HardDrop,
                ]
            };
            frame(&mut app, actions);
        }
        // Marathon clears a line with this seed before its top-out.
        assert_eq!(snapshot(&app).lines, 1, "driver must clear lines");
        assert_eq!(
            lifetime_total(&app),
            None,
            "no Zen counter without Zen play"
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Lifetime),
            None,
            "the lifetime slot is Zen-only"
        );
    }

    #[test]
    fn zen_hud_starts_at_zero_lifetime_before_any_record() {
        let mut app = hud_app(1);
        app.init_resource::<Records>();
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.start_mode(1, modes::ModeId::Zen);
        }
        frame(&mut app, &[]);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Lifetime),
            Some("LIFETIME\n0".to_string())
        );
    }

    /// Full app-exit round trip: Zen clears → `AppExit` → the records
    /// exit-flush persists them → a fresh `Records::load` of the isolated
    /// config dir shows the lifetime total (PRD: lifetime lines survive
    /// sessions; the debounced writer keeps per-line cost off the disk).
    #[test]
    fn zen_lifetime_survives_app_exit_flush() {
        let _env = crate::settings_persist::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "tetris-t14-exit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::env::set_var(crate::settings_persist::CONFIG_DIR_ENV, &dir);

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris T14 zen exit".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((CoreBridgePlugin, crate::records::RecordsPlugin, HudPlugin));
        app.insert_non_send(GameCore::new(7));
        app.init_resource::<AppState>();
        app.init_resource::<Settings>();
        app.update(); // Startup: records load (empty dir, never writes)

        drive_zen_clears(&mut app, 7, 240);
        let lines = snapshot(&app).lines;
        assert_eq!(lines, 4, "deterministic scripted clears");

        // App exit: the Last-schedule exit flush persists the dirty Records.
        app.world_mut().write_message(AppExit::Success);
        app.update();

        let loaded = crate::records::load_from(&dir);
        assert_eq!(
            loaded.record_for(crate::records::ZEN),
            Some(&Record::LifetimeLines { total: 4 }),
            "lifetime lines survive the app-exit flush"
        );

        std::env::remove_var(crate::settings_persist::CONFIG_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod t23_tests {
    use super::*;
    use crate::core_bridge::{CoreBridgePlugin, GameCore};
    use crate::modes::ModeId;
    use crate::mutators::Mutators;
    use crate::state::AppState;

    use bevy::window::WindowPlugin;

    fn hud_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris t23 hud".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((CoreBridgePlugin, HudPlugin));
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app.init_resource::<Settings>();
        app
    }

    fn next_previews(app: &mut App) -> Vec<(usize, Piece)> {
        let mut query = app.world_mut().query::<&NextPreview>();
        let mut previews: Vec<(usize, Piece)> = query
            .iter(app.world())
            .map(|preview| (preview.index, preview.piece))
            .collect();
        previews.sort_by_key(|(index, _)| *index);
        previews
    }

    fn snapshot(app: &App) -> GameSnapshot {
        app.world().non_send::<GameCore>().game.snapshot()
    }

    /// **One Preview** (T23): an active mutator pins the HUD next queue to
    /// exactly one preview; a clean run keeps the configured queue size.
    #[test]
    fn one_preview_mutator_forces_single_next_preview() {
        let mut app = hud_app(0x052);
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            next_previews(&mut app).len(),
            5,
            "baseline clean run shows the configured queue"
        );

        let seed = app.world().non_send::<GameCore>().seed;
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.selected_mutators = Mutators::ONE_PREVIEW;
            core.start_mode(seed, ModeId::Marathon);
        }
        let _ = app.world_mut().try_run_schedule(Update);
        let previews = next_previews(&mut app);
        assert_eq!(previews.len(), 1, "One Preview: exactly one slot");
        assert_eq!(previews[0].0, 0, "the surviving slot is index 0");
        assert_eq!(
            previews[0].1,
            snapshot(&app).next[0],
            "and it shows the first queued piece"
        );

        // A *settings* change must not undo the mutator clamp (the run's
        // mutators beat the global setting).
        app.world_mut().resource_mut::<Settings>().next_queue_size = 6;
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            next_previews(&mut app).len(),
            1,
            "mutator wins over Settings"
        );

        // Clean run back to the configured size (regression for other modes).
        let seed = app.world().non_send::<GameCore>().seed;
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.selected_mutators = Mutators::empty();
            core.start_mode(seed, ModeId::Marathon);
        }
        app.world_mut().resource_mut::<Settings>().next_queue_size = 5;
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            next_previews(&mut app).len(),
            5,
            "clean runs keep the existing HUD behavior"
        );
    }
}
