//! 记忆账本：本地 SQL 里的两层记忆，按 QQ 号（uid）把各个窗口串起来。
//!
//! - **未压缩区** = `messages` 表：每条消息原样入账，带说话人 uid；bot 的回复记下在回谁（reply_uid）。
//! - **已压缩区** = 夜间整理的结果：每个窗口的前情 / 记忆（`memories` 表，见 compact.rs），
//!   和每个人的长期印象（`digest` 表，kind = person，key = uid）。
//!
//! 每轮回复不再续 DSH 长会话，而是现场从库里组装上下文（长度有上限，token 不会越聊越多）：
//! 1. 【这里刚才】本窗口最近的未压缩消息——代替 DSH 会话历史；
//! 2. 【TA 最近在别处】同一个 uid 最近几天在别的群 / 私聊里和我的往来——跨窗口接得上话；
//! 3. 【对 TA 的长期印象】已压缩区里这个人的摘要。
//!
//! 夜里先把今天说过话的人各自的往来整合进长期印象（旧印象 + 新往来 → 新印象），
//! 再压缩各窗口，压完释放未压缩区（只留每个窗口最近几条接话用）。

use super::store::{RawLine, Store};
use super::{now_ts, Mind};

/// 【这里刚才】最多几条 / 几个字
const HERE_LINES: usize = 24;
const HERE_CHARS: usize = 1600;
/// 【TA 最近在别处】最多几条 / 几个字 / 回看几天
const TRAIL_LINES: usize = 12;
const TRAIL_CHARS: usize = 700;
const TRAIL_DAYS: i64 = 3;
/// 单条消息最多保留几个字
const LINE_CHARS: usize = 90;
/// 压缩后每个窗口留几条未压缩消息接话
pub const KEEP_TAIL: usize = 20;
/// 长期印象最多几个字
const PERSON_CHARS: usize = 160;
/// 整理一个人时最多给模型看几个字的往来
const PERSON_MATERIAL_CHARS: usize = 700;

/// 窗口的口头叫法（给模型看，不暴露完整群号）
pub fn window_label(scope: &str) -> String {
    if let Some(rest) = scope.strip_prefix("qq-group-") {
        let gid = rest.split('#').next().unwrap_or("");
        let tail: String = gid.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        format!("群（尾号{tail}）")
    } else if scope.starts_with("qq-c2c-") {
        "私聊".to_string()
    } else {
        "别处".to_string()
    }
}

/// 「刚才 / 5 分钟前 / 3 小时前 / 昨天 / 2 天前」
pub fn ago(now: i64, ts: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=119 => "刚才".into(),
        120..=3599 => format!("{} 分钟前", d / 60),
        3600..=86_399 => format!("{} 小时前", d / 3600),
        86_400..=172_799 => "昨天".into(),
        _ => format!("{} 天前", d / 86_400),
    }
}

/// 压成一行、截到 n 字
fn clip(text: &str, n: usize) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        one.chars().take(n).collect::<String>() + "…"
    } else {
        one
    }
}

/// 从新往旧装，装到预算为止，再按时间正序输出
fn fit(lines: Vec<String>, budget: usize) -> Vec<String> {
    let mut used = 0;
    let mut kept: Vec<String> = Vec::new();
    for l in lines.into_iter().rev() {
        let n = l.chars().count() + 1;
        if used + n > budget && !kept.is_empty() {
            break;
        }
        used += n;
        kept.push(l);
    }
    kept.reverse();
    kept
}

/// 【这里刚才】
pub fn render_here(lines: &[RawLine], budget: usize) -> Option<String> {
    let rows: Vec<String> = lines
        .iter()
        .filter(|l| !l.text.trim().is_empty())
        .map(|l| format!("{}: {}", if l.is_bot { "我" } else { l.sender.as_str() }, clip(&l.text, LINE_CHARS)))
        .collect();
    let rows = fit(rows, budget);
    (!rows.is_empty()).then(|| format!("【这里刚才】这个窗口最近的聊天（按时间顺序，「我」是你自己说的）：\n{}", rows.join("\n")))
}

