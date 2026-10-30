//! 早报卡片渲染：浅色清新风格（白底 + 淡蓝卡片 + 柔和强调色）。
//!
//! 与 `render.rs`（深色群日报）并列，专供每日 09:00 早报使用。
//! 复用 `render::load_font()` 的系统中文字体，逐字栅格化后按像素排版。

use image::{imageops::FilterType, Rgb, RgbImage, RgbaImage};

/// 一张早报卡片的结构化数据
pub struct ZxCard {
    pub date_line: String,              // 2026-09-11 · 星期五 · 农历七月二十
    pub news: Vec<String>,              // 60 秒读懂世界
    pub hot: Vec<(String, u64)>,        // 微博热搜（标题，热度值）
    pub anime: Vec<(String, f32, u32)>, // 今日番剧（名称，评分，在看人数）
    pub game: Vec<(String, String)>,    // 游戏专区（标题，类型标签）
    pub history: Vec<(String, String)>, // 历史上的今天（年份+事件，描述）
    pub tip: String,                    // 每日一句
    pub icon: Option<RgbaImage>,        // 人设 icon
    pub icon_note: String,              // 页脚来源说明
}

const W: u32 = 1080;
const PAD: u32 = 54;
const CANVAS_H: u32 = 6000;

// —— 配色：简约清爽 ——
const BG: Rgb<u8> = Rgb([252, 253, 255]);
const CARD: Rgb<u8> = Rgb([244, 248, 253]);
const TEXT: Rgb<u8> = Rgb([30, 37, 48]);
const DIM: Rgb<u8> = Rgb([128, 140, 158]);
const LINE: Rgb<u8> = Rgb([226, 233, 242]);
const BLUE: Rgb<u8> = Rgb([64, 128, 236]);
const GREEN: Rgb<u8> = Rgb([34, 168, 108]);
const ORANGE: Rgb<u8> = Rgb([244, 145, 60]);
const RED: Rgb<u8> = Rgb([236, 92, 92]);
const TEAL: Rgb<u8> = Rgb([42, 170, 190]);
#[allow(dead_code)]
const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

