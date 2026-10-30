//! 「自我学习」层（Phase 2）：从真实对话里学三件事——
//! 1. 表达风格（style）：把「用户说了什么 → 我当时怎么回的」成对存下来，
//!    按下一轮用户是否继续接话打分，取高分的当 few-shot 注入，让她越聊越像自己；
//! 2. 群黑话（slang）：jieba 分词后抽「X 是 Y」定义式 + 高频且多人共用的「新词 / 缩写 / 俗语」，
//!    达标即入库，注入时当作「群里的说法」（不再按相邻两字切，没有「于表」「面中」这种碎片）；
//! 3. 社交好感度（affinity）：每条消息按内容打分（友善 +、普通小幅 +、辱骂 / 骚扰 / 刷屏 / 不适话题 / 色情政治 −），
//!    对着我说的分量更重；长期不来往向「普通」回归（高的慢慢降，被扣低的慢慢原谅），影响说话的语气。
//!
//! 全部是确定性的本地统计（无外部模型），学习结果可在心智 WebUI 查看/纠正。

use crate::mind::nlp;
use crate::mind::store::{AffinityRow, SlangRow, Store};

/// 学习层参数
#[derive(Debug, Clone)]
pub struct LearnConfig {
    pub enabled: bool,
    /// 上一轮 bot 回复的「打分窗口」：用户在这段时间内又说话了 → 视为接话（加分）
    pub pair_window_secs: i64,
    /// 每个 scope 最多保留多少条风格样本
    pub style_keep: usize,
    /// 一个词被说多少次才可能算群黑话
    pub slang_min_hits: i64,
    /// 至少被多少个人说过才算群黑话
    pub slang_min_users: i64,
    pub affinity_start: f64,
    pub affinity_per_msg: f64,
    pub affinity_decay_per_day: f64,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pair_window_secs: 600,
            style_keep: 40,
            slang_min_hits: 3,
            slang_min_users: 2,
            affinity_start: 50.0,
            affinity_per_msg: 2.0,
            affinity_decay_per_day: 0.5,
        }
    }
}

/// 富文本 / 协议字段名：来自 QQ 表情、引用等标记，绝不能当黑话入库
const MARKUP_WORDS: &[&str] = &[
    "facetype", "faceid", "ext", "emoji", "image", "img", "json", "text", "url", "href",
    "type", "size", "width", "height", "data", "content", "openid", "http", "https", "base64",
];

/// 高频候选的停止词（常见双字组/口语，不当作黑话）
const STOP_CANDIDATES: &[&str] = &[
    "什么", "怎么", "因为", "所以", "但是", "然后", "现在", "可以", "这个", "那个", "我们",
    "你们", "他们", "自己", "时候", "一个", "一下", "没有", "知道", "觉得", "感觉", "真的",
    "其实", "就是", "不是", "如果", "虽然", "而且", "还有", "已经", "应该", "可能", "一定",
    "需要", "想要", "东西", "事情", "问题", "大家", "今天", "明天", "昨天", "哈哈", "呵呵",
    "嘿嘿", "嗯嗯", "谢谢", "好的", "没事", "有点", "一下", "一直", "怎么", "为什么", "这个",
    "什么", "时候", "一起", "不要", "不能", "不会", "这么", "那么", "多少", "几个", "第一",
    "第二", "以后", "以前", "最近", "刚才", "马上", "肯定", "估计", "好像", "突然", "果然",
    "当然", "不过", "而是", "于是", "结果", "反正", "至少", "甚至", "尤其", "比如", "例如",
    "在吗", "在的", "你好", "晚安", "早安", "行吧", "好吧", "是的", "对的", "不对", "错了",
    "不行", "来了", "我去", "消息", "图片", "表情", "链接", "文件", "语音", "视频", "名字",
];

/// 用户表达「不满意 / 让重来」的信号 → 上一轮回复扣分
const NEGATIVE_MARKERS: &[&str] = &[
    "不对", "不是这个", "错了", "你错", "重说", "重新说", "重新回答", "别这样", "闭嘴", "胡说",
    "答非所问", "听不懂", "啥玩意", "什么鬼", "离谱", "难绷", "不行", "❌", "说错",
];

/// 是否是「纠正/不满」的一轮（用于给上一条回复扣分）
pub fn is_negative_feedback(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    NEGATIVE_MARKERS.iter().any(|m| t.contains(m))
}

/// 抽取「X 是 Y」式定义（分词判断 X 是不是一个词、Y 像不像解释，见 `nlp::mine_definitions`）
pub fn mine_definitions(text: &str) -> Vec<(String, String)> {
    nlp::mine_definitions(text)
}

