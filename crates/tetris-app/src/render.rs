//! Snapshot-driven playfield renderer (T11).
//!
//! Draws flat colored cells **exclusively** from [`Game::snapshot`]: settled
//! board cells, the active piece and the ghost (at the core's `ghost_row`) —
//! no rules recomputed here. The drawn field is the full 22-row board
//! (20 playable + 2 spawn-buffer, per the M3 playtest fix), letterboxed
//! inside the window with equal cell size on both axes.
//! `GameCore` is a non-send resource (T10), read via the
//! `Option<NonSend<GameCore>>` parameter.
//!
//! Rendering model: one [`Sprite`] component (the Bevy 0.19 sprite entity;
//! its built-in quad mesh is supplied by the sprite pipeline) per cell,
//! pooled in [`CellPool`] and fully refreshed every `Update` run. The 60 Hz
//! fixed-step sim runs inside `RunFixedMainLoop`, strictly ahead of
//! `Update`, so simply reading the newest snapshot each `Update` keeps
//! render decoupled from sim rate.
//!
//! While a [`VersusMatch`] is active the very same draw path runs twice into
//! two half-window viewports (T26): both boards share one cell size (the
//! min of the two half fits, see [`versus_layouts`]) and each field is
//! centered in its own half. Solo rendering is untouched by the versus
//! branch — the solo pool is even fully despawned while versus runs, and the
//! versus pools while it does not, so no cell of one mode ever survives into
//! the other.
//!
//! Public API reusable by later tasks:
//! - [`PIECE_COLORS`] / [`piece_color`] — canonical PRD-style piece palette
//!   (HUD mini-grids and the hold box, T13).
//! - [`GHOST_ALPHA`] — ghost dimming factor (T13 preview dimming parity, T19).
//! - [`letterbox`] — window size -> `(cell, offset_x, offset_y)` centering
//!   math for any playfield-anchored UI (T13, T17 overlays, T19 juice).
//! - [`FieldLayout`] / [`versus_layouts`] — viewport-parameterized layout
//!   (`fit` is exactly the [`letterbox`] math; the versus pair is the two
//!   half-window fields with a shared cell size, T26).
//! - [`frame_cells`] — snapshot -> flat list of drawable [`SnapshotCell`]s
//!   (board + active + ghost, clipped only to the full 10x22 board).
//! - [`VersusCellSide`] — marks a pooled cell sprite with the match side it
//!   belongs to (`None`-marked entities are solo).
//!
//! [`Game::snapshot`]: tetris_core::game::Game::snapshot

use bevy::color::Alpha;
use bevy::prelude::*;
use bevy::window::Window;

use tetris_core::board::{COLS, ROWS};
use tetris_core::game::GameSnapshot;
use tetris_core::piece::Piece;
use tetris_core::srs;
use tetris_core::versus::Side;

use crate::art::{self, ArtAssets};
use crate::core_bridge::{GameCore, VersusMatch};

/// Topmost drawn row: 2 rows of headroom above the board (`srs::MIN_ROT_ROW`)
/// so kick-lifted pieces stay fully visible instead of clipping (M3 playtest).
pub const DRAWN_TOP: i32 = srs::MIN_ROT_ROW;

/// Rows of the drawn field: full board (22) plus the headroom above it.
pub const VISIBLE_ROWS: usize = ROWS + DRAWN_TOP.unsigned_abs() as usize;

/// Srgba channels (red, green, blue) per piece, in [`Piece::ALL`] order,
/// following the Tetris Guideline palette (PRD: flat colored cells, one
/// distinct color per tetromino).
pub const PIECE_RGB: [(f32, f32, f32); 7] = [
    (0.0, 0.75, 0.75),  // I  cyan
    (0.25, 0.35, 0.85), // J  blue
    (0.9, 0.5, 0.1),    // L  orange
    (0.95, 0.8, 0.1),   // O  yellow
    (0.2, 0.7, 0.25),   // S  green
    (0.65, 0.3, 0.8),   // T  purple
    (0.85, 0.2, 0.2),   // Z  red
];

/// Piece palette as full-alpha [`Color`]s, indexed by [`Piece::ALL`] order
/// (I, J, L, O, S, T, Z). Shared by T13 HUD mini-grids/hold box.
pub const PIECE_COLORS: [Color; 7] = [
    Color::Srgba(Srgba {
        red: PIECE_RGB[0].0,
        green: PIECE_RGB[0].1,
        blue: PIECE_RGB[0].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[1].0,
        green: PIECE_RGB[1].1,
        blue: PIECE_RGB[1].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[2].0,
        green: PIECE_RGB[2].1,
        blue: PIECE_RGB[2].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[3].0,
        green: PIECE_RGB[3].1,
        blue: PIECE_RGB[3].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[4].0,
        green: PIECE_RGB[4].1,
        blue: PIECE_RGB[4].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[5].0,
        green: PIECE_RGB[5].1,
        blue: PIECE_RGB[5].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[6].0,
        green: PIECE_RGB[6].1,
        blue: PIECE_RGB[6].2,
        alpha: 1.0,
    }),
];

/// Alpha applied to the active piece color when drawing the ghost outline
/// (M5: the ghost is an outlined ring texture, so it reads at a higher alpha
/// than the old solid 0.3 dim without competing with settled cells).
pub const GHOST_ALPHA: f32 = 0.65;

/// Neutral fill for incoming versus garbage cells (`Piece::Garbage`),
/// deliberately outside the tetromino palette so garbage reads as garbage.
pub const GARBAGE_CELL_COLOR: Color = Color::srgb(0.42, 0.42, 0.47);

/// T24 **Invisible**: fixed steps a locked cell stays at full alpha before
/// its fade starts (half a second at the 60 Hz fixed step).
pub const FADE_GRACE_TICKS: u16 = 30;

/// T24 **Invisible**: total cell age (fixed steps since the lock) at which
/// the fade reaches [`FADE_FLOOR_ALPHA`] — one second after the lock.
pub const FADE_TOTAL_TICKS: u16 = 60;

/// T24 **Invisible**: alpha of a fully faded locked cell — invisible.
pub const FADE_FLOOR_ALPHA: f32 = 0.0;

/// `lock_ages` entry for a cell that is absent or not tracked (a reappearing
/// cell re-stamps from 0 — fades restart; see [`advance_lock_ages`]).
const NO_LOCK: u16 = u16::MAX;

/// Draw alpha of a locked cell aged `age` fixed steps (T24 **Invisible**):
/// full until [`FADE_GRACE_TICKS`], then linear down to [`FADE_FLOOR_ALPHA`]
/// at [`FADE_TOTAL_TICKS`], staying there once faded.
#[must_use]
pub fn lock_fade_alpha(age: u16) -> f32 {
    if age <= FADE_GRACE_TICKS {
        1.0
    } else if age >= FADE_TOTAL_TICKS {
        FADE_FLOOR_ALPHA
    } else {
        let t = (age - FADE_GRACE_TICKS) as f32 / (FADE_TOTAL_TICKS - FADE_GRACE_TICKS) as f32;
        1.0 + (FADE_FLOOR_ALPHA - 1.0) * t
    }
}

/// Solid fill color for cells locked/spawned as `piece`.
pub fn piece_color(piece: Piece) -> Color {
    if piece == Piece::Garbage {
        return GARBAGE_CELL_COLOR;
    }
    let idx = Piece::ALL.iter().position(|p| *p == piece).unwrap_or(0);
    PIECE_COLORS[idx]
}

/// Which layer of the playfield a drawn cell belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellKind {
    /// Settled board cell.
    Board,
    /// Cell of the falling piece.
    Active,
    /// Ghost landing cell (active piece color, dimmed).
    Ghost,
}

/// One visible cell of a render frame, derived purely from a snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotCell {
    /// Row on the drawn field (`DRAWN_TOP..ROWS`); negative rows are
    /// kick-lifted headroom cells above the board.
    pub row: i32,
    /// Column (`0..COLS`).
    pub col: usize,
    /// Piece the cell is drawn with (board color, active/ghost color).
    pub piece: Piece,
    /// Layer this cell belongs to.
    pub kind: CellKind,
}

impl SnapshotCell {
    /// Fill color: piece color at full alpha, dimmed for the ghost.
    pub fn color(&self) -> Color {
        match self.kind {
            CellKind::Ghost => piece_color(self.piece).with_alpha(GHOST_ALPHA),
            _ => piece_color(self.piece),
        }
    }

    /// Draw order along the camera z axis (later = on top).
    pub fn z(&self) -> f32 {
        match self.kind {
            CellKind::Board => 0.0,
            CellKind::Ghost => 1.0,
            CellKind::Active => 2.0,
        }
    }
}

/// Letterbox fit of the 10-row x [`VISIBLE_ROWS`]-row drawn field inside a
/// `window_w` x `window_h` window: returns `(cell, offset_x, offset_y)`
/// with a single square cell size on both axes and the field centered in
/// the remaining margin.
pub fn letterbox(window_w: f32, window_h: f32) -> (f32, f32, f32) {
    let cell = (window_w / COLS as f32).min(window_h / VISIBLE_ROWS as f32);
    let offset_x = (window_w - cell * COLS as f32) * 0.5;
    let offset_y = (window_h - cell * VISIBLE_ROWS as f32) * 0.5;
    (cell, offset_x, offset_y)
}

