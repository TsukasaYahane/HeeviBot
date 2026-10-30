//! 群聊日报卡片渲染：浅色清新风格，排版参考早报（`zxcard.rs`）：白底 + 淡蓝卡片 + 小面积强调色。
//!
//! 字体：**Noto Sans SC**（正文 Regular、标题 / 数字 Bold，字形清晰、CJK 覆盖全），按字逐个回退到
//! 微软雅黑 → Segoe UI Emoji → Segoe UI Symbol，所以群友消息里的生僻字、符号、emoji 不会变成方框。
//! 找不到 Noto 时整体退回微软雅黑。全部本地栅格化（fontdue），不依赖任何 HTML / 图片服务。
//!
//! `load_font()` / `save_png()` 保持原样，早报还在用。

use image::imageops::FilterType;
use image::{Rgb, RgbImage, RgbaImage};
use std::path::Path;

/// 一张群日报卡片所需的结构化数据
pub struct DigestCard {
    /// 群名（取不到时用群号）
    pub group_name: String,
    /// 日期，如 2026-10-23
    pub date: String,
    /// 星期等补充（如「星期五」）
    pub weekday: String,
    pub msg_count: usize,
    pub member_count: usize,
    /// 最活跃的小时（0~23）及该小时的消息数
    pub peak_hour: Option<(u32, usize)>,
    /// 今日话题（每条一行）
    pub topics: Vec<String>,
    /// 话题热词 / 梗
    pub keywords: Vec<String>,
    /// 发言排行（昵称, 条数），已按条数降序
    pub ranking: Vec<(String, u32)>,
    /// 群友金句（昵称, 原话）
    pub quotes: Vec<(String, String)>,
    /// 今日总结
    pub summary: String,
    /// 页头图标（人设图，可选）
    pub icon: Option<RgbaImage>,
}

const W: u32 = 1080;
const PAD: u32 = 54;
const CANVAS_H: u32 = 6000;

// —— 配色：与早报同一套（简约清爽）——
const BG: Rgb<u8> = Rgb([252, 253, 255]);
const CARD: Rgb<u8> = Rgb([244, 248, 253]);
const TEXT: Rgb<u8> = Rgb([30, 37, 48]);
const DIM: Rgb<u8> = Rgb([128, 140, 158]);
const LINE: Rgb<u8> = Rgb([226, 233, 242]);
const BLUE: Rgb<u8> = Rgb([64, 128, 236]);
const GREEN: Rgb<u8> = Rgb([34, 168, 108]);
const ORANGE: Rgb<u8> = Rgb([244, 145, 60]);
const GOLD: Rgb<u8> = Rgb([232, 178, 46]);
const PURPLE: Rgb<u8> = Rgb([150, 120, 220]);
const TEAL: Rgb<u8> = Rgb([42, 170, 190]);
const BAR_BG: Rgb<u8> = Rgb([232, 239, 249]);

/// 热词胶囊的配色（背景, 文字），轮换使用
const CHIP_COLORS: [(Rgb<u8>, Rgb<u8>); 5] = [
    (Rgb([226, 237, 253]), Rgb([44, 100, 200])),
    (Rgb([224, 244, 233]), Rgb([22, 130, 84])),
    (Rgb([253, 236, 219]), Rgb([190, 102, 24])),
    (Rgb([238, 232, 251]), Rgb([118, 88, 200])),
    (Rgb([222, 243, 246]), Rgb([24, 128, 146])),
];

// ================= 字体 =================

