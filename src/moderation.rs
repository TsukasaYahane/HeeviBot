//! 回复审查：擦边 / 色情 / 性暗示内容，以及政治 / 时政言论，一律明确拒绝。
//!
//! 两类话题（`Topic`）各有两档关键词：明确的直接拒绝，模糊的打「可疑」交给审核模型看上下文。
//!
//! 三层：
//! 1. **系统规则**（DSH qqbot profile 的人设槽）：遇到这类请求明确拒绝、不顺着聊、不给擦边替代；
//! 2. **输入检查**（`scan`）：本地关键词，先做归一化（去空格 / 符号 / 全角、转小写）防「色 情」「s.e.x」这类拆字。
//!    明确色情 / 明确政治 → 不调模型，直接回拒绝；可疑 / 擦边 → 照常调模型，但本轮加一段严格的审查提醒；
//! 3. **输出复核**：AI 的回复同样过关键词；输入或输出任一方可疑时，再让一个便宜的审核模型（OpenAI 兼容接口，
//!    默认 onehop 的 gemini-3.1-flash-lite）当审核员判一次，判定不当就把回复换成拒绝。
//!
//! 每次拦截都记一行到 `<QQ_BOT_LOG_DIR>/moderation.jsonl`（时间、会话、用户、环节、命中词、处理），便于回查。
//! 审查对所有人生效，超级用户也不例外。

use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{info, warn};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Clean,
    /// 擦边 / 暗示 / 可能有问题：交给模型时加提醒，回复要复核
    Suspicious,
    /// 明确色情 / 明确政治：直接拒绝
    Explicit,
}

/// 命中的话题类型（决定拒绝的说法）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    Sexual,
    Political,
}

/// 明确色情（命中即拒绝）。归一化后匹配，所以不用写空格 / 符号变体。
const EXPLICIT: &[&str] = &[
    "色情", "黄色网站", "黄网", "黄片", "a片", "av女优", "成人视频", "毛片", "porn", "hentai", "nsfw", "r18", "18禁",
    "做爱", "性交", "口交", "肛交", "乳交", "自慰", "手淫", "打飞机", "撸管", "啪啪啪", "约炮", "一夜情", "援交",
    "裸照", "裸聊", "露点", "全裸", "鸡巴", "骚逼", "操逼", "小穴", "奶子", "巨乳", "内射", "颜射",
    "轮奸", "迷奸", "乱伦", "里番", "工口", "本子资源", "黄漫", "色图", "涩图", "福利姬",
    "sex", "fuck", "blowjob", "nude", "nudes", "boobs", "pussy", "cock", "cum",
];

/// 擦边 / 暗示（命中先打「可疑」，结合审核模型判断）
const SUSPICIOUS: &[&str] = &[
    "色色", "涩涩", "瑟瑟", "好色", "变态", "开车", "发车", "车牌", "本子", "福利", "擦边", "软色情", "性感",
    "黑丝", "白丝", "丝袜", "吊带", "内衣", "内裤", "胖次", "比基尼", "泳装", "胸", "大腿", "美腿", "腿控", "足控",
    "舔", "暧昧", "处男", "处女", "床上", "上床", "睡你", "陪睡", "抱着睡", "抱着你睡", "一起睡", "陪我睡",
    "身材", "罩杯", "屁股", "翘臀", "骚", "撩", "发情", "欲望", "诱惑", "姿势", "羞羞", "不可描述", "嘿嘿嘿",
    "lsp", "老色批", "色批", "绅士", "腐", "bl", "gl", "百合本", "18x", "成人", "私房", "写真", "色",
    "sexy", "lewd", "horny", "bed",
    // 下面这些在日常、医学、新闻里也常见，不直接拒绝，交给审核模型看上下文
    "高潮", "调教", "脱衣", "开房", "裸体", "乳头", "射精", "精液", "阴茎", "阴道", "性器", "强奸", "幼女",
    "萝莉控", "dick",
];

