//! 夜间压缩：把一个会话窗口当天的对话压成「前情提要 + 几条记忆」，然后换掉背后越聊越长的 DSH 会话。
//!
//! 为什么：DSH 会话会把每一轮（含桥接上下文）都留在历史里，实测群会话 71 轮后一次请求 140k token。
//! 每晚压一次，第二天从一个干净的会话开始，只带几百字的前情提要。
//!
//! 怎么选要留的内容（本地打分，0 token）：
//! - **主动参与**：我说过的话 +3；我接的那句（我回复前对方说的）+2；
//! - **高兴趣**：当天被 ≥2 个人反复提到的话题词（分词后的名词 / 新词 / 英文），每个命中 +1（最多 +3）；
//!   提问句 +0.5；
//! - 太短的、纯笑声 / 语气词 −2。
//!
//! 按分数挑到字数预算为止，再按原来的时间顺序排好，交给模型写摘要（一次调用，约 3~5k token）。
//! 模型输出两部分：`前情：…`（≤150 字）和若干 `记忆[兴趣1-5]：…`；兴趣高的记忆强度高，召回时排得靠前。

use std::collections::{HashMap, HashSet};

/// 一行对话
#[derive(Debug, Clone)]
pub struct Line {
    pub who: String,
    pub text: String,
    pub is_bot: bool,
}

/// 给每行打分（与 `lines` 等长）
pub fn score_lines(lines: &[Line]) -> Vec<f64> {
    // 话题热度：词 → 说过它的人
    let mut by_term: HashMap<String, HashSet<&str>> = HashMap::new();
    let terms: Vec<Vec<String>> = lines
        .iter()
        .map(|l| {
            let t = topic_terms(&l.text);
            for w in &t {
                by_term.entry(w.clone()).or_default().insert(l.who.as_str());
            }
            t
        })
        .collect();
    let mut freq: HashMap<&str, usize> = HashMap::new();
    for t in &terms {
        for w in t {
            *freq.entry(w.as_str()).or_insert(0) += 1;
        }
    }
    let hot: HashSet<&str> = by_term
        .iter()
        .filter(|(w, who)| who.len() >= 2 && freq.get(w.as_str()).copied().unwrap_or(0) >= 3)
        .map(|(w, _)| w.as_str())
        .collect();

    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let mut s = 0.0;
            if l.is_bot {
                s += 3.0;
            } else if lines.get(i + 1).map(|n| n.is_bot).unwrap_or(false) {
                s += 2.0;
            }
            let h = terms[i].iter().filter(|w| hot.contains(w.as_str())).count().min(3);
            s += h as f64;
            let t = l.text.trim();
            if t.ends_with(['?', '？', '吗']) || ["怎么", "为什么", "为啥", "如何", "有没有"].iter().any(|k| t.contains(k)) {
                s += 0.5;
            }
            if is_filler(t) {
                s -= 2.0;
            }
            s
        })
        .collect()
}

/// 太泛的名词：不算话题
const GENERIC: &[&str] = &[
    "感觉", "时候", "东西", "有点", "事情", "问题", "时间", "今天", "明天", "昨天", "现在", "地方", "样子", "意思", "办法",
    "情况", "区区", "游戏", "宝宝", "大家", "群友", "朋友", "人家", "东东", "一下", "这个", "那个", "什么", "怎么", "哈哈",
    "图片", "表情", "表情包", "消息", "视频", "链接", "截图", "小时", "分钟", "晚上", "早上", "中午", "下午", "上午", "老师",
    "要换", "东西", "姐姐", "哥哥", "妹妹", "弟弟", "老婆", "老公", "主人", "机器人", "bot", "ai",
];