/// 正文 / 粗体两套 + 逐字回退链
pub struct FontSet {
    regular: Vec<fontdue::Font>,
    bold: Vec<fontdue::Font>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Weight {
    Regular,
    Bold,
}

fn try_font(path: &str, idx: u32) -> Option<fontdue::Font> {
    let bytes = std::fs::read(path).ok()?;
    let settings = fontdue::FontSettings { collection_index: idx, ..Default::default() };
    fontdue::Font::from_bytes(bytes, settings).ok()
}

/// 依次尝试几个路径，取第一个能读的
fn first_font(paths: &[(&str, u32)]) -> Option<fontdue::Font> {
    paths.iter().find_map(|(p, i)| try_font(p, *i))
}

impl FontSet {
    pub fn load() -> FontSet {
        let here = env!("CARGO_MANIFEST_DIR");
        let local = |f: &str| format!("{here}\\meme_server\\fonts\\{f}");
        let noto_r = local("NotoSansSC-Regular.ttf");
        let noto_b = local("NotoSansSC-Bold.ttf");
        let primary_r = first_font(&[
            ("C:\\Windows\\Fonts\\NotoSansSC-Regular.ttf", 0),
            (noto_r.as_str(), 0),
            ("C:\\Windows\\Fonts\\msyh.ttc", 0),
        ]);
        let primary_b = first_font(&[
            ("C:\\Windows\\Fonts\\NotoSansSC-Bold.ttf", 0),
            (noto_b.as_str(), 0),
            ("C:\\Windows\\Fonts\\msyhbd.ttc", 0),
        ]);
        // 回退链：雅黑（覆盖部分 Noto 缺的符号）→ Emoji → 符号
        let fallbacks: Vec<fontdue::Font> = [
            ("C:\\Windows\\Fonts\\msyh.ttc", 0u32),
            ("C:\\Windows\\Fonts\\seguiemj.ttf", 0),
            ("C:\\Windows\\Fonts\\seguisym.ttf", 0),
        ]
        .iter()
        .filter_map(|(p, i)| try_font(p, *i))
        .collect();
        let mut regular: Vec<fontdue::Font> = Vec::new();
        if let Some(f) = primary_r {
            regular.push(f);
        }
        let mut bold: Vec<fontdue::Font> = Vec::new();
        if let Some(f) = primary_b {
            bold.push(f);
        }
        for f in &fallbacks {
            regular.push(f.clone());
            bold.push(f.clone());
        }
        if regular.is_empty() {
            regular.push(load_font());
        }
        if bold.is_empty() {
            bold = regular.clone();
        }
        FontSet { regular, bold }
    }

    /// 这个字用哪套字体画：第一个真有这个字形的；都没有就用主字体（画出 notdef 也比不画好）
    fn pick(&self, w: Weight, ch: char) -> &fontdue::Font {
        let list = if w == Weight::Bold { &self.bold } else { &self.regular };
        list.iter().find(|f| f.lookup_glyph_index(ch) != 0).unwrap_or(&list[0])
    }

    fn primary(&self, w: Weight) -> &fontdue::Font {
        if w == Weight::Bold {
            &self.bold[0]
        } else {
            &self.regular[0]
        }
    }

    pub fn advance(&self, w: Weight, px: f32, ch: char) -> f32 {
        self.pick(w, ch).metrics(ch, px).advance_width
    }

    pub fn width(&self, w: Weight, px: f32, s: &str) -> f32 {
        s.chars().map(|c| self.advance(w, px, c)).sum()
    }

    /// 字符级换行（中文逐字断行；英文 / 数字尽量不拆词）
    pub fn wrap(&self, w: Weight, px: f32, text: &str, max_w: f32) -> Vec<String> {
        let mut lines = Vec::new();
        for para in text.split('\n') {
            let mut cur = String::new();
            let mut width = 0.0f32;
            let chars: Vec<char> = para.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                // 连续的 ASCII 字母数字当一个词
                let mut j = i + 1;
                if chars[i].is_ascii_alphanumeric() {
                    while j < chars.len() && chars[j].is_ascii_alphanumeric() && j - i < 24 {
                        j += 1;
                    }
                }
                let word: String = chars[i..j].iter().collect();
                let ww = self.width(w, px, &word);
                // 避头尾：行首不放收尾标点（挂在上一行末，允许略超一点），行尾不留开引号 / 开括号（挪到下一行）
                let closing = word.chars().count() == 1 && "，。、；：！？）」』》”’…,.;:!?)".contains(chars[i]);
                if width + ww > max_w && !cur.is_empty() && !(closing && width + ww <= max_w + px * 1.2) {
                    let mut carry = String::new();
                    while cur.ends_with(['「', '『', '（', '《', '“', '‘', '(']) {
                        carry.insert(0, cur.pop().unwrap());
                    }
                    if cur.is_empty() {
                        cur = carry;
                        width = self.width(w, px, &cur);
                    } else {
                        lines.push(std::mem::take(&mut cur));
                        cur = carry;
                        width = self.width(w, px, &cur);
                    }
                }
                cur.push_str(&word);
                width += ww;
                i = j;
            }
            lines.push(cur);
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }
}

// ================= 绘制原语 =================

pub(crate) struct Painter<'a> {
    img: &'a mut RgbImage,
    fonts: &'a FontSet,
}