/// Fraction of window height reserved above the field for the portrait HUD
/// strip (score / hold / next queue) in [`playfield_view`].
pub const PORTRAIT_TOP_FRAC: f32 = 0.19;
/// Fraction of window height reserved below the field for the portrait
/// touch deck (buttons + combo/B2B strip) in [`playfield_view`].
pub const PORTRAIT_BOTTOM_FRAC: f32 = 0.19;

// Thread-local portrait force for unit tests (each cargo test thread is
// isolated, unlike a process env var). Production code never sets it.
#[cfg(test)]
thread_local! {
    static PORTRAIT_OVERRIDE: std::cell::Cell<Option<bool>> = const {
        std::cell::Cell::new(None)
    };
}

/// Test hook: force the portrait-native decision for this thread
/// (`Some(true)` / `Some(false)`) or restore environment-driven behavior
/// (`None`).
#[cfg(test)]
pub fn set_portrait_override(enabled: Option<bool>) {
    PORTRAIT_OVERRIDE.with(|slot| slot.set(enabled));
}

/// Portrait-native layout active? The Android build runs portrait-locked;
/// desktop opts in per-run with `TETRIS_PORTRAIT=1` for dev/testing. Only
/// ever active for windows taller than wide.
pub fn portrait_layout(window_w: f32, window_h: f32) -> bool {
    #[cfg(test)]
    if let Some(forced) = PORTRAIT_OVERRIDE.with(|slot| slot.get()) {
        return forced && window_h > window_w;
    }
    let enabled = cfg!(target_os = "android")
        || std::env::var_os("TETRIS_PORTRAIT").is_some_and(|value| value == "1");
    enabled && window_h > window_w
}

/// Viewport the playfield fits into plus the world-space `y` offset of that
/// viewport's center: `(view_w, view_h, center_y)`. Desktop (and any
/// landscape window) is the plain full window `(w, h, 0)`; portrait-native
/// mode reserves [`PORTRAIT_TOP_FRAC`] / [`PORTRAIT_BOTTOM_FRAC`] strips for
/// HUD and touch controls, so the field letterboxes inside the remainder,
/// centered in it.
pub fn playfield_view(window_w: f32, window_h: f32) -> (f32, f32, f32) {
    if !portrait_layout(window_w, window_h) {
        return (window_w, window_h, 0.0);
    }
    let top = window_h * PORTRAIT_TOP_FRAC;
    let bottom = window_h * PORTRAIT_BOTTOM_FRAC;
    (
        window_w,
        (window_h - top - bottom).max(1.0),
        (bottom - top) * 0.5,
    )
}

/// Cell size + top-left anchor of one drawn field inside a viewport (T26).
/// [`FieldLayout::fit`] is exactly the [`letterbox`] math, so the solo
/// full-window draw stays pixel-identical to T11.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldLayout {
    /// Square cell edge length in world units.
    pub cell: f32,
    /// World position of the drawn field's top-left corner.
    pub origin: Vec2,
}

impl FieldLayout {
    /// Letterbox fit of the drawn field inside a `view_w` x `view_h`
    /// viewport centered on the world origin (solo: the whole window).
    pub fn fit(view_w: f32, view_h: f32) -> Self {
        let (cell, offset_x, offset_y) = letterbox(view_w, view_h);
        Self {
            cell,
            origin: Vec2::new(-view_w * 0.5 + offset_x, view_h * 0.5 - offset_y),
        }
    }

    /// Letterbox fit for a window size, honoring the portrait-native
    /// viewport insets and center shift from [`playfield_view`] (identity
    /// for landscape/desktop windows).
    pub fn fit_window(window_w: f32, window_h: f32) -> Self {
        let (view_w, view_h, center_y) = playfield_view(window_w, window_h);
        let mut layout = Self::fit(view_w, view_h);
        layout.origin.y += center_y;
        layout
    }

    /// Place a field of a fixed `cell` size with its own center at `center`
    /// (versus: both halves share one cell size, each field centered in its
    /// half — margins may exceed the letterbox minimum).
    pub fn centered(cell: f32, center: Vec2) -> Self {
        Self {
            cell,
            origin: Vec2::new(
                center.x - cell * COLS as f32 * 0.5,
                center.y + cell * VISIBLE_ROWS as f32 * 0.5,
            ),
        }
    }

    /// World-space center of drawn cell `(row, col)` in this layout.
    pub fn cell_center(&self, row: i32, col: usize) -> Vec2 {
        Vec2::new(
            self.origin.x + (col as f32 + 0.5) * self.cell,
            self.origin.y - ((row - DRAWN_TOP) as f32 + 0.5) * self.cell,
        )
    }
}

/// The two half-window viewports of an active versus match (T26):
/// `[left, right]`, both at the shared cell size (the min of the two half
/// fits — the halves are equal by construction, so each half fits exactly)
/// with every field centered in its own half of the window.
pub fn versus_layouts(window_w: f32, window_h: f32) -> [FieldLayout; 2] {
    let (view_w, view_h, center_y) = playfield_view(window_w, window_h);
    let half_w = view_w * 0.5;
    let cell = FieldLayout::fit(half_w, view_h).cell;
    [
        FieldLayout::centered(cell, Vec2::new(-half_w * 0.5, center_y)),
        FieldLayout::centered(cell, Vec2::new(half_w * 0.5, center_y)),
    ]
}

/// Flatten a snapshot into the cells to draw this frame: all board rows
/// (including the 2-row spawn buffer, so kicked/spawning pieces never
/// disappear at the top edge), active piece cells and ghost cells, clipped
/// only to the full 10x22 board.
pub fn frame_cells(snapshot: &GameSnapshot) -> Vec<SnapshotCell> {
    let mut cells = Vec::with_capacity(ROWS * COLS + 8);
    for row in 0..ROWS {
        for col in 0..COLS {
            if let Some(piece) = snapshot.board.get(row, col) {
                cells.push(SnapshotCell {
                    row: row as i32,
                    col,
                    piece,
                    kind: CellKind::Board,
                });
            }
        }
    }
    if let Some(active) = snapshot.active {
        for (row, col) in active.cells() {
            push_visible(&mut cells, row, col, active.piece, CellKind::Active);
        }
        if let Some(ghost) = snapshot.ghost_row {
            for (row, col) in active.cells() {
                let shifted = ghost + (row - active.row);
                push_visible(&mut cells, shifted, col, active.piece, CellKind::Ghost);
            }
        }
    }
    cells
}

fn push_visible(cells: &mut Vec<SnapshotCell>, row: i32, col: i32, piece: Piece, kind: CellKind) {
    let Ok(col) = usize::try_from(col) else {
        return;
    };
    if !(DRAWN_TOP..ROWS as i32).contains(&row) || col >= COLS {
        return;
    }
    cells.push(SnapshotCell {
        row,
        col,
        piece,
        kind,
    });
}

/// Marker component for pooled playfield cell sprites; stores the layer so
/// tests and later systems (T14 FPS diagnostics, T19 juice) can filter.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayfieldCell {
    /// Layer this sprite currently draws.
    pub kind: CellKind,
}

/// Marks a pooled playfield cell with the match side it belongs to (T26).
/// Solo cells never carry it, and versus cells always do, so the two modes
/// are trivially distinguishable in tests and by later systems.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersusCellSide(pub Side);

/// Pooled cell sprite entities (rebuilt to the needed count each frame).
#[derive(Resource)]
struct CellPool {
    entities: Vec<Entity>,
    /// T24 **Invisible**: lock age (fixed core steps since the cell was
    /// first observed) of the solo board's settled cells, `[row][col]`.
    /// `NO_LOCK` = absent/untracked. Fixed 22×10 table — zero allocations
    /// per frame, ages saturate at [`FADE_TOTAL_TICKS`] so they never
    /// collide with the sentinel.
    lock_ages: [[u16; COLS]; ROWS],
    /// `GameCore::steps` at the previous fade update; `None` until the
    /// first INVISIBLE frame. A `steps` rewind (or `steps == 0`, both only
    /// possible on a fresh run) resets the whole table.
    fade_last_steps: Option<u64>,
}

impl Default for CellPool {
    fn default() -> Self {
        Self {
            entities: Vec::new(),
            lock_ages: [[NO_LOCK; COLS]; ROWS],
            fade_last_steps: None,
        }
    }
}

/// Color of the boundary frame drawn around each playfield (solo and both
/// versus halves) so the well is visible even where it is empty.
pub const FRAME_COLOR: Color = Color::srgb(0.55, 0.55, 0.62);

/// Marks one of the four bar sprites that outline a playfield frame.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldFrame {
    /// Match side of the framed field; `None` for the solo field.
    pub side: Option<Side>,
}

