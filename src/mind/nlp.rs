//! 心智学习的语言处理：jieba 分词 + 词性，替代以前的「相邻两字」切分。
//!
//! 以前的问题（实测）：把每条消息按相邻两个字切开计数，「用于表达」切出「于表」、「画面中」切出「面中」，
//! 而且 AI 的识图描述（「[图片] 这是一张……截图」）也被当成群友的话学进去，黑话表里全是碎片。
//!
//! 现在：
//! - `clean`：学习前先去掉识图描述、表情 / 卡片占位、链接、@，只学人真正打的字；
//! - `slang_candidates`：只收「像黑话」的**完整词**——词典里没有的新词、专有名词、英文缩写、成语俗语；
//!   普通名词、代词、虚词、口头禅一律不收；
//! - `mine_definitions`：「X 是 / 就是 / 指的是 Y」，要求 X 是**一个词**（不是半句话），
//!   跳过疑问、否定、假设句，释义得像个解释（含实词、不以「不 / 没」开头、不是语气词）；
//! - `extract_facts`：从「我喜欢 / 我在玩 / 我是 / 我住在 / 记住……」这类自述里抽出关于这个人的事实，
//!   写成自然的短句（「阿猫喜欢猫」），过滤提问、假设、反问、敏感数字。

use jieba_rs::Jieba;
use std::sync::OnceLock;

fn jieba() -> &'static Jieba {
    static J: OnceLock<Jieba> = OnceLock::new();
    J.get_or_init(Jieba::new)
}

pub fn is_cjk(c: char) -> bool {
    let u = c as u32;
    (0x4E00..=0x9FFF).contains(&u) || (0x3400..=0x4DBF).contains(&u) || (0xF900..=0xFAFF).contains(&u)
}

/// 学习前清洗：去掉不是人打的内容。
/// - `[图片] …` / `[表情包] …`：识图描述，总是追加在消息末尾 → 从这里截断；
/// - `[表情:微笑]` `[表情]` `[合并转发]` `[戳一戳]` `[骰子:3]` 等占位；
/// - `[卡片消息] …`（卡片渲染文本）→ 整条丢弃；
/// - 链接、`@昵称`。
pub fn clean(text: &str) -> String {
    let mut t = text.to_string();
    if t.trim_start().starts_with("[卡片消息]") {
        return String::new();
    }
    for marker in ["[图片]", "[表情包]"] {
        if let Some(i) = t.find(marker) {
            t.truncate(i);
        }
    }
    // [xxx] / [xxx:yyy] 形式的占位
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            let rest: String = chars.clone().take(16).collect();
            if let Some(end) = rest.find(']') {
                let inner = &rest[..end];
                let known = ["表情", "合并转发", "戳一戳", "骰子", "猜拳", "语音", "视频", "文件", "回复"];
                if known.iter().any(|k| inner.starts_with(k)) {
                    for _ in 0..inner.chars().count() + 1 {
                        chars.next();
                    }
                    out.push(' ');
                    continue;
                }
            }
        }
        out.push(c);
    }
    // 链接、@
    out.split_whitespace()
        .filter(|w| {
            !w.starts_with("http://")
                && !w.starts_with("https://")
                && !w.starts_with('@')
                // 链接 / JSON / 转义残片（卡片里漏出来的 `mqqapi://…%7B%22`）
                && !w.contains(['%', '{', '}', '&'])
                && !w.contains("://")
                && !(w.is_ascii() && w.len() > 24)
        })
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// 带词性的分词（HMM 打开：能切出词典里没有的新词，标为 `x`）
pub fn tag(text: &str) -> Vec<(String, String)> {
    tag_with(text, true)
}

fn tag_with(text: &str, hmm: bool) -> Vec<(String, String)> {
    jieba().tag(text, hmm).into_iter().map(|t| (t.word.to_string(), t.tag.to_string())).collect()
}