/// 【TA 最近在别处】：同一个 uid 在别的窗口和我的往来。在群里时，私聊里的话标「私下」
pub fn render_trail(lines: &[RawLine], here_private: bool, now: i64, budget: usize) -> Option<String> {
    let mut secret = false;
    let rows: Vec<String> = lines
        .iter()
        .filter(|l| !l.text.trim().is_empty())
        .map(|l| {
            let private = l.scope.starts_with("qq-c2c-") && !here_private;
            secret |= private;
            format!(
                "[{}·{}{}] {}：{}",
                window_label(&l.scope),
                ago(now, l.ts),
                if private { "·私下" } else { "" },
                if l.is_bot { "我" } else { "TA" },
                clip(&l.text, LINE_CHARS)
            )
        })
        .collect();
    let rows = fit(rows, budget);
    if rows.is_empty() {
        return None;
    }
    let mut out = String::from(
        "【TA 最近在别处】同一个 QQ 号最近在别的窗口和你的往来——是同一个人，可以自然地接上（比如「你上次说的…后来怎么样了」），别生硬复述：\n",
    );
    out.push_str(&rows.join("\n"));
    if secret {
        out.push_str("\n（标了「私下」的是私聊里说的，心里有数就行，在群里别说破。）");
    }
    Some(out)
}

/// 夜间整理「对一个人的长期印象」的提示词。每人一段材料，输出每人一行
pub const PERSON_PROMPT: &str = "你是 Heevi，睡前在整理对几个人的长期印象。下面每个人有两部分：之前对 TA 的印象（可能没有），\
和最近 TA 在各个群 / 私聊里和你的往来（「TA」是这个人，「我」是你）。\n\
把两部分整合成新的印象：TA 是个怎样的人、最近在做或在意什么、和你约好 / 没聊完的事、你对 TA 的感觉。\n\
只写有依据的；旧印象里被新往来推翻或过时的删掉；私聊里的隐私照写（只给你自己看）。\n\
每人一行，严格按这个格式，不要输出别的内容：\n\
【QQ号】印象（120 字以内，第三人称写 TA）\n\
没什么值得记的人就不写。";

/// 解析 `【uid】印象` 行
pub fn parse_people(reply: &str) -> Vec<(String, String)> {
    reply
        .lines()
        .filter_map(|l| {
            let l = l.trim().trim_start_matches(['-', '·', '*', ' ']);
            let rest = l.strip_prefix('【')?;
            let (uid, text) = rest.split_once('】')?;
            let uid = uid.trim();
            let text = text.trim().trim_start_matches([':', '：']).trim();
            (uid.chars().all(|c| c.is_ascii_digit()) && !uid.is_empty() && text.chars().count() >= 4)
                .then(|| (uid.to_string(), text.chars().take(PERSON_CHARS).collect()))
        })
        .collect()
}

/// 一个人的整理材料：旧印象 + `since` 以来各窗口的往来（超出预算时留最新的）
fn person_material(store: &Store, uid: &str, nick: &str, since: i64, now: i64) -> String {
    let old = store.digest("person", uid).ok().flatten().map(|(t, _)| t).unwrap_or_else(|| "无".into());
    let rows: Vec<String> = store
        .person_lines(uid, since, 200)
        .unwrap_or_default()
        .iter()
        .filter(|l| !l.text.trim().is_empty())
        .map(|l| format!("[{}·{}] {}：{}", window_label(&l.scope), ago(now, l.ts), if l.is_bot { "我" } else { "TA" }, clip(&l.text, 70)))
        .collect();
    let rows = fit(rows, PERSON_MATERIAL_CHARS);
    format!("【{uid}】（昵称：{nick}）\n之前的印象：{old}\n最近的往来：\n{}", rows.join("\n"))
}