/// `(center, size)` of the four outline bars (top, bottom, left, right) of
/// the field rectangle of `layout`, sitting just outside the cell grid.
pub fn frame_bars(layout: &FieldLayout) -> [(Vec2, Vec2); 4] {
    let c = layout.cell;
    let pad = 0.18 * c;
    let t = 0.14 * c;
    let field_w = COLS as f32 * c;
    let field_h = VISIBLE_ROWS as f32 * c;
    let (x0, y_top) = (layout.origin.x, layout.origin.y);
    let y_bot = y_top - field_h;
    let long_h = field_w + 2.0 * (pad + t);
    let inset = pad + t * 0.5;
    [
        (
            Vec2::new(x0 + field_w * 0.5, y_top + inset),
            Vec2::new(long_h, t),
        ),
        (
            Vec2::new(x0 + field_w * 0.5, y_bot - inset),
            Vec2::new(long_h, t),
        ),
        (
            Vec2::new(x0 - inset, y_top - field_h * 0.5),
            Vec2::new(t, field_h),
        ),
        (
            Vec2::new(x0 + field_w + inset, y_top - field_h * 0.5),
            Vec2::new(t, field_h),
        ),
    ]
}

/// Pooled boundary-frame bars (exactly four per visible field).
#[derive(Resource, Default)]
struct FramePools {
    solo: Vec<Entity>,
    versus_left: Vec<Entity>,
    versus_right: Vec<Entity>,
}

/// Pooled versus cell sprites, one pool per match side (T26). Fully
/// despawned whenever versus is inactive, and vice versa for [`CellPool`],
/// so neither mode can leak entities into the other's frame.
#[derive(Resource, Default)]
struct VersusCellPools {
    left: Vec<Entity>,
    right: Vec<Entity>,
}

/// Z of the well backdrop: above the frame bars (`-0.1`), below every cell
/// layer (board `0`).
const BACKDROP_Z: f32 = -0.09;

/// Marks the dark grid panel behind one drawn field (M5). No marker without
/// [`ArtAssets`], so headless tests never see it.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldBackdrop {
    /// Match side of the backed field; `None` for the solo field.
    pub side: Option<Side>,
}

/// Pooled well backdrops (exactly one per visible field).
#[derive(Resource, Default)]
struct BackdropPools {
    solo: Vec<Entity>,
    versus_left: Vec<Entity>,
    versus_right: Vec<Entity>,
}

/// Bring the one-entity well backdrop of a field in line with `layout`.
/// `cells.len()`-sized panel exactly under the grid, so the generated grid
/// texture lines up with cell borders at any window size.
fn sync_backdrop(
    commands: &mut Commands,
    pool: &mut Vec<Entity>,
    layout: &FieldLayout,
    side: Option<Side>,
    art: &ArtAssets,
) {
    let field_w = COLS as f32 * layout.cell;
    let field_h = VISIBLE_ROWS as f32 * layout.cell;
    let center = Vec2::new(
        layout.origin.x + field_w * 0.5,
        layout.origin.y - field_h * 0.5,
    );
    let sprite = Sprite {
        image: art.well.clone(),
        custom_size: Some(Vec2::new(field_w, field_h)),
        ..default()
    };
    let transform = Transform::from_xyz(center.x, center.y, BACKDROP_Z);
    let marker = FieldBackdrop { side };
    if let Some(entity) = pool.first() {
        commands.entity(*entity).insert((marker, sprite, transform));
    } else {
        pool.push(commands.spawn((marker, sprite, transform)).id());
    }
}

/// Advance the per-cell lock-age table to `snapshot` (T24 **Invisible**).
///
/// Ages are keyed by `(col, row)` and tied to presence continuity: a cell
/// newly present in the board snapshot is stamped at `age = 0`; a still
/// present cell ages by the number of applied core fixed steps (the
/// `GameCore::steps` delta — frame-rate independent, frozen during pause and
/// the pre-roll, which never advances `steps`); a vanished cell is
/// unstamped, so a cell that *reappears* at the same `(col,row)` counts as
/// new and restarts its fade. Documented artifact of this rule on row
/// clears: rows sliding down into an occupied `(col,row)` keep the old age,
/// while a cell sliding into a free one is stamped fresh. Cells already
/// present when an INVISIBLE run begins (e.g. Dig's buried-garbage start
/// board) are stamped as just-locked — full alpha through the grace, then
/// the normal fade — never retro-faded to the floor. A fresh run
/// (`steps == 0`, or a `steps` rewind) clears the whole table, so no age is
/// ever inherited across runs; faded cells simply stay invisible until
/// cleared.
fn advance_lock_ages(
    ages: &mut [[u16; COLS]; ROWS],
    last_steps: &mut Option<u64>,
    snapshot: &GameSnapshot,
    steps: u64,
) {
    let fresh_run = steps == 0
        || match *last_steps {
            None => true,
            Some(prev) => steps < prev,
        };
    // Ages saturate at FADE_TOTAL_TICKS, so a bigger delta can be capped.
    let delta = if fresh_run {
        0
    } else {
        (steps - last_steps.unwrap_or(steps)).min(FADE_TOTAL_TICKS as u64)
    } as u16;
    *last_steps = Some(steps);
    if fresh_run {
        for row in ages.iter_mut() {
            *row = [NO_LOCK; COLS];
        }
    }
    for (row, age_row) in ages.iter_mut().enumerate() {
        for (col, age) in age_row.iter_mut().enumerate() {
            if snapshot.board.get(row, col).is_some() {
                *age = if *age == NO_LOCK {
                    0
                } else {
                    age.saturating_add(delta).min(FADE_TOTAL_TICKS)
                };
            } else {
                *age = NO_LOCK;
            }
        }
    }
}

/// Bring one pooled entity list in line with the cells of one frame: reuse
/// the prefix, despawn the surplus tail, spawn what is missing. With
/// `fades` (T24 **Invisible**), [`CellKind::Board`] cells draw at
/// [`lock_fade_alpha`] of their table entry; the other layers are untouched.
/// With `art` (M5), every cell draws through a procedural texture — the
/// beveled tile for board/active cells, the outlined ring for the ghost —
/// tinted by the (unchanged) snapshot color; without it (headless tests)
/// cells stay flat quads.
fn sync_pool(
    commands: &mut Commands,
    pool: &mut Vec<Entity>,
    cells: &[SnapshotCell],
    layout: &FieldLayout,
    side: Option<VersusCellSide>,
    fades: Option<&[[u16; COLS]; ROWS]>,
    art: Option<&ArtAssets>,
) {
    if pool.len() > cells.len() {
        let surplus = pool.split_off(cells.len());
        for entity in surplus {
            commands.entity(entity).despawn();
        }
    }

    for (index, frame_cell) in cells.iter().enumerate() {
        let center = layout.cell_center(frame_cell.row, frame_cell.col);
        let mut color = frame_cell.color();
        if let (Some(ages), CellKind::Board) = (fades, frame_cell.kind) {
            let age = ages
                .get(frame_cell.row as usize)
                .and_then(|row| row.get(frame_cell.col))
                .copied()
                .unwrap_or(0);
            color = color.with_alpha(lock_fade_alpha(age));
        }
        let mut sprite = Sprite {
            color,
            custom_size: Some(Vec2::splat(layout.cell)),
            ..default()
        };
        if let Some(art) = art {
            sprite.image = match frame_cell.kind {
                CellKind::Ghost => art.ghost.clone(),
                _ => art.tile.clone(),
            };
        }
        let transform = Transform::from_xyz(center.x, center.y, frame_cell.z());
        let marker = PlayfieldCell {
            kind: frame_cell.kind,
        };
        if index < pool.len() {
            let entity = pool[index];
            commands.entity(entity).insert((marker, sprite, transform));
            if let Some(side) = side {
                commands.entity(entity).insert(side);
            }
        } else {
            let entity = commands.spawn((marker, sprite, transform)).id();
            if let Some(side) = side {
                commands.entity(entity).insert(side);
            }
            pool.push(entity);
        }
    }
}

/// Despawn every entity of a pool (mode switch).
fn clear_pool(commands: &mut Commands, pool: &mut Vec<Entity>) {
    for entity in pool.drain(..) {
        commands.entity(entity).despawn();
    }
}

/// Bring one pooled boundary frame (four bars) in line with `layout`:
/// reuse, despawn surplus, spawn missing, and reposition every bar so the
/// frame follows window resizes.
fn sync_frame(
    commands: &mut Commands,
    pool: &mut Vec<Entity>,
    layout: &FieldLayout,
    side: Option<VersusCellSide>,
) {
    let bars = frame_bars(layout);
    if pool.len() > bars.len() {
        let surplus = pool.split_off(bars.len());
        for entity in surplus {
            commands.entity(entity).despawn();
        }
    }
    for (index, (center, size)) in bars.iter().enumerate() {
        let sprite = Sprite {
            color: FRAME_COLOR,
            custom_size: Some(*size),
            ..default()
        };
        let transform = Transform::from_xyz(center.x, center.y, -0.1);
        let marker = FieldFrame {
            side: side.map(|s| s.0),
        };
        if index < pool.len() {
            commands
                .entity(pool[index])
                .insert((marker, sprite, transform));
        } else {
            let entity = commands.spawn((marker, sprite, transform)).id();
            pool.push(entity);
        }
    }
}