impl<'a> Painter<'a> {
    /// 心智卡片（src/mindcard.rs）也用这套画笔；画布宽度固定为 W
    pub(crate) fn new(img: &'a mut RgbImage, fonts: &'a FontSet) -> Self {
        Self { img, fonts }
    }

    /// 实心圆点（抗锯齿）
    pub(crate) fn dot(&mut self, cx: f32, cy: f32, r: f32, color: Rgb<u8>) {
        let (x0, x1) = ((cx - r - 1.0).floor() as i64, (cx + r + 1.0).ceil() as i64);
        let (y0, y1) = ((cy - r - 1.0).floor() as i64, (cy + r + 1.0).ceil() as i64);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, color, (cov * 255.0) as u32);
                }
            }
        }
    }

    /// 粗线（沿线按半像素步长盖圆点）
    pub(crate) fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Rgb<u8>) {
        let len = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        let steps = (len * 2.0).ceil().max(1.0) as usize;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            self.dot(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, width / 2.0, color);
        }
    }

    /// 虚线（网格线用）
    pub(crate) fn dashed_h(&mut self, x0: u32, x1: u32, y: u32, color: Rgb<u8>) {
        let mut x = x0;
        while x < x1 {
            self.fill_rect(x, y, 6.min(x1 - x), 1, color);
            x += 12;
        }
    }
    fn blend(&mut self, x: i64, y: i64, color: Rgb<u8>, alpha: u32) {
        if x < 0 || y < 0 || x >= W as i64 || y >= self.img.height() as i64 {
            return;
        }
        let cur = *self.img.get_pixel(x as u32, y as u32);
        let f = |c: u8, cc: u8| ((cc as u32 * alpha + c as u32 * (255 - alpha)) / 255) as u8;
        self.img.put_pixel(x as u32, y as u32, Rgb([f(cur[0], color[0]), f(cur[1], color[1]), f(cur[2], color[2])]));
    }

    pub(crate) fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Rgb<u8>) {
        for yy in y..(y + h).min(self.img.height()) {
            for xx in x..(x + w).min(W) {
                self.img.put_pixel(xx, yy, color);
            }
        }
    }

    /// 圆角矩形（边缘 1px 抗锯齿）
    pub(crate) fn round_rect(&mut self, x: u32, y: u32, w: u32, h: u32, r: u32, color: Rgb<u8>) {
        if w == 0 || h == 0 {
            return;
        }
        let r = r.min(w / 2).min(h / 2) as f32;
        let (x0, y0, x1, y1) = (x as f32, y as f32, (x + w) as f32, (y + h) as f32);
        for yy in y..(y + h).min(self.img.height()) {
            for xx in x..(x + w).min(W) {
                let (px, py) = (xx as f32 + 0.5, yy as f32 + 0.5);
                let cx = px.clamp(x0 + r, x1 - r);
                let cy = py.clamp(y0 + r, y1 - r);
                let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(xx as i64, yy as i64, color, (cov * 255.0) as u32);
                }
            }
        }
    }

    fn blit(&mut self, src: &RgbaImage, x: u32, y: u32, side: u32) {
        let scaled = image::imageops::resize(src, side, side, FilterType::Lanczos3);
        for yy in 0..side {
            for xx in 0..side {
                let p = scaled.get_pixel(xx, yy);
                if p[3] > 0 {
                    self.blend((x + xx) as i64, (y + yy) as i64, Rgb([p[0], p[1], p[2]]), p[3] as u32);
                }
            }
        }
    }

    /// 画一行字，`y` 为行顶。基线 = 行顶 + 主字体 ascent；位图左上角 = 基线 - 高 - ymin
    pub(crate) fn text(&mut self, w: Weight, px: f32, x: u32, y: u32, s: &str, color: Rgb<u8>) {
        let ascent = self.fonts.primary(w).horizontal_line_metrics(px).map(|m| m.ascent).unwrap_or(px * 0.88);
        let baseline = y as f32 + ascent;
        let mut cx = x as f32;
        for ch in s.chars() {
            let font = self.fonts.pick(w, ch);
            let (m, bitmap) = font.rasterize(ch, px);
            let gx = (cx + m.xmin as f32).round() as i64;
            let gy = (baseline - (m.height as f32 + m.ymin as f32)).round() as i64;
            for (i, a) in bitmap.iter().enumerate() {
                if *a > 0 {
                    self.blend(gx + (i % m.width.max(1)) as i64, gy + (i / m.width.max(1)) as i64, color, *a as u32);
                }
            }
            cx += m.advance_width;
        }
    }

    pub(crate) fn text_right(&mut self, w: Weight, px: f32, x_right: u32, y: u32, s: &str, color: Rgb<u8>) {
        let tw = self.fonts.width(w, px, s);
        self.text(w, px, (x_right as f32 - tw).max(0.0) as u32, y, s, color);
    }

    pub(crate) fn text_center(&mut self, w: Weight, px: f32, x: u32, width: u32, y: u32, s: &str, color: Rgb<u8>) {
        let tw = self.fonts.width(w, px, s);
        self.text(w, px, (x as f32 + (width as f32 - tw) / 2.0).max(0.0) as u32, y, s, color);
    }

    /// 自动换行，返回画完后的 y（下一行行顶）
    pub(crate) fn wrapped(&mut self, w: Weight, px: f32, x: u32, y: u32, max_w: u32, s: &str, color: Rgb<u8>, lh: f32) -> u32 {
        let step = (px * lh).round() as u32;
        let mut yy = y;
        for line in self.fonts.wrap(w, px, s, max_w as f32) {
            self.text(w, px, x, yy, &line, color);
            yy += step;
        }
        yy
    }

    pub(crate) fn wrapped_h(&self, w: Weight, px: f32, s: &str, max_w: u32, lh: f32) -> u32 {
        self.fonts.wrap(w, px, s, max_w as f32).len() as u32 * (px * lh).round() as u32
    }

    /// 版块标题：左侧色条 + 粗体标题 + 右侧灰色小注
    fn section(&mut self, title: &str, note: &str, color: Rgb<u8>, y: u32) -> u32 {
        self.fill_rect(PAD, y + 4, 6, 28, color);
        self.text(Weight::Bold, 29.0, PAD + 20, y, title, TEXT);
        if !note.is_empty() {
            self.text_right(Weight::Regular, 19.0, W - PAD - 4, y + 9, note, DIM);
        }
        y + 52
    }
}

