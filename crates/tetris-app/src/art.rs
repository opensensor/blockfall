//! Procedural texture generation for the modernized default look (M5).
//!
//! All game art is generated in code at startup — zero asset files, identical
//! on desktop and Android. The textures are white (or neutral) with baked
//! **shading and alpha**, so the existing tint-by-`Sprite::color` pipeline
//! colors them: piece palettes stay the single source of truth and the pool
//! in [`crate::render`] keeps batching everything into one sprite pass.
//!
//! - [`tile_image`] — beveled block: top highlight, bottom/right shade, soft
//!   rounded corners and a transparent gutter around the cell, which creates
//!   the visual gap between adjacent pieces *without* shrinking the sprite
//!   (cell geometry stays exactly one grid cell — see the `custom_size`
//!   contracts in T11's render tests).
//! - [`ghost_image`] — outlined rounded square with a transparent center, so
//!   the landing preview reads as a preview instead of a dimmed solid blob.
//! - [`well_image`] — the playfield backdrop: dark panel + faint cell grid,
//!   sized exactly `COLS × VISIBLE_ROWS` cells (stretched to the layout, so
//!   grid lines always land on cell borders).
//! - [`vignette_image`] — radial edge darkening stretched over the window;
//!   gives the pillar-box voids depth behind the field.
//!
//! Everything is straight (non-premultiplied) RGBA8 sRGB; transparent pixels
//! carry white RGB so bilinear filtering never fringes the edges.

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

/// Edge length of the generated tile/ghost textures in pixels.
const TILE_PX: u32 = 64;

/// Transparent gutter around the tile content, as a fraction of the texture
/// edge. Creates the visible gap between neighbouring cells.
const TILE_GUTTER: f32 = 0.045;

/// Corner radius of tile/ghost shapes, as a fraction of the content edge.
const CORNER_RADIUS: f32 = 0.14;

/// Ring thickness of the ghost outline, as a fraction of the content edge.
const GHOST_RING: f32 = 0.13;

/// Pixels per cell of the generated well texture.
const WELL_CELL_PX: u32 = 64;

/// Pixel edge of the generated vignette texture.
const VIGNETTE_PX: u32 = 256;

/// Long edge of the generated CANNONS barrel texture (128×64, faces right).
const CANNON_PX: u32 = 128;

/// Pixel edge of the generated CANNONS cannonball texture.
const BALL_PX: u32 = 64;

/// Generated art handles shared by the render, HUD and juice systems.
/// Inserted by [`init_art_assets`] in `Startup`; consumers take
/// `Option<Res<ArtAssets>>` so headless test apps (no `Startup`) keep the
/// flat-color fallback.
#[derive(Resource)]
pub struct ArtAssets {
    /// Beveled block tile, tinted by the piece color.
    pub tile: Handle<Image>,
    /// Outlined ghost square, tinted by the (dimmed) piece color.
    pub ghost: Handle<Image>,
    /// Dark well panel with the cell grid, stretched to the field.
    pub well: Handle<Image>,
    /// Radial vignette, stretched over the window.
    pub vignette: Handle<Image>,
    /// CANNONS: side-battery barrel facing **right**; the left battery
    /// mirrors it via `Sprite::flip_x`. Luminance texture, tinted iron.
    pub cannon: Handle<Image>,
    /// CANNONS: cannonball (shaded iron sphere). Tinted near-black.
    pub ball: Handle<Image>,
}

/// Startup system: generate every procedural texture and publish them.
/// Headless test apps without `Assets<Image>` (MinimalPlugins) are skipped
/// and keep the flat-color render path.
pub fn init_art_assets(mut commands: Commands, images: Option<ResMut<Assets<Image>>>) {
    let Some(mut images) = images else { return };
    commands.insert_resource(ArtAssets {
        tile: images.add(tile_image()),
        ghost: images.add(ghost_image()),
        well: images.add(well_image()),
        vignette: images.add(vignette_image()),
        cannon: images.add(cannon_image()),
        ball: images.add(ball_image()),
    });
}

fn image(data: Vec<u8>, width: u32, height: u32) -> Image {
    Image::new(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    )
}

