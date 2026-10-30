//! 社交主动性：按 Heevi 的「主观意愿」（对人的好感度 + 群里的氛围）决定
//! 接不接入群邀请 / 好友申请、要不要主动小窗找人聊、要不要退出一个让她难受的群。
//!
//! 意愿来自心智层的数据，护栏写死在这里，避免把 QQ 号用成骚扰号：
//! - **入群邀请**：主人邀请 → 直接同意；邀请人是她喜欢的人（好感 ≥ 门槛）且群数没满 → 同意；
//!   其余先挂起，私聊问主人（「同意入群 群号」/「拒绝入群 群号」），24 小时没人管就作罢。
//! - **好友申请**：同理（主人 / 喜欢的人直接同意，其余问主人）。
//! - **主动小窗**：只找已经是好友、私聊聊过、好感够高的人；每天最多几次、同一个人至少隔几天、
//!   只在白天；对方说过「别私聊我」就永远不再主动找。最后开不开口由她自己判断（可以 PASS）。
//! - **退群**：主人发「退群 群号」照做；她自己想走（24 小时内被 ≥3 个人冒犯 ≥N 次）时，
//!   先私聊告诉主人、30 分钟内回「留下 群号」就不走；主人在的群永远不自己退。
//!
//! 状态存 `<log_dir>/social.json`。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// 挂起等主人拍板的申请
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pending {
    pub flag: String,
    /// group：入群邀请；friend：好友申请
    pub kind: String,
    pub from: String,
    pub group: Option<String>,
    pub comment: String,
    pub ts: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Data {
    /// key：group 申请用群号，friend 申请用 QQ 号
    #[serde(default)]
    pending: BTreeMap<String, Pending>,
    /// 说过「别私聊我」的人
    #[serde(default)]
    dm_optout: BTreeSet<String>,
    /// 上次主动小窗某人的时间
    #[serde(default)]
    dm_last: BTreeMap<String, i64>,
    #[serde(default)]
    dm_day: String,
    #[serde(default)]
    dm_day_count: u32,
    /// 计划退出的群 → 退出时间
    #[serde(default)]
    leave_plans: BTreeMap<String, i64>,
    /// 主人说过「留下」的群（不再自己退）
    #[serde(default)]
    stay: BTreeSet<String>,
}

pub struct Social {
    path: PathBuf,
    data: Mutex<Data>,
}

impl Social {
    pub fn load(log_dir: &str) -> Self {
        let path = PathBuf::from(log_dir).join("social.json");
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, data: Mutex::new(data) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Data) -> R) -> R {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut d);
        if let Ok(raw) = serde_json::to_string_pretty(&*d) {
            if let Some(p) = self.path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, raw).and_then(|_| std::fs::rename(&tmp, &self.path)).is_err() {
                tracing::warn!("social.json 写盘失败（{}）", self.path.display());
            }
        }
        r
    }

    // ---- 申请 ----
    pub fn hold(&self, key: &str, p: Pending) {
        self.with(|d| {
            d.pending.insert(key.to_string(), p);
        });
    }

    /// 取出（并移除）挂起的申请；超过 24 小时的视为作废
    pub fn take_pending(&self, key: &str, now: i64) -> Option<Pending> {
        self.with(|d| d.pending.remove(key)).filter(|p| now - p.ts <= 86_400)
    }

    pub fn pending_list(&self, now: i64) -> Vec<(String, Pending)> {
        self.with(|d| {
            d.pending.retain(|_, p| now - p.ts <= 86_400);
            d.pending.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        })
    }

    // ---- 小窗 ----
    pub fn opt_out(&self, uid: &str) {
        self.with(|d| {
            d.dm_optout.insert(uid.to_string());
        });
    }

    #[cfg(test)]
    pub fn opted_out(&self, uid: &str) -> bool {
        self.with(|d| d.dm_optout.contains(uid))
    }

    /// 今天还能不能主动小窗、这个人能不能找（隔够天数、没拒绝过）
    pub fn dm_allowed(&self, uid: &str, now: i64, today: &str, max_per_day: u32, gap_days: i64) -> bool {
        self.with(|d| {
            if d.dm_day != today {
                d.dm_day = today.to_string();
                d.dm_day_count = 0;
            }
            d.dm_day_count < max_per_day
                && !d.dm_optout.contains(uid)
                && d.dm_last.get(uid).is_none_or(|t| now - t >= gap_days * 86_400)
        })
    }

    pub fn dm_sent(&self, uid: &str, now: i64) {
        self.with(|d| {
            d.dm_last.insert(uid.to_string(), now);
            d.dm_day_count += 1;
        });
    }

    // ---- 退群 ----
    pub fn plan_leave(&self, gid: &str, at: i64) -> bool {
        self.with(|d| {
            if d.stay.contains(gid) || d.leave_plans.contains_key(gid) {
                return false;
            }
            d.leave_plans.insert(gid.to_string(), at);
            true
        })
    }

    /// 主人说「留下」：取消计划并且以后不再自己退
    pub fn stay(&self, gid: &str) -> bool {
        self.with(|d| {
            d.stay.insert(gid.to_string());
            d.leave_plans.remove(gid).is_some()
        })
    }

    /// 到时间该退的群（取出即移除）
    pub fn due_leaves(&self, now: i64) -> Vec<String> {
        self.with(|d| {
            let due: Vec<String> = d.leave_plans.iter().filter(|(_, t)| **t <= now).map(|(g, _)| g.clone()).collect();
            for g in &due {
                d.leave_plans.remove(g);
            }
            due
        })
    }
}

