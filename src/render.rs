use crate::{
    metrics::Snapshot,
    session::{AgentSession, AgentSnapshot, OpenState, ViewMode, ViewState, WorkState},
};
use ab_glyph::{Font, FontArc, FontRef, PxScale, point};
use anyhow::{Context, Result};
use image::{RgbaImage, imageops};
use std::{collections::HashMap, path::Path};
use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

const WHITE: [u8; 3] = [230, 235, 245];
const MUTED: [u8; 3] = [140, 148, 168];
const LABEL: [u8; 3] = [100, 108, 130];
const DIM: [u8; 3] = [70, 76, 95];
const PANEL: [u8; 3] = [26, 30, 43];
const BORDER: [u8; 3] = [38, 42, 58];
const BAR: [u8; 3] = [38, 43, 59];
const GREEN: [u8; 3] = [52, 211, 153];
const BLUE: [u8; 3] = [66, 133, 244];
const ORANGE: [u8; 3] = [251, 191, 36];
const CYAN: [u8; 3] = [34, 211, 238];
const PURPLE: [u8; 3] = [167, 139, 250];
const RED: [u8; 3] = [239, 68, 68];
const GLYPH_CACHE_BYTES: usize = 8 * 1024 * 1024;
const GLYPH_CACHE_ENTRIES: usize = 4096;

type GlyphKey = (char, u16, usize);

struct RasterGlyph {
    advance: f32,
    left: i32,
    top: i32,
    width: usize,
    height: usize,
    bitmap: Vec<u8>,
}
impl RasterGlyph {
    fn new(font: &FontArc, ch: char, size: u16) -> Self {
        let id = font.glyph_id(ch);
        let mut glyph = Self {
            advance: advance(font, ch, size),
            left: 0,
            top: 0,
            width: 0,
            height: 0,
            bitmap: Vec::new(),
        };
        if let Some(outline) =
            font.outline_glyph(id.with_scale_and_position(font_scale(font, size), point(0.0, 0.0)))
        {
            let bounds = outline.px_bounds();
            glyph.left = bounds.min.x as i32;
            glyph.top = bounds.min.y as i32;
            glyph.width = bounds.width() as usize;
            glyph.height = bounds.height() as usize;
            glyph.bitmap = vec![0; glyph.width * glyph.height];
            outline.draw(|x, y, coverage| {
                glyph.bitmap[y as usize * glyph.width + x as usize] =
                    (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
            });
        }
        glyph
    }
}

// ab_glyph scales by ascent minus descent; dashboard sizes are pixels per em.
fn font_scale(font: &FontArc, size: u16) -> PxScale {
    PxScale::from(size as f32 * font.height_unscaled() / font.units_per_em().unwrap())
}
fn advance(font: &FontArc, ch: char, size: u16) -> f32 {
    font.h_advance_unscaled(font.glyph_id(ch)) * size as f32 / font.units_per_em().unwrap()
}

struct CachedGlyph {
    glyph: RasterGlyph,
    last_used: u64,
}

fn load_font(path: &Path) -> Result<FontArc> {
    let bytes = crate::fonts::bytes(path).with_context(|| {
        format!(
            "Cannot read font {}; configure a Chinese-capable TTF/TTC via settings.font",
            path.display()
        )
    })?;
    let font = FontRef::try_from_slice_and_index(bytes, 0)
        .with_context(|| format!("Parsing font {}", path.display()))?;
    anyhow::ensure!(
        font.units_per_em().is_some(),
        "Font has no em size: {}",
        path.display()
    );
    Ok(FontArc::new(font))
}

fn load_fonts(main: &Path, auxiliary_directory: Option<&Path>) -> Result<Vec<FontArc>> {
    // Main/default and explicitly configured fonts must remain valid. Optional
    // weight/Latin fonts can fail independently without stopping LCD output.
    let regular = load_font(main)?;
    let mut fonts = vec![regular.clone()];
    for name in ["msyhbd.ttc", "segoeui.ttf", "segoeuib.ttf"] {
        let font = auxiliary_directory
            .and_then(|directory| load_font(&directory.join(name)).ok())
            .unwrap_or_else(|| regular.clone());
        fonts.push(font);
    }
    Ok(fonts)
}
struct GlyphCache {
    entries: HashMap<GlyphKey, CachedGlyph>,
    bytes: usize,
    budget: usize,
    clock: u64,
    uncached: Option<RasterGlyph>,
}
impl GlyphCache {
    fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            budget,
            clock: 0,
            uncached: None,
        }
    }
    fn cost(glyph: &RasterGlyph) -> usize {
        glyph.bitmap.capacity() + std::mem::size_of::<(GlyphKey, CachedGlyph)>()
    }
    fn get_or_insert_with(
        &mut self,
        key: GlyphKey,
        load: impl FnOnce() -> RasterGlyph,
    ) -> &RasterGlyph {
        self.clock += 1;
        self.uncached = None;
        if !self.entries.contains_key(&key) {
            let glyph = load();
            let cost = Self::cost(&glyph);
            // A single unusually large glyph must not exceed the cache budget.
            if cost > self.budget {
                self.uncached = Some(glyph);
                return self.uncached.as_ref().unwrap();
            }
            while self.bytes + cost > self.budget || self.entries.len() >= GLYPH_CACHE_ENTRIES {
                // Fixed labels stay recent through normal rendering; retaining
                // stale ASCII keys would displace frequently reused CJK text.
                let oldest = *self
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_used)
                    .unwrap()
                    .0;
                let removed = self.entries.remove(&oldest).unwrap();
                self.bytes -= Self::cost(&removed.glyph);
            }
            self.bytes += cost;
            self.entries.insert(
                key,
                CachedGlyph {
                    glyph,
                    last_used: self.clock,
                },
            );
        }
        let entry = self.entries.get_mut(&key).unwrap();
        entry.last_used = self.clock;
        &entry.glyph
    }
}
fn color(pct: f32) -> [u8; 3] {
    if pct < 50.0 {
        GREEN
    } else if pct < 75.0 {
        ORANGE
    } else {
        RED
    }
}

