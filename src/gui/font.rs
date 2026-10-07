//! Embedded GUI font and a bounded glyph atlas for Fira Code's programming
//! ligatures. Normal glyphs and Unicode fallback still use egui's font atlas.
mod shaping;

use ab_glyph::{Font as _, FontRef, Glyph, GlyphId, OutlinedGlyph, PxScaleFactor};
use egui::{
    Align2, Color32, ColorImage, FontData, FontDefinitions, FontFamily, FontId, Painter, Pos2,
    Rect, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};
use std::{collections::HashMap, sync::Arc};

pub(super) const FONT_NAME: &str = "FiraCode Nerd Font Mono";
pub(super) const FONT_BYTES: &[u8] =
    include_bytes!("../../assets/fonts/FiraCodeNerdFontMono-Regular.ttf");
pub(super) const FONT_LICENSE: &str = include_str!("../../assets/fonts/FiraCode-LICENSE.txt");
const MAX_SHAPED_RUNS: usize = 1024;
const ATLAS_SIDE: usize = 1024;

pub(super) fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        FONT_NAME.into(),
        Arc::new(FontData::from_static(FONT_BYTES)),
    );
    for family in [FontFamily::Monospace, FontFamily::Proportional] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, FONT_NAME.into());
    }
    // Retain egui's bundled fallbacks for characters outside Fira's repertoire.
    ctx.set_fonts(fonts);
}

pub(super) struct FontRenderer {
    font: FontRef<'static>,
    calt: shaping::Calt,
    runs: HashMap<String, Arc<Vec<ttf_parser::GlyphId>>>,
    atlas: Option<Atlas>,
}

impl Default for FontRenderer {
    fn default() -> Self {
        Self {
            font: FontRef::try_from_slice(FONT_BYTES).expect("embedded Fira Code"),
            calt: shaping::Calt::new(FONT_BYTES),
            runs: HashMap::new(),
            atlas: None,
        }
    }
}

impl FontRenderer {
    /// `at` is the left-center of the first cell. Only printable ASCII enters
    /// this path; tabs have already been expanded by the shared canvas.
    pub(super) fn paint(
        &mut self,
        painter: &Painter,
        text: &str,
        at: Pos2,
        font: &FontId,
        cell_width: f32,
        color: Color32,
    ) {
        let glyphs = if let Some(cached) = self.runs.get(text) {
            cached.clone()
        } else {
            if self.runs.len() >= MAX_SHAPED_RUNS {
                self.runs.clear();
            }
            let glyphs = Arc::new(self.calt.shape(text));
            self.runs.insert(text.into(), glyphs.clone());
            glyphs
        };
        let pixels_per_point = painter.ctx().pixels_per_point();
        let scale = font.size * pixels_per_point / self.font.units_per_em().unwrap();
        let key = (font.size.to_bits(), pixels_per_point.to_bits());
        if self.atlas.as_ref().is_some_and(|a| a.scale_key != key) {
            self.atlas = None;
        }
        let changed = |i: usize, c: u8| glyphs[i].0 != self.font.glyph_id(char::from(c)).0;
        let has_ligature = text.bytes().enumerate().any(|(i, c)| changed(i, c));
        let baseline = if has_ligature {
            let galley = painter.layout_no_wrap("M".into(), font.clone(), color);
            let row = &galley.rows[0];
            at.y - galley.size().y / 2.0 + row.pos.y + row.glyphs[0].pos.y
        } else {
            at.y
        };
        // Allocate every changed glyph before painting. If this bounded atlas
        // fills at an extreme DPI, use ordinary glyphs for the entire run.
        let mut shaped = has_ligature;
        if shaped {
            let atlas = self
                .atlas
                .get_or_insert_with(|| Atlas::new(painter.ctx(), key));
            for (i, c) in text.bytes().enumerate() {
                if changed(i, c) {
                    let x = (at.x + i as f32 * cell_width) * pixels_per_point;
                    let (_, phase) = subpixel(x);
                    if atlas
                        .glyph(&self.font, GlyphId(glyphs[i].0), phase, scale)
                        .is_none()
                    {
                        shaped = false;
                        break;
                    }
                }
            }
        }
        for (i, c) in text.bytes().enumerate() {
            let cell_at = at + vec2(i as f32 * cell_width, 0.0);
            if shaped && changed(i, c) {
                let atlas = self.atlas.as_ref().unwrap();
                let (x, phase) = subpixel(cell_at.x * pixels_per_point);
                if let Some(glyph) = atlas.glyphs[&(glyphs[i].0, phase)] {
                    let pos = (vec2(x, (baseline * pixels_per_point).round()) + glyph.offset)
                        / pixels_per_point;
                    painter.image(
                        atlas.texture.id(),
                        Rect::from_min_size(pos.to_pos2(), glyph.size / pixels_per_point),
                        glyph.uv,
                        color,
                    );
                }
            } else if c != b' ' {
                painter.text(
                    cell_at,
                    Align2::LEFT_CENTER,
                    char::from(c),
                    font.clone(),
                    color,
                );
            }
        }
    }
}

fn subpixel(x: f32) -> (f32, u8) {
    let quarter = (x * 4.0).round();
    ((quarter / 4.0).floor(), quarter.rem_euclid(4.0) as u8)
}

#[derive(Clone, Copy)]
struct RasterGlyph {
    uv: Rect,
    offset: Vec2,
    size: Vec2,
}

struct Atlas {
    texture: TextureHandle,
    scale_key: (u32, u32),
    side: usize,
    cursor: [usize; 2],
    row_height: usize,
    glyphs: HashMap<(u16, u8), Option<RasterGlyph>>,
}