/// 虚词 / 代词 / 动词 / 数量词等「功能性」词性
fn is_function_tag(t: &str) -> bool {
    t.starts_with('u') || t.starts_with('v') || matches!(t, "r" | "d" | "p" | "c" | "y" | "m" | "f" | "e" | "o" | "zg" | "ad")
}

/// HMM 拼出来的「新词」是不是其实由常用虚词 / 动词拼成（「是不是」「我要」「说不下去」「买个」）
fn glued_from_function_words(w: &str) -> bool {
    tag_with(w, false).iter().any(|(_, g)| is_function_tag(g))
}

/// 协议 / 网址残片，不当黑话
const MARKUP: &[&str] = &[
    "at", "cn", "com", "www", "markdown", "mention", "mqqapi", "tinyid", "png", "jpg", "jpeg", "gif", "html",
    "src", "url", "json", "faceid", "facetype", "ext", "uin", "appid", "share", "source", "medium", "android", "bbid", "ts", "tv", "b23",
];

/// 实词词性（名词 / 动名词 / 英文 / 新词等）
fn is_content_tag(t: &str) -> bool {
    t.starts_with('n') || matches!(t, "vn" | "an" | "eng" | "x" | "l" | "i" | "j" | "nz" | "v" | "a")
}

/// 口语 / 网络常见说法：不是群黑话
const COMMON: &[&str] = &[
    "哈哈", "哈哈哈", "嘿嘿", "呵呵", "嗯嗯", "好的", "好吧", "行吧", "可以", "没事", "谢谢", "晚安", "早安", "在吗",
    "牛逼", "nb", "ok", "okk", "hh", "hhh", "lol", "qq", "bot", "ai", "gg", "emm", "emmm", "awsl", "xswl", "yyds",
    "the", "and", "you", "is", "it", "to", "of", "a", "in", "on", "for", "this", "that", "what", "no", "yes",
    "啊啊", "啊啊啊", "呜呜", "呜呜呜", "草", "绷", "难绷", "笑死", "离谱", "真的", "确实", "好家伙",
];

/// 黑话候选：一条消息里「像黑话」的完整词。
/// 只收两类：词典外的新词（x，如「星痕」「灰神」「箱庭」）和英文缩写（eng，如 zmd / byd / dps）。
/// jieba 的 l（习用语）/ nz / j / i 实测几乎全是日常说法（「是不是」「就行了」「聊天」「上下文」），不收；
/// HMM 把虚词拼成的「新词」（「我要」「买个」）也不收；普通名词动词是在聊话题，不是黑话。
pub fn slang_candidates(text: &str) -> Vec<String> {
    let t = clean(text);
    if t.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for (w, tg) in tag(&t) {
        let wl = w.to_lowercase();
        let n = w.chars().count();
        let cjk = w.chars().all(is_cjk);
        let ascii = w.chars().all(|c| c.is_ascii_alphanumeric());
        let keep = match tg.as_str() {
            "x" => (cjk && (2..=6).contains(&n) && !glued_from_function_words(&w))
                || (ascii && (2..=10).contains(&n) && w.chars().any(|c| c.is_ascii_alphabetic())),
            "eng" => (2..=10).contains(&n) && w.chars().any(|c| c.is_ascii_alphabetic()),
            _ => false,
        };
        if !keep || COMMON.contains(&wl.as_str()) || MARKUP.contains(&wl.as_str()) || wl.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if !out.contains(&wl) {
            out.push(wl);
        }
    }
    out
}