// ================= 卡片 =================

pub fn render_card(card: &DigestCard) -> RgbImage {
    let fonts = FontSet::load();
    let mut img = RgbImage::from_pixel(W, CANVAS_H, BG);
    let mut p = Painter { img: &mut img, fonts: &fonts };
    let inner = W - PAD * 2;

    // ---------- 页头 ----------
    p.round_rect(PAD, PAD, inner, 148, 26, CARD);
    let mut tx = PAD + 28;
    if let Some(icon) = &card.icon {
        p.blit(icon, tx, PAD + 26, 96);
        tx += 96 + 22;
    }
    p.text(Weight::Bold, 38.0, tx, PAD + 30, "Heevi 群日报", TEXT);
    let sub = if card.weekday.is_empty() { card.date.clone() } else { format!("{} · {}", card.date, card.weekday) };
    p.text(Weight::Regular, 22.0, tx + 2, PAD + 90, &sub, DIM);
    let gname = clip_to(&fonts, Weight::Bold, 22.0, &card.group_name, 330.0);
    p.text_right(Weight::Bold, 22.0, W - PAD - 28, PAD + 44, &gname, BLUE);
    p.text_right(Weight::Regular, 19.0, W - PAD - 28, PAD + 86, "每日 00:01", DIM);
    let mut y = PAD + 148 + 30;

    // ---------- 今日概况：三个统计块 ----------
    let stats: Vec<(String, &str)> = vec![
        (card.msg_count.to_string(), "条消息"),
        (card.member_count.to_string(), "位群友发言"),
        (
            card.peak_hour.map(|(h, _)| format!("{h:02}:00")).unwrap_or_else(|| "—".into()),
            "最活跃时段",
        ),
    ];
    let gap = 18u32;
    let bw = (inner - gap * 2) / 3;
    for (i, (num, label)) in stats.iter().enumerate() {
        let bx = PAD + i as u32 * (bw + gap);
        p.round_rect(bx, y, bw, 118, 20, CARD);
        let color = [BLUE, GREEN, ORANGE][i];
        p.text_center(Weight::Bold, 40.0, bx, bw, y + 18, num, color);
        p.text_center(Weight::Regular, 20.0, bx, bw, y + 76, label, DIM);
    }
    y += 118 + 40;

    // ---------- 今日话题 ----------
    if !card.topics.is_empty() {
        y = p.section("今日话题", &format!("{} 个", card.topics.len()), BLUE, y);
        for (i, t) in card.topics.iter().enumerate() {
            let num_color = if i < 3 { BLUE } else { DIM };
            p.text(Weight::Bold, 23.0, PAD + 8, y, &format!("{}", i + 1), num_color);
            y = p.wrapped(Weight::Regular, 24.0, PAD + 46, y, inner - 46, t, TEXT, 1.5) + 6;
        }
        y += 26;
    }

    // ---------- 话题热词：胶囊流式排列 ----------
    if !card.keywords.is_empty() {
        y = p.section("话题热词", "", TEAL, y);
        let (px, chip_h, hpad, gap) = (22.0f32, 46u32, 22u32, 14u32);
        let mut x = PAD;
        for (i, k) in card.keywords.iter().enumerate() {
            let label = clip_to(&fonts, Weight::Bold, px, k, (inner - hpad * 2) as f32);
            let cw = fonts.width(Weight::Bold, px, &label).ceil() as u32 + hpad * 2;
            if x + cw > W - PAD {
                x = PAD;
                y += chip_h + gap;
            }
            let (bg, fg) = CHIP_COLORS[i % CHIP_COLORS.len()];
            p.round_rect(x, y, cw, chip_h, chip_h / 2, bg);
            p.text(Weight::Bold, px, x + hpad, y + 9, &label, fg);
            x += cw + gap;
        }
        y += chip_h + 40;
    }

    // ---------- 发言排行榜 ----------
    if !card.ranking.is_empty() {
        y = p.section("发言排行榜", "Top 10", ORANGE, y);
        let max_c = card.ranking[0].1.max(1) as f32;
        let name_w = 330u32;
        let count_w = 96u32;
        let bar_x = PAD + 60 + name_w + 16;
        let bar_w = W - PAD - count_w - bar_x;
        for (i, (name, count)) in card.ranking.iter().take(10).enumerate() {
            let row_h = 50u32;
            // 名次：前三用奖牌色圆章，其余灰色数字
            if i < 3 {
                let medal = [GOLD, Rgb([168, 178, 192]), Rgb([214, 146, 96])][i];
                p.round_rect(PAD + 6, y + 6, 36, 36, 18, medal);
                p.text_center(Weight::Bold, 20.0, PAD + 6, 36, y + 11, &format!("{}", i + 1), Rgb([255, 255, 255]));
            } else {
                p.text_center(Weight::Bold, 21.0, PAD + 6, 36, y + 10, &format!("{}", i + 1), DIM);
            }
            let nm = clip_to(&fonts, Weight::Regular, 23.0, name, name_w as f32);
            p.text(Weight::Regular, 23.0, PAD + 60, y + 9, &nm, TEXT);
            let fill = ((*count as f32 / max_c) * bar_w as f32).round().max(10.0) as u32;
            p.round_rect(bar_x, y + 16, bar_w, 18, 9, BAR_BG);
            let bar_color = if i == 0 { ORANGE } else if i < 3 { Rgb([248, 178, 112]) } else { Rgb([158, 192, 244]) };
            p.round_rect(bar_x, y + 16, fill.min(bar_w), 18, 9, bar_color);
            p.text_right(Weight::Bold, 22.0, W - PAD - 4, y + 10, &format!("{count} 条"), if i == 0 { ORANGE } else { DIM });
            y += row_h;
        }
        y += 30;
    }

    // ---------- 群友金句 ----------
    if !card.quotes.is_empty() {
        y = p.section("群友金句", "原话摘录", PURPLE, y);
        for (who, text) in &card.quotes {
            let body = format!("「{}」", text.trim());
            let body_w = inner - 64;
            let h = p.wrapped_h(Weight::Regular, 24.0, &body, body_w, 1.55) + 32 + 40;
            p.round_rect(PAD, y, inner, h, 20, Rgb([246, 243, 253]));
            p.fill_rect(PAD, y + 18, 5, h - 36, PURPLE);
            let ty = p.wrapped(Weight::Regular, 24.0, PAD + 32, y + 22, body_w, &body, TEXT, 1.55);
            let who = format!("—— {}", clip_to(&fonts, Weight::Regular, 20.0, who, 500.0));
            p.text_right(Weight::Regular, 20.0, W - PAD - 30, ty + 4, &who, PURPLE);
            y += h + 16;
        }
        y += 14;
    }

    // ---------- 今日总结 ----------
    if !card.summary.trim().is_empty() {
        y = p.section("今日总结", "", GREEN, y);
        let body_w = inner - 64;
        let h = p.wrapped_h(Weight::Regular, 23.5, card.summary.trim(), body_w, 1.6) + 52;
        p.round_rect(PAD, y, inner, h, 20, CARD);
        p.wrapped(Weight::Regular, 23.5, PAD + 32, y + 26, body_w, card.summary.trim(), Rgb([56, 72, 96]), 1.6);
        y += h + 30;
    }

    // ---------- 页脚 ----------
    p.fill_rect(PAD, y, inner, 1, LINE);
    y += 20;
    let y_end = p.wrapped(
        Weight::Regular,
        19.0,
        PAD,
        y,
        inner,
        "由 Heevi 自动生成 · 数据范围：本群当日聊天记录 · 金句为群友原话摘录",
        DIM,
        1.5,
    );
    let total = (y_end + PAD - 20).min(CANVAS_H).max(320);
    image::imageops::crop_imm(&img, 0, 0, W, total).to_image()
}