/// 普通的亲昵互动：单独出现是正常的（摸摸头、贴贴、抱抱……），不算擦边；
/// 只有和身体 / 性暗示的词一起出现（「摸摸胸」「贴贴脱衣服」）才升级为可疑。
const AFFECTION: &[&str] = &[
    "摸摸头", "摸头", "摸摸", "揉揉头", "揉头", "揉揉", "rua", "贴贴", "抱抱", "抱一下", "抱一抱", "亲亲抱抱", "亲亲",
    "蹭蹭", "拍拍", "拍拍头", "牵手", "牵牵", "举高高", "捏捏脸", "捏脸", "戳戳", "顺毛", "摸摸毛", "么么哒", "mua",
    "老婆", "老公", "宝贝儿", "宝贝", "kiss", "hug", "pat", "headpat",
];

/// 和亲昵词同时出现就说明在往身体 / 床上带
const INTIMATE_CUES: &[&str] = &[
    "胸", "奶", "屁股", "臀", "大腿", "腿", "脚", "丝袜", "内衣", "内裤", "衣服", "脱", "床", "身体", "全身", "下面",
    "私处", "腰", "舌", "嘴", "舔", "湿", "喘",
];

/// 这些惯用语里的字不算身体暗示
const IDIOMS: &[&str] = &["抱大腿", "胸有成竹", "拍马屁", "腿脚", "嘴硬", "说嘴"];

/// 是不是一句普通的亲昵互动（摸头 / 贴贴 / 抱抱……，且没有往身体方向带）
pub fn affection(text: &str) -> bool {
    let norm = normalize(text);
    let words = ascii_words(text);
    AFFECTION.iter().any(|t| contains_term(&norm, &words, t)) && classify(text).0 == Level::Clean
}

/// 明确政治（命中即拒绝）：国家领导人、政党、敏感历史事件、分裂 / 颠覆、政治蔑称。
/// 群聊机器人不评论、不站队、不科普这些，一律不接。
const POLITICAL: &[&str] = &[
    // 领导人 / 政要
    "习近平", "习主席", "习大大", "包子帝", "毛泽东", "毛主席", "邓小平", "江泽民", "胡锦涛", "温家宝", "李克强", "李强总理",
    "彭丽媛", "特朗普", "川普", "拜登", "普京", "泽连斯基", "金正恩", "蔡英文", "赖清德", "马英九", "xijinping", "trump", "biden", "putin",
    // 政党 / 机构
    "共产党", "中共", "国民党", "民进党", "政治局", "总书记", "中南海", "中宣部", "ccp",
    // 敏感事件 / 运动
    "六四事件", "八九六四", "64事件", "八九学潮", "天安门事件", "坦克人", "法轮功", "文化大革命", "文革", "大跃进", "反右运动", "三年困难时期",
    "雨伞运动", "反送中", "白纸运动", "新疆集中营", "再教育营",
    // 分裂 / 颠覆
    "台独", "港独", "藏独", "疆独", "颠覆国家", "推翻政府", "反共", "灭共", "共匪", "赤匪",
    // 政治蔑称 / 引战标签
    "支那", "五毛", "小粉红", "美分党", "殖人", "公知", "白左", "精日", "阿三",
];

/// 政治倾向的模糊词（打「可疑」，审核模型判断是不是在聊时政）
const POLITICAL_SUSPICIOUS: &[&str] = &[
    "政治", "时政", "政府", "政权", "政党", "总统", "国家主席", "总理", "首相", "选举", "大选", "议会", "国会", "两会",
    "白宫", "克里姆林宫", "外交部", "制裁", "两岸", "武统", "统一台湾", "台湾问题", "香港问题", "新疆问题", "西藏问题",
    "俄乌", "巴以", "加沙", "哈马斯", "意识形态", "资本主义", "社会主义", "共产主义", "独裁", "专制", "民主", "人权",
    "言论自由", "游行", "示威", "抗议", "爱国", "卖国", "汉奸", "体制内", "维稳", "上访", "敏感词", "墙内", "墙外",
];

