//! 心智卡片（QQ 前台）：把「她对一件事的了解程度和自己的理解」「她眼中的某个人」画成一张图。
//! 复用群日报的画笔和字体（render.rs），宽 1080，按内容裁高度。

use crate::mind::insight::{ConceptView, PersonaView};
use crate::render::{FontSet, Painter, Weight};
use image::{Rgb, RgbImage};

const W: u32 = 1080;
const PAD: u32 = 54;
const BG: Rgb<u8> = Rgb([250, 250, 246]);
const PANEL: Rgb<u8> = Rgb([255, 255, 252]);
const INK: Rgb<u8> = Rgb([34, 36, 31]);
const DIM: Rgb<u8> = Rgb([138, 140, 132]);
const LINE: Rgb<u8> = Rgb([222, 221, 212]);
const GREEN: Rgb<u8> = Rgb([47, 125, 91]);
const ORANGE: Rgb<u8> = Rgb([181, 101, 29]);
const RED: Rgb<u8> = Rgb([178, 59, 59]);
const TRACK: Rgb<u8> = Rgb([236, 235, 228]);

fn conf_color(c: i64) -> Rgb<u8> {
    match c {
        2 => GREEN,
        1 => ORANGE,
        _ => RED,
    }
}

fn day(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(ts, 0).single().map(|d| d.format("%m/%d").to_string()).unwrap_or_default()
}

fn finish(img: RgbImage, used_h: u32) -> Option<Vec<u8>> {
    let h = (used_h + PAD).min(img.height());
    let cropped = image::imageops::crop_imm(&img, 0, 0, W, h).to_image();
    crate::png8::encode_indexed_png(&cropped).or_else(|| {
        let mut buf = std::io::Cursor::new(Vec::new());
        cropped.write_to(&mut buf, image::ImageFormat::Png).ok()?;
        Some(buf.into_inner())
    })
}

/// 进度条（圆角）
fn meter(p: &mut Painter, x: u32, y: u32, w: u32, h: u32, pct: f64, color: Rgb<u8>) {
    p.round_rect(x, y, w, h, h / 2, TRACK);
    let fw = ((w as f64) * pct.clamp(0.0, 100.0) / 100.0).round() as u32;
    if fw > 0 {
        p.round_rect(x, y, fw.max(h), h, h / 2, color);
    }
}

/// 折线：points 为 (t, 0~100)；可选第二条（任意量纲，按自身最大值归一）
fn chart(p: &mut Painter, x: u32, y: u32, w: u32, h: u32, a: &[(i64, f64)], b: &[(i64, f64)]) {
    p.round_rect(x, y, w, h, 14, PANEL);
    let (px, py) = (x as f32 + 56.0, y as f32 + 24.0);
    let (pw, ph) = (w as f32 - 80.0, h as f32 - 64.0);
    for v in [0.0f32, 50.0, 100.0] {
        let yy = py + ph - ph * v / 100.0;
        p.dashed_h(px as u32, (px + pw) as u32, yy as u32, LINE);
        p.text_right(Weight::Regular, 18.0, px as u32 - 10, yy as u32 - 11, &format!("{}", v as u32), DIM);
    }
    if a.is_empty() {
        return;
    }
    let t0 = a.iter().chain(b).map(|p| p.0).min().unwrap_or(0);
    let t1 = a.iter().chain(b).map(|p| p.0).max().unwrap_or(t0).max(t0 + 1);
    let fx = |t: i64| px + pw * (t - t0) as f32 / (t1 - t0) as f32;
    let fy = |v: f64| py + ph - ph * (v.clamp(0.0, 100.0) as f32) / 100.0;
    if b.len() > 1 {
        let bmax = b.iter().map(|p| p.1).fold(1.0, f64::max);
        for w2 in b.windows(2) {
            p.line(fx(w2[0].0), fy(w2[0].1 / bmax * 100.0), fx(w2[1].0), fy(w2[1].1 / bmax * 100.0), 2.5, ORANGE);
        }
    }
    for w2 in a.windows(2) {
        p.line(fx(w2[0].0), fy(w2[0].1), fx(w2[1].0), fy(w2[1].1), 4.5, GREEN);
    }
    for pt in a {
        p.dot(fx(pt.0), fy(pt.1), 6.0, GREEN);
        p.dot(fx(pt.0), fy(pt.1), 3.0, PANEL);
    }
    let by = (py + ph + 10.0) as u32;
    p.text(Weight::Regular, 18.0, px as u32, by, &day(t0), DIM);
    p.text_right(Weight::Regular, 18.0, (px + pw) as u32, by, &day(t1), DIM);
}