pub fn render(card: &ZxCard) -> RgbImage {
    let font = crate::render::load_font();
    let mut img = RgbImage::from_pixel(W, CANVAS_H, BG);
    let mut p = Painter { img: &mut img, font: &font, y: PAD };

    // ================= 页头 =================
    p.round_rect(PAD, PAD, W - PAD * 2, 148, 26, CARD);
    let mut ix = PAD + 26;
    if let Some(icon) = &card.icon {
        let side = 96u32;
        p.blit(icon, ix, PAD + 26, side, side, CARD);
        ix += side + 22;
    }
    p.text(38.0, ix, PAD + 36, "Heevi 早报", TEXT);
    p.text(22.0, ix + 2, PAD + 92, &card.date_line, DIM);
    p.text_right(20.0, W - PAD - 28, PAD + 44, "每日 09:00", DIM);
    let mut y = PAD + 148 + 34;

    // ================= 60 秒读懂世界 =================
    if !card.news.is_empty() {
        section(&mut p, "60 秒读懂世界", &format!("{} 条", card.news.len()), BLUE, &mut y);
        for (i, item) in card.news.iter().enumerate() {
            let num = format!("{}", i + 1);
            let num_color = if i < 3 { BLUE } else { DIM };
            let top = p.y;
            p.text(23.0, PAD + 6, top, &num, num_color);
            p.wrapped(23.5, PAD + 46, W - PAD * 2 - 46, item, TEXT, 1.5);
            p.y += 6;
        }
        y = p.y + 26;
    }

    // ================= 微博热搜 =================
    if !card.hot.is_empty() {
        section(&mut p, "微博热搜", "实时", ORANGE, &mut y);
        for (i, (title, value)) in card.hot.iter().enumerate() {
            let rank_color = match i {
                0 => RED,
                1 => ORANGE,
                2 => Rgb([232, 178, 46]),
                _ => DIM,
            };
            let label = format!("{}", i + 1);
            let hot_txt = fmt_hot(*value);
            let top = p.y;
            p.text(22.0, PAD + 8, top, &label, rank_color);
            let right_w = text_w(&font, 20.0, &hot_txt);
            p.wrapped(23.5, PAD + 44, W - PAD * 2 - 44 - right_w as u32 - 18, title, TEXT, 1.5);
            let after = p.y;
            p.y = top;
            p.text_right(20.0, W - PAD - 8, top + 3, &hot_txt, DIM);
            p.y = after + 6;
        }
        y = p.y + 26;
    }

    // ================= 今日番剧 =================
    if !card.anime.is_empty() {
        section(&mut p, "今日番剧", "Bangumi 放送", GREEN, &mut y);
        for (name, score, doing) in card.anime.iter() {
            let top = p.y;
            let tail = format!("{} 人在看", fmt_doing(*doing));
            let tail_w = text_w(&font, 20.0, &tail).ceil() as u32;
            let chip_w = 62u32;
            let name_max = W - PAD * 2 - chip_w - tail_w - 46;
            p.wrapped(23.5, PAD + 6, name_max, name, TEXT, 1.5);
            let name_end = p.y;
            // 评分胶囊（靠右：评分 + 在看人数）
            let chip_x = W - PAD - tail_w - 20 - chip_w;
            let (chip_bg, chip_fg) = score_style(*score);
            p.round_rect(chip_x, top, chip_w, 32, 16, chip_bg);
            p.text_center(21.0, chip_x, chip_w, top + 5, &format!("{:.1}", score), chip_fg);
            p.y = top;
            p.text_right(20.0, W - PAD - 8, top + 6, &tail, DIM);
            p.y = name_end + 6;
        }
        y = p.y + 26;
    }

    // ================= 游戏专区 =================
    if !card.game.is_empty() {
        section(&mut p, "游戏专区", "机核 GCORES", TEAL, &mut y);
        for (i, (title, kind)) in card.game.iter().enumerate() {
            let top = p.y;
            let tag_w = text_w(&font, 19.0, kind).ceil() as u32;
            let num_color = if i == 0 { TEAL } else { DIM };
            p.text(22.0, PAD + 8, top, &format!("{}", i + 1), num_color);
            p.wrapped(23.5, PAD + 44, W - PAD * 2 - 44 - tag_w - 18, title, TEXT, 1.5);
            let after = p.y;
            p.y = top;
            p.text_right(19.0, W - PAD - 8, top + 4, kind, DIM);
            p.y = after + 6;
        }
        y = p.y + 26;
    }

    // ================= 历史上的今天 =================
    if !card.history.is_empty() {
        section(&mut p, "历史上的今天", "", Rgb([150, 120, 220]), &mut y);
        for (title, desc) in card.history.iter() {
            p.wrapped(24.0, PAD + 6, W - PAD * 2 - 6, title, TEXT, 1.45);
            p.y += 2;
            p.wrapped(20.5, PAD + 6, W - PAD * 2 - 6, desc, DIM, 1.5);
            p.y += 14;
        }
        y = p.y + 14;
    }

    // ================= 每日一句 =================
    if !card.tip.trim().is_empty() {
        let text = format!("「{}」", card.tip.trim());
        let h = wrapped_height(&font, 23.0, &text, W - PAD * 2 - 64, 1.55) + 52;
        p.round_rect(PAD, y, W - PAD * 2, h, 20, CARD);
        let mut ty = y + 26;
        p.y = ty;
        p.wrapped(23.0, PAD + 32, W - PAD * 2 - 64, &text, Rgb([70, 96, 130]), 1.55);
        ty = p.y + 26;
        y = ty;
    }

    // ================= 页脚 =================
    y += 12;
    p.fill_rect(PAD, y, W - PAD * 2, 1, LINE);
    y += 20;
    p.y = y;
    p.wrapped(19.0, PAD, W - PAD * 2, &card.icon_note, DIM, 1.5);
    let total = (p.y + PAD).min(CANVAS_H).max(240);

    let mut out = RgbImage::from_pixel(W, total, BG);
    for yy in 0..total {
        for xx in 0..W {
            out.put_pixel(xx, yy, *img.get_pixel(xx, yy));
        }
    }
    out
}