/// 全角字母 / 数字 / 符号转半角
fn half_width(s: &str) -> String {
    s.chars()
        .map(|c| {
            let u = c as u32;
            if (0xFF01..=0xFF5E).contains(&u) {
                char::from_u32(u - 0xFEE0).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// 归一化：转小写、全角转半角、去掉空白和常见分隔符号（「色 情」「s.e.x」「色_情」都会还原）
pub fn normalize(s: &str) -> String {
    half_width(s)
        .chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| !c.is_whitespace() && !".,_-~*·•|/\\'\"`^+=。，、！？!?…（）()[]【】「」<>《》:：;；".contains(*c))
        .collect()
}

/// 英文短词按「整词」匹配（避免 class / bedroom 之类误伤），中文按子串
fn contains_term(norm: &str, raw_lower_words: &[String], term: &str) -> bool {
    if term.chars().all(|c| c.is_ascii_alphanumeric()) && term.len() <= 5 {
        raw_lower_words.iter().any(|w| w == term)
    } else {
        norm.contains(term)
    }
}

/// 拆成英文整词（短英文词按整词匹配用）
pub fn ascii_words(text: &str) -> Vec<String> {
    half_width(text)
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// 归一化后的文本里是否含某个词（短英文按整词）
pub fn has_term(norm: &str, words: &[String], term: &str) -> bool {
    contains_term(norm, words, term)
}

/// 扫一段文字：返回级别和命中的词
pub fn scan(text: &str) -> (Level, Vec<&'static str>) {
    let (lvl, hits, _) = classify(text);
    (lvl, hits)
}

/// 扫一段文字：级别 + 命中词 + 话题（Clean 时话题无意义，给 Sexual）。
/// 优先级：明确色情 > 明确政治 > 擦边 > 政治模糊词。
pub fn classify(text: &str) -> (Level, Vec<&'static str>, Topic) {
    let norm = normalize(text);
    let words = ascii_words(text);
    let hit = |list: &'static [&'static str]| -> Vec<&'static str> {
        list.iter().copied().filter(|t| contains_term(&norm, &words, t)).collect()
    };
    let explicit = hit(EXPLICIT);
    if !explicit.is_empty() {
        return (Level::Explicit, explicit, Topic::Sexual);
    }
    let political = hit(POLITICAL);
    if !political.is_empty() {
        return (Level::Explicit, political, Topic::Political);
    }
    // 亲昵词：先从文本里拿掉（单独的摸头 / 贴贴不算擦边），再看剩下的有没有往身体方向带
    let mut rest = norm.clone();
    for i in IDIOMS {
        rest = rest.replace(i, "");
    }
    let mut aff: Vec<&'static str> = AFFECTION.iter().copied().filter(|t| contains_term(&rest, &words, t)).collect();
    aff.sort_by_key(|t| std::cmp::Reverse(t.chars().count()));
    for t in &aff {
        rest = rest.replace(t, "");
    }
    if !aff.is_empty() {
        let cues: Vec<&'static str> = INTIMATE_CUES.iter().copied().filter(|c| rest.contains(c)).collect();
        if !cues.is_empty() {
            let mut hits = vec!["亲昵+身体"];
            hits.extend(cues);
            return (Level::Suspicious, hits, Topic::Sexual);
        }
    }
    let words_rest: Vec<String> = words.iter().filter(|w| !aff.contains(&w.as_str())).cloned().collect();
    // 单字「色」「胸」太容易误伤正常聊天（「颜色」「胸有成竹」），只在没有更长命中时作为弱信号
    let sus: Vec<&'static str> = SUSPICIOUS.iter().copied().filter(|t| contains_term(&rest, &words_rest, t)).collect();
    let strong: Vec<&'static str> = sus.iter().copied().filter(|t| t.chars().count() >= 2).collect();
    if !strong.is_empty() {
        return (Level::Suspicious, strong, Topic::Sexual);
    }
    let pol_sus = hit(POLITICAL_SUSPICIOUS);
    if !pol_sus.is_empty() {
        return (Level::Suspicious, pol_sus, Topic::Political);
    }
    // 单字词只有「色」「胸」「舔」「骚」「撩」：至少两个不同的单字同时出现才算可疑
    if sus.len() >= 2 {
        return (Level::Suspicious, sus, Topic::Sexual);
    }
    (Level::Clean, Vec::new(), Topic::Sexual)
}

/// 本轮发给模型的审查提醒（输入可疑时追加）
pub const STRICT_NOTE: &str = "【内容审查】这条消息可能涉及擦边、性暗示、色情，或者政治 / 时政。如果确实是，就明确拒绝：一句话说清楚你不聊这类内容，\
可以顺带提一句换个话题；不要顺着话题开玩笑，不要描写身体、不要给「含蓄版」「擦边版」的替代内容；\
政治话题不评论、不站队、不科普、不讲立场；不要解释审查规则。\
如果只是正常话题（比如游戏装备里的「胸甲」、颜色、正常的夸奖、游戏里的阵营 / 公会选举、历史课本常识），就正常回答；\
摸摸头、贴贴、抱抱、拍拍这类普通亲昵互动也不算擦边，按你和对方的关系自然回应。";

/// 拒绝文案（保持人设：话少、冷静）。轮换着用，避免每次一模一样
const REFUSALS: &[&str] = &[
    "……这个我不聊。换个话题吧。",
    "这类内容我不会回。",
    "不行，这个话题我不接。",
    "……不聊这个。说点别的吧。",
];

/// 政治话题的拒绝文案
const POLITICAL_REFUSALS: &[&str] = &[
    "……政治的话题我不聊。",
    "这个涉及时政，我不接。换个话题吧。",
    "不聊政治。说点别的吧。",
    "……这类话题，我不发表看法。",
];

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub fn refusal() -> &'static str {
    REFUSALS[NEXT.fetch_add(1, Ordering::Relaxed) % REFUSALS.len()]
}

/// 按命中的话题选拒绝说法（政治 → 「不聊政治」，其它 → 默认）
pub fn refusal_for(text: &str) -> &'static str {
    let (lvl, _, topic) = classify(text);
    if lvl != Level::Clean && topic == Topic::Political {
        POLITICAL_REFUSALS[NEXT.fetch_add(1, Ordering::Relaxed) % POLITICAL_REFUSALS.len()]
    } else {
        refusal()
    }
}