fn blend(bg: [u8; 3], fg: [u8; 3], alpha: f32) -> [u8; 3] {
    std::array::from_fn(|i| (bg[i] as f32 * (1.0 - alpha) + fg[i] as f32 * alpha) as u8)
}

pub struct Renderer {
    canvas: Pixmap,
    fonts: Vec<FontArc>,
    glyphs: GlyphCache,
    cat: RgbaImage,
    pika: RgbaImage,
}
impl Renderer {
    pub fn new(font_path: Option<&Path>) -> Result<Self> {
        let font_dir = crate::fonts::directory();
        let default_path = crate::fonts::default_path();
        let path = font_path.unwrap_or(&default_path);
        // Default Latin and Chinese fonts have real regular/bold weights. An
        // explicit custom font applies to both scripts and takes precedence.
        let fonts = load_fonts(path, font_path.is_none().then_some(font_dir.as_path()))?;
        Ok(Self {
            canvas: Pixmap::new(1920, 480).context("Allocating dashboard")?,
            fonts,
            glyphs: GlyphCache::new(GLYPH_CACHE_BYTES),
            cat: imageops::resize(
                &image::load_from_memory(include_bytes!("../assets/BongoCat.png"))?.to_rgba8(),
                148,
                75,
                imageops::FilterType::Triangle,
            ),
            pika: imageops::resize(
                &image::load_from_memory(include_bytes!("../assets/Pikachu.png"))?.to_rgba8(),
                132,
                132,
                imageops::FilterType::Triangle,
            ),
        })
    }
    fn rect(&mut self, bounds: [f32; 4], c: [u8; 3]) {
        let [x, y, w, h] = bounds;
        let mut paint = Paint::default();
        paint.set_color_rgba8(c[0], c[1], c[2], 255);
        if let Some(rect) = Rect::from_xywh(x, y, w, h) {
            self.canvas
                .fill_rect(rect, &paint, Transform::identity(), None);
        }
    }
    fn rounded(&mut self, bounds: [f32; 4], radius: f32, c: [u8; 3]) {
        let [x, y, w, h] = bounds;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let r = radius.min(w / 2.0).min(h / 2.0);
        let mut p = PathBuilder::new();
        p.move_to(x + r, y);
        p.line_to(x + w - r, y);
        p.quad_to(x + w, y, x + w, y + r);
        p.line_to(x + w, y + h - r);
        p.quad_to(x + w, y + h, x + w - r, y + h);
        p.line_to(x + r, y + h);
        p.quad_to(x, y + h, x, y + h - r);
        p.line_to(x, y + r);
        p.quad_to(x, y, x + r, y);
        p.close();
        let mut paint = Paint::default();
        paint.set_color_rgba8(c[0], c[1], c[2], 255);
        if let Some(path) = p.finish() {
            self.canvas.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
    fn line(&mut self, points: &[(f32, f32)], c: [u8; 3], width: f32) {
        let Some(&(x, y)) = points.first() else {
            return;
        };
        let mut p = PathBuilder::new();
        p.move_to(x, y);
        for &(x, y) in &points[1..] {
            p.line_to(x, y);
        }
        let mut paint = Paint::default();
        paint.set_color_rgba8(c[0], c[1], c[2], 255);
        if let Some(path) = p.finish() {
            self.canvas.stroke_path(
                &path,
                &paint,
                &Stroke {
                    width,
                    line_cap: LineCap::Round,
                    ..Stroke::default()
                },
                Transform::identity(),
                None,
            );
        }
    }
    fn font_index(ch: char, bold: bool) -> usize {
        usize::from(ch.is_ascii()) * 2 + usize::from(bold)
    }
    fn measure(&self, s: &str, size: u16, bold: bool) -> f32 {
        s.chars()
            .map(|ch| advance(&self.fonts[Self::font_index(ch, bold)], ch, size))
            .sum()
    }
    fn text(&mut self, s: &str, pos: (f32, f32), size: u16, c: [u8; 3], width: f32, bold: bool) {
        let (x, y) = pos;
        let truncated = self.measure(s, size, bold) > width;
        let ellipsis = self.measure("…", size, bold);
        let mut fitted = String::new();
        let mut used = 0.0;
        for ch in s.chars() {
            let advance = advance(&self.fonts[Self::font_index(ch, bold)], ch, size);
            if used + advance > width - if truncated { ellipsis } else { 0.0 } {
                break;
            }
            fitted.push(ch);
            used += advance;
        }
        if truncated && width >= ellipsis {
            fitted.push('…');
        }
        let mut cursor = x;
        for ch in fitted.chars() {
            let index = Self::font_index(ch, bold);
            let glyph = self.glyphs.get_or_insert_with((ch, size, index), || {
                RasterGlyph::new(&self.fonts[index], ch, size)
            });
            let gx = cursor as i32 + glyph.left;
            let gy = y as i32 + size as i32 + glyph.top;
            for row in 0..glyph.height {
                for col in 0..glyph.width {
                    let px = gx + col as i32;
                    let py = gy + row as i32;
                    if !(0..1920).contains(&px) || !(0..480).contains(&py) {
                        continue;
                    }
                    let alpha = glyph.bitmap[row * glyph.width + col] as u32;
                    if alpha == 0 {
                        continue;
                    }
                    let i = (py as usize * 1920 + px as usize) * 4;
                    let data = self.canvas.data_mut();
                    for channel in 0..3 {
                        data[i + channel] = ((c[channel] as u32 * alpha
                            + data[i + channel] as u32 * (255 - alpha))
                            / 255) as u8;
                    }
                }
            }
            cursor += glyph.advance;
        }
    }
    fn right(&mut self, s: &str, edge: f32, y: f32, size: u16, c: [u8; 3], bold: bool) {
        let w = self.measure(s, size, bold);
        self.text(s, (edge - w, y), size, c, w + 1.0, bold);
    }
    fn center(&mut self, s: &str, cx: f32, y: f32, size: u16, c: [u8; 3], bold: bool) {
        let w = self.measure(s, size, bold);
        self.text(s, (cx - w / 2.0, y), size, c, w + 1.0, bold);
    }
    fn panel(&mut self, x: f32, w: f32, accent: [u8; 3]) {
        self.rounded([x, 14.0, w, 452.0], 16.0, PANEL);
        self.rounded([x + 2.0, 14.0, w - 4.0, 3.0], 1.5, accent);
        for i in 0..6 {
            self.rect(
                [x + 2.0, 18.0 + i as f32, w - 4.0, 1.0],
                blend(PANEL, accent, 0.12 * (1.0 - i as f32 / 6.0)),
            );
        }
    }
    fn bar(&mut self, bounds: [f32; 4], pct: f32, c: [u8; 3]) {
        let [x, y, w, h] = bounds;
        self.rounded(bounds, h / 2.0, BAR);
        if pct > 0.0 {
            self.rounded(
                [x, y, (w * pct.clamp(0.0, 100.0) / 100.0).max(h).min(w), h],
                h / 2.0,
                c,
            );
        }
    }
    fn ring(&mut self, x: f32, y: f32, pct: f32, c: [u8; 3]) {
        for (fraction, col) in [
            (1.0, blend(PANEL, c, 0.4)),
            ((pct / 100.0).clamp(0.0, 1.0), c),
        ] {
            if fraction <= 0.0 {
                continue;
            }
            let steps = (fraction * 180.0).ceil() as usize;
            let points: Vec<_> = (0..=steps)
                .map(|n| {
                    let a = (135.0 + n as f32 / steps as f32 * fraction * 270.0).to_radians();
                    (x + a.cos() * 70.0, y + a.sin() * 70.0)
                })
                .collect();
            self.line(&points, col, 13.0);
        }
        self.center(&format!("{pct:.0}"), x, y - 28.0, 50, WHITE, true);
        self.center("%", x, y + 24.0, 20, MUTED, false);
    }
    pub fn render_view(&mut self, s: &Snapshot, view: &ViewState, t: f64) -> RgbaImage {
        for y in 0..480 {
            self.rect(
                [0.0, y as f32, 1920.0, 1.0],
                blend([10, 12, 20], [16, 18, 28], y as f32 / 479.0),
            );
        }
        self.panel(14.0, 370.0, BLUE);
        self.panel(394.0, 1130.0, PURPLE);
        self.panel(1534.0, 370.0, GREEN);
        self.text("CPU", (34.0, 28.0), 24, BLUE, 200.0, true);
        self.ring(114.0, 152.0, s.cpu, color(s.cpu));
        self.cores(&s.cores);
        self.text(&s.cpu_name, (32.0, 364.0), 12, LABEL, 332.0, false);
        self.text("Temp", (32.0, 396.0), 26, LABEL, 150.0, false);
        let temp = s
            .temperature
            .map(|v| format!("{v:.0}°C"))
            .unwrap_or_else(|| "— °C".into());
        let temp_color = s
            .temperature
            .map(|v| {
                if v > 65.0 {
                    RED
                } else if v > 50.0 {
                    ORANGE
                } else {
                    GREEN
                }
            })
            .unwrap_or(MUTED);
        self.right(&temp, 366.0, 388.0, 42, temp_color, true);
        self.text("Cores", (32.0, 432.0), 22, LABEL, 120.0, false);
        self.right(
            &format!("{} logical", s.cores.len()),
            366.0,
            432.0,
            26,
            MUTED,
            false,
        );
        self.sessions(&s.agents, view, t);
        self.memory(s);
        let active = s
            .agents
            .sessions
            .iter()
            .any(|s| s.open_state == OpenState::Open && s.work_state == WorkState::Working);
        let bounce = if active {
            (t * std::f64::consts::TAU).sin().abs() * 9.0
        } else {
            0.0
        };
        let pika_y = 224.0 - bounce as f32;
        self.electricity(114.0, pika_y + 66.0, s.cpu, t);
        self.keyboard();
        let mut frame =
            image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(1920, 480, self.canvas.data_mut())
                .expect("fixed canvas size");
        // Embedded artwork from the reference; attribution in THIRD_PARTY.md.
        if active && (t * 2.0) as u64 % 4 >= 2 {
            imageops::overlay(
                &mut frame,
                &imageops::flip_horizontal(&self.pika),
                48,
                pika_y as i64,
            );
        } else {
            imageops::overlay(&mut frame, &self.pika, 48, pika_y as i64);
        }
        imageops::overlay(&mut frame, &self.cat, 1556, 256);
        for i in 0..2 {
            let raised = active && ((t * 5.0) as usize + i).is_multiple_of(2);
            let y = if raised { 310.0 } else { 326.0 };
            let x = 1583.0 + i as f32 * 65.0;
            self.rounded([x, y, 28.0, 22.0], 11.0, [30, 34, 48]);
            self.rounded([x + 2.0, y + 2.0, 24.0, 18.0], 9.0, [244, 150, 174]);
        }
        if !active {
            self.text("z", (1677.0, 250.0), 16, LABEL, 25.0, true);
        }
        RgbaImage::from_raw(1920, 480, self.canvas.data().to_vec()).expect("fixed canvas size")
    }
    fn cores(&mut self, cores: &[f32]) {
        // Every core remains represented; large CPUs use labelled averaged groups.
        let group = cores.len().max(1).div_ceil(32);
        let count = cores.len().div_ceil(group);
        let columns = if count > 16 { 2 } else { 1 };
        let rows = count.div_ceil(columns).max(1);
        let spacing = (320.0 / rows as f32).min(33.0);
        for (i, chunk) in cores.chunks(group).enumerate() {
            let pct = chunk.iter().sum::<f32>() / chunk.len() as f32;
            let x = 208.0 + (i / rows) as f32 * 79.0;
            let y = 36.0 + spacing / 2.0 + (i % rows) as f32 * spacing;
            let label = if group == 1 {
                format!("C{}", i + 1)
            } else {
                format!("{}–{}", i * group + 1, i * group + chunk.len())
            };
            let (size, bar_x, bar_w, edge) = if columns == 1 {
                (if count > 9 { 12 } else { 16 }, x + 28.0, 74.0, 348.0)
            } else {
                (9, x + 28.0, 23.0, x + 77.0)
            };
            let (label_size, bar_x, bar_w) = if group > 1 {
                (7, x + 36.0, 15.0)
            } else {
                (size, bar_x, bar_w)
            };
            self.text(&label, (x, y), label_size, LABEL, bar_x - x - 2.0, false);
            self.bar(
                [bar_x, y + 4.0, bar_w, if columns == 1 { 10.0 } else { 7.0 }],
                pct,
                color(pct),
            );
            self.right(&format!("{pct:.0}%"), edge, y, size, MUTED, false);
        }
    }
    fn memory(&mut self, s: &Snapshot) {
        let pct = if s.total_memory > 0 {
            s.used_memory as f32 / s.total_memory as f32 * 100.0
        } else {
            0.0
        };
        self.text("MEMORY", (1554.0, 28.0), 24, GREEN, 230.0, true);
        self.right(
            &format!("{:.0} GB", gb(s.total_memory)),
            1880.0,
            30.0,
            18,
            DIM,
            false,
        );
        self.ring(1634.0, 152.0, pct, color(pct));
        self.text("Breakdown", (1728.0, 62.0), 18, LABEL, 152.0, false);
        // Use Windows physical memory/pagefile instead of macOS-only categories.
        for (i, (label, value, total, accent)) in [
            ("Used", s.used_memory, s.total_memory, GREEN),
            ("Available", s.available_memory, s.total_memory, CYAN),
            ("Pagefile", s.swap_used, s.swap_total, PURPLE),
        ]
        .into_iter()
        .enumerate()
        {
            let y = 90.0 + i as f32 * 48.0;
            self.text(label, (1728.0, y), 17, LABEL, 92.0, false);
            let value_text = if label == "Pagefile" {
                format!("{:.1}/{:.0}G", gb(value), gb(total))
            } else {
                format!("{:.1}G", gb(value))
            };
            self.right(&value_text, 1880.0, y, 17, MUTED, false);
            self.bar(
                [1728.0, y + 24.0, 152.0, 10.0],
                if total > 0 {
                    value as f32 / total as f32 * 100.0
                } else {
                    0.0
                },
                accent,
            );
        }
        let now = chrono::Local::now();
        self.text(
            &now.format("%Y-%m-%d").to_string(),
            (1724.0, 270.0),
            22,
            WHITE,
            165.0,
            true,
        );
        self.text(
            &now.format("%A").to_string(),
            (1724.0, 296.0),
            16,
            MUTED,
            165.0,
            false,
        );
        let hours = s.uptime / 3600;
        let uptime = if hours >= 24 {
            format!("{}d {}h", hours / 24, hours % 24)
        } else {
            format!("{hours}h {}m", s.uptime / 60 % 60)
        };
        for (label, value, y) in [
            ("Uptime", uptime, 324.0),
            ("Procs", s.processes.to_string(), 346.0),
        ] {
            self.text(label, (1724.0, y), 15, LABEL, 90.0, false);
            self.right(&value, 1888.0, y, 15, MUTED, false);
        }
        self.line(&[(1550.0, 350.0), (1888.0, 350.0)], BORDER, 1.0);
        self.center(
            &now.format("%H:%M:%S").to_string(),
            1719.0,
            380.0,
            66,
            WHITE,
            true,
        );
    }
    fn keyboard(&mut self) {
        self.rounded([1554.0, 335.0, 152.0, 15.0], 4.0, [30, 34, 48]);
        self.rounded([1555.5, 336.5, 149.0, 12.0], 3.0, [210, 216, 230]);
        for i in 1..12 {
            let x = 1554.0 + i as f32 * 13.0;
            self.line(&[(x, 338.0), (x, 347.0)], LABEL, 1.0);
        }
    }
    fn electricity(&mut self, cx: f32, cy: f32, pct: f32, t: f64) {
        let level = (pct / 100.0).clamp(0.0, 1.0);
        let bolts = 2 + (level * 6.0) as usize;
        for i in 0..bolts {
            if ((t * 14.0) as usize + i * 5).is_multiple_of(3) {
                continue;
            }
            let angle = i as f32 / bolts as f32 * std::f32::consts::TAU + t as f32 * 0.7;
            let ax = cx + angle.cos() * 58.0;
            let ay = cy + angle.sin() * 55.0;
            let len = 9.0 + level * 15.0;
            let (dx, dy) = (angle.cos(), angle.sin());
            self.line(
                &[
                    (ax, ay),
                    (
                        ax + dx * len * 0.45 - dy * 5.0,
                        ay + dy * len * 0.45 + dx * 5.0,
                    ),
                    (
                        ax + dx * len * 0.65 + dy * 4.0,
                        ay + dy * len * 0.65 - dx * 4.0,
                    ),
                    (ax + dx * len, ay + dy * len),
                ],
                [255, 230, 38],
                2.0,
            );
        }
    }
    fn sessions(&mut self, snapshot: &AgentSnapshot, view: &ViewState, t: f64) {
        self.text("AI SESSIONS", (416.0, 28.0), 24, PURPLE, 300.0, true);
        let sessions: Vec<_> = view
            .keys
            .iter()
            .filter_map(|key| snapshot.sessions.iter().find(|s| s.key == *key))
            .collect();
        let opened = sessions
            .iter()
            .filter(|s| s.open_state == OpenState::Open)
            .count();
        let pending = sessions.len() - opened;
        let working = sessions
            .iter()
            .filter(|s| s.open_state == OpenState::Open && s.work_state == WorkState::Working)
            .count();
        let blocked = sessions
            .iter()
            .filter(|s| s.open_state == OpenState::Open && s.work_state == WorkState::Blocked)
            .count();
        self.right(
            &format!("已打开 {opened}   工作中 {working}   待处理 {blocked}   待确认 {pending}"),
            1502.0,
            34.0,
            18,
            MUTED,
            false,
        );
        let visible: Vec<_> = view
            .range()
            .filter_map(|i| sessions.get(i).copied())
            .collect();
        if visible.is_empty() {
            self.center("没有打开会话", 959.0, 190.0, 28, MUTED, true);
            self.center(
                "等待运行实例或调整来源 / Agent 筛选",
                959.0,
                236.0,
                18,
                LABEL,
                false,
            );
        }
        for (index, session) in visible.iter().enumerate() {
            let source = snapshot
                .origins
                .iter()
                .find(|o| o.origin_id == session.key.origin_id)
                .map(|o| o.display_name.as_str())
                .unwrap_or("来源未知");
            let short_id = snapshot.short_id(&session.key);
            let overview = view.mode == ViewMode::Overview;
            let (x, y, width, height) = if overview {
                (
                    416.0 + (index % 3) as f32 * 374.0,
                    86.0 + (index / 3) as f32 * 145.0,
                    362.0,
                    133.0,
                )
            } else if visible.len() == 1 {
                (416.0, 86.0, 1110.0, 290.0)
            } else {
                (416.0 + index as f32 * 561.0, 86.0, 549.0, 290.0)
            };
            self.session_card(
                session,
                source,
                &short_id,
                [x, y, width, height],
                overview,
                t,
            );
        }
        if view.pages() > 1 {
            let step = 1110.0 / view.pages() as f32;
            let gap = 4.0f32.min(step * 0.3);
            for page in 0..view.pages() {
                let color = if page == view.page {
                    [227, 149, 117]
                } else if page < view.page {
                    [164, 118, 103]
                } else {
                    [41, 45, 62]
                };
                self.rect([416.0 + page as f32 * step, 389.0, step - gap, 5.0], color);
            }
        }
        self.line(&[(416.0, 406.0), (1502.0, 406.0)], BORDER, 1.0);
        let usage = &snapshot.daily_usage;
        self.text(
            "今日 Token · 全部来源",
            (416.0, 418.0),
            18,
            LABEL,
            300.0,
            false,
        );
        self.text(
            &if usage.available {
                compact(usage.input.saturating_add(usage.output))
            } else {
                "—".into()
            },
            (736.0, 412.0),
            36,
            WHITE,
            250.0,
            true,
        );
        self.right(
            &if usage.available {
                format!(
                    "In {}   Out {}",
                    compact(usage.input),
                    compact(usage.output)
                )
            } else {
                "用量未取得".into()
            },
            1502.0,
            414.0,
            20,
            MUTED,
            false,
        );
        self.right(&usage.coverage, 1502.0, 444.0, 14, LABEL, false);
    }
    fn session_card(
        &mut self,
        session: &AgentSession,
        source: &str,
        id: &str,
        bounds: [f32; 4],
        overview: bool,
        t: f64,
    ) {
        let [x, y, width, height] = bounds;
        let accent = if session.open_state == OpenState::Unconfirmed {
            DIM
        } else {
            match session.work_state {
                WorkState::Working => GREEN,
                WorkState::Blocked => ORANGE,
                _ => MUTED,
            }
        };
        let flash = session.usage.attention && session.open_state == OpenState::Open;
        self.rounded(
            bounds,
            9.0,
            if flash {
                blend(BORDER, accent, (0.2 + 0.2 * (t * 4.0).sin()) as f32)
            } else {
                BORDER
            },
        );
        self.rounded([x + 1.0, y + 1.0, width - 2.0, height - 2.0], 8.0, PANEL);
        if session.open_state == OpenState::Unconfirmed {
            for n in 0..(width as usize / 12) {
                self.rect([x + n as f32 * 12.0, y, 6.0, 1.0], DIM);
                self.rect([x + n as f32 * 12.0, y + height - 1.0, 6.0, 1.0], DIM);
            }
            for n in 0..(height as usize / 12) {
                self.rect([x, y + n as f32 * 12.0, 1.0, 6.0], DIM);
                self.rect([x + width - 1.0, y + n as f32 * 12.0, 1.0, 6.0], DIM);
            }
        }
        let inset = x + 12.0;
        self.text(
            session.key.agent_kind.name(),
            (inset, y + 9.0),
            18,
            accent,
            115.0,
            true,
        );
        self.right(
            session.label(),
            x + width - 12.0,
            y + 11.0,
            if overview { 14 } else { 17 },
            accent,
            false,
        );
        self.text(
            if session.usage.project.is_empty() {
                "项目未知"
            } else {
                &session.usage.project
            },
            (inset, y + 37.0),
            if overview { 22 } else { 26 },
            WHITE,
            width - 24.0,
            true,
        );
        self.text(
            &format!("{source} · {id}"),
            (inset, y + 67.0),
            14,
            LABEL,
            width - 24.0,
            false,
        );
        if overview {
            let summary = session
                .usage
                .plan
                .as_ref()
                .map(|p| p.text.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&session.usage.message)
                .replace(['\n', '\r'], " ");
            self.text(
                if summary.is_empty() {
                    "暂无消息"
                } else {
                    &summary
                },
                (inset, y + 91.0),
                16,
                MUTED,
                width - 24.0,
                false,
            );
            self.right(
                &activity(session.usage.age),
                x + width - 12.0,
                y + 115.0,
                11,
                LABEL,
                false,
            );
        } else {
            self.text(
                if session.usage.model.is_empty() {
                    "模型：未提供"
                } else {
                    &session.usage.model
                },
                (inset, y + 91.0),
                16,
                LABEL,
                width - 24.0,
                false,
            );
            let mut message_y = y + 116.0;
            if let Some(plan) = &session.usage.plan {
                self.text(
                    &format!("步骤 {}/{} · {}", plan.current, plan.total, plan.text),
                    (inset, message_y),
                    17,
                    accent,
                    width - 24.0,
                    false,
                );
                message_y += 28.0;
            }
            self.message(
                if session.usage.message.is_empty() {
                    "暂无日志消息"
                } else {
                    &session.usage.message
                },
                inset,
                message_y,
                width - 24.0,
                accent,
            );
            self.text(
                &format!("{} · {}", activity(session.usage.age), session.evidence),
                (inset, y + height - 27.0),
                13,
                LABEL,
                width - 24.0,
                false,
            );
        }
    }
    fn message(&mut self, s: &str, x: f32, y: f32, width: f32, accent: [u8; 3]) {
        let mut cy = y;
        let mut table_header = true;
        let lines: Vec<_> = s
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        // Reserve space for following blocks so a long introduction cannot hide
        // the table. Prose has a two-line minimum, table rows consume one line.
        let mut reserve = vec![0usize; lines.len() + 1];
        for i in (0..lines.len()).rev() {
            let table = lines[i].starts_with('|') && lines[i].matches('|').count() >= 2;
            let separator = table && lines[i].chars().all(|c| matches!(c, '|' | '-' | ':' | ' '));
            reserve[i] = reserve[i + 1]
                + if separator {
                    0
                } else if table {
                    1
                } else {
                    2
                };
        }
        for (line_index, line) in lines.into_iter().enumerate() {
            if cy + 24.0 > 328.0 {
                break;
            }
            if line.starts_with('|') && line.matches('|').count() >= 2 {
                let cells: Vec<_> = line.trim_matches('|').split('|').map(str::trim).collect();
                if cells
                    .iter()
                    .all(|s| !s.is_empty() && s.chars().all(|c| matches!(c, '-' | ':' | ' ')))
                {
                    continue;
                }
                let cw = (width - 8.0 * (cells.len() - 1) as f32) / cells.len() as f32;
                for (i, cell) in cells.iter().enumerate() {
                    self.text(
                        &strip_markdown(cell),
                        (x + i as f32 * (cw + 8.0), cy),
                        16,
                        if table_header { accent } else { MUTED },
                        cw,
                        table_header,
                    );
                }
                cy += 24.0;
                if table_header {
                    self.line(&[(x, cy - 4.0), (x + width, cy - 4.0)], BORDER, 1.0);
                }
                table_header = false;
            } else {
                table_header = true;
                let normalized = strip_markdown(line);
                let remaining = ((328.0 - cy) / 26.0) as usize;
                let cap = remaining
                    .saturating_sub(reserve[line_index + 1])
                    .max(2)
                    .min(remaining);
                if cap == 0 {
                    break;
                }
                let mut chunk = String::new();
                let mut used = 0.0;
                let mut wrapped = 0;
                let mut clipped = false;
                for (index, ch) in normalized.char_indices() {
                    let advance = advance(&self.fonts[Self::font_index(ch, false)], ch, 19);
                    if used + advance > width {
                        if wrapped + 1 >= cap {
                            chunk.push_str(&normalized[index..]);
                            self.text(&chunk, (x, cy), 19, MUTED, width, false);
                            cy += 26.0;
                            clipped = true;
                            break;
                        }
                        self.text(&chunk, (x, cy), 19, MUTED, width, false);
                        cy += 26.0;
                        wrapped += 1;
                        if cy + 24.0 > 328.0 {
                            return;
                        }
                        chunk.clear();
                        used = 0.0;
                    }
                    chunk.push(ch);
                    used += advance;
                }
                if !clipped {
                    self.text(&chunk, (x, cy), 19, MUTED, width, false);
                    cy += 26.0;
                }
            }
        }
    }
}
fn strip_markdown(s: &str) -> String {
    s.replace("**", "")
        .replace('`', "")
        .trim_start_matches('#')
        .trim_start()
        .to_owned()
}
fn gb(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}
fn compact(n: u64) -> String {
    let (value, suffix) = if n >= 100_000_000 {
        (n as f64 / 100_000_000.0, "亿")
    } else if n >= 10_000 {
        (n as f64 / 10_000.0, "万")
    } else {
        return n.to_string();
    };
    if value < 100.0 {
        format!("{value:.2}{suffix}")
    } else if value < 1000.0 {
        format!("{value:.1}{suffix}")
    } else {
        format!("{value:.0}{suffix}")
    }
}

fn activity(age: u64) -> String {
    if age == u64::MAX {
        "最近活动未知".into()
    } else if age < 60 {
        "最近活动 <1 分钟".into()
    } else if age < 3600 {
        format!("最近活动 {} 分钟前", age / 60)
    } else {
        format!("最近活动 {} 小时前", age / 3600)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn glyph(bytes: usize) -> RasterGlyph {
        RasterGlyph {
            advance: 1.0,
            left: 0,
            top: 0,
            width: bytes,
            height: 1,
            bitmap: vec![255; bytes],
        }
    }

    // This measures two eviction policies with actual glyph rasterization and
    // identical limits. Run explicitly; font/CPU timings are machine specific.
    #[cfg(windows)]
    #[test]
    #[ignore = "manual glyph eviction benchmark"]
    fn compare_ascii_priority_and_lru_under_label_and_cjk_churn() {
        use std::time::Instant;
        let renderer = Renderer::new(None).unwrap();
        for corpus in [300u32, 3900] {
            let mut trace = Vec::new();
            // Old dynamic English text leaves these keys behind after the
            // workload switches to CJK sessions. Sizes/weights occur in cards.
            for size in [14, 16, 22, 26] {
                for bold in [false, true] {
                    for ch in 'a'..='z' {
                        trace.push((ch, size, Renderer::font_index(ch, bold)));
                    }
                }
            }
            for frame in 0..120u32 {
                for (label, size, bold) in [
                    ("CPU AI SESSIONS", 24, true),
                    ("Cores Temp", 26, false),
                    ("0123456789 %", 26, true),
                    ("已打开 工作中 待处理 待确认", 18, false),
                    ("今日 Token 全部来源", 18, false),
                ] {
                    trace.extend(
                        label
                            .chars()
                            .map(|ch| (ch, size, Renderer::font_index(ch, bold))),
                    );
                }
                // Six overview cards expose rotating CJK message/project text.
                for position in 0..200u32 {
                    let ch = char::from_u32(0x4e00 + (frame * 200 + position) % corpus).unwrap();
                    trace.push((ch, 19, 0));
                }
            }
            for ascii_priority in [true, false] {
                let started = Instant::now();
                let mut cache = GlyphCache::new(GLYPH_CACHE_BYTES);
                let mut misses = 0;
                for &key in &trace {
                    cache.clock += 1;
                    if !cache.entries.contains_key(&key) {
                        misses += 1;
                        let glyph = RasterGlyph::new(&renderer.fonts[key.2], key.0, key.1);
                        let cost = GlyphCache::cost(&glyph);
                        assert!(cost <= cache.budget);
                        while cache.bytes + cost > cache.budget
                            || cache.entries.len() >= GLYPH_CACHE_ENTRIES
                        {
                            let oldest = *cache
                                .entries
                                .iter()
                                .min_by_key(|(key, entry)| {
                                    (ascii_priority && key.0.is_ascii(), entry.last_used)
                                })
                                .unwrap()
                                .0;
                            let removed = cache.entries.remove(&oldest).unwrap();
                            cache.bytes -= GlyphCache::cost(&removed.glyph);
                        }
                        cache.bytes += cost;
                        cache.entries.insert(
                            key,
                            CachedGlyph {
                                glyph,
                                last_used: cache.clock,
                            },
                        );
                    }
                    cache.entries.get_mut(&key).unwrap().last_used = cache.clock;
                }
                eprintln!(
                    "corpus={corpus} policy={} references={} misses={misses} hit_rate={:.2}% elapsed_ms={} entries={} bytes={}",
                    if ascii_priority {
                        "ascii-priority"
                    } else {
                        "lru"
                    },
                    trace.len(),
                    (trace.len() - misses) as f64 * 100.0 / trace.len() as f64,
                    started.elapsed().as_millis(),
                    cache.entries.len(),
                    cache.bytes,
                );
            }
        }
    }

    #[test]
    fn cache_evicts_least_recently_used_glyph_regardless_of_script() {
        let cost = GlyphCache::cost(&glyph(16));
        let mut cache = GlyphCache::new(cost * 2);
        for ch in ['甲', '乙'] {
            cache.get_or_insert_with((ch, 24, 0), || glyph(16));
        }
        cache.get_or_insert_with(('甲', 24, 0), || panic!("cached glyph must be reused"));
        cache.get_or_insert_with(('丙', 24, 0), || glyph(16));
        assert!(cache.entries.contains_key(&('甲', 24, 0)));
        assert!(!cache.entries.contains_key(&('乙', 24, 0)));
        assert_eq!(cache.bytes, cost * 2);

        let mut cache = GlyphCache::new(cost * 2);
        for ch in ['0', '甲', '乙'] {
            cache.get_or_insert_with((ch, 24, 0), || glyph(16));
        }
        assert!(!cache.entries.contains_key(&('0', 24, 0)));
        assert!(cache.entries.contains_key(&('甲', 24, 0)));

        let mut cache = GlyphCache::new(cost * 2);
        for ch in ['0', '甲'] {
            cache.get_or_insert_with((ch, 24, 0), || glyph(16));
        }
        cache.get_or_insert_with(('0', 24, 0), || panic!("fixed label should stay cached"));
        cache.get_or_insert_with(('乙', 24, 0), || glyph(16));
        assert!(cache.entries.contains_key(&('0', 24, 0)));
        assert!(!cache.entries.contains_key(&('甲', 24, 0)));
    }

    #[test]
    fn cache_bounds_memory_and_metadata_during_character_churn() {
        let mut cache = GlyphCache::new(GLYPH_CACHE_BYTES);
        for codepoint in 0x4e00..0x4e00 + 6000 {
            let ch = char::from_u32(codepoint).unwrap();
            cache.get_or_insert_with((ch, 24, 0), || glyph(4096));
            assert!(cache.bytes <= GLYPH_CACHE_BYTES);
            assert!(cache.entries.len() <= GLYPH_CACHE_ENTRIES);
        }
        let mut cache = GlyphCache::new(GLYPH_CACHE_BYTES);
        for codepoint in 0x4e00..0x4e00 + 6000 {
            let ch = char::from_u32(codepoint).unwrap();
            cache.get_or_insert_with((ch, 24, 0), || glyph(0));
        }
        assert_eq!(cache.entries.len(), GLYPH_CACHE_ENTRIES);
    }

    #[test]
    fn oversized_glyph_is_drawable_without_displacing_the_cache() {
        let cost = GlyphCache::cost(&glyph(16));
        let mut cache = GlyphCache::new(cost);
        cache.get_or_insert_with(('0', 24, 0), || glyph(16));
        let oversized = cache.get_or_insert_with(('甲', 24, 0), || glyph(cost + 1));
        assert_eq!(oversized.bitmap.len(), cost + 1);
        assert_eq!(cache.bytes, cost);
        assert_eq!(cache.entries.len(), 1);
        cache.get_or_insert_with(('0', 24, 0), || panic!("cached glyph must be reused"));
        assert!(cache.uncached.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn optional_fonts_fall_back_for_missing_unreadable_and_invalid_files() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("msyhbd.ttc"), b"invalid font").unwrap();
        let inaccessible = directory.path().join("segoeui.ttf");
        std::fs::write(&inaccessible, b"font bytes").unwrap();
        let _exclusive = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&inaccessible)
            .unwrap();
        // segoeuib.ttf is missing. All three failures should reuse the valid
        // regular font, without weakening errors for a configured main font.
        let fonts = load_fonts(&crate::fonts::default_path(), Some(directory.path())).unwrap();
        for font in &fonts[1..] {
            assert_eq!(font.font_data().as_ptr(), fonts[0].font_data().as_ptr());
        }
        assert!(Renderer::new(Some(&directory.path().join("msyhbd.ttc"))).is_err());
        assert!(Renderer::new(Some(&inaccessible)).is_err());
        assert!(Renderer::new(Some(&directory.path().join("missing.ttf"))).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn chinese_and_latin_fonts_preserve_em_size_and_baseline() {
        let renderer = Renderer::new(None).unwrap();
        for (ch, font_index) in [('中', 0), ('国', 1), ('A', 2), ('g', 3), (' ', 2)] {
            let font = &renderer.fonts[font_index];
            let raster = RasterGlyph::new(font, ch, 24);
            assert!(raster.advance > 0.0);
            if ch == ' ' {
                assert!(raster.bitmap.is_empty());
                assert!(raster.advance > 0.0);
            } else {
                assert!(raster.bitmap.iter().any(|&value| value > 0));
                assert!(raster.top < 0, "glyph should extend above the baseline");
                if ch == 'g' {
                    assert!(
                        raster.top + raster.height as i32 > 0,
                        "descender must extend below the baseline"
                    );
                }
            }
            if !ch.is_ascii() {
                assert!((raster.advance - 24.0).abs() < 0.001);
            }
        }
        let font_path = crate::fonts::default_path();
        let custom = Renderer::new(Some(&font_path)).unwrap();
        let bytes = crate::fonts::bytes(&font_path).unwrap();
        for font in &custom.fonts {
            assert_eq!(
                font.font_data().as_ptr(),
                bytes.as_ptr(),
                "renderer and UI must borrow the same font storage"
            );
        }
        assert_eq!(
            custom.measure("中Ag", 24, false),
            custom.measure("中Ag", 24, true)
        );
    }
}