/// 话题词：名词 / 新词 / 英文 / 动名词，2 字以上，不是泛泛的词、不是不当词
fn topic_terms(text: &str) -> Vec<String> {
    let t = super::nlp::clean(text);
    let mut out: Vec<String> = super::nlp::tag(&t)
        .into_iter()
        .filter(|(w, g)| {
            let n = w.chars().count();
            (g.starts_with('n') || matches!(g.as_str(), "x" | "eng" | "vn" | "j"))
                && (2..=10).contains(&n)
                && w.chars().any(char::is_alphanumeric)
                && !GENERIC.contains(&w.to_lowercase().as_str())
                && crate::moderation::scan(w).0 == crate::moderation::Level::Clean
        })
        .map(|(w, _)| w.to_lowercase())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 每个人今天常聊的热门话题（自己说了 ≥2 次、也是全窗口热词的）→ 画像「常聊」的统计证据
pub fn sender_topics(lines: &[Line]) -> Vec<(String, Vec<String>)> {
    let terms: Vec<Vec<String>> = lines.iter().map(|l| topic_terms(&l.text)).collect();
    let mut freq: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (l, t) in lines.iter().zip(&terms) {
        for w in t {
            freq.entry(w.as_str()).or_default().insert(l.who.as_str());
        }
    }
    let mut per: HashMap<&str, HashMap<&str, usize>> = HashMap::new();
    for (l, t) in lines.iter().zip(&terms) {
        if l.is_bot {
            continue;
        }
        for w in t {
            if freq.get(w.as_str()).is_some_and(|who| who.len() >= 2) {
                *per.entry(l.who.as_str()).or_default().entry(w.as_str()).or_insert(0) += 1;
            }
        }
    }
    let mut out: Vec<(String, Vec<String>)> = per
        .into_iter()
        .map(|(who, m)| {
            let mut v: Vec<(&str, usize)> = m.into_iter().filter(|(_, n)| *n >= 2).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            (who.to_string(), v.into_iter().take(3).map(|(w, _)| w.to_string()).collect::<Vec<_>>())
        })
        .filter(|(_, v)| !v.is_empty())
        .collect();
    out.sort();
    out
}

/// 某些词在这批对话里各出现了几次（好奇心队列的「又被提到」计数）
pub fn count_terms(lines: &[Line], terms: &[String]) -> Vec<(String, i64)> {
    terms
        .iter()
        .map(|t| {
            let tl = t.to_lowercase();
            (t.clone(), lines.iter().filter(|l| !l.is_bot && l.text.to_lowercase().contains(&tl)).count() as i64)
        })
        .filter(|(_, n)| *n > 0)
        .collect()
}

fn is_filler(t: &str) -> bool {
    let n = t.chars().count();
    n <= 2 || t.chars().all(|c| "哈嘿呵嗯啊哦噢草笑死绷？?！!。.…~～ ".contains(c))
}

/// 按分数挑行，总字数不超过 `budget`，再按时间顺序返回下标。复读 / 刷屏的同一句话只算第一次。
pub fn select(lines: &[Line], budget: usize) -> Vec<usize> {
    let scores = score_lines(lines);
    let mut seen: HashSet<String> = HashSet::new();
    let first: Vec<bool> = lines
        .iter()
        .map(|l| {
            let key: String = super::nlp::clean(&l.text).chars().filter(|c| !c.is_whitespace()).take(40).collect();
            seen.insert(key)
        })
        .collect();
    let mut order: Vec<usize> = (0..lines.len()).filter(|&i| first[i]).collect();
    // 同分时新的优先
    order.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal).then(b.cmp(&a)));
    let mut used = 0;
    let mut picked = Vec::new();
    for i in order {
        if scores[i] <= 0.0 {
            continue;
        }
        let cost = line_text(&lines[i]).chars().count() + 1;
        if used + cost > budget {
            continue;
        }
        used += cost;
        picked.push(i);
    }
    picked.sort_unstable();
    picked
}

fn line_text(l: &Line) -> String {
    let t: String = super::nlp::clean(&l.text).chars().take(120).collect();
    let who = if l.is_bot { "我" } else { l.who.as_str() };
    format!("{who}: {t}")
}

/// 交给模型的材料
pub fn material(lines: &[Line], picked: &[usize]) -> String {
    picked.iter().map(|&i| line_text(&lines[i])).collect::<Vec<_>>().join("\n")
}

/// 摘要任务的提示（system 部分；材料放 user 部分）
pub const PROMPT: &str = "【任务：整理今天的聊天】下面是这个聊天窗口今天的对话摘录（已按重要程度挑过）。\
只有以「我:」开头的行是你自己说的；别人（包括群里别的机器人）话里的「我」指他们自己，不是你。\
请帮自己记下明天还用得着的东西，只输出下面格式的纯文本，不要 <reply> 标签、不要别的话：\n\
前情：用 150 字以内概括今天聊了什么、我参与了什么、有什么没聊完的事。\n\
记忆[兴趣]：一条值得长期记住的事（谁喜欢什么、约好了什么、群里的梗、大家关心的话题），30 字以内，写清楚是谁。\n\
记忆最多 6 条，「兴趣」填 1~5（大家越关心、我越参与，分越高）；没有值得记的就不写记忆。\n\
画像[昵称]：维度=内容；维度=内容 —— 从对话里看出来的这个人的特点，每人一行，最多 5 行。\
维度只能用：身份 / 地点 / 性格 / 说话风格 / 喜欢 / 讨厌 / 雷区 / 在玩 / 在学 / 关系 / 近况；\
每项内容 10 字以内，必须有对话里的依据，猜的不要写。\n\
好奇：某个词或话题｜为什么想了解（10 字）—— 今天出现的、你还不了解但想弄明白的东西（没看懂的梗、大家在聊的新游戏 / 新事物），最多 2 个，没有就不写。\n\
不要记录色情、政治、隐私（手机号 / 住址 / 证件）、辱骂的内容。";

