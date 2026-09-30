use crate::{
    agents::{AgentKind, Usage},
    config::Settings,
    metrics::Snapshot,
};
use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
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
    fonts: Vec<Font>,
    glyphs: HashMap<(char, u16, usize), (fontdue::Metrics, Vec<u8>)>,
    cat: RgbaImage,
    pika: RgbaImage,
}
impl Renderer {
    pub fn new(font_path: Option<&Path>) -> Result<Self> {
        let font_dir = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "C:/Windows".into())
            .join("Fonts");
        let default_path = font_dir.join("msyh.ttc");
        let path = font_path.unwrap_or(&default_path);
        let bytes = std::fs::read(path).with_context(|| {
            format!(
                "Cannot read font {}; configure a Chinese-capable TTF/TTC via settings.font",
                path.display()
            )
        })?;
        // Default Latin and Chinese fonts have real regular/bold weights. An
        // explicit custom font applies to both scripts and takes precedence.
        let mut fonts = Vec::new();
        for name in ["msyh.ttc", "msyhbd.ttc", "segoeui.ttf", "segoeuib.ttf"] {
            let data = if font_path.is_none() {
                std::fs::read(font_dir.join(name)).unwrap_or_else(|_| bytes.clone())
            } else {
                bytes.clone()
            };
            fonts.push(
                Font::from_bytes(data, FontSettings::default()).map_err(|e| anyhow::anyhow!(e))?,
            );
        }
        Ok(Self {
            canvas: Pixmap::new(1920, 480).context("Allocating dashboard")?,
            fonts,
            glyphs: HashMap::new(),
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
            .map(|ch| {
                self.fonts[Self::font_index(ch, bold)]
                    .metrics(ch, size as f32)
                    .advance_width
            })
            .sum()
    }
    fn text(&mut self, s: &str, pos: (f32, f32), size: u16, c: [u8; 3], width: f32, bold: bool) {
        let (x, y) = pos;
        let truncated = self.measure(s, size, bold) > width;
        let ellipsis = self.measure("…", size, bold);
        let mut fitted = String::new();
        let mut used = 0.0;
        for ch in s.chars() {
            let advance = self.fonts[Self::font_index(ch, bold)]
                .metrics(ch, size as f32)
                .advance_width;
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
            let (m, bitmap) = self
                .glyphs
                .entry((ch, size, index))
                .or_insert_with(|| self.fonts[index].rasterize(ch, size as f32));
            let gx = cursor as i32 + m.xmin;
            let gy = y as i32 + size as i32 - m.height as i32 - m.ymin;
            for row in 0..m.height {
                for col in 0..m.width {
                    let px = gx + col as i32;
                    let py = gy + row as i32;
                    if !(0..1920).contains(&px) || !(0..480).contains(&py) {
                        continue;
                    }
                    let alpha = bitmap[row * m.width + col] as u32;
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
            cursor += m.advance_width;
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
    pub fn render(&mut self, s: &Snapshot, settings: &Settings, t: f64) -> RgbaImage {
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
        self.text("AI AGENTS", (414.0, 28.0), 24, PURPLE, 350.0, true);
        self.line(&[(959.0, 66.0), (959.0, 452.0)], BORDER, 1.0);
        for (kind, x) in [(settings.left, 416.0), (settings.right, 977.0)] {
            self.agent(kind, s.agents.get(&kind).unwrap_or(&Usage::default()), x, t);
        }
        self.memory(s);
        let active = s.agents.values().any(|u| u.working);
        let bounce = if active {
            (t * std::f64::consts::TAU).sin().abs() * 9.0
        } else {
            0.0
        };
        let pika_y = 224.0 - bounce as f32;
        self.electricity(114.0, pika_y + 66.0, s.cpu, t);
        self.keyboard();
        let mut frame =
            RgbaImage::from_raw(1920, 480, self.canvas.data().to_vec()).expect("fixed canvas size");
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
        self.canvas = Pixmap::from_vec(
            frame.into_raw(),
            tiny_skia::IntSize::from_wh(1920, 480).unwrap(),
        )
        .unwrap();
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
    fn agent(&mut self, kind: AgentKind, u: &Usage, x: f32, t: f64) {
        let accent = match kind {
            AgentKind::Claude => [217, 119, 87],
            AgentKind::Codex => CYAN,
            AgentKind::Cursor => WHITE,
        };
        let phase = t.rem_euclid(5.0) as f32 / 5.0;
        let breath = if phase < 0.5 {
            phase * 2.0
        } else {
            (1.0 - phase) * 2.0
        };
        let base = if kind == AgentKind::Claude {
            0.09
        } else {
            0.08
        };
        let (alpha, border) = if u.attention {
            if ((t * 2.0) as u64).is_multiple_of(2) {
                (0.36, 0.9)
            } else {
                (0.1, 0.25)
            }
        } else if u.working {
            (base + 0.13 * breath, 0.12 + 0.28 * breath)
        } else {
            (base, 0.0)
        };
        self.rounded(
            [x - 12.0, 56.0, 549.0, 396.0],
            12.0,
            blend(PANEL, accent, border),
        );
        let bg = blend(PANEL, accent, alpha);
        self.rounded([x - 10.5, 57.5, 546.0, 393.0], 11.0, bg);
        let name = kind.name().to_uppercase();
        self.text(&name, (x, 64.0), 24, accent, 200.0, true);
        let status = if u.waiting {
            "待输入".into()
        } else if !u.available {
            "not found".into()
        } else if u.working || (!u.project.is_empty() && u.age < 90) {
            "now".into()
        } else if u.project.is_empty() {
            "no session".into()
        } else if u.age < 3600 {
            format!("{}m ago", u.age / 60)
        } else if u.age < 86400 {
            format!("{}h ago", u.age / 3600)
        } else {
            format!("{}d ago", u.age / 86400)
        };
        let status_color = if u.waiting {
            ORANGE
        } else if u.working || (!u.project.is_empty() && u.age < 90) {
            GREEN
        } else {
            MUTED
        };
        self.right(&status, x + 525.0, 70.0, 17, status_color, false);
        let status_w = self.measure(&status, 17, false);
        self.rounded(
            [x + 525.0 - status_w - 18.0, 75.0, 10.0, 10.0],
            5.0,
            status_color,
        );
        let model_x = x + self.measure(&name, 24, true) + 14.0;
        let model_w = x + 525.0 - status_w - 28.0 - model_x;
        if !u.model.is_empty() && self.measure(&u.model, 16, false) < model_w {
            self.text(&u.model, (model_x, 70.0), 16, LABEL, model_w, false);
        }
        let mut project_w = 525.0;
        if let Some(plan) = &u.plan
            && plan.total > 0
        {
            let badge = format!("步骤 {}/{}", plan.current, plan.total);
            self.right(&badge, x + 525.0, 108.0, 18, accent, true);
            project_w -= self.measure(&badge, 18, true) + 16.0;
        }
        self.text(
            if u.project.is_empty() {
                "暂无本地会话"
            } else {
                &u.project
            },
            (x, 104.0),
            26,
            WHITE,
            project_w,
            true,
        );
        let mut message_y = 142.0;
        if let Some(plan) = &u.plan
            && plan.total > 0
        {
            let count = plan.total.min(40);
            let width = (525.0 - 4.0 * (count - 1) as f32) / count as f32;
            for n in 0..count {
                let c = if n < plan.completed {
                    blend(bg, accent, 0.5)
                } else if n + 1 == plan.current {
                    accent
                } else {
                    BAR
                };
                self.rounded([x + n as f32 * (width + 4.0), 142.0, width, 7.0], 3.0, c);
            }
            message_y += 20.0;
        }
        self.message(
            if u.message.is_empty() {
                if let Some(plan) = &u.plan
                    && !plan.text.is_empty()
                {
                    &plan.text
                } else if u.available {
                    "等待会话记录。启动 AI 助手后将自动显示项目、消息与计划。"
                } else {
                    "尚未发现此助手的日志目录。"
                }
            } else {
                &u.message
            },
            x,
            message_y,
            525.0,
            accent,
        );
        self.line(&[(x, 328.0), (x + 525.0, 328.0)], BORDER, 1.0);
        self.text("今日 Token", (x, 340.0), 19, LABEL, 200.0, false);
        self.text(
            &if kind == AgentKind::Cursor {
                "—".into()
            } else {
                compact(u.input.saturating_add(u.output))
            },
            (x, 364.0),
            46,
            WHITE,
            330.0,
            true,
        );
        if kind != AgentKind::Cursor {
            self.right(
                &format!("In  {}", compact(u.input)),
                x + 525.0,
                346.0,
                20,
                MUTED,
                false,
            );
            self.right(
                &format!("Out  {}", compact(u.output)),
                x + 525.0,
                376.0,
                20,
                MUTED,
                false,
            );
        }
        if kind == AgentKind::Codex {
            if let Some(used) = u.quota_used {
                let remaining = (100.0 - used).clamp(0.0, 100.0) as f32;
                let c = if remaining > 50.0 {
                    GREEN
                } else if remaining > 20.0 {
                    ORANGE
                } else {
                    RED
                };
                self.text(
                    &format!("剩余额度 {remaining:.0}%"),
                    (x, 418.0),
                    21,
                    c,
                    270.0,
                    true,
                );
                let reset = u
                    .quota_reset
                    .map(|ts| {
                        let secs = (ts - chrono::Local::now().timestamp()).max(0);
                        if secs >= 86400 {
                            format!("{}天后重置", secs / 86400)
                        } else if secs >= 3600 {
                            format!("{}小时后重置", secs / 3600)
                        } else {
                            format!("{}分钟后重置", (secs / 60).max(1))
                        }
                    })
                    .unwrap_or_default();
                self.right(
                    &format!("日志读数  {reset}"),
                    x + 525.0,
                    421.0,
                    15,
                    DIM,
                    false,
                );
                self.bar([x, 446.0, 525.0, 8.0], remaining, c);
            }
        } else if kind == AgentKind::Cursor
            && let Some(used) = u.context_used
        {
            self.text(
                &format!("上下文 {used:.0}%"),
                (x, 418.0),
                21,
                accent,
                300.0,
                false,
            );
            self.bar(
                [x, 446.0, 525.0, 8.0],
                used as f32,
                if used < 50.0 {
                    accent
                } else if used < 80.0 {
                    ORANGE
                } else {
                    RED
                },
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
                    let advance = self.fonts[Self::font_index(ch, false)]
                        .metrics(ch, 19.0)
                        .advance_width;
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