impl Atlas {
    fn new(ctx: &egui::Context, scale_key: (u32, u32)) -> Self {
        let side = ATLAS_SIDE.min(ctx.input(|i| i.max_texture_side));
        Self {
            texture: ctx.load_texture(
                "Fira Code ligatures",
                ColorImage::filled([side, side], Color32::TRANSPARENT),
                TextureOptions::LINEAR,
            ),
            scale_key,
            side,
            cursor: [1, 1],
            row_height: 0,
            glyphs: HashMap::new(),
        }
    }

    /// Outer None means full, inner None means an intentionally blank glyph.
    fn glyph(
        &mut self,
        font: &FontRef<'_>,
        id: GlyphId,
        phase: u8,
        scale: f32,
    ) -> Option<Option<RasterGlyph>> {
        if let Some(cached) = self.glyphs.get(&(id.0, phase)) {
            return Some(*cached);
        }
        let Some(outline) = font.outline(id) else {
            self.glyphs.insert((id.0, phase), None);
            return Some(None);
        };
        let glyph = OutlinedGlyph::new(
            Glyph {
                id,
                scale: 0.0.into(),
                position: ab_glyph::point(f32::from(phase) / 4.0, 0.0),
            },
            outline,
            PxScaleFactor {
                horizontal: scale,
                vertical: scale,
            },
        );
        let bounds = glyph.px_bounds();
        let size = [bounds.width() as usize, bounds.height() as usize];
        if size[0] + 2 > self.side || size[1] + 2 > self.side {
            return None;
        }
        let mut cursor = self.cursor;
        let mut row_height = self.row_height;
        if cursor[0] + size[0] + 1 > self.side {
            cursor = [1, cursor[1] + row_height + 1];
            row_height = 0;
        }
        if cursor[1] + size[1] + 1 > self.side {
            return None;
        }
        let mut image = ColorImage::filled(size, Color32::TRANSPARENT);
        let alpha = egui::epaint::AlphaFromCoverage::default();
        glyph.draw(|x, y, coverage| {
            image[(x as usize, y as usize)] = alpha.color_from_coverage(coverage);
        });
        self.texture
            .set_partial(cursor, image, TextureOptions::LINEAR);
        let side = self.side as f32;
        let result = RasterGlyph {
            uv: Rect::from_min_max(
                pos2(cursor[0] as f32 / side, cursor[1] as f32 / side),
                pos2(
                    (cursor[0] + size[0]) as f32 / side,
                    (cursor[1] + size[1]) as f32 / side,
                ),
            ),
            offset: vec2(bounds.min.x, bounds.min.y),
            size: vec2(size[0] as f32, size[1] as f32),
        };
        self.cursor = [cursor[0] + size[0] + 1, cursor[1]];
        self.row_height = row_height.max(size[1]);
        self.glyphs.insert((id.0, phase), Some(result));
        Some(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_font_is_default_and_contains_nerd_symbols() {
        let face = ttf_parser::Face::parse(FONT_BYTES, 0).unwrap();
        for ch in ['M', 'é', '\u{e0b0}', '\u{f120}', '\u{f07c}', '\u{f0001}'] {
            assert!(face.glyph_index(ch).is_some(), "missing {ch}");
        }
        let ctx = egui::Context::default();
        install(&ctx);
        let _ = ctx.run(Default::default(), |ctx| {
            ctx.fonts_mut(|fonts| {
                for family in [FontFamily::Monospace, FontFamily::Proportional] {
                    let font = FontId::new(15.0, family);
                    let width = fonts.glyph_width(&font, 'M');
                    let expected = f32::from(
                        face.glyph_hor_advance(face.glyph_index('M').unwrap())
                            .unwrap(),
                    ) * 15.0
                        / f32::from(face.units_per_em());
                    assert!((width - expected).abs() < 0.01);
                    assert!(fonts.has_glyph(&font, '\u{f120}'));
                }
            });
        });
    }

    #[test]
    fn atlas_reuses_glyphs_and_rebuilds_for_zoom_and_dpi() {
        let ctx = egui::Context::default();
        install(&ctx);
        let mut renderer = FontRenderer::default();
        let mut previous_texture = None;
        for (size, dpi) in [(15.0, 1.0), (22.0, 1.0), (22.0, 2.0)] {
            ctx.set_pixels_per_point(dpi);
            let output = ctx.run(Default::default(), |ctx| {
                let painter = ctx.layer_painter(egui::LayerId::background());
                let font = FontId::monospace(size);
                let width = ctx.fonts_mut(|f| f.glyph_width(&font, 'M'));
                renderer.paint(
                    &painter,
                    "a != b -> c",
                    pos2(10.0, 30.0),
                    &font,
                    width,
                    Color32::WHITE,
                );
                let count = renderer.atlas.as_ref().unwrap().glyphs.len();
                renderer.paint(
                    &painter,
                    "a != b -> c",
                    pos2(10.0, 60.0),
                    &font,
                    width,
                    Color32::LIGHT_GREEN,
                );
                assert_eq!(renderer.atlas.as_ref().unwrap().glyphs.len(), count);
            });
            let atlas = renderer.atlas.as_ref().unwrap();
            assert!(!atlas.glyphs.is_empty());
            assert_ne!(previous_texture, Some(atlas.texture.id()));
            previous_texture = Some(atlas.texture.id());
            assert!(
                output
                    .textures_delta
                    .set
                    .iter()
                    .any(|(id, delta)| *id == atlas.texture.id() && delta.pos.is_some())
            );
            assert!(
                !ctx.tessellate(output.shapes, output.pixels_per_point)
                    .is_empty()
            );
        }
    }
}