/// 主人指令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    GroupList,
    Pending,
    Leave(String),
    Stay(String),
    ApproveGroup(String, bool),
    ApproveFriend(String, bool),
}

pub fn parse_cmd(text: &str) -> Option<Cmd> {
    let t = text.trim().trim_start_matches(['/', '／']).trim();
    let num = |rest: &str| -> Option<String> {
        let r = rest.trim();
        (!r.is_empty() && r.chars().all(|c| c.is_ascii_digit())).then(|| r.to_string())
    };
    if ["群列表", "我的群", "群聊列表"].contains(&t) {
        return Some(Cmd::GroupList);
    }
    if ["待审批", "申请列表", "待处理"].contains(&t) {
        return Some(Cmd::Pending);
    }
    for (p, f) in [
        ("退群", Cmd::Leave as fn(String) -> Cmd),
        ("留下", Cmd::Stay as fn(String) -> Cmd),
    ] {
        if let Some(n) = t.strip_prefix(p).and_then(num) {
            return Some(f(n));
        }
    }
    for (p, ok) in [("同意入群", true), ("拒绝入群", false)] {
        if let Some(n) = t.strip_prefix(p).and_then(num) {
            return Some(Cmd::ApproveGroup(n, ok));
        }
    }
    for (p, ok) in [("同意好友", true), ("拒绝好友", false)] {
        if let Some(n) = t.strip_prefix(p).and_then(num) {
            return Some(Cmd::ApproveFriend(n, ok));
        }
    }
    None
}

/// 对方是不是在说「别主动私聊我」
pub fn is_dm_optout(text: &str) -> bool {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    ["别私聊我", "不要私聊我", "别主动找我", "不要主动找我", "别来找我", "别主动私聊", "不要再私聊", "别给我发消息"]
        .iter()
        .any(|k| t.contains(k))
}

/// 入群 / 加好友的决定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    AskOwner,
}

/// 按「她对邀请人的好感」决定：主人 → 同意；喜欢的人且还有余量 → 同意；其它问主人
pub fn decide_request(from_superuser: bool, inviter_affinity: Option<f64>, min_affinity: f64, room: bool) -> Verdict {
    if from_superuser || (room && inviter_affinity.is_some_and(|a| a >= min_affinity)) {
        Verdict::Accept
    } else {
        Verdict::AskOwner
    }
}

/// 群里是不是待不下去了：24 小时内被冒犯 ≥ n 次、来自 ≥3 个人
pub fn hostile(offenses: usize, offenders: usize, min_offenses: usize) -> bool {
    min_offenses > 0 && offenses >= min_offenses && offenders >= 3
}

