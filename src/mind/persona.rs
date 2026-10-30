//! 人物画像：每个群友 / 好友在每个窗口里的多面画像（身份、喜好、在玩什么、性格、说话风格、关系、近况……）。
//!
//! 一个「面」= (维度, 内容)，比如 (喜欢, 猫)、(在玩, 星痕共鸣)、(性格, 嘴硬心软)。每个面带证据：
//! - 来源权重：本人亲口说的 1.0 · 夜间整理时模型从对话里推断的 0.6 · 本地统计（常聊的话题）0.4；
//! - 置信度 = (1 − e^(−证据/1.5)) × e^(−距上次佐证天数/90)：同一件事被反复印证会越来越确定，
//!   很久没再出现就慢慢淡掉——这就是画像的「学习曲线」；
//! - 矛盾的面互相顶替：新说「讨厌 X」会删掉「喜欢 X」，反之亦然；
//! - 每个维度最多留 5 个内容（证据最多的）。
//!
//! 注入时按维度合并成一行行短句，不确定的加「（好像）」，太弱的不说。

/// 画像维度（顺序即注入时的展示顺序）
pub const FACETS: &[&str] = &[
    "身份", "地点", "性格", "说话风格", "喜欢", "讨厌", "雷区", "在玩", "在学", "在看", "养了", "常聊", "关系", "近况",
];

/// 模型 / 别名写法 → 标准维度
pub fn normalize_facet(raw: &str) -> Option<&'static str> {
    let r = raw.trim().trim_matches(['【', '】', '[', ']']);
    if let Some(f) = FACETS.iter().find(|f| **f == r) {
        return Some(f);
    }
    Some(match r {
        "职业" | "工作" | "身份背景" | "学生" | "年龄" => "身份",
        "所在地" | "城市" | "住址" | "来自" => "地点",
        "爱好" | "兴趣" | "兴趣爱好" | "偏好" => "喜欢",
        "不喜欢" | "厌恶" => "讨厌",
        "禁忌" | "敏感点" => "雷区",
        "游戏" | "在玩的游戏" => "在玩",
        "学习" => "在学",
        "宠物" => "养了",
        "个性" | "性格特点" => "性格",
        "语气" | "口癖" | "风格" => "说话风格",
        "人际" | "人际关系" | "朋友" => "关系",
        "最近" | "近期" | "状态" => "近况",
        "话题" => "常聊",
        _ => return None,
    })
}

/// 证据来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 本人亲口说的
    SelfSaid,
    /// 夜间整理时模型推断
    Inferred,
    /// 本地统计
    Stats,
}

impl Source {
    pub fn weight(self) -> f64 {
        match self {
            Source::SelfSaid => 1.0,
            Source::Inferred => 0.6,
            Source::Stats => 0.4,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Source::SelfSaid => "self",
            Source::Inferred => "infer",
            Source::Stats => "stats",
        }
    }
}

/// 与之矛盾的维度（新证据会把对面的同内容删掉）
pub fn opposite(facet: &str) -> Option<&'static str> {
    match facet {
        "喜欢" => Some("讨厌"),
        "讨厌" => Some("喜欢"),
        _ => None,
    }
}

/// 置信度（0~1）
pub fn confidence(evidence: f64, last_ts: i64, now: i64) -> f64 {
    let days = ((now - last_ts).max(0) as f64) / 86_400.0;
    (1.0 - (-evidence / 1.5).exp()) * (-days / 90.0).exp()
}

/// 画像的一个面
#[derive(Debug, Clone)]
pub struct FacetRow {
    pub facet: String,
    pub value: String,
    pub evidence: f64,
    pub hits: i64,
    pub last_ts: i64,
}