/// Marks the full-window vignette sprite (M5): radial edge darkening behind
/// everything, giving the pillar-box voids around the field some depth.
#[derive(Component)]
pub struct Vignette;

/// Z of the vignette — behind the frame bars and the well backdrop.
const VIGNETTE_Z: f32 = -10.0;

/// Spawn the vignette once art is up (never headless) and keep it slightly
/// oversized to the window so camera shake never reveals its edge.
fn update_vignette(
    mut commands: Commands,
    art: Option<Res<ArtAssets>>,
    windows: Query<&Window>,
    mut sprites: Query<&mut Sprite, With<Vignette>>,
) {
    let Some(art) = art else { return };
    let Some(window) = windows.iter().next() else {
        return;
    };
    let size = window.resolution.size();
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }
    match sprites.iter_mut().next() {
        Some(mut sprite) => {
            sprite.custom_size = Some(Vec2::new(size.x * 1.04, size.y * 1.04));
        }
        None => {
            commands.spawn((
                Vignette,
                Sprite {
                    image: art.vignette.clone(),
                    custom_size: Some(Vec2::new(size.x * 1.04, size.y * 1.04)),
                    ..default()
                },
                Transform::from_xyz(0.0, 0.0, VIGNETTE_Z),
            ));
        }
    }
}

/// Color the window clears to before the vignette: a deep blue-charcoal,
/// clearly distinct from both the well panel and the settled stack.
pub const WINDOW_BG: Color = Color::srgb(0.043, 0.05, 0.075);

/// Full refresh of all cell sprites from the newest snapshot. The fixed-step
/// sim has already run for this frame (`RunFixedMainLoop` precedes
/// `Update`), so this always reads the latest state; per-frame redraw is
/// fine at <= ~210 cells. With [`VersusMatch::active`] the solo pool is
/// despawned and both match boards are drawn instead, one per half-viewport
/// of [`versus_layouts`] (T26).
#[allow(clippy::too_many_arguments)]
fn render_playfield(
    mut commands: Commands,
    core: Option<NonSend<GameCore>>,
    versus: Option<NonSend<VersusMatch>>,
    windows: Query<&Window>,
    mut pool: ResMut<CellPool>,
    mut versus_pools: ResMut<VersusCellPools>,
    mut frames: ResMut<FramePools>,
    mut backdrops: ResMut<BackdropPools>,
    art: Option<Res<ArtAssets>>,
) {
    let Some(window) = windows.iter().next() else {
        return;
    };
    let size = window.resolution.size();
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }

    if let Some(versus) = versus.filter(|versus| versus.active) {
        clear_pool(&mut commands, &mut pool.entities);
        clear_pool(&mut commands, &mut frames.solo);
        clear_pool(&mut commands, &mut backdrops.solo);
        let snapshot = versus.match_.snapshot();
        let [left, right] = versus_layouts(size.x, size.y);
        sync_frame(
            &mut commands,
            &mut frames.versus_left,
            &left,
            Some(VersusCellSide(Side::Left)),
        );
        sync_frame(
            &mut commands,
            &mut frames.versus_right,
            &right,
            Some(VersusCellSide(Side::Right)),
        );
        if let Some(art) = art.as_deref() {
            sync_backdrop(
                &mut commands,
                &mut backdrops.versus_left,
                &left,
                Some(Side::Left),
                art,
            );
            sync_backdrop(
                &mut commands,
                &mut backdrops.versus_right,
                &right,
                Some(Side::Right),
                art,
            );
        }
        let cells = frame_cells(&snapshot.left);
        sync_pool(
            &mut commands,
            &mut versus_pools.left,
            &cells,
            &left,
            Some(VersusCellSide(Side::Left)),
            None,
            art.as_deref(),
        );
        let cells = frame_cells(&snapshot.right);
        sync_pool(
            &mut commands,
            &mut versus_pools.right,
            &cells,
            &right,
            Some(VersusCellSide(Side::Right)),
            None,
            art.as_deref(),
        );
        return;
    }

    clear_pool(&mut commands, &mut versus_pools.left);
    clear_pool(&mut commands, &mut versus_pools.right);
    clear_pool(&mut commands, &mut frames.versus_left);
    clear_pool(&mut commands, &mut frames.versus_right);
    clear_pool(&mut commands, &mut backdrops.versus_left);
    clear_pool(&mut commands, &mut backdrops.versus_right);
    let Some(core) = core else { return };
    let layout = FieldLayout::fit_window(size.x, size.y);
    sync_frame(&mut commands, &mut frames.solo, &layout, None);
    if let Some(art) = art.as_deref() {
        sync_backdrop(&mut commands, &mut backdrops.solo, &layout, None, art);
    }
    let snapshot = core.game.snapshot();
    let mut cells = frame_cells(&snapshot);
    // T23 **No Ghost**: render-only suppression — the core keeps computing
    // `ghost_row` and the snapshot wire is untouched, the cells are simply
    // never handed to the sprite pool.
    if core
        .active_mode
        .mutators
        .contains(crate::mutators::Mutators::NO_GHOST)
    {
        cells.retain(|cell| cell.kind != CellKind::Ghost);
    }
    // T24 **Invisible**: render-only lock fade — per-cell ages live in the
    // solo sprite pool's fixed table and never feed back into the core or
    // the snapshot; only `CellKind::Board` sprites get their alpha scaled
    // (the active piece and the ghost are untouched).
    let invisible = core
        .active_mode
        .mutators
        .contains(crate::mutators::Mutators::INVISIBLE);
    // Split the pool borrow: the age table is read while its entity list
    // is mutated by the sync below (disjoint fields).
    let CellPool {
        entities,
        lock_ages,
        fade_last_steps,
    } = &mut *pool;
    if invisible {
        advance_lock_ages(lock_ages, fade_last_steps, &snapshot, core.steps);
    }
    sync_pool(
        &mut commands,
        entities,
        &cells,
        &layout,
        None,
        invisible.then_some(&*lock_ages),
        art.as_deref(),
    );
}