/// 她对一件事的了解：阶段、理解度、曲线、自己的理解、来源
pub fn concept_card(v: &ConceptView) -> Option<Vec<u8>> {
    let fonts = FontSet::load();
    let mut img = RgbImage::from_pixel(W, 2600, BG);
    let mut p = Painter::new(&mut img, &fonts);
    let inner = W - PAD * 2;
    let mut y = PAD;

    p.text(Weight::Regular, 22.0, PAD, y, "Heevi 的认知", DIM);
    let conf = ["可信度 低", "可信度 中", "可信度 高"][v.conf.clamp(0, 2) as usize];
    let cw = fonts.width(Weight::Bold, 22.0, conf) as u32 + 32;
    p.round_rect(W - PAD - cw, y - 4, cw, 38, 19, conf_color(v.conf));
    p.text_center(Weight::Bold, 22.0, W - PAD - cw, cw, y + 2, conf, PANEL);
    y += 40;
    y = p.wrapped(Weight::Bold, 52.0, PAD, y, inner, &v.term, INK, 1.25);
    if !v.why.is_empty() {
        p.text(Weight::Regular, 22.0, PAD, y, &format!("为什么想学：{}　·　被聊到 {} 次", v.why, v.mentions), DIM);
        y += 40;
    }
    y += 12;

    // 学习阶段
    let names = ["是什么", "为什么", "怎么做"];
    let bw = (inner - 2 * 40) / 3;
    for (i, n) in names.iter().enumerate() {
        let x = PAD + i as u32 * (bw + 40);
        let done = v.stage > i as i64;
        p.round_rect(x, y, bw, 64, 14, if done { GREEN } else { TRACK });
        p.text_center(Weight::Bold, 26.0, x, bw, y + 16, n, if done { PANEL } else { DIM });
        if i < 2 {
            p.line((x + bw + 8) as f32, (y + 32) as f32, (x + bw + 32) as f32, (y + 32) as f32, 3.0, if v.stage > i as i64 + 1 { GREEN } else { LINE });
        }
    }
    y += 92;

    // 理解度
    p.text(Weight::Bold, 26.0, PAD, y, "理解度", INK);
    let num = format!("{}", v.understanding.round() as i64);
    p.text_right(Weight::Bold, 40.0, W - PAD, y - 8, &num, GREEN);
    y += 44;
    meter(&mut p, PAD, y, inner, 18, v.understanding, GREEN);
    y += 40;
    let mut flags = Vec::new();
    if v.unsure {
        flags.push("还不确定");
    }
    if v.corrected {
        flags.push("主人纠正过");
    }
    if !flags.is_empty() {
        p.text(Weight::Regular, 22.0, PAD, y, &flags.join(" · "), ORANGE);
        y += 38;
    }

    // 曲线
    if v.curve.len() > 1 {
        let a: Vec<(i64, f64)> = v.curve.iter().map(|c| (c.ts, c.understanding)).collect();
        let b: Vec<(i64, f64)> = v.curve.iter().map(|c| (c.ts, c.mentions as f64)).collect();
        chart(&mut p, PAD, y, inner, 300, &a, &b);
        y += 312;
        p.line(PAD as f32, (y + 12) as f32, (PAD + 36) as f32, (y + 12) as f32, 4.5, GREEN);
        p.text(Weight::Regular, 20.0, PAD + 46, y, "理解度", DIM);
        p.line((PAD + 160) as f32, (y + 12) as f32, (PAD + 196) as f32, (y + 12) as f32, 2.5, ORANGE);
        p.text(Weight::Regular, 20.0, PAD + 206, y, "被聊到的次数", DIM);
        y += 48;
    }

    // 她的理解
    let rows = [("是什么", &v.what), ("为什么", &v.why_text), ("怎么做", &v.how), ("我觉得", &v.view)];
    if rows.iter().all(|(_, t)| t.is_empty()) {
        p.text(Weight::Regular, 26.0, PAD, y, "……还没学。今晚去查查。", DIM);
        y += 50;
    }
    for (label, text) in rows {
        if text.is_empty() {
            continue;
        }
        p.fill_rect(PAD, y + 6, 6, 28, if label == "我觉得" { ORANGE } else { GREEN });
        p.text(Weight::Bold, 26.0, PAD + 20, y, label, INK);
        y += 42;
        y = p.wrapped(Weight::Regular, 26.0, PAD + 20, y, inner - 20, text, INK, 1.55) + 14;
    }

    // 来源
    if !v.sources.is_empty() {
        let s = v
            .sources
            .iter()
            .map(|s| {
                let mark = match s.trust {
                    "high" => "✓",
                    "low" => "✗",
                    "local" => "◆",
                    _ => "·",
                };
                format!("{mark} {}", s.domain)
            })
            .collect::<Vec<_>>()
            .join("　");
        y = p.wrapped(Weight::Regular, 20.0, PAD, y + 6, inner, &format!("来源：{s}"), DIM, 1.5);
    }
    p.fill_rect(PAD, y + 18, inner, 1, LINE);
    p.text(Weight::Regular, 20.0, PAD, y + 34, "说得不对？发「纠正 词：正确的说法」", DIM);
    let used = y + 70;
    drop(p);
    finish(img, used)
}