/// Coverage of a rounded rectangle in normalized coords (`p` in `0..=1`),
/// supersampled 4×4 per pixel for smooth edges.
fn round_rect_coverage(px: f32, py: f32, size: f32, radius: f32) -> f32 {
    let r = radius.min(size * 0.5);
    // Clamping the point to the [r, size-r] core gives the corner circle
    // center; inside the straight bands the distance is zero.
    let inside = |x: f32, y: f32| {
        let cx = x.clamp(r, size - r);
        let cy = y.clamp(r, size - r);
        let dx = x - cx;
        let dy = y - cy;
        dx * dx + dy * dy <= r * r
    };
    let mut hits = 0u32;
    for sy in 0..4 {
        for sx in 0..4 {
            // Quarter-pixel samples across this texel.
            let x = px + (sx as f32 + 0.5) / 4.0;
            let y = py + (sy as f32 + 0.5) / 4.0;
            if inside(x, y) {
                hits += 1;
            }
        }
    }
    hits as f32 / 16.0
}

/// Brightness of a beveled tile at content coords `u, v` in `0..=1`
/// (v: 0 = top edge). Top-lit: highlight band up, shade bands bottom/right,
/// plus a soft vertical gradient. Peaks at 1.0 — the piece tint multiplies
/// it down from there.
fn tile_brightness(u: f32, v: f32) -> f32 {
    // Soft base gradient, brightest just below the highlight.
    let mut b = 0.97 - 0.16 * v;

    // Left edge catch-light, right edge shade.
    let left = (u / 0.10).min(1.0);
    b = b.lerp(0.93, 1.0 - left);
    let right = ((1.0 - u) / 0.10).min(1.0);
    b = b.lerp(0.80, 1.0 - right);

    // Top highlight band and bottom shade band (they dominate the corners).
    if v < 0.16 {
        let t = (v / 0.16).smoothstep01();
        b = b.lerp(1.0, 1.0 - t);
    } else if v > 0.84 {
        let t = ((v - 0.84) / 0.16).smoothstep01();
        b = b.lerp(0.66, 1.0 - t);
    }
    b
}

trait SmoothStep01 {
    fn smoothstep01(self) -> Self;
}
impl SmoothStep01 for f32 {
    fn smoothstep01(self) -> Self {
        let t = self.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }
}

/// Push an RGBA texel: luminance `lum` as neutral gray, straight alpha `a`.
/// Transparent texels keep white RGB to avoid filter fringes.
fn put_white(data: &mut Vec<u8>, a: f32, lum: f32) {
    let v = (lum.clamp(0.0, 1.0) * 255.0) as u8;
    let v = if a <= 0.002 { 255 } else { v };
    data.extend_from_slice(&[v, v, v, (a.clamp(0.0, 1.0) * 255.0) as u8]);
}

/// 64×64 beveled block: rounded corners, transparent gutter, top-lit shade.
pub fn tile_image() -> Image {
    let n = TILE_PX;
    let gutter = n as f32 * TILE_GUTTER;
    let content = n as f32 - 2.0 * gutter;
    let radius = content * CORNER_RADIUS;

    let mut data = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let cov = round_rect_coverage(x as f32 - gutter, y as f32 - gutter, content, radius);
            if cov <= 0.0 {
                put_white(&mut data, 0.0, 1.0);
                continue;
            }
            let u = ((x as f32 - gutter) / content).clamp(0.0, 1.0);
            let v = ((y as f32 - gutter) / content).clamp(0.0, 1.0);
            put_white(&mut data, cov, tile_brightness(u, v));
        }
    }
    image(data, n, n)
}

/// 64×64 ghost preview: rounded outline ring, transparent center and gutter.
pub fn ghost_image() -> Image {
    let n = TILE_PX;
    let gutter = n as f32 * TILE_GUTTER;
    let content = n as f32 - 2.0 * gutter;
    let radius = content * CORNER_RADIUS;
    let ring = content * GHOST_RING;
    let inner = content - 2.0 * ring;
    let inner_radius = (radius - ring).max(1.0);

    let mut data = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let outer = round_rect_coverage(x as f32 - gutter, y as f32 - gutter, content, radius);
            let hole = round_rect_coverage(
                x as f32 - gutter - ring,
                y as f32 - gutter - ring,
                inner,
                inner_radius,
            );
            put_white(&mut data, (outer - hole).max(0.0), 1.0);
        }
    }
    image(data, n, n)
}