/// 句子按标点切成小句
fn clauses(text: &str) -> Vec<String> {
    text.split(|c: char| "，。！？!?；;…\n~～".contains(c) || c == ',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn is_question(clause: &str, whole: &str) -> bool {
    let c = clause.trim();
    c.ends_with(['吗', '呢', '么', '嘛', '啥'])
        || ["什么是", "啥是", "谁是", "是不是", "是否", "有没有", "什么意思", "啥意思", "怎么", "为什么", "哪"]
            .iter()
            .any(|k| c.contains(k))
        || (whole.contains(['?', '？']) && whole.chars().count() <= 40)
}

fn is_hypothetical(clause: &str) -> bool {
    ["如果", "假如", "要是", "万一", "假设", "比如说", "据说", "听说", "好像", "可能"].iter().any(|k| clause.contains(k))
}

/// 定义式黑话：「X 是 / 就是 / 指的是 Y」→ (X, Y)。宁少勿滥。
pub fn mine_definitions(text: &str) -> Vec<(String, String)> {
    let whole = clean(text);
    let n = whole.chars().count();
    if !(3..=80).contains(&n) {
        return Vec::new();
    }
    for c in clauses(&whole) {
        if is_question(&c, &whole) || is_hypothetical(&c) {
            continue;
        }
        for sep in ["的意思是", "指的是", "意思是", "就是指", "是指", "就是", "即", "＝", "=", "叫做", "是"] {
            let Some((l, r)) = c.split_once(sep) else { continue };
            let term = l.trim_matches(|c: char| !c.is_alphanumeric() && !is_cjk(c)).trim().to_string();
            let meaning = r.trim_matches(|c: char| !c.is_alphanumeric() && !is_cjk(c)).trim().to_string();
            if term.is_empty() || meaning.is_empty() || term == meaning {
                break;
            }
            // X 必须是「一个词」：分词后只有 1 个（或者整个是英文缩写），且不是代词 / 时间 / 数量 / 虚词
            let tt = tag(&term);
            let single = tt.len() == 1 || term.chars().all(|c| c.is_ascii_alphanumeric());
            // X 得是个「名字」：名词 / 专名 / 英文缩写 / 新词 / 简称 / 俗语（「主要」「估计」「看看」「这位」都不是）
            let bad_tag = tt
                .first()
                .map(|(w, g)| {
                    !((g.starts_with('n') && g != "ns") || matches!(g.as_str(), "eng" | "x" | "j"))
                        || (g == "x" && w.chars().all(is_cjk) && glued_from_function_words(w))
                        || MARKUP.contains(&w.to_lowercase().as_str())
                })
                .unwrap_or(true);
            let tl = term.chars().count();
            let timey = ["今天", "明天", "昨天", "现在", "刚才", "最近", "以前", "以后"].iter().any(|p| term.starts_with(p));
            if !single || bad_tag || timey || !(2..=8).contains(&tl) {
                break;
            }
            // Y 要像个解释
            let ml = meaning.chars().count();
            let mt = tag(&meaning);
            // Y 里要有名词性的东西（「丑了点」「提醒我了」「啥样的」不是解释）
            let has_content = mt.iter().any(|(w, g)| {
                (g.starts_with('n') || matches!(g.as_str(), "vn" | "eng" | "x" | "j" | "l" | "i"))
                    && w.chars().any(|c| c.is_alphanumeric() || is_cjk(c))
            });
            let asks = mt.first().map(|(w, _)| ["啥", "什么", "哪", "怎么", "咋"].iter().any(|q| w.starts_with(q))).unwrap_or(false);
            let ascii_blob = meaning.is_ascii() && ml > 12;
            // 否定：看第一个词（「别人……」不是否定，「别……」「不……」「没……」是）
            let neg = mt.first().map(|(w, _)| w.starts_with(['不', '没']) || w == "别" || w == "非").unwrap_or(false);
            // 评价句：「真的好」「很难打」——副词开头、形容词 / 动词收尾，是在评价，不是解释
            let judging = mt.len() >= 2
                && mt.first().map(|(_, g)| g == "d").unwrap_or(false)
                && mt.last().map(|(_, g)| matches!(g.as_str(), "a" | "v" | "d" | "ad" | "ul" | "y")).unwrap_or(false);
            let particle_only = meaning.chars().all(|c| "吧啊吗呢了嘛哦呀哈呗啦么的".contains(c));
            let ends_particle = meaning.ends_with(['啊', '吧', '呀', '嘛', '哈', '的', '了']);
            if !(2..=24).contains(&ml) || !has_content || neg || judging || asks || ascii_blob || particle_only || ends_particle {
                break;
            }
            // 「是 / 就是」太泛：X 若是常见名词（「商店是每周 50」「感觉就是……」），只认明确的
            // 「指的是 / 意思是 / = / 叫做」；Y 也不能以代词 / 数字开头（「这个是我的」）
            let explicit = !matches!(sep, "是" | "就是" | "即");
            let x_common = tt.first().map(|(_, g)| g == "n").unwrap_or(false) && jieba().has_word(&term);
            if !explicit && (x_common || mt.first().map(|(_, g)| g == "r" || g == "m").unwrap_or(false)) {
                break;
            }
            // Y 以时间词开头的是在讲经历（「以前那种……」）
            if mt.first().map(|(_, g)| g == "t").unwrap_or(false) {
                break;
            }
            if crate::moderation::scan(&format!("{term}{meaning}")).0 != crate::moderation::Level::Clean {
                break;
            }
            return vec![(term, meaning)];
        }
    }
    Vec::new()
}

/// 一条关于某人的事实
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// 写进记忆的自然短句，如「阿猫喜欢猫」
    pub text: String,
    /// profile（关于这个人）/ note（让我记住的事）
    pub kind: &'static str,
    pub strength: f64,
    /// 画像维度（profile 才有，如「喜欢」「在玩」「身份」；note 为空）
    pub facet: &'static str,
    /// 画像内容（如「猫」）
    pub value: String,
}