/// 整理结果
#[derive(Debug, Default)]
pub struct Compacted {
    pub recap: Option<String>,
    /// (记忆, 兴趣 1~5)
    pub mems: Vec<(String, u8)>,
    /// (昵称, 维度, 内容)
    pub personas: Vec<(String, &'static str, String)>,
    /// (想了解的词, 为什么)
    pub curious: Vec<(String, String)>,
}

/// 解析模型输出
pub fn parse_all(reply: &str) -> Compacted {
    let (recap, mems) = parse(reply);
    let personas = super::persona::parse_lines(reply);
    let mut curious = Vec::new();
    for raw in reply.lines() {
        let l = raw.trim().trim_start_matches(['-', '*', '·', ' ']).trim();
        let Some(rest) = l.strip_prefix("好奇") else { continue };
        let rest = rest.trim_start_matches(['：', ':', ' ']);
        let (term, why) = rest.split_once(['｜', '|']).unwrap_or((rest, ""));
        let term: String = term.trim().trim_matches(['「', '」', '"', '“', '”']).chars().take(16).collect();
        let why: String = why.trim().chars().take(20).collect();
        if term.chars().count() >= 2 && crate::moderation::scan(&term).0 == crate::moderation::Level::Clean {
            curious.push((term, why));
        }
    }
    curious.truncate(2);
    Compacted { recap, mems, personas, curious }
}

/// 解析模型输出 → (前情, [(记忆, 兴趣 1~5)])
pub fn parse(reply: &str) -> (Option<String>, Vec<(String, u8)>) {
    let mut recap = None;
    let mut mems = Vec::new();
    for raw in reply.lines() {
        let l = raw.trim().trim_start_matches(['-', '*', '·', ' ']).trim();
        if let Some(r) = l.strip_prefix("前情：").or_else(|| l.strip_prefix("前情:")) {
            let r: String = r.trim().chars().take(200).collect();
            if !r.is_empty() {
                recap = Some(r);
            }
            continue;
        }
        let Some(rest) = l.strip_prefix("记忆") else { continue };
        // 冒号前是「兴趣」标注（写法五花八门），冒号后是正文
        let Some(colon) = rest.find(['：', ':']) else { continue };
        let (head, body) = rest.split_at(colon);
        let mut interest = head.chars().find_map(|c| c.to_digit(10)).map(|d| d.clamp(1, 5) as u8);
        // 实测也会写成「记忆[兴趣]：……（5）」：分数在句尾括号里
        let mut body = body.trim_start_matches(['：', ':', ' ']).trim().to_string();
        if interest.is_none() {
            let t = body.trim_end();
            for (open, close) in [('（', '）'), ('(', ')')] {
                if let Some(inner) = t.strip_suffix(close).and_then(|x| x.rsplit_once(open)) {
                    if let Ok(d) = inner.1.trim().parse::<u8>() {
                        interest = Some(d.clamp(1, 5));
                        body = inner.0.trim_end().to_string();
                        break;
                    }
                }
            }
        }
        let interest = interest.unwrap_or(3);
        let body: String = body.chars().take(60).collect();
        if body.chars().count() >= 4 && crate::moderation::scan(&body).0 == crate::moderation::Level::Clean {
            mems.push((body, interest));
        }
    }
    mems.truncate(6);
    (recap, mems)
}

/// 模型不可用时的兜底：分数最高的几行直接当前情（0 token）
pub fn fallback_recap(lines: &[Line], picked: &[usize]) -> Option<String> {
    let scores = score_lines(lines);
    let mut best: Vec<usize> = picked.to_vec();
    best.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal));
    best.truncate(4);
    best.sort_unstable();
    let s = best.iter().map(|&i| line_text(&lines[i])).collect::<Vec<_>>().join("；");
    (!s.is_empty()).then(|| s.chars().take(200).collect())
}