/// Flat colored playfield renderer fed exclusively by `Game::snapshot()`
/// (solo) or the active [`VersusMatch`] snapshot (1v1, two fields).
pub struct RenderPlugin;

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(WINDOW_BG))
            .init_resource::<CellPool>()
            .init_resource::<VersusCellPools>()
            .init_resource::<FramePools>()
            .init_resource::<BackdropPools>()
            .add_systems(Startup, art::init_art_assets)
            .add_systems(Update, (update_vignette, render_playfield).chain());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_bridge::{CoreBridgePlugin, PendingActions};
    use crate::state::AppState;
    use tetris_core::board::{Board, HIDDEN_ROWS};
    use tetris_core::piece::{PieceState, Rotation};

    use bevy::app::FixedUpdate;
    use bevy::window::WindowPlugin;
    use tetris_core::actions::Action;
    use tetris_core::game::Game;

    const EPS: f32 = 1e-4;

    /// Regression: versus garbage (`Piece::Garbage`) must render in its own
    /// neutral color — it used to be stamped as `Piece::O`, which drew 18
    /// incoming rows as a giant yellow O-piece tower.
    #[test]
    fn garbage_cells_render_neutral_not_piece_colored() {
        assert_eq!(piece_color(Piece::Garbage), GARBAGE_CELL_COLOR);
        for piece in Piece::ALL {
            assert_ne!(
                piece_color(piece),
                GARBAGE_CELL_COLOR,
                "garbage must not share a tetromino palette color"
            );
        }
    }

    /// Portrait-native viewport: HUD strip + touch deck reserved, field
    /// inside the remainder, versus boards share it; landscape windows stay
    /// plain full-window.
    #[test]
    fn playfield_view_reserves_portrait_strips() {
        set_portrait_override(Some(true));
        let (w, h) = (1080.0, 2404.0);
        let (view_w, view_h, center_y) = playfield_view(w, h);
        assert_eq!(view_w, w);
        assert!(
            (view_h - h * (1.0 - PORTRAIT_TOP_FRAC - PORTRAIT_BOTTOM_FRAC)).abs() < EPS,
            "view_h {view_h} does not match the strip remainder"
        );
        assert!(
            (center_y - h * (PORTRAIT_BOTTOM_FRAC - PORTRAIT_TOP_FRAC) * 0.5).abs() < EPS,
            "viewport center shift {center_y}"
        );

        let layout = FieldLayout::fit_window(w, h);
        assert!(layout.cell * COLS as f32 <= w + EPS);
        assert!(layout.cell * VISIBLE_ROWS as f32 <= view_h + EPS);
        assert!(layout.origin.y <= h * 0.5 - h * PORTRAIT_TOP_FRAC + EPS);
        assert!(
            layout.origin.y - layout.cell * VISIBLE_ROWS as f32
                >= -h * 0.5 + h * PORTRAIT_BOTTOM_FRAC - EPS
        );

        let [left, right] = versus_layouts(w, h);
        for layout in [left, right] {
            assert!(layout.origin.x >= -w * 0.5 - EPS);
            assert!(layout.origin.x + layout.cell * COLS as f32 <= w * 0.5 + EPS);
            assert!(
                layout.origin.y - layout.cell * VISIBLE_ROWS as f32
                    >= -h * 0.5 + h * PORTRAIT_BOTTOM_FRAC - EPS,
                "versus field sinks into the bottom deck"
            );
        }

        // Landscape windows ignore the override entirely.
        assert_eq!(playfield_view(2404.0, 1080.0), (2404.0, 1080.0, 0.0));
        assert!(!portrait_layout(2404.0, 1080.0));
    }

    /// Letterbox math: square cells on both axes, centered, field aspect
    /// preserved, cell as large as possible at several window shapes.
    #[test]
    fn letterbox_fits_and_centers_square_cells() {
        let check = |w: f32, h: f32| {
            let (cell, ox, oy) = letterbox(w, h);
            let expect_cell = (w / COLS as f32).min(h / VISIBLE_ROWS as f32);
            assert!(
                (cell - expect_cell).abs() < EPS,
                "window {w}x{h}: cell {cell} != expected {expect_cell}"
            );
            assert!(cell * COLS as f32 <= w + EPS, "field wider than window");
            assert!(cell * VISIBLE_ROWS as f32 <= h + EPS, "field taller");
            assert!(
                (2.0 * ox + cell * COLS as f32 - w).abs() < EPS,
                "not horizontally centered: {w}x{h} -> ox {ox}"
            );
            assert!(
                (2.0 * oy + cell * VISIBLE_ROWS as f32 - h).abs() < EPS,
                "not vertically centered: {w}x{h} -> oy {oy}"
            );
            // Maximality: scaling the cell up by any epsilon breaks a bound.
            let bigger = cell + EPS;
            assert!(bigger * COLS as f32 > w || bigger * VISIBLE_ROWS as f32 > h);
        };
        check(1280.0, 720.0); // height-limited, side pillar boxes
        check(320.0, 240.0); // tiny window
        check(3440.0, 1440.0); // ultrawide
        check(800.0, 800.0); // square: height-limited
        check(100.0, 240.0); // exact 10:24 fit (22 board + 2 headroom rows)
    }

    fn render_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris T11 render smoke".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((CoreBridgePlugin, RenderPlugin));
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app
    }

    /// One scripted frame: apply `actions` through the real fixed schedule,
    /// then run `Update` (render).
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

    type Drawn = (CellKind, Color, Option<Vec2>, Vec3);

    fn drawn(app: &mut App) -> Vec<Drawn> {
        let mut query = app
            .world_mut()
            .query::<(&PlayfieldCell, &Sprite, &Transform)>();
        query
            .iter(app.world())
            .map(|(cell, sprite, transform)| {
                (
                    cell.kind,
                    sprite.color,
                    sprite.custom_size,
                    transform.translation,
                )
            })
            .collect()
    }

    fn cells_of(app: &mut App, kind: CellKind) -> Vec<Drawn> {
        drawn(app)
            .into_iter()
            .filter(|(cell_kind, ..)| *cell_kind == kind)
            .collect()
    }

    fn count(app: &mut App) -> usize {
        let mut query = app.world_mut().query::<&PlayfieldCell>();
        query.iter(app.world()).count()
    }

    /// Snapshot cell counts recomputed independently of `frame_cells` using
    /// only core APIs, for cross-checking the drawn entity count.
    fn expected_count(snapshot: &GameSnapshot) -> usize {
        let visible =
            |row: i32, col: i32| (row as usize) < ROWS && col >= 0 && (col as usize) < COLS;
        let board = (0..ROWS)
            .flat_map(|r| (0..COLS).map(move |c| (r, c)))
            .filter(|&(r, c)| snapshot.board.get(r, c).is_some())
            .count();
        let (active, ghost) = match snapshot.active {
            Some(ps) => {
                let active = ps.cells().iter().filter(|&&(r, c)| visible(r, c)).count();
                let ghost = match snapshot.ghost_row {
                    Some(g) => ps
                        .cells()
                        .iter()
                        .filter(|&&(r, c)| visible(g + (r - ps.row), c))
                        .count(),
                    None => 0,
                };
                (active, ghost)
            }
            None => (0, 0),
        };
        board + active + ghost
    }

    /// Kick-lifted cells in the headroom rows above the board must still
    /// render (M3 playtest: an L rotated at the wall "turned into 3 squares"
    /// because negative rows were clipped out of the frame).
    #[test]
    fn headroom_rows_render_kick_lifted_cells() {
        let snapshot = GameSnapshot {
            board: Board::new(),
            active: Some(PieceState {
                piece: Piece::L,
                rot: Rotation::Cw,
                row: DRAWN_TOP,
                col: 4,
            }),
            ghost_row: None,
            hold: None,
            hold_used: false,
            next: vec![],
            score: 0,
            level: 1,
            lines: 0,
            combo: 0,
            b2b: false,
            game_over: false,
        };
        let cells = frame_cells(&snapshot);
        let active: Vec<SnapshotCell> = cells
            .iter()
            .filter(|c| c.kind == CellKind::Active)
            .copied()
            .collect();
        assert_eq!(active.len(), 4, "L must render all four cells: {active:?}");
        assert!(active
            .iter()
            .all(|c| (DRAWN_TOP..ROWS as i32).contains(&c.row)));
        assert!(active.iter().any(|c| c.row < 0), "headroom cell expected");
        // Cells above the drawn top are dropped, never wrapped into view.
        let mut lifted = snapshot;
        lifted.active = Some(PieceState {
            piece: Piece::L,
            rot: Rotation::Cw,
            row: DRAWN_TOP - 1,
            col: 4,
        });
        assert!(
            frame_cells(&lifted)
                .iter()
                .all(|c| c.row >= DRAWN_TOP && c.row < ROWS as i32),
            "off-field rows must be clipped"
        );
    }

    #[test]
    fn spawn_buffer_cells_are_drawn() {
        // M3 playtest fix: the 2 spawn-buffer rows are rendered so pieces
        // entering (or kicked into) rows 0..=1 never clip at the top edge.
        let fresh = Game::new(1).snapshot();
        let ps = fresh.active.expect("fresh game has an active piece");
        assert!(ps.cells().iter().all(|(r, _)| (*r as usize) < HIDDEN_ROWS));
        let mut app = render_app(1);
        frame(&mut app, &[]);
        let fresh = app.world().non_send::<GameCore>().game.snapshot();
        assert_eq!(count(&mut app), expected_count(&fresh));
        assert_eq!(cells_of(&mut app, CellKind::Active).len(), 4);
        assert_eq!(cells_of(&mut app, CellKind::Ghost).len(), 4);

        frame(&mut app, &[Action::SoftDrop]);
        assert_eq!(
            count(&mut app),
            expected_count(&app.world().non_send::<GameCore>().game.snapshot())
        );
    }

    #[test]
    fn cells_track_scripted_actions_with_prd_colors_and_alpha() {
        let mut app = render_app(1);
        let first = Game::new(1).snapshot().active.unwrap().piece;
        let second = Game::new(1).peek_next(1)[0];

        // Lock the first piece; its cells become board cells, next spawns hidden.
        frame(&mut app, &[Action::HardDrop]);
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        assert_eq!(snapshot.active.unwrap().piece, second);
        let board = cells_of(&mut app, CellKind::Board);
        let ghost = cells_of(&mut app, CellKind::Ghost);
        assert_eq!(
            count(&mut app),
            expected_count(&snapshot),
            "settled + spawned-piece buffer cells + ghost"
        );
        assert_eq!(cells_of(&mut app, CellKind::Active).len(), 4);
        assert!(
            !board.is_empty(),
            "hard drop must leave visible settled cells"
        );
        for (_, color, ..) in &board {
            assert_eq!(*color, piece_color(first), "board cell palette");
        }
        for (_, color, ..) in &ghost {
            assert_eq!(
                color.to_srgba(),
                piece_color(second).with_alpha(GHOST_ALPHA).to_srgba(),
                "ghost dimming uses the active piece color"
            );
        }

        // Drop the second piece into the visible field.
        for _ in 0..3 {
            frame(&mut app, &[Action::SoftDrop]);
        }
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        let active = cells_of(&mut app, CellKind::Active);
        assert_eq!(count(&mut app), expected_count(&snapshot));
        assert_eq!(
            active.len(),
            4,
            "fully visible active piece draws four cells"
        );
        for (_, color, ..) in &active {
            assert_eq!(*color, piece_color(second), "active piece palette");
        }

        // 1280x720 -> cell 36, field 360x720 centered: check one known cell.
        let ps = snapshot.active.unwrap();
        let (cell, ox, _) = letterbox(1280.0, 720.0);
        let expected_x = -640.0 + ox + (ps.col as f32 + 0.5) * cell;
        let xs: Vec<f32> = active.iter().map(|(_, _, _, t)| t.x).collect();
        assert!(
            xs.iter().any(|x| (x - expected_x).abs() < EPS),
            "active cell column {expected_x} missing from {xs:?}"
        );
        for (_, _, custom_size, translation) in &active {
            assert_eq!(*custom_size, Some(Vec2::splat(cell)));
            assert_eq!(translation.z, 2.0, "active draws on top");
        }
    }

    #[test]
    fn sprites_reflow_on_window_resize() {
        let mut app = render_app(1);
        for _ in 0..3 {
            frame(&mut app, &[Action::SoftDrop]);
        }
        let (cell_old, ox_old, _) = letterbox(1280.0, 720.0);
        {
            let mut windows = app.world_mut().query::<&mut Window>();
            windows
                .single_mut(app.world_mut())
                .unwrap()
                .resolution
                .set_physical_resolution(800, 800);
        }
        let _ = app.world_mut().try_run_schedule(Update);
        let (cell, _, _) = letterbox(800.0, 800.0);
        assert_ne!(cell_old, cell);
        assert!(cell_old > 0.0 && ox_old > 0.0);

        let active = cells_of(&mut app, CellKind::Active);
        assert!(!active.is_empty());
        for (_, _, custom_size, _) in &active {
            assert_eq!(*custom_size, Some(Vec2::splat(cell)));
        }
    }

    // ------------------------------------------------------------------
    // T26: versus (two-field) rendering
    // ------------------------------------------------------------------

    use crate::core_bridge::{end_versus, start_versus, VersusMatch, VersusWinner};
    use crate::input::VersusActions;
    use tetris_core::versus::{AttackRule, Side};

    /// Activate a versus match through the shared lifecycle path (the same
    /// one the menu buttons call), without touching the process env.
    fn open_versus(app: &mut App, rule: AttackRule) {
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        rule,
                        crate::core_bridge::Controller::Human,
                        crate::core_bridge::Controller::Human,
                    );
                });
            });
    }

    fn close_versus(app: &mut App) {
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    end_versus(versus.into_inner(), winner.into_inner(), state.into_inner());
                });
            });
    }

    fn versus_step(app: &mut App) {
        app.world_mut().run_schedule(FixedUpdate);
        let _ = app.world_mut().try_run_schedule(Update);
    }

    fn push_versus(app: &mut App, actions: &[Action]) {
        let mut queue = app.world_mut().resource_mut::<VersusActions>();
        queue.left.extend(actions.iter().copied());
        queue.right.extend(actions.iter().copied());
    }

    fn versus_cells(app: &mut App) -> Vec<(Side, Vec3)> {
        let mut query = app.world_mut().query::<(&VersusCellSide, &Transform)>();
        let mut cells: Vec<(Side, Vec3)> = query
            .iter(app.world())
            .map(|(side, transform)| (side.0, transform.translation))
            .collect();
        cells.sort_by_key(|(side, pos)| {
            (
                format!("{side:?}"),
                (pos.x * 1000.0) as i64,
                (pos.y * 1000.0) as i64,
            )
        });
        cells
    }

    /// Deterministic multiset of everything on screen (kind, color, place).
    fn snapshot_drawn(app: &mut App) -> Vec<(CellKind, Color, Vec3)> {
        let mut items: Vec<(CellKind, Color, Vec3)> = drawn(app)
            .into_iter()
            .map(|(kind, color, _, translation)| (kind, color, translation))
            .collect();
        items.sort_by(|a, b| {
            (format!("{:?} {:?}", a.0, a.1), a.2.to_array())
                .partial_cmp(&(format!("{:?} {:?}", b.0, b.1), b.2.to_array()))
                .unwrap()
        });
        items
    }

    #[test]
    fn versus_layout_shares_cell_size_and_stays_in_its_half() {
        let check = |w: f32, h: f32| {
            let [left, right] = versus_layouts(w, h);
            let expect_cell = (w * 0.5 / COLS as f32).min(h / VISIBLE_ROWS as f32);
            assert!(
                (left.cell - expect_cell).abs() < EPS,
                "cell {} != half fit {expect_cell} for {w}x{h}",
                left.cell
            );
            assert_eq!(left.cell, right.cell, "both fields share one size");
            assert!((left.origin.y - right.origin.y).abs() < EPS, "same height");

            let half = w * 0.5;
            let field_w = left.cell * COLS as f32;
            let field_h = left.cell * VISIBLE_ROWS as f32;
            assert!(
                field_w <= half + EPS && field_h <= h + EPS,
                "fields must fit their halves: {w}x{h}"
            );
            assert!(
                left.origin.x >= -half - EPS && left.origin.x + field_w <= EPS,
                "left field outside left half: {}..{}",
                left.origin.x,
                left.origin.x + field_w
            );
            assert!(
                right.origin.x >= -EPS && right.origin.x + field_w <= half + EPS,
                "right field outside right half: {}..{}",
                right.origin.x,
                right.origin.x + field_w
            );
            assert!(
                left.origin.y <= h * 0.5 + EPS && left.origin.y - field_h >= -h * 0.5 - EPS,
                "fields outside window height"
            );
            // Centered in their own halves.
            let center_l = Vec2::new(left.origin.x + field_w * 0.5, left.origin.y - field_h * 0.5);
            let center_r = Vec2::new(
                right.origin.x + field_w * 0.5,
                right.origin.y - field_h * 0.5,
            );
            assert!(
                (center_l.x + half * 0.5).abs() < EPS && center_l.y.abs() < EPS,
                "left field not centered: {center_l:?}"
            );
            assert!(
                (center_r.x - half * 0.5).abs() < EPS && center_r.y.abs() < EPS,
                "right field not centered: {center_r:?}"
            );
        };
        check(1280.0, 720.0);
        check(1920.0, 1080.0);
        check(900.0, 600.0);
        check(500.0, 1200.0);
        check(688.0, 1200.0); // exact 10:24 half fit
    }

    #[test]
    fn solo_field_layout_matches_letterbox_exactly() {
        for (w, h) in [(1280.0f32, 720.0f32), (800.0, 800.0), (320.0, 240.0)] {
            let layout = FieldLayout::fit(w, h);
            let (cell, offset_x, offset_y) = letterbox(w, h);
            assert_eq!(layout.cell, cell);
            assert_eq!(
                layout.origin,
                Vec2::new(-w * 0.5 + offset_x, h * 0.5 - offset_y)
            );
        }
    }

    #[test]
    fn versus_frame_draws_both_boards() {
        let mut app = render_app(1);
        app.add_plugins(crate::hud::HudPlugin);
        open_versus(&mut app, AttackRule::Garbage);
        push_versus(&mut app, &[Action::HardDrop]);
        versus_step(&mut app);

        let snapshot = app.world().non_send::<VersusMatch>().match_.snapshot();
        let expected = expected_count(&snapshot.left) + expected_count(&snapshot.right);
        assert!(
            snapshot.left.score > 0 && snapshot.right.score > 0,
            "precondition: both sides dropped"
        );
        assert_eq!(
            count(&mut app),
            expected,
            "cells from left + right snapshot"
        );

        // Every drawn cell belongs to a side; the two boards sit in their
        // own halves with the shared cell size.
        let mut marked = app.world_mut().query::<(&PlayfieldCell, &VersusCellSide)>();
        assert_eq!(
            marked.iter(app.world()).count(),
            expected,
            "no solo (unmarked) cells while versus is up"
        );
        let [left, right] = versus_layouts(1280.0, 720.0);
        let sides = versus_cells(&mut app);
        let (left_cells, right_cells): (Vec<_>, Vec<_>) =
            sides.into_iter().partition(|(side, _)| *side == Side::Left);
        assert!(!left_cells.is_empty() && !right_cells.is_empty());
        for (side, pos) in &left_cells {
            assert_eq!(*side, Side::Left);
            assert!(pos.x < 0.0 && pos.x >= -640.0, "left cell at {pos:?}");
            assert!(
                pos.x >= left.origin.x - EPS
                    && pos.x <= left.origin.x + left.cell * COLS as f32 + EPS
            );
        }
        for (side, pos) in &right_cells {
            assert_eq!(*side, Side::Right);
            assert!(pos.x > 0.0 && pos.x <= 640.0, "right cell at {pos:?}");
            assert!(
                pos.x >= right.origin.x - EPS
                    && pos.x <= right.origin.x + right.cell * COLS as f32 + EPS
            );
        }
    }

    #[test]
    fn field_frames_outline_the_active_field_layouts() {
        let collect = |app: &mut App| -> Vec<(Option<Side>, Vec3, Vec2)> {
            let mut bars = app
                .world_mut()
                .query::<(&FieldFrame, &Transform, &Sprite)>();
            bars.iter(app.world())
                .map(|(f, t, s)| (f.side, t.translation, s.custom_size.unwrap_or(Vec2::ZERO)))
                .collect()
        };
        let matches = |bars: &[(Option<Side>, Vec3, Vec2)], layout: &FieldLayout, side| {
            let expected = frame_bars(layout);
            for (bar_side, pos, size) in bars {
                assert_eq!(*bar_side, side);
                assert!(
                    expected.iter().any(|(center, bar)| {
                        (pos.x - center.x).abs() < EPS
                            && (pos.y - center.y).abs() < EPS
                            && (size.x - bar.x).abs() < EPS
                            && (size.y - bar.y).abs() < EPS
                    }),
                    "frame bar {pos:?} {size:?} outside the layout outline"
                );
            }
        };

        let mut app = render_app(1);
        frame(&mut app, &[Action::HardDrop]);
        let solo = collect(&mut app);
        assert_eq!(solo.len(), 4, "solo field frame has four bars");
        matches(&solo, &FieldLayout::fit(1280.0, 720.0), None);

        open_versus(&mut app, AttackRule::Garbage);
        app.update();
        let versus_bars = collect(&mut app);
        assert_eq!(versus_bars.len(), 8, "one outline per versus field");
        let [left, right] = versus_layouts(1280.0, 720.0);
        let (left_bars, right_bars): (Vec<_>, Vec<_>) = versus_bars
            .into_iter()
            .partition(|(side, _, _)| *side == Some(Side::Left));
        assert_eq!(left_bars.len(), 4);
        matches(&left_bars, &left, Some(Side::Left));
        matches(&right_bars, &right, Some(Side::Right));
    }

    #[test]
    fn versus_pools_despawn_on_exit_and_solo_renders_like_fresh() {
        // Reference: a fresh solo app scripted with two hard drops.
        let mut fresh = render_app(5);
        frame(&mut fresh, &[Action::HardDrop]);
        frame(&mut fresh, &[Action::HardDrop]);
        let reference = snapshot_drawn(&mut fresh);

        let mut app = render_app(5);
        frame(&mut app, &[Action::HardDrop]);
        frame(&mut app, &[Action::HardDrop]);
        open_versus(&mut app, AttackRule::Garbage);
        push_versus(&mut app, &[Action::HardDrop]);
        versus_step(&mut app);

        let mut solo = app
            .world_mut()
            .query::<(&PlayfieldCell, Option<&VersusCellSide>)>();
        let solo_only = solo
            .iter(app.world())
            .filter(|(.., side)| side.is_none())
            .count();
        assert_eq!(
            solo_only, 0,
            "solo pool must be despawned while versus draws"
        );
        assert!(
            !versus_cells(&mut app).is_empty(),
            "versus pools have cells"
        );

        close_versus(&mut app);
        let _ = app.world_mut().try_run_schedule(Update);

        assert!(
            versus_cells(&mut app).is_empty(),
            "versus pools must be despawned after end_versus"
        );
        assert_eq!(
            snapshot_drawn(&mut app),
            reference,
            "solo after versus renders exactly like a fresh solo"
        );
    }
}