fn section(p: &mut Painter, title: &str, chip: &str, color: Rgb<u8>, y: &mut u32) {
    p.y = *y;
    p.fill_rect(PAD, p.y + 2, 5, 26, color);
    p.text(29.0, PAD + 18, p.y, title, TEXT);
    if !chip.is_empty() {
        p.text_right(19.0, W - PAD - 4, p.y + 9, chip, DIM);
    }
    p.y += 44;
    *y = p.y;
}

fn score_style(score: f32) -> (Rgb<u8>, Rgb<u8>) {
    if score >= 7.0 {
        (Rgb([224, 244, 233]), Rgb([22, 140, 90]))
    } else if score >= 6.0 {
        (Rgb([253, 236, 219]), Rgb([196, 108, 26]))
    } else if score > 0.0 {
        (Rgb([235, 238, 243]), Rgb([120, 130, 148]))
    } else {
        (Rgb([235, 238, 243]), Rgb([160, 168, 180]))
    }
}

fn fmt_hot(v: u64) -> String {
    if v >= 10_000 {
        format!("{:.1} 万", v as f64 / 10_000.0)
    } else {
        format!("{v}")
    }
}

fn fmt_doing(v: u32) -> String {
    if v >= 1000 {
        format!("{:.1}k", v as f64 / 1000.0)
    } else {
        format!("{v}")
    }
}

// ---------- 绘制原语 ----------

pub struct Painter<'a> {
    img: &'a mut RgbImage,
    font: &'a fontdue::Font,
    pub y: u32,
}

impl<'a> Painter<'a> {
    fn blend(&mut self, x: i64, y: i64, color: Rgb<u8>, alpha: u32) {
        if x < 0 || y < 0 || x >= W as i64 || y >= self.img.height() as i64 {
            return;
        }
        let cur = *self.img.get_pixel(x as u32, y as u32);
        let f = |c: u8, cc: u8| ((cc as u32 * alpha + c as u32 * (255 - alpha)) / 255) as u8;
        self.img.put_pixel(
            x as u32,
            y as u32,
            Rgb([f(cur[0], color[0]), f(cur[1], color[1]), f(cur[2], color[2])]),
        );
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Rgb<u8>) {
        for yy in y..(y + h).min(self.img.height()) {
            for xx in x..(x + w).min(W) {
                self.img.put_pixel(xx, yy, color);
            }
        }
    }

    /// 圆角矩形（半径 r，边缘 1px 抗锯齿）
    pub fn round_rect(&mut self, x: u32, y: u32, w: u32, h: u32, r: u32, color: Rgb<u8>) {
        if w == 0 || h == 0 {
            return;
        }
        let r = r.min(w / 2).min(h / 2) as f32;
        let (x0, y0) = (x as i64, y as i64);
        let (x1, y1) = ((x + w) as i64, (y + h) as i64);
        for yy in y0..y1 {
            for xx in x0..x1 {
                // 到最近圆角圆心的距离，决定覆盖率
                let cx = if (xx as f32) < x0 as f32 + r {
                    x0 as f32 + r
                } else if (xx as f32) > x1 as f32 - r - 1.0 {
                    x1 as f32 - r - 1.0
                } else {
                    xx as f32
                };
                let cy = if (yy as f32) < y0 as f32 + r {
                    y0 as f32 + r
                } else if (yy as f32) > y1 as f32 - r - 1.0 {
                    y1 as f32 - r - 1.0
                } else {
                    yy as f32
                };
                let d = (((xx as f32 + 0.5) - cx).powi(2) + ((yy as f32 + 0.5) - cy).powi(2)).sqrt();
                let cov = if d <= r - 0.5 {
                    1.0
                } else if d >= r + 0.5 {
                    0.0
                } else {
                    r + 0.5 - d
                };
                if cov > 0.0 {
                    self.blend(xx, yy, color, (cov * 255.0) as u32);
                }
            }
        }
    }