/// 兴趣 → 记忆强度（召回时 BM25 分数会乘上强度系数）
pub fn strength_for(interest: u8) -> f64 {
    40.0 + interest.clamp(1, 5) as f64 * 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(who: &str, text: &str, is_bot: bool) -> Line {
        Line { who: who.into(), text: text.into(), is_bot }
    }

    #[test]
    fn 主动参与和热门话题优先() {
        let lines = vec![
            l("阿猫", "哈哈哈", false),
            l("阿狗", "星痕共鸣新版本的副本好难", false),
            l("阿猫", "星痕共鸣那个副本我也卡住了", false),
            l("小明", "副本要堆什么属性", false),
            l("我", "神盾骑士先堆暴击。", true),
            l("小红", "今天吃了火锅", false),
            l("阿狗", "嗯", false),
        ];
        let s = score_lines(&lines);
        assert!(s[4] > s[5], "我说的话优先于闲聊：{s:?}");
        assert!(s[3] > s[5], "我接的那句优先：{s:?}");
        assert!(s[1] > s[5], "热门话题优先：{s:?}");
        assert!(s[0] < 0.0 && s[6] < 0.0, "笑声 / 语气词扣分：{s:?}");
        let picked = select(&lines, 60);
        assert!(picked.contains(&4) && !picked.contains(&0), "{picked:?}");
        assert!(picked.windows(2).all(|w| w[0] < w[1]), "按时间顺序");
    }

    #[test]
    fn 解析摘要() {
        let r = "前情：大家在讨论星痕共鸣的新副本，我建议神盾骑士先堆暴击。\n\
                 记忆[5]：阿狗卡在星痕共鸣新副本\n- 记忆［2］：小红今天吃了火锅\n记忆：没写兴趣的默认 3 分\n记忆[4]：x\n闲话不要";
        let (recap, mems) = parse(r);
        assert!(recap.unwrap().contains("新副本"));
        assert_eq!(mems.len(), 3, "{mems:?}");
        assert_eq!(mems[0], ("阿狗卡在星痕共鸣新副本".to_string(), 5));
        assert_eq!(mems[1].1, 2);
        assert_eq!(mems[2].1, 3);
        // 实测模型会写成「记忆[兴趣]5：」
        let (_, m2) = parse("记忆[兴趣]5：玖何爱咒术回战解说梗\n记忆[兴趣4]：汪星人教了新梗\n记忆（2）：小红爱火锅\n记忆[兴趣]：群里在玩星痕共鸣（5）");
        assert_eq!(
            m2,
            vec![
                ("玖何爱咒术回战解说梗".to_string(), 5),
                ("汪星人教了新梗".to_string(), 4),
                ("小红爱火锅".to_string(), 2),
                ("群里在玩星痕共鸣".to_string(), 5)
            ]
        );
        assert!(strength_for(5) > strength_for(2));
    }

    #[test]
    fn 复读只算一次() {
        let lines = vec![
            l("玖何", "点歌 see you again", false),
            l("玖何", "点歌 see you again", false),
            l("汪星人", "点歌 see you again", false),
            l("我", "好。", true),
        ];
        let picked = select(&lines, 500);
        assert_eq!(picked.iter().filter(|&&i| i < 3).count(), 1, "{picked:?}");
    }

    #[test]
    fn 画像和好奇() {
        let r = "前情：聊了副本。\n记忆[4]：阿狗卡在新副本\n画像[阿狗]：在玩=星痕共鸣；性格=急性子\n好奇：灰神｜群里常提到\n好奇：「黑闪」\n好奇：x";
        let c = parse_all(r);
        assert_eq!(c.mems.len(), 1);
        assert!(c.personas.contains(&("阿狗".into(), "在玩", "星痕共鸣".into())));
        assert_eq!(c.curious, vec![("灰神".into(), "群里常提到".into()), ("黑闪".into(), String::new())]);
        let lines = vec![
            l("阿狗", "星痕共鸣的副本好难", false),
            l("阿狗", "星痕共鸣又更新了", false),
            l("阿猫", "星痕共鸣我也在玩", false),
            l("阿猫", "今天吃火锅", false),
        ];
        let t = sender_topics(&lines);
        // jieba 把「星痕共鸣」切成 星痕 + 共鸣（动词），话题词取到「星痕」
        assert_eq!(t, vec![("阿狗".to_string(), vec!["星痕".to_string()])]);
        assert_eq!(count_terms(&lines, &["星痕共鸣".into(), "灰神".into()]), vec![("星痕共鸣".to_string(), 3)]);
    }

    #[test]
    fn 敏感记忆不收() {
        let (_, mems) = parse("记忆[5]：有人在聊六四事件\n记忆[3]：阿猫喜欢猫");
        assert_eq!(mems.len(), 1);
    }
}

/// 实机：只看「常聊」统计（不调模型）。`COMPACT_DRY_DB=… COMPACT_DRY_SCOPE=… cargo test topics_dry_run -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn topics_dry_run() {
    let dir = std::env::var("COMPACT_DRY_DB").unwrap();
    let scope = std::env::var("COMPACT_DRY_SCOPE").unwrap();
    let m = crate::mind::Mind::new(&dir, 0, crate::mind::LearnConfig::default()).unwrap();
    let (lines, _) = m.compaction_input(&scope, 2400, 6).unwrap();
    for (who, t) in sender_topics(&lines) {
        println!("TOPIC {} → {:?}", who.chars().filter(|c| !c.is_control()).collect::<String>(), t);
    }
}