/// 超过宽度的单行文字截断加省略号
fn clip_to(fonts: &FontSet, w: Weight, px: f32, s: &str, max_w: f32) -> String {
    if fonts.width(w, px, s) <= max_w {
        return s.to_string();
    }
    let ell = fonts.width(w, px, "…");
    let mut out = String::new();
    let mut width = 0.0;
    for ch in s.chars() {
        let a = fonts.advance(w, px, ch);
        if width + a + ell > max_w {
            break;
        }
        width += a;
        out.push(ch);
    }
    out + "…"
}

/// 输出到文件（自动创建父目录）
pub fn save_png(img: &RgbImage, path: &Path) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    img.save(path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
}

/// 早报用的单一中文字体（保持原样）
pub(crate) fn load_font() -> fontdue::Font {
    let candidates: [(&str, u32); 4] = [
        ("C:\\Windows\\Fonts\\msyh.ttc", 0),
        ("C:\\Windows\\Fonts\\simhei.ttf", 0),
        ("C:\\Windows\\Fonts\\msyhbd.ttc", 0),
        ("C:\\Windows\\Fonts\\simsun.ttc", 0),
    ];
    for (path, idx) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let settings = fontdue::FontSettings {
                collection_index: idx,
                ..Default::default()
            };
            if let Ok(font) = fontdue::Font::from_bytes(bytes, settings) {
                return font;
            }
        }
    }
    panic!("未找到可用的中文字体（尝试过微软雅黑/黑体/宋体）");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 字体回退能找到字形() {
        let f = FontSet::load();
        for ch in ['中', 'A', '…', '「', '℃', '😂'] {
            let font = f.pick(Weight::Regular, ch);
            assert_ne!(font.lookup_glyph_index(ch), 0, "「{ch}」没有任何字体能画");
        }
    }

    #[test]
    fn 截断与换行() {
        let f = FontSet::load();
        let long = "这是一个非常非常长的群名称用来测试截断效果到底好不好";
        let c = clip_to(&f, Weight::Bold, 22.0, long, 200.0);
        assert!(c.ends_with('…') && f.width(Weight::Bold, 22.0, &c) <= 200.0);
        let lines = f.wrap(Weight::Regular, 24.0, "Hello world 中文混排测试 abcdefghij", 160.0);
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|l| !l.is_empty()));
        // 避头尾：任何宽度下都不能出现「行尾是开引号」或「行首是收尾标点」
        let text = "有人把机器人调教成「鲸鲸」「饼干」还闹起智械危机的玩笑，整场是插科打诨的欢乐水群节奏。";
        for max_w in (120..600).step_by(7) {
            for l in f.wrap(Weight::Regular, 23.5, text, max_w as f32) {
                assert!(!l.ends_with('「'), "行尾留了开引号（宽 {max_w}）：{l}");
                assert!(!l.starts_with(['，', '。', '」']), "行首是收尾标点（宽 {max_w}）：{l}");
            }
        }
    }
}