    /// 把一张 RGBA 图缩放到 side×side 后贴到 (x, y)，底下垫底色（用于透明背景 icon）
    pub fn blit(&mut self, src: &RgbaImage, x: u32, y: u32, side: u32, side_h: u32, under: Rgb<u8>) {
        let scaled = image::imageops::resize(src, side, side_h, FilterType::Lanczos3);
        for yy in 0..side_h {
            for xx in 0..side {
                let px = scaled.get_pixel(xx, yy);
                let a = px[3] as u32;
                if a == 0 {
                    continue;
                }
                let base = if a == 255 {
                    Rgb([px[0], px[1], px[2]])
                } else {
                    let mix = |s: u8, u: u8| ((s as u32 * a + u as u32 * (255 - a)) / 255) as u8;
                    Rgb([
                        mix(px[0], under[0]),
                        mix(px[1], under[1]),
                        mix(px[2], under[2]),
                    ])
                };
                self.blend((x + xx) as i64, (y + yy) as i64, base, a);
            }
        }
    }

    pub fn text(&mut self, px: f32, x: u32, y: u32, s: &str, color: Rgb<u8>) {
        self.draw_text(px, x as i64, y, s, color);
    }

    pub fn text_right(&mut self, px: f32, x_right: u32, y: u32, s: &str, color: Rgb<u8>) {
        let w = text_w(self.font, px, s);
        let x = (x_right as f32 - w).max(0.0) as i64;
        self.draw_text(px, x, y, s, color);
    }

    pub fn text_center(&mut self, px: f32, x: u32, w: u32, y: u32, s: &str, color: Rgb<u8>) {
        let tw = text_w(self.font, px, s);
        let x = x as f32 + (w as f32 - tw) / 2.0;
        self.draw_text(px, x as i64, y, s, color);
    }

    /// 画一行文字；`y` 是这一行的顶部。
    ///
    /// fontdue 的 `ymin` 是「位图底边相对**基线**的偏移」，所以位图左上角 =
    /// 基线 - 位图高 - ymin，基线 = 行顶 + ascent。少了这步换算，矮字形（`.`,`%`,`-`）
    /// 会被画到行顶，看起来像上标（实测句点被读成撇号）。
    fn draw_text(&mut self, px: f32, x: i64, y: u32, s: &str, color: Rgb<u8>) {
        let ascent = self
            .font
            .horizontal_line_metrics(px)
            .map(|m| m.ascent)
            .unwrap_or(px * 0.8);
        let baseline = y as f32 + ascent;
        let mut cx = x;
        for ch in s.chars() {
            if ch == '\n' {
                continue;
            }
            let (m, bitmap) = self.font.rasterize(ch, px);
            let gx = cx + m.xmin as i64;
            let gy = (baseline - (m.height as f32 + m.ymin as f32)).round() as i64;
            if m.width > 0 {
                for (i, a) in bitmap.iter().enumerate() {
                    if *a == 0 {
                        continue;
                    }
                    let bx = gx + (i as i64 % m.width as i64);
                    let by = gy + (i as i64 / m.width as i64);
                    self.blend(bx, by, color, *a as u32);
                }
            }
            cx += m.advance_width as i64;
        }
    }

    /// 自动换行段落，返回后 `self.y` 为下一行基线
    pub fn wrapped(&mut self, px: f32, x: u32, max_w: u32, s: &str, color: Rgb<u8>, lh: f32) {
        let step = (px * lh) as u32;
        for line in wrap(self.font, px, s, max_w as f32) {
            self.draw_text(px, x as i64, self.y, &line, color);
            self.y += step;
        }
    }
}

pub fn text_w(font: &fontdue::Font, px: f32, s: &str) -> f32 {
    s.chars().map(|c| font.metrics(c, px).advance_width).sum()
}

fn wrapped_height(font: &fontdue::Font, px: f32, s: &str, max_w: u32, lh: f32) -> u32 {
    wrap(font, px, s, max_w as f32).len() as u32 * (px * lh) as u32
}

/// 字符级换行（中文无空格逐字断行）
pub fn wrap(font: &fontdue::Font, px: f32, text: &str, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut w = 0.0f32;
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut cur));
            w = 0.0;
            continue;
        }
        let adv = font.metrics(ch, px).advance_width;
        if w + adv > max_w && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            w = 0.0;
        }
        w += adv;
        cur.push(ch);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}