impl Mind {
    /// 账本上下文：【这里刚才】+【TA 最近在别处】+【对 TA 的长期印象】。
    /// `linked` = 这个 uid 在别处的当前窗口；`lite` = 节能模式，预算减半。
    pub fn ledger_block(&self, scope: &str, uid: &str, linked: &[String], lite: bool) -> String {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ts();
        let k = if lite { 2 } else { 1 };
        let mut parts: Vec<String> = Vec::new();
        if !uid.is_empty() {
            if let Some((d, _)) = store.digest("person", uid).ok().flatten() {
                parts.push(format!("【对 TA 的长期印象】{d}（整理过的印象，自然地用上，别背出来。）"));
            }
            let others: Vec<String> = linked.iter().filter(|s| *s != scope).cloned().collect();
            let trail = store.trail_of(uid, &others, now - TRAIL_DAYS * 86_400, TRAIL_LINES / k).unwrap_or_default();
            if let Some(b) = render_trail(&trail, scope.starts_with("qq-c2c-"), now, TRAIL_CHARS / k) {
                parts.push(b);
            }
        }
        let here = store.raw_recent(scope, HERE_LINES / k).unwrap_or_default();
        if let Some(b) = render_here(&here, HERE_CHARS / k) {
            parts.push(b);
        }
        parts.join("\n\n")
    }

