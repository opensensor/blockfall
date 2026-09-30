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

use tetris_core::board::COLS;
use tetris_core::game::GameSnapshot;
use tetris_core::piece::{Piece, Rotation};
use tetris_core::versus::Side;

use crate::core_bridge::{GameCore, VersusMatch};
use crate::input::{Bind, BindSlot, KeyBindings};
use crate::render::{self, GHOST_ALPHA, VISIBLE_ROWS};
use crate::state::Settings;

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
        };
    }
    let y = match slot {
        HudTextSlot::Score => anchor.field_top - 4.0 * c,
        HudTextSlot::Level => anchor.field_top - 7.0 * c,
        HudTextSlot::Lines => anchor.field_top - 10.0 * c,
        HudTextSlot::Combo => anchor.field_top - 13.0 * c,
        HudTextSlot::B2B => anchor.field_top - 15.0 * c,
        HudTextSlot::PauseHint => anchor.field_top - 18.0 * c,
    };
    Vec2::new(anchor.left_panel_x, y)
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

/// Pooled text entities (combo/b2b come and go with snapshot flags).
#[derive(Resource, Default)]
pub struct HudTextEntities {
    score: Option<Entity>,
    level: Option<Entity>,
    lines: Option<Entity>,
    combo: Option<Entity>,
    b2b: Option<Entity>,
    pause_hint: Option<Entity>,
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
        HudTextSlot::Combo | HudTextSlot::B2B | HudTextSlot::PauseHint => return None,
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
            TextColor::WHITE,
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

/// Score/level/lines always present; combo/b2b only while active; pause
/// hint reflects the live pause chord.
#[allow(clippy::too_many_arguments)]
fn sync_hud_texts(
    mut commands: Commands,
    core: Option<NonSend<GameCore>>,
    fixture: Option<Res<HudFixture>>,
    bindings: Option<Res<KeyBindings>>,
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

fn hold_cell_color(piece: Piece, used: bool) -> Color {
    let color = render::piece_color(piece);
    if used {
        color.with_alpha(GHOST_ALPHA)
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
    Pending,
    /// `FINISHED` badge for a side that completed a Race target (empty
    /// text while it still races or tops out).
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

/// `FINISHED` badge text for a side that completed a Race target (empty
/// while it still races).
fn versus_status_text(finished: bool) -> String {
    if finished {
        "FINISHED".to_string()
    } else {
        String::new()
    }
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
#[allow(clippy::too_many_arguments)]
fn sync_versus_hud(
    versus: Option<NonSend<VersusMatch>>,
    fixture: Option<Res<VersusHudFixture>>,
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

    for (meta, mut text, mut font, mut transform) in texts.iter_mut() {
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
            VersusHudSlot::Pending => versus_pending_text(pending),
            VersusHudSlot::Status => versus_status_text(finished),
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
                    (sync_hud_texts, sync_hud_previews).chain(),
                    sync_versus_hud,
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
            assert_eq!(color.to_srgba().alpha, GHOST_ALPHA, "dimmed preview");
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
}