/// 她眼中的某个人：各个面的置信度 + 小曲线
pub fn persona_card(v: &PersonaView) -> Option<Vec<u8>> {
    let fonts = FontSet::load();
    let rows = v.facets.iter().filter(|f| f.confidence >= 20.0).take(16).collect::<Vec<_>>();
    let mut img = RgbImage::from_pixel(W, 400 + rows.len() as u32 * 76, BG);
    let mut p = Painter::new(&mut img, &fonts);
    let inner = W - PAD * 2;
    let mut y = PAD;
    p.text(Weight::Regular, 22.0, PAD, y, "Heevi 眼中的 TA", DIM);
    y += 38;
    let name = if v.nick.is_empty() { v.uid.clone() } else { v.nick.clone() };
    p.text(Weight::Bold, 48.0, PAD, y, &name, INK);
    p.text_right(Weight::Bold, 40.0, W - PAD, y + 4, &format!("{}", v.depth.round() as i64), GREEN);
    y += 66;
    y += 8;
    p.text(Weight::Regular, 20.0, PAD, y, "了解程度", DIM);
    meter(&mut p, PAD + 110, y + 6, inner - 110, 14, v.depth, GREEN);
    y += 52;
    if rows.is_empty() {
        p.text(Weight::Regular, 26.0, PAD, y, "……还不太了解。", DIM);
        y += 50;
    }
    for f in rows {
        p.round_rect(PAD, y, inner, 64, 12, PANEL);
        p.text(Weight::Regular, 22.0, PAD + 20, y + 18, &f.facet, DIM);
        let mut val = f.value.clone();
        if f.confidence < 60.0 {
            val.push_str("（好像）");
        }
        p.text(Weight::Bold, 26.0, PAD + 140, y + 15, &val, INK);
        // 小曲线
        let (cx, cw, cy, ch) = (W - PAD - 420, 150.0f32, y + 12, 40.0f32);
        if f.curve.len() > 1 {
            let t0 = f.curve[0].0;
            let t1 = f.curve.last().map(|c| c.0).unwrap_or(t0).max(t0 + 1);
            let fx = |t: i64| cx as f32 + cw * (t - t0) as f32 / (t1 - t0) as f32;
            let fy = |v: f64| cy as f32 + ch - ch * (v.clamp(0.0, 100.0) as f32) / 100.0;
            for w2 in f.curve.windows(2) {
                p.line(fx(w2[0].0), fy(w2[0].1), fx(w2[1].0), fy(w2[1].1), 2.5, GREEN);
            }
        }
        let col = if f.confidence >= 60.0 { GREEN } else { ORANGE };
        meter(&mut p, W - PAD - 240, y + 25, 150, 14, f.confidence, col);
        p.text_right(Weight::Bold, 24.0, W - PAD - 20, y + 17, &format!("{}", f.confidence.round() as i64), col);
        y += 76;
    }
    p.fill_rect(PAD, y + 8, inner, 1, LINE);
    p.text(Weight::Regular, 20.0, PAD, y + 24, "说得不对？发「忘掉我的 xxx」，或者直接告诉我「其实我……」", DIM);
    let used = y + 60;
    drop(p);
    finish(img, used)
}

/// QQ 里的认知 / 画像指令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// 「了解 X」「你对 X 了解多少」→ 认知卡片
    Concept(String),
    /// 「我的画像」「你眼中的我」→ 自己的画像卡片
    MyPersona,
    /// 「画像 QQ号」（主人）→ 别人的画像卡片
    Persona(String),
    /// 「纠正 X：正确说法」
    Correct(String, String),
    /// 「忘掉我的 X」
    ForgetMine(String),
}