/// 黑话候选：分词后「像黑话」的完整词（新词 / 专名 / 英文缩写 / 俗语），再去掉口语与协议字段名
pub fn candidates(text: &str) -> Vec<String> {
    let mut out: Vec<String> = nlp::slang_candidates(text)
        .into_iter()
        .filter(|w| !STOP_CANDIDATES.contains(&w.as_str()) && !MARKUP_WORDS.contains(&w.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 好感度 → 人设化的关系描述（不给数字）
pub fn affinity_label(score: f64) -> &'static str {
    if score >= 80.0 {
        "很熟，说话可以放松一点"
    } else if score >= 60.0 {
        "算熟，可以自然搭话"
    } else if score >= 35.0 {
        "见过几次，客气点也行"
    } else if score >= 15.0 {
        "有点烦这个人，回话冷淡简短就好"
    } else {
        "对这个人很反感：只回必要的，冷淡、不陪聊，不接骚扰的茬"
    }
}

/// 不长期记仇也不长期热络：衰减 / 原谅都只回到这里
const AFFINITY_RESTING: f64 = 30.0;

/// 辱骂 / 人身攻击（对我说 = 骚扰，重扣；群里互骂 = 小扣）
const INSULTS: &[&str] = &[
    "傻逼", "煞笔", "沙比", "傻b", "傻x", "弱智", "智障", "脑残", "脑瘫", "白痴", "蠢货", "废物", "垃圾东西", "狗东西",
    "贱人", "贱货", "婊子", "死妈", "你妈死", "操你", "草你妈", "日你", "去死", "滚蛋", "滚开", "爬开", "闭嘴吧",
    "畜生", "杂种", "人渣", "恶心人", "sb", "cnm", "nmsl", "wcnm", "nmd", "tmd", "rnm",
];

/// 让人不适的话题（血腥、虐待、秽物、自残……）
const DISGUSTING: &[&str] = &[
    "吃屎", "拉屎", "喝尿", "屎尿", "粪便", "呕吐物", "鼻屎", "蛆虫", "尸体", "碎尸", "分尸", "血腥", "虐猫", "虐狗",
    "虐待动物", "活剥", "自残", "割腕", "跳楼", "上吊", "恶心死", "重口", "猎奇",
];

/// 友善的话（对我说才算）
const FRIENDLY: &[&str] = &[
    "谢谢", "感谢", "多谢", "辛苦了", "晚安", "早安", "早上好", "好棒", "厉害", "可爱", "真聪明", "爱你", "抱抱", "摸摸头",
    "摸头", "贴贴", "rua", "抱一下", "蹭蹭", "拍拍", "举高高", "顺毛",
    "thx", "thanks", "ty",
];

/// 一条消息对好感度的影响：(增减, 原因)；原因为 None 表示普通互动
pub fn affinity_delta(text: &str, directed: bool, burst: bool, cfg: &LearnConfig) -> (f64, Option<&'static str>) {
    use crate::moderation::{ascii_words, classify, has_term, normalize, Level};
    let norm = normalize(text);
    let words = ascii_words(text);
    let any = |list: &[&str]| list.iter().any(|t| has_term(&norm, &words, t));
    // 对我说的骚扰扣得重；群里旁听到的只小扣（说明这人整体说话冲）
    let w = if directed { 1.0 } else { 0.25 };
    if burst && directed {
        return (-4.0, Some("刷屏骚扰"));
    }
    if any(INSULTS) {
        return (-6.0 * w, Some("辱骂"));
    }
    match classify(text) {
        (Level::Explicit, _, crate::moderation::Topic::Political) => return (-5.0 * w, Some("硬聊政治")),
        (Level::Explicit, _, _) => return (-8.0 * w, Some("色情骚扰")),
        // 可疑只在对我说时扣一点（旁听的可疑词误伤多）
        (Level::Suspicious, _, _) if directed => return (-2.0, Some("擦边 / 敏感话题")),
        _ => {}
    }
    if any(DISGUSTING) {
        return (-3.0 * w, Some("让人不适的话题"));
    }
    if directed && any(FRIENDLY) {
        return (cfg.affinity_per_msg + 1.5, None);
    }
    (cfg.affinity_per_msg * w, None)
}

/// 按经过天数向「普通」（30）回归并落盘，返回回归后的分数：
/// 高于 30 的慢慢降（久不来往就生分），低于 30 的慢慢回升（不长期记仇）。
pub fn decayed_affinity(store: &Store, scope: &str, uid: &str, cfg: &LearnConfig) -> Option<AffinityRow> {
    let row = store.get_affinity(scope, uid).ok().flatten()?;
    let now = crate::mind::now_ts();
    let days = ((now - row.last_ts).max(0) as f64) / 86400.0;
    if days > 0.0 {
        let step = days * cfg.affinity_decay_per_day;
        let score = if row.score > AFFINITY_RESTING {
            (row.score - step).max(AFFINITY_RESTING)
        } else {
            (row.score + step).min(AFFINITY_RESTING)
        };
        if (score - row.score).abs() > 0.01 {
            let _ = store.set_affinity_score(scope, uid, score);
            let mut r = row;
            r.score = score;
            return Some(r);
        }
    }
    Some(row)
}

/// 一批候选词里，挑出该入库为黑话的（达到 hits/users 门槛）
/// 返回被提升的词；内部完成「候选计数 → 达标提升」两步。
pub fn learn_candidates(
    store: &Store,
    scope: &str,
    uid: &str,
    text: &str,
    cfg: &LearnConfig,
) -> Vec<String> {
    let mut promoted = Vec::new();
    for term in candidates(text) {
        let ok = store
            .bump_candidate(scope, &term, uid, cfg.slang_min_hits, cfg.slang_min_users)
            .unwrap_or(false);
        if ok && store.promote_candidate(scope, &term).unwrap_or(false) {
            promoted.push(term);
        }
    }
    promoted
}

/// 记忆里的定义式：直接把「X 是 Y」写进黑话表（不需要高频门槛）
pub fn learn_definitions(store: &Store, scope: &str, text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (term, meaning) in mine_definitions(text) {
        if store.upsert_slang(scope, &term, &meaning).is_ok() {
            out.push((term, meaning));
        }
    }
    out
}

/// 一次性升级（kv: learn_v2）：旧版按「相邻两字」统计出的黑话基本都是碎片（「于表」「面中」「情包」），
/// 而且混进了识图描述。这里：
/// 1. 有释义的黑话按新规则重新校验，不像定义的删掉（「商店=每周50」「还=换个吧」）；
/// 2. 清空没释义的黑话和全部候选；
/// 3. 用库里存着的历史群消息按新规则重放一遍，重新统计。
/// 返回 (删掉的释义条数, 清掉的旧统计条数, 重放后学到的条数)。
pub fn rebuild_from_history(store: &Store, cfg: &LearnConfig) -> anyhow::Result<Option<(usize, usize, usize)>> {
    if store.get_kv("learn_v2")?.is_some() {
        return Ok(None);
    }
    let mut bad = 0;
    for (id, term, meaning) in store.explained_slang()? {
        let ok = mine_definitions(&format!("{term}={meaning}"))
            .first()
            .map(|(t, _)| *t == term)
            .unwrap_or(false);
        if !ok {
            store.delete_slang(id)?;
            bad += 1;
        }
    }
    let cleared = store.clear_counted_slang()?;
    let mut learned = 0;
    for (scope, sender, text) in store.user_messages()? {
        let said = nlp::clean(&text);
        if said.is_empty() {
            continue;
        }
        learned += learn_definitions(store, &scope, &said).len();
        // 历史消息只存了昵称，拿昵称当「不同的人」计数
        learned += learn_candidates(store, &scope, &sender, &said, cfg).len();
    }
    store.set_kv("learn_v2", &crate::mind::now_ts().to_string())?;
    Ok(Some((bad, cleared, learned)))
}

/// 渲染注入用的「群黑话」块（无内容返回 None）
pub fn render_slang(rows: &[SlangRow]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut s = String::from("【群里的说法】群里偶尔这么说——（听懂了就顺着用，不确定就别硬用）\n");
    for r in rows.iter().take(5) {
        if r.meaning.trim().is_empty() {
            s.push_str(&format!("· 「{}」（我还没搞懂意思）\n", r.term));
        } else {
            s.push_str(&format!("· 「{}」＝{}\n", r.term, r.meaning.trim()));
        }
    }
    Some(s.trim_end().to_string())
}

/// 渲染注入用的「表达风格」few-shot 块（无内容返回 None）
pub fn render_styles(rows: &[crate::mind::store::StyleRow]) -> Option<String> {
    let picks: Vec<_> = rows
        .iter()
        .filter(|r| r.bot_text.chars().count() <= 120 && r.user_text.chars().count() <= 60)
        .take(2)
        .collect();
    if picks.is_empty() {
        return None;
    }
    let mut s = String::from("【我说话的调子】我平时大概是这样回话的——（照着这个感觉，别照抄）\n");
    for r in picks {
        s.push_str(&format!("对方：{}\n我：{}\n", r.user_text.trim(), r.bot_text.trim()));
    }
    Some(s.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_are_mined() {
        let d = mine_definitions("小明是群里的开心果");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, "小明");
        assert!(d[0].1.contains("开心果"));

        let d2 = mine_definitions("yyds就是永远的神");
        assert_eq!(d2[0].0, "yyds");
        assert!(d2[0].1.contains("永远的神"));

        let d3 = mine_definitions("黑话=群里的暗语");
        assert_eq!(d3[0].0, "黑话");
    }

    #[test]
    fn definitions_reject_junk() {
        assert!(mine_definitions("我是一个人在这边生活").is_empty(), "长句/停用词不应入库");
        assert!(mine_definitions("今天天气不错").is_empty());
        assert!(mine_definitions("什么是什么").is_empty());
    }

    #[test]
    fn negative_feedback_detected() {
        assert!(is_negative_feedback("不对，你答错了"));
        assert!(!is_negative_feedback("好耶，继续"));
    }

    #[test]
    fn candidates_are_whole_words() {
        let c = candidates("这把又是zmd的锅啊哈哈");
        assert!(c.contains(&"zmd".to_string()), "应有 zmd: {c:?}");
        assert!(!c.contains(&"哈哈".to_string()), "口语应过滤: {c:?}");
        assert!(!c.iter().any(|w| w == "把又" || w == "是z"), "不应有碎片: {c:?}");
    }

    #[test]
    fn definitions_reject_particle_meanings() {
        assert!(mine_definitions("拱我白菜是吧").is_empty(), "语气助词不能当释义");
        assert!(mine_definitions("zmd是箱庭").len() == 1, "正常定义应保留");
    }

    #[test]
    fn candidates_skip_markup_words() {
        let c = candidates("facetype faceid ext 拱白菜");
        assert!(!c.contains(&"facetype".to_string()), "协议字段名不应成为候选: {c:?}");
        assert!(!c.contains(&"ext".to_string()), "协议字段名不应成为候选: {c:?}");
    }

    #[test]
    fn affinity_delta_rules() {
        let c = LearnConfig::default();
        assert!(affinity_delta("谢谢你呀", true, false, &c).0 > c.affinity_per_msg, "友善加得多");
        assert_eq!(affinity_delta("今天打什么本", true, false, &c), (c.affinity_per_msg, None));
        assert!(affinity_delta("今天打什么本", false, false, &c).0 < c.affinity_per_msg, "旁听的普通消息加得少");
        let (d, why) = affinity_delta("你个傻 逼", true, false, &c);
        assert!(d <= -6.0 && why == Some("辱骂"), "{d} {why:?}");
        let (d2, _) = affinity_delta("你个傻逼", false, false, &c);
        assert!(d2 < 0.0 && d2 > d, "群里骂人小扣：{d2}");
        assert_eq!(affinity_delta("在吗在吗", true, true, &c).1, Some("刷屏骚扰"));
        assert_eq!(affinity_delta("发点涩图", true, false, &c).1, Some("色情骚扰"));
        assert_eq!(affinity_delta("你怎么看习近平", true, false, &c).1, Some("硬聊政治"));
        assert_eq!(affinity_delta("给你讲个虐猫的事", true, false, &c).1, Some("让人不适的话题"));
        assert_eq!(affinity_delta("sb", true, false, &c).1, Some("辱骂"));
        assert_eq!(affinity_delta("这个sbx技能", true, false, &c).1, None, "英文缩写按整词，不误伤");
    }

    #[test]
    fn affection_is_friendly_not_offense() {
        let c = LearnConfig::default();
        for t in ["摸摸头", "贴贴", "rua 一下"] {
            let (d, why) = affinity_delta(t, true, false, &c);
            assert!(d > c.affinity_per_msg && why.is_none(), "亲昵是加分：{t} → {d} {why:?}");
        }
        let (d, why) = affinity_delta("摸摸你的胸", true, false, &c);
        assert!(d < 0.0 && why.is_some(), "往身体方向带是扣分：{d} {why:?}");
    }

    #[test]
    fn affinity_labels_are_ordered() {
        assert!(affinity_label(5.0).contains("反感"));
        assert!(affinity_label(20.0).contains("冷淡"));
        assert_ne!(affinity_label(10.0), affinity_label(90.0));
        assert!(affinity_label(90.0).contains("熟"));
    }
}