#[cfg(test)]
mod t23_tests {
    use super::*;
    use crate::core_bridge::{CoreBridgePlugin, GameCore};
    use crate::modes::ModeId;
    use crate::mutators::Mutators;
    use crate::state::AppState;

    use bevy::app::FixedUpdate;
    use bevy::window::WindowPlugin;

    fn render_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris t23 render".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((CoreBridgePlugin, RenderPlugin));
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app
    }

    fn render_frame(app: &mut App) {
        app.world_mut().run_schedule(FixedUpdate);
        let _ = app.world_mut().try_run_schedule(Update);
    }

    fn drawn_cells(app: &mut App, kind: CellKind) -> usize {
        let mut query = app.world_mut().query::<&PlayfieldCell>();
        query
            .iter(app.world())
            .filter(|cell| cell.kind == kind)
            .count()
    }

    /// **No Ghost** (T23): the renderer skips ghost cells while the mutator
    /// is active on the run — the core keeps computing `ghost_row` (the
    /// snapshot is untouched), and a clean restart brings the ghost back.
    #[test]
    fn no_ghost_mutator_suppresses_ghost_cells_in_render() {
        let mut app = render_app(0x051);
        render_frame(&mut app);
        let baseline_ghost = drawn_cells(&mut app, CellKind::Ghost);
        assert!(
            baseline_ghost > 0,
            "baseline clean run draws ghost cells (got {baseline_ghost})"
        );
        let baseline_active = drawn_cells(&mut app, CellKind::Active);

        let seed = app.world().non_send::<GameCore>().seed;
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.selected_mutators = Mutators::NO_GHOST;
            core.start_mode(seed, ModeId::Marathon);
        }
        render_frame(&mut app);
        assert_eq!(
            drawn_cells(&mut app, CellKind::Ghost),
            0,
            "No Ghost: zero drawn ghost cells"
        );
        assert_eq!(
            drawn_cells(&mut app, CellKind::Active),
            baseline_active,
            "the active piece still renders"
        );
        assert!(
            app.world()
                .non_send::<GameCore>()
                .game
                .snapshot()
                .ghost_row
                .is_some(),
            "core still computes the ghost — snapshot untouched"
        );

        let seed = app.world().non_send::<GameCore>().seed;
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.selected_mutators = Mutators::empty();
            core.start_mode(seed, ModeId::Marathon);
        }
        render_frame(&mut app);
        assert_eq!(
            drawn_cells(&mut app, CellKind::Ghost),
            baseline_ghost,
            "clean re-run draws the ghost again (regression)"
        );
    }
}