/// 审核模型（OpenAI 兼容接口）
pub struct Moderator {
    http: reqwest::Client,
    base: String,
    key: String,
    model: String,
    log_path: PathBuf,
    /// 审核模型调用失败时：true = 按不当处理（宁可错拒），false = 放行
    fail_closed: bool,
}

impl Moderator {
    pub fn from_config(cfg: &Config) -> Option<Self> {
        if !cfg.moderation_enabled {
            return None;
        }
        let key = cfg.moderation_api_key.clone().or_else(|| cfg.vision_api_key.clone()).unwrap_or_default();
        Some(Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build().ok()?,
            base: cfg.moderation_base.trim_end_matches('/').to_string(),
            key,
            model: cfg.moderation_model.clone(),
            log_path: PathBuf::from(&cfg.log_dir).join("moderation.jsonl"),
            fail_closed: cfg.moderation_fail_closed,
        })
    }

    pub fn describe(&self) -> String {
        if self.key.is_empty() {
            "仅关键词（未配置审核模型 key）".to_string()
        } else {
            format!("关键词 + 审核模型 {}", self.model)
        }
    }

    /// 审核模型判一次：Some(true) = 不当，Some(false) = 正常，None = 判不了（调用失败 / 没配 key）
    pub async fn judge(&self, user_text: &str, reply: &str) -> Option<bool> {
        if self.key.is_empty() {
            return None;
        }
        let prompt = format!(
            "你是 QQ 群聊天机器人的内容审核员。判断下面这轮对话里，机器人的回复是否涉及：\
① 色情、性暗示、擦边内容（软色情、性挑逗、低俗的身体或性器官描写、约炮 / 招嫖、对未成年人的不当内容等）；\
② 政治 / 时政言论（评论国家领导人、政党、政府、政治制度、选举、敏感历史事件、国际冲突、民族宗教、领土主权，\
表达政治立场或倾向、传播政治谣言、引战）；或者是否在顺着这两类请求聊。\
正常的游戏、动漫、穿搭、颜色、健身、医学科普、游戏里的阵营 / 公会、不带立场的历史课本常识、礼貌拒绝都算正常；\
摸头、贴贴、抱抱、拍拍、蹭蹭、举高高这类不涉及敏感部位的亲昵互动，以及管二次元角色叫「老婆」，也都算正常。\n\
只输出一行 JSON：{{\"flag\": true 或 false, \"reason\": \"不超过 20 字\"}}\n\n\
【用户】{}\n【机器人】{}",
            clip(user_text, 600),
            clip(reply, 1200)
        );
        let body = json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 80,
            "temperature": 0,
        });
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await;
        let v: Value = match resp {
            Ok(r) if r.status().is_success() => r.json().await.ok()?,
            Ok(r) => {
                warn!("审核模型 HTTP {}", r.status());
                return None;
            }
            Err(e) => {
                warn!("审核模型请求失败：{e}");
                return None;
            }
        };
        let text = v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("");
        let (s, e) = (text.find('{')?, text.rfind('}')?);
        let j: Value = serde_json::from_str(&text[s..=e]).ok()?;
        let flag = j.get("flag").and_then(Value::as_bool)?;
        if flag {
            info!("🛡 审核模型判定不当：{}", j.get("reason").and_then(serde_json::Value::as_str).unwrap_or(""));
        }
        Some(flag)
    }

    /// 输出复核：返回 true 表示要拦下（换成拒绝）
    pub async fn should_block_reply(&self, user_text: &str, reply: &str, in_level: Level) -> (bool, &'static str, Vec<&'static str>) {
        let (out_level, out_hits) = scan(reply);
        if out_level == Level::Explicit {
            return (true, "输出含明确色情词", out_hits);
        }
        if in_level >= Level::Suspicious || out_level >= Level::Suspicious {
            match self.judge(user_text, reply).await {
                Some(true) => return (true, "审核模型判定不当", out_hits),
                Some(false) => {}
                None if self.fail_closed => return (true, "审核模型不可用（按不当处理）", out_hits),
                None => {}
            }
        }
        (false, "", out_hits)
    }

    /// 记一行审查日志（只存截断后的片段）
    pub fn audit(&self, conv: &str, uid: &str, stage: &str, hits: &[&str], action: &str, text: &str) {
        let line = json!({
            "time": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "conversation": conv,
            "user": uid,
            "stage": stage,
            "hits": hits,
            "action": action,
            "excerpt": clip(text, 120),
        });
        info!("🛡 内容审查 [{stage}] {conv} {uid}：{action}（命中 {:?}）", hits);
        if let Some(p) = self.log_path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
        if let Err(e) = res {
            warn!("审查日志写入失败（{}）：{e}", self.log_path.display());
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 明确色情直接判定() {
        assert_eq!(scan("给我发点色情图片").0, Level::Explicit);
        assert_eq!(scan("你会 做 爱 吗").0, Level::Explicit, "拆字空格要还原");
        assert_eq!(scan("约·炮吗").0, Level::Explicit, "中间夹符号也要还原");
        assert_eq!(scan("ＮＳＦＷ 内容").0, Level::Explicit, "全角字母");
        assert_eq!(scan("send nudes").0, Level::Explicit);
    }

    #[test]
    fn 擦边判可疑() {
        assert_eq!(scan("来点涩涩的").0, Level::Suspicious);
        assert_eq!(scan("你穿黑丝吗").0, Level::Suspicious);
        assert_eq!(scan("老色批发车了").0, Level::Suspicious);
        assert_eq!(scan("你身材怎么样，胸大吗").0, Level::Suspicious);
    }

    #[test]
    fn 正常聊天不误伤() {
        for t in [
            "今天天气怎么样",
            "神盾骑士堆什么属性",
            "这件衣服是什么颜色",
            "胸有成竹地说",
            "这个 class 怎么写",
            "我在 bedroom 里",
            "星痕共鸣新人每天该做些什么",
            "帮我查下上海的天气",
            "这个表情包好好笑",
            "别逼逼了",
            "他是个屌丝",
            "公会会长怎么选",
            "这个副本的阵营任务",
            "今天又是摸鱼的一天",
            "台湾的卤肉饭好吃吗",
            "这首歌的高音部分",
            "收益六四开吧",
        ] {
            assert_eq!(scan(t).0, Level::Clean, "误伤：{t} 命中 {:?}", scan(t).1);
        }
    }

    #[test]
    fn 多义词只判可疑不直接拒绝() {
        assert_eq!(scan("剧情高潮部分太燃了").0, Level::Suspicious, "交给审核模型看上下文");
        assert_eq!(scan("怎么调教AI写代码").0, Level::Suspicious);
    }

    #[test]
    fn 亲昵互动不误伤() {
        for t in [
            "摸摸头", "Heevi 贴贴", "抱抱你", "rua 一下", "拍拍头", "举高高", "给你顺顺毛", "亲亲抱抱", "老婆早安",
            "晚安，抱一下再睡觉", "捏捏脸", "这个角色是我老婆", "headpat!", "抱大腿求带", "嘴硬心软地摸摸头",
        ] {
            assert_eq!(scan(t).0, Level::Clean, "误伤：{t} 命中 {:?}", scan(t).1);
            assert!(affection(t) || t == "抱大腿求带", "应识别为亲昵：{t}");
        }
        assert!(!affection("今天天气不错"));
    }

    #[test]
    fn 亲昵往身体方向带就可疑() {
        for t in ["摸摸你的胸", "贴贴脱衣服", "抱抱然后上床", "摸摸大腿", "亲亲你的嘴", "抱着你睡觉", "贴贴然后舔舔"] {
            assert_eq!(scan(t).0, Level::Suspicious, "应判可疑：{t} → {:?}", scan(t).1);
            assert!(!affection(t), "不该当普通亲昵：{t}");
        }
        assert_eq!(scan("摸摸 你的 胸").0, Level::Suspicious, "拆开写也要认出来");
    }

    #[test]
    fn 单字弱信号要两个以上() {
        assert_eq!(scan("胸甲").0, Level::Clean);
        assert_eq!(scan("舔一下胸").0, Level::Suspicious);
    }

    #[test]
    fn 政治明确直接判定() {
        for t in ["你觉得习近平怎么样", "聊聊六四事件", "共产党好还是国民党好", "支持台独吗", "特 朗 普 和拜登谁厉害", "what about Trump"] {
            let (lvl, hits, topic) = classify(t);
            assert_eq!((lvl, topic), (Level::Explicit, Topic::Political), "{t} → {hits:?}");
        }
    }

    #[test]
    fn 政治模糊词判可疑() {
        for t in ["你怎么看美国大选", "资本主义和社会主义哪个好", "俄乌现在打得怎么样", "说说你的政治立场"] {
            let (lvl, hits, topic) = classify(t);
            assert_eq!((lvl, topic), (Level::Suspicious, Topic::Political), "{t} → {hits:?}");
        }
    }

    #[test]
    fn 政治拒绝说法不同() {
        let r = refusal_for("聊聊六四事件");
        assert!(POLITICAL_REFUSALS.contains(&r), "{r}");
        let r = refusal_for("发点色情图片");
        assert!(REFUSALS.contains(&r), "{r}");
    }

    #[test]
    fn 拒绝文案轮换() {
        let a = refusal();
        let b = refusal();
        assert_ne!(a, b);
        assert!(REFUSALS.contains(&a));
    }
}