/// Well backdrop: dark panel with a faint `COLS × VISIBLE_ROWS` cell grid.
/// Rendered at [`WELL_CELL_PX`] pixels per cell and stretched to the field,
/// so grid lines always fall on cell borders.
pub fn well_image() -> Image {
    use tetris_core::board::{COLS, ROWS};
    const HEADROOM: u32 = 2; // must match render::DRAWN_TOP headroom
    let w = COLS as u32 * WELL_CELL_PX;
    let h = (ROWS as u32 + HEADROOM) * WELL_CELL_PX;

    // Panel: deep blue-charcoal, near-opaque.
    let (pr, pg, pb, pa) = (18u8, 20u8, 28u8, 232u8);
    let mut data = vec![0u8; (w * h * 4) as usize];
    for chunk in data.chunks_exact_mut(4) {
        chunk[0] = pr;
        chunk[1] = pg;
        chunk[2] = pb;
        chunk[3] = pa;
    }

    // Faint grid: light the single pixel line on each cell border.
    let line = |x: u32, y: u32, data: &mut Vec<u8>| {
        let i = ((y * w + x) as usize) * 4;
        data[i] = 190;
        data[i + 1] = 200;
        data[i + 2] = 225;
        data[i + 3] = 26; // ~10% over the panel
    };
    for col in 1..COLS as u32 {
        let x = col * WELL_CELL_PX;
        for y in 0..h {
            line(x, y, &mut data);
        }
    }
    for row in 1..(ROWS as u32 + HEADROOM) {
        let y = row * WELL_CELL_PX;
        for x in 0..w {
            line(x, y, &mut data);
        }
    }
    image(data, w, h)
}

/// Radial vignette: transparent center fading to dark edges.
pub fn vignette_image() -> Image {
    let n = VIGNETTE_PX;
    let mut data = Vec::with_capacity((n * n * 4) as usize);
    let c = (n - 1) as f32 / 2.0;
    for y in 0..n {
        for x in 0..n {
            let dx = (x as f32 - c) / c;
            let dy = (y as f32 - c) / c;
            let d = (dx * dx + dy * dy).sqrt();
            let a = ((d - 0.42) / 0.62).smoothstep01() * 0.5;
            // Near-black blue; tinted by the sprite color if ever re-hued.
            data.push(6);
            data.push(7);
            data.push(12);
            data.push((a * 255.0) as u8);
        }
    }
    image(data, n, n)
}

/// CANNONS barrel (128×64, muzzle facing **right**): an iron tube with a
/// top-lit highlight, a banded muzzle ring, a dark bore, and a trunnion
/// knob on the bore axis. Luminance texture tinted by the barrel color.
pub fn cannon_image() -> Image {
    let w = CANNON_PX;
    let h = CANNON_PX / 2;
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            // Normalized coords, u: 0 (breech) → 1 (muzzle), v: 0 top.
            let u = x as f32 / w as f32;
            let v = y as f32 / h as f32;
            // Tube: rounded rectangle spanning the barrel band.
            let tube = round_rect_norm(u, v, 0.06, 0.94, 0.26, 0.74, 0.10);
            // Muzzle ring (wider collar) near the bore end.
            let ring = round_rect_norm(u, v, 0.78, 0.90, 0.20, 0.80, 0.06);
            // Bore (dark muzzle opening) at the far right.
            let bore_d = ((u - 0.965) / 0.05).powi(2) + ((v - 0.5) / 0.16).powi(2);
            let bore = ((1.0 - bore_d).max(0.0)).smoothstep01();
            // Trunnion knob on the axis at the breech end (pivots the barrel).
            let knob_d = ((u - 0.16) / 0.10).powi(2) + ((v - 0.5) / 0.22).powi(2);
            let knob = ((1.0 - knob_d).max(0.0)).smoothstep01();

            let cover = (tube + ring + knob).clamp(0.0, 1.0);
            if cover <= 0.0 {
                put_white(&mut data, 0.0, 1.0);
                continue;
            }
            // Top-lit tube shading, brightened by the ring, darkened by the bore.
            let mut b = 0.55 - 0.80 * (v - 0.30).abs();
            b = b.clamp(0.28, 0.92);
            b += 0.18 * ring;
            b *= 1.0 - 0.85 * bore;
            put_white(&mut data, cover, b.clamp(0.0, 1.0));
        }
    }
    image(data, w, h)
}

/// CANNONS cannonball (64×64): a top-lit iron sphere with a specular
/// catch-light upper-left. Luminance texture tinted near-black by the render.
pub fn ball_image() -> Image {
    let n = BALL_PX;
    let mut data = Vec::with_capacity((n * n * 4) as usize);
    let c = n as f32 / 2.0;
    for y in 0..n {
        for x in 0..n {
            // Sphere disc with a soft ~1px edge.
            let dx = x as f32 + 0.5 - c;
            let dy = y as f32 + 0.5 - c;
            let r = (dx * dx + dy * dy).sqrt() / c;
            let cover = (1.0 - (r - 0.86) / 0.14).clamp(0.0, 1.0).smoothstep01();
            if cover <= 0.0 {
                put_white(&mut data, 0.0, 1.0);
                continue;
            }
            // Lambert-ish top-left lighting across the lit hemisphere…
            let nx = dx / c;
            let ny = -(dy / c);
            let ndotl = (0.58 * nx + 0.80 * ny).clamp(0.0, 1.0);
            let mut b = 0.30 + 0.55 * ndotl;
            // Tight specular catch-light.
            let sx = nx + 0.42;
            let sy = ny - 0.42;
            let spec = (1.0 - (sx * sx + sy * sy) / 0.10).max(0.0);
            b += 0.45 * spec;
            put_white(&mut data, cover, b.clamp(0.0, 1.0));
        }
    }
    image(data, n, n)
}