/// 自述触发词 → 画像维度
fn facet_of(trig: &str) -> &'static str {
    match trig {
        "不喜欢" | "讨厌" | "受不了" => "讨厌",
        "在玩" => "在玩",
        "在学" => "在学",
        "在看" => "在看",
        "养了" => "养了",
        "住在" | "来自" => "地点",
        "是" => "身份",
        _ => "喜欢",
    }
}

/// 自述句式：(触发词, 记忆模板, 强度)。模板里 {who} = 昵称，{x} = 宾语
const SELF_PATTERNS: &[(&str, &str, f64)] = &[
    ("最喜欢", "{who}最喜欢{x}", 70.0),
    ("特别喜欢", "{who}特别喜欢{x}", 65.0),
    ("超喜欢", "{who}特别喜欢{x}", 65.0),
    ("很喜欢", "{who}很喜欢{x}", 60.0),
    ("不喜欢", "{who}不喜欢{x}", 60.0),
    ("讨厌", "{who}讨厌{x}", 60.0),
    ("受不了", "{who}受不了{x}", 55.0),
    ("喜欢", "{who}喜欢{x}", 55.0),
    ("超爱", "{who}超爱{x}", 60.0),
    ("最爱", "{who}最爱{x}", 65.0),
    ("在玩", "{who}在玩{x}", 55.0),
    ("在学", "{who}在学{x}", 55.0),
    ("在看", "{who}在看{x}", 45.0),
    ("养了", "{who}养了{x}", 65.0),
    ("住在", "{who}住在{x}", 60.0),
    ("来自", "{who}来自{x}", 60.0),
    ("是", "{who}是{x}", 55.0),
];