/// 渲染「关于这个人」：按维度合并，置信度 <0.3 的不说，<0.6 的加「（好像）」；总长约 220 字封顶
pub fn render(rows: &[FacetRow], now: i64) -> Option<String> {
    let mut lines = Vec::new();
    for f in FACETS {
        let mut vals: Vec<(&FacetRow, f64)> = rows
            .iter()
            .filter(|r| r.facet == *f)
            .map(|r| (r, confidence(r.evidence, r.last_ts, now)))
            .filter(|(_, c)| *c >= 0.3)
            .collect();
        if vals.is_empty() {
            continue;
        }
        vals.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let s = vals
            .iter()
            .take(4)
            .map(|(r, c)| if *c < 0.6 { format!("{}（好像）", r.value) } else { r.value.clone() })
            .collect::<Vec<_>>()
            .join("、");
        lines.push(format!("{f}：{s}"));
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = String::new();
    for l in lines {
        if out.chars().count() + l.chars().count() > 220 {
            break;
        }
        if !out.is_empty() {
            out.push('；');
        }
        out.push_str(&l);
    }
    Some(out)
}

/// 解析夜间整理里的画像行：`画像[昵称]：维度=内容；维度=内容`
pub fn parse_lines(reply: &str) -> Vec<(String, &'static str, String)> {
    let mut out = Vec::new();
    for raw in reply.lines() {
        let l = raw.trim().trim_start_matches(['-', '*', '·', ' ']).trim();
        let Some(rest) = l.strip_prefix("画像") else { continue };
        let rest = rest.trim_start();
        let Some(open) = rest.strip_prefix('[').or_else(|| rest.strip_prefix('［')).or_else(|| rest.strip_prefix('【')) else {
            continue;
        };
        let Some(end) = open.find([']', '］', '】']) else { continue };
        let nick = open[..end].trim().to_string();
        let body = open[end..].trim_start_matches([']', '］', '】', '：', ':', ' ']);
        for part in body.split(['；', ';', '，', ',']) {
            let Some((k, v)) = part.split_once(['=', '＝', '：', ':']) else { continue };
            let Some(facet) = normalize_facet(k) else { continue };
            let v: String = v.trim().trim_matches(['。', '.', '"', '“', '”']).chars().take(20).collect();
            if !nick.is_empty() && v.chars().count() >= 1 && clean_value(&v) {
                out.push((nick.clone(), facet, v));
            }
        }
    }
    out
}

/// 画像里不存的东西：敏感数字、联系方式、色情 / 政治、太空泛的话
fn clean_value(v: &str) -> bool {
    let digits = v.chars().filter(|c| c.is_ascii_digit()).count();
    digits < 6
        && !v.contains('@')
        && !["未知", "不详", "无", "不确定", "没有"].contains(&v)
        && crate::moderation::scan(v).0 == crate::moderation::Level::Clean
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 置信度曲线() {
        let now = 100 * 86_400;
        let one = confidence(1.0, now, now);
        let three = confidence(3.0, now, now);
        assert!(one < 0.6 && three > 0.8, "反复印证越来越确定：{one} {three}");
        assert!(confidence(0.4, now, now) < 0.3, "只有一次统计证据不足以说出口");
        assert!(confidence(3.0, now - 180 * 86_400, now) < 0.3, "半年没再佐证就淡掉");
    }

    #[test]
    fn 解析画像行() {
        let r = "前情：……\n画像[阿猫]：喜欢=猫；性格=嘴硬心软；职业=程序员\n画像【小明】：游戏=星痕共鸣，电话=13800138000\n画像[阿狗]：瞎写=啥";
        let p = parse_lines(r);
        assert!(p.contains(&("阿猫".into(), "喜欢", "猫".into())));
        assert!(p.contains(&("阿猫".into(), "身份", "程序员".into())), "别名归一：{p:?}");
        assert!(p.contains(&("小明".into(), "在玩", "星痕共鸣".into())));
        assert!(!p.iter().any(|(_, _, v)| v.contains("138")), "不收电话");
        assert!(!p.iter().any(|(n, _, _)| n == "阿狗"), "未知维度丢掉");
    }

    #[test]
    fn 渲染按维度合并() {
        let now = 1_000_000;
        let row = |f: &str, v: &str, e: f64| FacetRow { facet: f.into(), value: v.into(), evidence: e, hits: 1, last_ts: now };
        let s = render(&[row("喜欢", "猫", 3.0), row("喜欢", "火锅", 1.0), row("在玩", "星痕共鸣", 2.0), row("常聊", "副本", 0.4)], now).unwrap();
        assert!(s.starts_with("喜欢：猫、火锅（好像）"), "{s}");
        assert!(s.contains("在玩：星痕共鸣") && !s.contains("副本"), "{s}");
        assert!(render(&[row("常聊", "副本", 0.2)], now).is_none());
    }
}