/// Coverage of an axis-aligned rounded rectangle in normalized `u/v` space
/// (`[u0,u1] × [v0,v1]`, corner radius `radius` in u units), supersampled
/// 4×4 per texel for smooth edges (radius clamped to half the band height).
fn round_rect_norm(u: f32, v: f32, u0: f32, u1: f32, v0: f32, v1: f32, radius: f32) -> f32 {
    let rx = radius.min((u1 - u0) * 0.5);
    let ry = radius.min((v1 - v0) * 0.5);
    let du = 1.0 / CANNON_PX as f32;
    let dv = 2.0 / CANNON_PX as f32;
    let inside = |u: f32, v: f32| {
        let cx = u.clamp(u0 + rx, u1 - rx);
        let cy = v.clamp(v0 + ry, v1 - ry);
        let dx = (u - cx) / rx;
        let dy = (v - cy) / ry;
        dx * dx + dy * dy <= 1.0
    };
    let mut hits = 0u32;
    for sv in 0..4 {
        for su in 0..4 {
            let x = u + (su as f32 + 0.5) / 4.0 * du;
            let y = v + (sv as f32 + 0.5) / 4.0 * dv;
            if inside(x, y) {
                hits += 1;
            }
        }
    }
    hits as f32 / 16.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(img: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width() + x) as usize) * 4;
        let d = img.data.as_ref().expect("texture data");
        [d[i], d[i + 1], d[i + 2], d[i + 3]]
    }

    #[test]
    fn tile_is_opaque_in_the_middle_with_shaded_edges() {
        let tile = tile_image();
        assert_eq!(tile.width(), TILE_PX);
        assert_eq!(tile.height(), TILE_PX);
        let mid = sample(&tile, TILE_PX / 2, TILE_PX / 2);
        assert_eq!(mid[3], 255, "tile center fully covered");
        let top = sample(&tile, TILE_PX / 2, 6);
        let bottom = sample(&tile, TILE_PX / 2, TILE_PX - 7);
        assert!(top[0] > mid[0], "top highlight above base: {top:?} {mid:?}");
        assert!(
            bottom[0] < mid[0],
            "bottom shade below base: {bottom:?} {mid:?}"
        );
    }

    #[test]
    fn tile_has_transparent_gutter_and_corners() {
        let tile = tile_image();
        assert_eq!(sample(&tile, 0, 0)[3], 0, "corner rounded away");
        assert_eq!(
            sample(&tile, TILE_PX / 2, 0)[3],
            0,
            "gutter keeps a cell gap"
        );
    }

    #[test]
    fn ghost_is_a_ring_with_a_clear_center() {
        let ghost = ghost_image();
        assert_eq!(
            sample(&ghost, TILE_PX / 2, TILE_PX / 2)[3],
            0,
            "ghost center transparent"
        );
        assert!(
            sample(&ghost, TILE_PX / 2, 5)[3] > 250,
            "ghost ring opaque at the top edge"
        );
    }

    #[test]
    fn well_is_dark_with_grid_lines_on_cell_borders() {
        let well = well_image();
        use tetris_core::board::COLS;
        let inside = sample(&well, WELL_CELL_PX / 2, WELL_CELL_PX / 2);
        assert!(inside[0] < 40 && inside[2] < 60, "panel dark: {inside:?}");
        let line = sample(&well, COLS as u32 / 2 * WELL_CELL_PX, WELL_CELL_PX / 2);
        assert!(line[0] > 150, "grid line lit at a cell border: {line:?}");
    }

    #[test]
    fn vignette_clears_in_the_center_darkens_to_the_edge() {
        let vignette = vignette_image();
        let c = VIGNETTE_PX / 2;
        assert_eq!(sample(&vignette, c, c)[3], 0, "center clear");
        assert!(
            sample(&vignette, 0, 0)[3] > sample(&vignette, c, c)[3],
            "corners darker"
        );
    }
}