/// 从一条消息里抽关于说话人的事实（自述）和「让我记住」的事
pub fn extract_facts(who: &str, text: &str) -> Vec<Fact> {
    let whole = clean(text);
    let who = who.trim();
    if whole.is_empty() || who.is_empty() || whole.chars().count() > 120 {
        return Vec::new();
    }
    let mut out: Vec<Fact> = Vec::new();
    // 「记住：…」「帮我记一下…」「你要记得…」→ 明确让我记的事
    for head in ["帮我记住", "帮我记一下", "你要记得", "你记住", "记住", "记一下", "别忘了"] {
        if let Some(i) = whole.find(head) {
            let x = whole[i + head.len()..].trim_start_matches(['：', ':', '，', ',', ' ']).trim();
            let x: String = x.chars().take(60).collect();
            if x.chars().count() >= 4 && !is_question(&x, &whole) && safe(&x) {
                out.push(Fact { text: format!("{who}让我记住：{x}"), kind: "note", strength: 80.0, facet: "", value: String::new() });
            }
            break;
        }
    }
    for c in clauses(&whole) {
        if is_question(&c, &whole) || is_hypothetical(&c) {
            continue;
        }
        // 主语必须是「我」开头（允许「其实我」「本人」）
        let body = c
            .trim_start_matches("其实")
            .trim_start_matches("本人")
            .trim_start_matches("我这个人")
            .trim();
        let Some(rest) = body.strip_prefix('我') else { continue };
        let rest = strip_heads(rest, &["也", "还", "就", "真", "一直", "平时"]).trim();
        for (trig, tpl, strength) in SELF_PATTERNS {
            let Some(x) = rest.strip_prefix(trig) else { continue };
            // 「我是说 / 我是想 / 我是真的」不是身份
            if *trig == "是" && ["说", "想", "真", "不", "在", "觉", "因为", "为了", "来", "去"].iter().any(|k| x.starts_with(k)) {
                break;
            }
            let x = strip_heads(x, &["一个", "一只", "个", "的"]);
            // 宾语截到连接词前，长度 2~16
            let mut obj = x.to_string();
            for stop in ["但是", "但", "不过", "所以", "因为", "然后", "而且", "了"] {
                if let Some(i) = obj.find(stop) {
                    if i > 0 {
                        obj.truncate(i);
                    }
                }
            }
            let obj = obj.trim().trim_end_matches(['的', '啊', '呀', '吧', '呢', '哦']).trim().to_string();
            let ol = obj.chars().count();
            // 单字宾语只收名词（「猫」「狗」）
            let one_noun = ol == 1 && tag(&obj).first().map(|(_, g)| g.starts_with('n')).unwrap_or(false);
            if !(one_noun || (2..=16).contains(&ol)) || obj.contains('你') || obj.contains('您') || !safe(&obj) {
                break;
            }
            // 宾语里要有实词（「我喜欢这样」「我是这么想的」不算）
            let ot = tag(&obj);
            let content = ot.iter().any(|(w, g)| is_content_tag(g) && !matches!(g.as_str(), "v" | "a") && w.chars().count() >= 1);
            let pron_head = ot.first().map(|(_, g)| g == "r").unwrap_or(true);
            if !content || pron_head {
                break;
            }
            // 「我是」只收身份类名词（学生 / 程序员 / 广东人），形容词、动词不收
            if *trig == "是" && !ot.last().map(|(_, g)| g.starts_with('n')).unwrap_or(false) {
                break;
            }
            let text = tpl.replace("{who}", who).replace("{x}", &obj);
            if !out.iter().any(|f| f.text == text) {
                out.push(Fact { text, kind: "profile", strength: *strength, facet: facet_of(trig), value: obj.clone() });
            }
            break;
        }
    }
    out
}

/// 反复去掉开头的若干副词 / 量词（「我也一直喜欢」→「喜欢」）
fn strip_heads<'a>(mut s: &'a str, heads: &[&str]) -> &'a str {
    loop {
        let before = s;
        for h in heads {
            s = s.strip_prefix(h).unwrap_or(s);
        }
        if s == before {
            return s;
        }
    }
}