pub fn parse_cmd(text: &str) -> Option<Cmd> {
    let t = text.trim().trim_start_matches(['/', '／']).trim();
    if ["我的画像", "你眼中的我", "你怎么看我", "你对我了解多少"].contains(&t.trim_end_matches(['?', '？'])) {
        return Some(Cmd::MyPersona);
    }
    if let Some(rest) = t.strip_prefix("画像") {
        let r = rest.trim();
        if !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()) {
            return Some(Cmd::Persona(r.to_string()));
        }
    }
    for p in ["了解 ", "认知 ", "了解：", "认知：", "了解:", "认知:"] {
        if let Some(r) = t.strip_prefix(p) {
            let r = r.trim().trim_matches(['「', '」', '"']);
            if (1..=20).contains(&r.chars().count()) {
                return Some(Cmd::Concept(r.to_string()));
            }
        }
    }
    if let Some(r) = t.strip_prefix("你对") {
        for suf in ["了解多少", "懂多少", "知道多少", "了解吗", "的理解"] {
            if let Some(term) = r.trim_end_matches(['?', '？', '。']).strip_suffix(suf) {
                let term = term.trim().trim_matches(['「', '」', '"']);
                if (1..=20).contains(&term.chars().count()) {
                    return Some(Cmd::Concept(term.to_string()));
                }
            }
        }
    }
    if let Some(r) = t.strip_prefix("纠正") {
        let r = r.trim_start_matches([' ', '：', ':']);
        if let Some((term, fix)) = r.split_once(['：', ':', '，', ',']) {
            let (term, fix) = (term.trim().trim_matches(['「', '」', '"']), fix.trim());
            if (1..=20).contains(&term.chars().count()) && fix.chars().count() >= 2 {
                return Some(Cmd::Correct(term.to_string(), fix.to_string()));
            }
        }
    }
    if let Some(r) = t.strip_prefix("忘掉我的").or_else(|| t.strip_prefix("忘了我的")) {
        let r = r.trim();
        if (1..=20).contains(&r.chars().count()) {
            return Some(Cmd::ForgetMine(r.to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mind::insight::{CurvePoint, FacetView, SourceView};

    #[test]
    fn 指令解析() {
        assert_eq!(parse_cmd("了解 灰神"), Some(Cmd::Concept("灰神".into())));
        assert_eq!(parse_cmd("你对星痕共鸣了解多少？"), Some(Cmd::Concept("星痕共鸣".into())));
        assert_eq!(parse_cmd("我的画像"), Some(Cmd::MyPersona));
        assert_eq!(parse_cmd("画像 12345"), Some(Cmd::Persona("12345".into())));
        assert_eq!(parse_cmd("纠正 灰神：是星痕共鸣的 Boss"), Some(Cmd::Correct("灰神".into(), "是星痕共鸣的 Boss".into())));
        assert_eq!(parse_cmd("忘掉我的 香菜"), Some(Cmd::ForgetMine("香菜".into())));
        assert_eq!(parse_cmd("我想了解一下"), None);
        assert_eq!(parse_cmd("纠正一下"), None);
        assert_eq!(parse_cmd("画像真好看"), None);
    }

    #[test]
    #[ignore]
    fn 渲染样例() {
        let now = 1_793_000_000;
        let c = ConceptView {
            term: "灰神".into(),
            why: "群里常提到".into(),
            interest: 4,
            mentions: 9,
            stage: 2,
            conf: 1,
            understanding: 63.3,
            what: "星痕共鸣里新副本的尾王，机制复杂，需要躲地面红圈。".into(),
            why_text: "版本最难的 Boss，掉落本赛季最好的装备，所以大家一直在聊怎么过。".into(),
            how: String::new(),
            view: String::new(),
            unsure: false,
            corrected: false,
            sources: vec![
                SourceView { url: String::new(), domain: "wiki.biligame.com".into(), trust: "high" },
                SourceView { url: String::new(), domain: "资料库".into(), trust: "local" },
            ],
            curve: vec![
                CurvePoint { ts: now - 6 * 86400, understanding: 0.0, mentions: 1, event: "queued".into() },
                CurvePoint { ts: now - 5 * 86400, understanding: 36.7, mentions: 2, event: "learned".into() },
                CurvePoint { ts: now - 3 * 86400, understanding: 36.7, mentions: 6, event: "mentioned".into() },
                CurvePoint { ts: now - 2 * 86400, understanding: 63.3, mentions: 9, event: "learned".into() },
            ],
            corrections: vec![],
            updated: now,
        };
        let png = concept_card(&c).unwrap();
        std::fs::write(std::env::temp_dir().join("mindcard_concept.png"), png).unwrap();
        let pv = PersonaView {
            scope: "g".into(),
            uid: "1".into(),
            nick: "转生到阿斯特里斯的我".into(),
            depth: 58.0,
            facets: vec![
                FacetView { facet: "在玩".into(), value: "星痕共鸣".into(), evidence: 3.0, hits: 3, confidence: 86.0, last_ts: now, curve: vec![(now - 5 * 86400, 49.0), (now - 2 * 86400, 74.0), (now, 86.0)] },
                FacetView { facet: "性格".into(), value: "热心教配装".into(), evidence: 0.6, hits: 1, confidence: 33.0, last_ts: now, curve: vec![(now - 1 * 86400, 33.0), (now, 33.0)] },
                FacetView { facet: "常聊".into(), value: "模组".into(), evidence: 1.2, hits: 3, confidence: 55.0, last_ts: now, curve: vec![(now - 4 * 86400, 23.0), (now - 2 * 86400, 41.0), (now, 55.0)] },
            ],
        };
        std::fs::write(std::env::temp_dir().join("mindcard_persona.png"), persona_card(&pv).unwrap()).unwrap();
    }
}