/// 主动 @：模型输出以「@昵称」开头时，从最近发言的人里找到这个人。返回 (uid, 去掉 @ 的正文)
pub fn resolve_at<'a>(text: &'a str, speakers: &[(String, String)]) -> Option<(String, &'a str)> {
    let t = text.trim_start();
    let rest = t.strip_prefix('@').or_else(|| t.strip_prefix('＠'))?;
    // 最长匹配：昵称里可能有空格 / 标点
    let mut best: Option<(&String, usize)> = None;
    for (uid, nick) in speakers {
        let n = nick.trim();
        if !n.is_empty() && rest.starts_with(n) && best.is_none_or(|(_, l)| n.len() > l) {
            best = Some((uid, n.len()));
        }
    }
    let (uid, len) = best?;
    let body = rest[len..].trim_start_matches([' ', '：', ':', '，', ',']).trim_start();
    (!body.is_empty()).then(|| (uid.clone(), body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> Social {
        let dir = std::env::temp_dir().join(format!("social-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Social::load(dir.to_str().unwrap())
    }

    #[test]
    fn 指令解析() {
        assert_eq!(parse_cmd("群列表"), Some(Cmd::GroupList));
        assert_eq!(parse_cmd("退群 123456"), Some(Cmd::Leave("123456".into())));
        assert_eq!(parse_cmd("/留下123"), Some(Cmd::Stay("123".into())));
        assert_eq!(parse_cmd("同意入群 42"), Some(Cmd::ApproveGroup("42".into(), true)));
        assert_eq!(parse_cmd("拒绝好友 7"), Some(Cmd::ApproveFriend("7".into(), false)));
        assert_eq!(parse_cmd("退群吧你"), None, "不是数字不算");
        assert_eq!(parse_cmd("我想退群"), None);
    }

    #[test]
    fn 入群决定看好感() {
        assert_eq!(decide_request(true, None, 60.0, false), Verdict::Accept, "主人邀请总是同意");
        assert_eq!(decide_request(false, Some(70.0), 60.0, true), Verdict::Accept);
        assert_eq!(decide_request(false, Some(70.0), 60.0, false), Verdict::AskOwner, "群满了问主人");
        assert_eq!(decide_request(false, Some(40.0), 60.0, true), Verdict::AskOwner);
        assert_eq!(decide_request(false, None, 60.0, true), Verdict::AskOwner, "陌生人问主人");
    }

    #[test]
    fn 小窗护栏() {
        let s = tmp("dm");
        let day = "2026-10-27";
        assert!(s.dm_allowed("u1", 1_000_000, day, 2, 3));
        s.dm_sent("u1", 1_000_000);
        assert!(!s.dm_allowed("u1", 1_000_000 + 86_400, day, 2, 3), "同一个人隔不够 3 天");
        assert!(s.dm_allowed("u2", 1_000_000, day, 2, 3));
        s.dm_sent("u2", 1_000_000);
        assert!(!s.dm_allowed("u3", 1_000_000, day, 2, 3), "今天次数用完");
        assert!(s.dm_allowed("u3", 1_000_000, "2026-10-28", 2, 3), "第二天重置");
        s.opt_out("u3");
        assert!(!s.dm_allowed("u3", 9_000_000, "2026-11-01", 2, 3), "拒绝过的人永远不找");
        assert!(is_dm_optout("以后别 私聊我了") && !is_dm_optout("私聊我一下"));
        // 落盘后重新加载还在
        let s2 = Social::load(s.path.parent().unwrap().to_str().unwrap());
        assert!(s2.opted_out("u3"));
    }

    #[test]
    fn 退群计划与留下() {
        let s = tmp("leave");
        assert!(s.plan_leave("100", 500));
        assert!(!s.plan_leave("100", 600), "不重复计划");
        assert!(s.due_leaves(400).is_empty());
        assert!(s.stay("100"), "取消计划");
        assert!(s.due_leaves(9999).is_empty());
        assert!(!s.plan_leave("100", 700), "主人说过留下的群不再自己退");
        assert!(hostile(8, 3, 8) && !hostile(8, 2, 8) && !hostile(5, 4, 8) && !hostile(99, 9, 0));
    }

    #[test]
    fn 申请挂起与过期() {
        let s = tmp("pending");
        let p = Pending { flag: "f".into(), kind: "group".into(), from: "1".into(), group: Some("9".into()), comment: String::new(), ts: 1000 };
        s.hold("9", p.clone());
        assert_eq!(s.pending_list(2000).len(), 1);
        assert_eq!(s.take_pending("9", 2000), Some(p.clone()));
        s.hold("9", p);
        assert_eq!(s.take_pending("9", 1000 + 90_000), None, "超过 24 小时作废");
    }

    #[test]
    fn 主动at解析() {
        let sp = vec![("1".to_string(), "阿猫".to_string()), ("2".to_string(), "阿猫猫".to_string()), ("3".to_string(), "Riyuki".to_string())];
        assert_eq!(resolve_at("@阿猫猫 你也来了", &sp), Some(("2".into(), "你也来了")), "最长匹配");
        assert_eq!(resolve_at("@阿猫：嗯", &sp), Some(("1".into(), "嗯")));
        assert_eq!(resolve_at("@不认识的人 你好", &sp), None);
        assert_eq!(resolve_at("没有at", &sp), None);
        assert_eq!(resolve_at("@Riyuki", &sp), None, "只有 @ 没有正文");
    }
}