    /// 今晚要整理的人：上次整理以来说过话的（话多的在前），每人一段材料
    pub fn person_batch(&self, max_people: usize) -> (i64, Vec<(String, String)>) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ts();
        let last = store.get_kv("ledger:person").ok().flatten().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        let since = (last + 1).max(now - 36 * 3600);
        let people = store.active_people(since, max_people).unwrap_or_default();
        let batch = people.into_iter().map(|(uid, nick, _)| {
            let m = person_material(&store, &uid, &nick, since, now);
            (uid, m)
        });
        (now, batch.collect())
    }

    /// 写入新印象；`upto` = 这次整理覆盖到的时刻（下次从这之后接着整理）
    pub fn save_people(&self, people: &[(String, String)], upto: i64) -> usize {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let n = people.iter().filter(|(uid, text)| store.set_digest("person", uid, text).is_ok()).count();
        let _ = store.set_kv("ledger:person", &upto.to_string());
        n
    }

    /// 释放一个窗口的未压缩区（已经压缩进记忆的部分），返回删了几条
    pub fn free_raw(&self, scope: &str, upto: i64) -> usize {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.free_raw(scope, upto, KEEP_TAIL).unwrap_or(0)
    }

    /// 对一个人的长期印象（测试）
    #[cfg(test)]
    pub fn person_digest(&self, uid: &str) -> Option<String> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.digest("person", uid).ok().flatten().map(|(t, _)| t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mind::LearnConfig;
    use std::sync::Arc;

    fn tmp(tag: &str) -> Arc<Mind> {
        let dir = std::env::temp_dir().join(format!("ledger-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Mind::new(dir.to_str().unwrap(), 0, LearnConfig::default()).unwrap()
    }

    /// 同一个 uid：群 A 说的事，在群 B / 私聊里能看到；别人的话不跨窗口；私聊内容在群里标「私下」
    #[test]
    fn cross_window_trail() {
        let m = tmp("trail");
        let (ga, gb, dm) = ("qq-group-111#0", "qq-group-222#0", "qq-c2c-u1#0");
        m.on_incoming(ga, ga, "u1", "阿猫", "我明天要去杭州出差三天");
        m.after_reply_to(ga, ga, "我明天要去杭州出差三天", "……一路顺风。", "u1");
        m.observe(ga, ga, "u2", "阿狗", "我也想去杭州");
        m.on_incoming(dm, dm, "u1", "小林", "其实我是去面试的，别告诉别人");

        let b = m.ledger_block(gb, "u1", &[ga.to_string(), dm.to_string()], false);
        assert!(b.contains("杭州出差") && b.contains("一路顺风"), "群 A 的往来带过来：{b}");
        assert!(b.contains("群（尾号0111）") || b.contains("群（尾号111）"), "{b}");
        assert!(b.contains("面试") && b.contains("私下"), "私聊内容在群里标私下：{b}");
        assert!(!b.contains("我也想去杭州"), "别人的话不跨窗口：{b}");

        let d = m.ledger_block(dm, "u1", &[ga.to_string()], false);
        assert!(d.contains("杭州出差") && !d.contains("私下"), "{d}");
        assert!(d.contains("【这里刚才】") && d.contains("面试"), "私聊自己的近况：{d}");

        // 没有 linked：只有本窗口
        assert!(!m.ledger_block(gb, "u1", &[], false).contains("杭州"));
    }

    #[test]
    fn budgets_keep_newest() {
        let m = tmp("budget");
        let s = "qq-group-1#0";
        for i in 0..80 {
            m.observe(s, s, "u2", "阿狗", &format!("第{i}条消息，内容稍微长一点点用来占预算"));
        }
        let full = m.ledger_block(s, "u1", &[], false);
        let lite = m.ledger_block(s, "u1", &[], true);
        assert!(full.contains("第79条") && !full.contains("第10条"), "只留最新的");
        assert!(lite.chars().count() < full.chars().count() && lite.contains("第79条"));
        assert!(full.chars().count() < HERE_CHARS + 200);
    }

    /// 夜间：整理人物印象 → 压缩后释放未压缩区（留尾巴）
    #[test]
    fn person_digest_and_free() {
        let m = tmp("digest");
        let (ga, dm) = ("qq-group-1#0", "qq-c2c-u1#0");
        for i in 0..30 {
            m.observe(ga, ga, "u2", "阿狗", &format!("闲聊{i}"));
        }
        m.on_incoming(ga, ga, "u1", "阿猫", "我在准备考研");
        m.after_reply_to(ga, ga, "我在准备考研", "……加油。", "u1");
        m.on_incoming(dm, dm, "u1", "阿猫", "数学好难");
        let (upto, batch) = m.person_batch(10);
        let u1 = batch.iter().find(|(u, _)| u == "u1").map(|(_, t)| t.clone()).unwrap();
        assert!(u1.contains("考研") && u1.contains("数学好难") && u1.contains("加油") && u1.contains("之前的印象：无"), "{u1}");

        let parsed = parse_people("【u1】不是数字\n【10001】在准备考研，数学吃力，私下会抱怨。\n闲话\n- 【10002】：喜欢猫，常发猫图");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "10001");
        m.save_people(&[("u1".into(), "在准备考研，数学吃力".into())], upto);
        assert_eq!(m.person_digest("u1").as_deref(), Some("在准备考研，数学吃力"));
        assert!(m.ledger_block(ga, "u1", &[], false).contains("【对 TA 的长期印象】在准备考研"));
        // 下次整理只看 upto 之后的
        assert!(m.person_batch(10).1.is_empty());

        let freed = m.free_raw(ga, now_ts());
        assert_eq!(freed, 32 - KEEP_TAIL, "压缩后释放，只留尾巴");
        assert_eq!(m.free_raw(dm, now_ts()), 0, "不够尾巴数的窗口不删");
    }

    #[test]
    fn labels_and_ago() {
        assert_eq!(window_label("qq-group-123451302#0"), "群（尾号1302）");
        assert_eq!(window_label("qq-c2c-5#1"), "私聊");
        assert_eq!(ago(10_000, 9_990), "刚才");
        assert_eq!(ago(10_000, 10_000 - 7200), "2 小时前");
        assert_eq!(ago(200_000, 200_000 - 90_000), "昨天");
    }
}