#[cfg(test)]
mod t24_tests {
    //! T24 **Invisible** mutator: locked board cells fade out over
    //! [`FADE_TOTAL_TICKS`] fixed steps ([`FADE_GRACE_TICKS`] at full alpha,
    //! then linear to [`FADE_FLOOR_ALPHA`]). Render-only; the active piece
    //! and the ghost (per NO_GHOST) are untouched.
    use super::*;
    use crate::core_bridge::{CoreBridgePlugin, GameCore, PendingActions};
    use crate::modes::ModeId;
    use crate::mutators::Mutators;
    use crate::state::AppState;

    use bevy::app::FixedUpdate;
    use bevy::window::WindowPlugin;
    use tetris_core::actions::Action;

    const EPS: f32 = 1e-4;

    fn render_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris t24 render".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((CoreBridgePlugin, RenderPlugin));
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app
    }

    /// One scripted frame: apply `actions` through the real fixed schedule,
    /// then run `Update` (render).
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

    /// Snapshot of settled board occupancy — the (col,row) keys the fade
    /// table is keyed by.
    fn locked_cells(app: &App) -> Vec<(usize, usize)> {
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        (0..ROWS)
            .flat_map(|r| (0..COLS).map(move |c| (r, c)))
            .filter(|&(r, c)| snapshot.board.get(r, c).is_some())
            .collect()
    }

    /// Alpha of the drawn sprite at the board position `(row, col)`, or
    /// `None` when nothing is drawn there.
    fn board_alpha(app: &mut App, row: usize, col: usize) -> Option<f32> {
        let layout = FieldLayout::fit_window(1280.0, 720.0);
        let center = layout.cell_center(row as i32, col);
        let mut query = app
            .world_mut()
            .query::<(&PlayfieldCell, &Sprite, &Transform)>();
        query
            .iter(app.world())
            .find(|(cell, _, t)| {
                cell.kind == CellKind::Board
                    && (t.translation.x - center.x).abs() < 1e-3
                    && (t.translation.y - center.y).abs() < 1e-3
            })
            .map(|(_, sprite, _)| sprite.color.to_srgba().alpha)
    }

    fn alphas_at(app: &mut App, cells: &[(usize, usize)]) -> Vec<f32> {
        cells
            .iter()
            .map(|&(row, col)| {
                board_alpha(app, row, col)
                    .unwrap_or_else(|| panic!("board cell ({row},{col}) not drawn"))
            })
            .collect()
    }

    /// Locks the first piece of an INVISIBLE run and returns the positions
    /// of its settled cells.
    fn start_invisible_and_lock(seed: u64) -> (App, Vec<(usize, usize)>) {
        let mut app = render_app(seed);
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.selected_mutators = Mutators::INVISIBLE;
            core.start_mode(seed, ModeId::Marathon);
        }
        frame(&mut app, &[Action::HardDrop]);
        let cells = locked_cells(&app);
        assert!(!cells.is_empty(), "hard drop must settle board cells");
        (app, cells)
    }

    /// The fade curve itself, at the named sample ages.
    #[test]
    fn fade_curve_is_grace_then_linear_floor() {
        assert_eq!(lock_fade_alpha(0), 1.0);
        assert_eq!(lock_fade_alpha(FADE_GRACE_TICKS - 1), 1.0);
        assert_eq!(lock_fade_alpha(FADE_GRACE_TICKS), 1.0);
        let mid = (FADE_GRACE_TICKS + FADE_TOTAL_TICKS) / 2;
        let mid_alpha = lock_fade_alpha(mid);
        assert!((mid_alpha - 0.5).abs() < EPS, "mid-fade alpha {mid_alpha}");
        assert!(
            lock_fade_alpha(FADE_GRACE_TICKS + 1) < lock_fade_alpha(FADE_GRACE_TICKS),
            "strictly decreasing past the grace"
        );
        assert_eq!(lock_fade_alpha(FADE_TOTAL_TICKS), FADE_FLOOR_ALPHA);
        assert_eq!(
            lock_fade_alpha(FADE_TOTAL_TICKS + 500),
            FADE_FLOOR_ALPHA,
            "stays invisible once faded"
        );
    }

    /// A locked cell's drawn alpha follows the curve at ages 0 / grace / mid
    /// / full fade (one fixed step per frame), and stays invisible after.
    #[test]
    fn locked_cells_follow_the_fade_curve_over_60_ticks() {
        let (mut app, cells) = start_invisible_and_lock(0x024);
        // Age 0 — the frame the lock is first observed: full alpha.
        assert!(
            alphas_at(&mut app, &cells)
                .iter()
                .all(|a| (a - 1.0).abs() < EPS),
            "fresh lock draws at full alpha: {:?}",
            alphas_at(&mut app, &cells)
        );
        // Age 30 == grace boundary: still fully visible.
        for _ in 0..FADE_GRACE_TICKS {
            frame(&mut app, &[]);
        }
        assert!(
            alphas_at(&mut app, &cells)
                .iter()
                .all(|a| (a - 1.0).abs() < EPS),
            "grace keeps locked cells fully visible: {:?}",
            alphas_at(&mut app, &cells)
        );
        // Age 45 == halfway through the fade: alpha 0.5.
        let half = (FADE_TOTAL_TICKS - FADE_GRACE_TICKS) / 2;
        assert_eq!(
            FADE_GRACE_TICKS + half,
            (FADE_GRACE_TICKS + FADE_TOTAL_TICKS) / 2
        );
        for _ in 0..half {
            frame(&mut app, &[]);
        }
        let mid_alphas = alphas_at(&mut app, &cells);
        assert!(
            mid_alphas.iter().all(|a| (a - lock_fade_alpha(
                (FADE_GRACE_TICKS + FADE_TOTAL_TICKS) / 2
            ))
            .abs()
                < EPS),
            "mid-fade alpha {mid_alphas:?}"
        );
        // Age >= FADE_TOTAL_TICKS: invisible, and stays invisible.
        let rest = (FADE_TOTAL_TICKS - FADE_GRACE_TICKS) - half;
        for _ in 0..=rest {
            frame(&mut app, &[]);
        }
        let floor = alphas_at(&mut app, &cells);
        assert!(
            floor.iter().all(|a| (*a - FADE_FLOOR_ALPHA).abs() < EPS),
            "locked cells at floor after {FADE_TOTAL_TICKS} ticks: {floor:?}"
        );
        for _ in 0..30 {
            frame(&mut app, &[]);
        }
        let floor = alphas_at(&mut app, &cells);
        assert!(
            floor.iter().all(|a| (*a - FADE_FLOOR_ALPHA).abs() < EPS),
            "faded cells stay invisible until cleared: {floor:?}"
        );
    }

    /// The active piece draws at full alpha and the ghost at
    /// [`GHOST_ALPHA`] even with INVISIBLE on — only locked cells fade.
    #[test]
    fn invisible_never_touches_active_piece_or_ghost() {
        let (mut app, _cells) = start_invisible_and_lock(0x024);
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        let piece = snapshot.active.expect("spawned piece").piece;
        let mut query = app.world_mut().query::<(&PlayfieldCell, &Sprite)>();
        let (mut active, mut ghost) = (0usize, 0usize);
        for (cell, sprite) in query.iter(app.world()) {
            match cell.kind {
                CellKind::Active => {
                    active += 1;
                    assert_eq!(sprite.color.to_srgba(), piece_color(piece).to_srgba());
                }
                CellKind::Ghost => {
                    ghost += 1;
                    assert_eq!(
                        sprite.color.to_srgba(),
                        piece_color(piece).with_alpha(GHOST_ALPHA).to_srgba()
                    );
                }
                CellKind::Board => {}
            }
        }
        assert_eq!(active, 4, "active piece still draws its four cells");
        assert!(ghost > 0, "ghost untouched by INVISIBLE (NO_GHOST off)");
    }

    /// Exact sprite color drawn at board position `(row, col)`.
    fn board_color(app: &mut App, row: usize, col: usize) -> Option<Color> {
        let layout = FieldLayout::fit_window(1280.0, 720.0);
        let center = layout.cell_center(row as i32, col);
        let mut query = app
            .world_mut()
            .query::<(&PlayfieldCell, &Sprite, &Transform)>();
        query
            .iter(app.world())
            .find(|(cell, _, t)| {
                cell.kind == CellKind::Board
                    && (t.translation.x - center.x).abs() < 1e-3
                    && (t.translation.y - center.y).abs() < 1e-3
            })
            .map(|(_, sprite, _)| sprite.color)
    }

    /// **Disabled** mutator ⇒ byte-for-byte today's colors: every settled
    /// cell keeps its exact palette color (alpha 1.0) forever — nothing ever
    /// fades on a clean run.
    #[test]
    fn disabled_invisible_draws_exactly_todays_colors() {
        let mut app = render_app(0x024);
        frame(&mut app, &[Action::HardDrop]);
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        let cells = locked_cells(&app);
        assert!(!cells.is_empty());

        // Right after the lock: the exact `piece_color` of each settled piece
        // (alpha 1.0) — the pre-T24 color, constant for constant.
        for &(row, col) in &cells {
            let piece = snapshot.board.get(row, col).expect("settled");
            assert_eq!(
                board_color(&mut app, row, col),
                Some(piece_color(piece)),
                "clean run draws today's exact palette color"
            );
        }

        // And at every layer, 200 frames later (>= 2 full fade windows):
        // still today's colors, nothing dimmed.
        for _ in 0..200 {
            frame(&mut app, &[]);
        }
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        for (row, col) in locked_cells(&app) {
            let piece = snapshot.board.get(row, col).expect("settled");
            assert_eq!(
                board_color(&mut app, row, col),
                Some(piece_color(piece)),
                "clean run never fades a locked cell ({row},{col})"
            );
        }
        let mut query = app.world_mut().query::<(&PlayfieldCell, &Sprite)>();
        for (cell, sprite) in query.iter(app.world()) {
            let alpha = sprite.color.to_srgba().alpha;
            let expect = if cell.kind == CellKind::Ghost {
                GHOST_ALPHA
            } else {
                1.0
            };
            assert!(
                (alpha - expect).abs() < EPS,
                "clean run {cell:?} alpha {alpha} != {expect}"
            );
        }
    }

    /// A locked cell that vanishes (run reset) and is later refilled at the
    /// same (col,row) is a **new** stamp: the fade restarts from full alpha
    /// (documented presence-continuity rule — no stale invisible ages).
    #[test]
    fn refill_after_clear_restart_the_fade() {
        let (mut app, cells) = start_invisible_and_lock(0x024);
        // Fade the first lock fully out.
        for _ in 0..=FADE_TOTAL_TICKS {
            frame(&mut app, &[]);
        }
        let floor = alphas_at(&mut app, &cells);
        assert!(
            floor.iter().all(|a| (*a - FADE_FLOOR_ALPHA).abs() < EPS),
            "precondition: fully faded: {floor:?}"
        );

        // New INVISIBLE run (same seed): the old stack vanishes first.
        let seed = app.world().non_send::<GameCore>().seed;
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.selected_mutators = Mutators::INVISIBLE;
            core.start_mode(seed, ModeId::Marathon);
        }
        frame(&mut app, &[]);
        assert!(
            locked_cells(&app).is_empty(),
            "fresh run starts with an empty board"
        );
        assert_eq!(
            app.world()
                .resource::<CellPool>()
                .lock_ages
                .iter()
                .flatten()
                .copied()
                .max()
                .expect("table"),
            NO_LOCK,
            "run reset unstamps every cell"
        );

        // Same scripted first lock lands in the same (col,row) cells.
        frame(&mut app, &[Action::HardDrop]);
        assert_eq!(
            locked_cells(&app),
            cells,
            "same seed + drop refills the same cells"
        );
        let fresh = alphas_at(&mut app, &cells);
        assert!(
            fresh.iter().all(|a| (*a - 1.0).abs() < EPS),
            "refilled cells are stamped fresh (fade restarts): {fresh:?}"
        );
    }

    /// A run that **starts** with an existing stack (Dig's buried garbage,
    /// the only start board) never starts faded: present cells are stamped
    /// as just-locked (documented choice — age 0, grace applies, then fade),
    /// never retro-faded to the floor.
    #[test]
    fn start_board_is_never_retro_faded() {
        let mut app = render_app(0x024);
        {
            let mut core = app.world_mut().non_send_mut::<GameCore>();
            core.selected_mutators = Mutators::INVISIBLE;
            core.start_mode(0x024, ModeId::Dig);
        }
        frame(&mut app, &[]);
        let cells = locked_cells(&app);
        assert!(
            cells.len() > 50,
            "Dig's buried-garbage start board has many settled cells (got {})",
            cells.len()
        );
        let fresh = alphas_at(&mut app, &cells);
        assert!(
            fresh.iter().all(|a| (*a - 1.0).abs() < EPS),
            "pre-existing stack starts fully visible, never retro-faded: {fresh:?}"
        );
        // ...and ages normally from there: fully faded after grace + fade.
        for _ in 0..FADE_TOTAL_TICKS {
            frame(&mut app, &[]);
        }
        let faded = alphas_at(&mut app, &cells);
        assert!(
            faded.iter().all(|a| (*a - FADE_FLOOR_ALPHA).abs() < EPS),
            "start-board cells fade like any lock: {faded:?}"
        );
    }
}