/// 不记敏感内容：长串数字（手机号 / 证件 / 账号）、邮箱、色情擦边
fn safe(s: &str) -> bool {
    let mut run = 0;
    for c in s.chars() {
        if c.is_ascii_digit() {
            run += 1;
            if run >= 6 {
                return false;
            }
        } else {
            run = 0;
        }
    }
    !s.contains('@') && crate::moderation::scan(s).0 == crate::moderation::Level::Clean
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 清洗掉识图描述和占位() {
        assert_eq!(clean("笑死 [图片] 这是一张游戏截图，画面中……"), "笑死");
        assert_eq!(clean("[表情:微笑]好耶[表情]"), "好耶");
        assert_eq!(clean("看这个 https://b23.tv/xx @小明 绝了"), "看这个 绝了");
        assert_eq!(clean("[卡片消息] 小程序 title: x"), "");
        assert_eq!(clean("[哈哈] 不是占位"), "[哈哈] 不是占位", "不认识的方括号保留");
    }

    #[test]
    fn 黑话候选是完整词不是碎片() {
        // 以前会切出「于表」「面中」「作的」这类碎片
        let c = slang_candidates("[图片] 这是一张截图，画面中男子做出警告动作，用于表达质疑");
        assert!(c.is_empty(), "识图描述不该进学习：{c:?}");
        let c = slang_candidates("今天又被zmd拱了白菜，真是一言难尽");
        assert!(!c.iter().any(|w| ["于表", "面中", "又被", "一言"].contains(&w.as_str())), "{c:?}");
        assert!(c.iter().any(|w| w == "zmd"), "英文缩写应保留：{c:?}");
        for t in ["是不是又要开服了", "我要先打团本", "说不下去了", "买个直购", "mqqapi://card?tinyid=1%7B%22"] {
            let c = slang_candidates(t);
            assert!(!c.iter().any(|w| ["是不是", "我要", "说不下去", "买个", "mqqapi", "tinyid"].contains(&w.as_str())), "{t} → {c:?}");
        }
        let c = slang_candidates("哈哈哈 好的 谢谢");
        assert!(c.is_empty(), "口头禅不收：{c:?}");
    }

    #[test]
    fn 定义抽取更准() {
        assert_eq!(mine_definitions("yyds就是永远的神"), vec![("yyds".into(), "永远的神".into())]);
        assert_eq!(mine_definitions("zmd指的是终末地"), vec![("zmd".into(), "终末地".into())]);
        assert_eq!(mine_definitions("猪猪的意思是对你的爱称"), vec![("猪猪".into(), "对你的爱称".into())]);
        // 以前会误收的
        for t in [
            "商店是每周50",        // 普通名词 + 「是」：在描述事实，不是定义
            "高达是高达啊",          // 自我重复 + 语气词
            "拱我白菜是吧",          // 语气
            "zmd是什么意思",         // 疑问
            "如果我是群主就好了",    // 假设
            "今天天气是真的好",      // 主语是时间词 / 半句话
            "这个是我的",            // 代词
            "副本就是不好打",        // 否定式描述
            "我是一个人在这边生活",  // 代词主语
            "主要是属性",            // 副词当主语
            "估计是沙箱模式",
            "红武是啥样的",          // 问句式释义
            "斧头就是丑了点",        // 评价，不是解释
            "行情是绿的",
        ] {
            assert!(mine_definitions(t).is_empty(), "不该当定义：{t} → {:?}", mine_definitions(t));
        }
    }

    #[test]
    fn 自述事实抽取() {
        let f = extract_facts("阿猫", "我最喜欢猫了，但是家里不让养");
        assert_eq!(f, vec![Fact { text: "阿猫最喜欢猫".into(), kind: "profile", strength: 70.0, facet: "喜欢", value: "猫".into() }]);
        let f = extract_facts("阿狗", "我在玩星痕共鸣，神盾骑士");
        assert_eq!(f[0].text, "阿狗在玩星痕共鸣");
        let f = extract_facts("小明", "我是程序员，天天加班");
        assert_eq!(f[0].text, "小明是程序员");
        let f = extract_facts("小红", "记住：周五晚上八点打团本");
        assert_eq!(f[0].kind, "note");
        assert!(f[0].text.contains("周五晚上八点打团本"));
        // 不该记的
        for t in [
            "我喜欢你",               // 对 bot 的话，不是爱好
            "我是说你别这样",          // 「我是说」
            "你喜欢猫吗",              // 不是自述
            "我是不是很傻",            // 疑问
            "如果我是你我就去了",      // 假设
            "我喜欢这样",              // 宾语没实词
            "我的手机号是13800138000", // 敏感数字（也不是「我+触发词」）
            "我是真的服了",            // 「我是真的」
            "记住我手机号13800138000", // 敏感
        ] {
            assert!(extract_facts("某人", t).is_empty(), "不该抽：{t} → {:?}", extract_facts("某人", t));
        }
    }
}



