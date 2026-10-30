//! 心智系统（天使记忆 + 内心 + 自我学习）。
//!
//! 移植自三个 AstrBot 插件，为 QQ 机器人赋予「记忆 + 内心 + 会学」：
//! - 记忆（angel_memory）：消息入账 → BM25(CJK bigram) 召回 → 注入 DSH 上下文；
//! - 内心（angel_heart）：按会话的精力/情绪，影响注入量与建议长度；
//! - 自我学习（self_learning）：表达风格 few-shot、群黑话、社交好感度（Phase 2）。
//!
//! 数据落在 bot 根目录 `<mind_data>/`（mind.db + notes/），内嵌 axum WebUI 提供可视化管理。

pub mod compact;
pub mod heart;
pub mod insight;
pub mod learn;
pub mod ledger;
pub mod memory;
pub mod nlp;
pub mod persona;
pub mod research;
pub mod store;
pub mod sysstat;
pub mod webui;

use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use heart::HeartConfig;
pub use learn::LearnConfig;
use store::Store;

/// 「聊到记得的事」的 BM25 门槛（经验值：两三个词对上才算）
const INTEREST_MIN_SCORE: f64 = 4.0;

/// 当前 Unix 时间戳（秒）
pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 心智系统总入口（Bot 持有一个 Arc，供各异步任务共享）
pub struct Mind {
    pub store: Arc<Mutex<Store>>,
    pub heart: HeartConfig,
    pub learn: LearnConfig,
    #[allow(dead_code)]
    pub data_dir: PathBuf,
    /// 每个人最近对我说的话（刷屏检测）+ 最近一次冒犯（注入时提醒语气），只在内存里
    tracks: Mutex<HashMap<(String, String), Track>>,
    /// 对我的冒犯记录（窗口, 人, 时间），保留 48 小时：判断一个群是不是待不下去了
    offenses: Mutex<VecDeque<(String, String, i64)>>,
    _webui: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
struct Track {
    recent: VecDeque<(i64, String)>,
    offense: Option<(i64, &'static str)>,
}

/// 刷屏：60 秒内对我说 ≥5 条，或 5 分钟内同一句话说 ≥3 遍
const BURST_WINDOW: i64 = 60;
const BURST_MSGS: usize = 5;
const REPEAT_WINDOW: i64 = 300;
const REPEAT_TIMES: usize = 3;
/// 冒犯后多久内回话都带着点冷淡
const OFFENSE_MEMORY_SECS: i64 = 600;

impl Mind {
    /// 打开存储并（可选）内嵌启动 WebUI。失败不致命（返回 Err，由调用方降级）。
    pub fn new(data_dir: &str, webui_port: u16, learn: LearnConfig) -> Result<Arc<Self>> {
        let dir = PathBuf::from(data_dir);
        let store = Store::open(&dir)?;
        // 一次性：旧版「相邻两字」学出来的碎片黑话清掉，按新规则用历史消息重学
        match learn::rebuild_from_history(&store, &learn) {
            Ok(Some((bad, cleared, learned))) => tracing::info!(
                "心智学习升级：删不像定义的释义 {bad} 条、清旧统计 {cleared} 条，历史消息重学得到 {learned} 条"
            ),
            Ok(None) => {}
            Err(e) => tracing::warn!("心智学习升级失败：{e}"),
        }
        let store = Arc::new(Mutex::new(store));
        let heart = HeartConfig::default();

        let mut mind = Self {
            store: Arc::clone(&store),
            heart,
            learn,
            data_dir: dir.clone(),
            tracks: Mutex::new(HashMap::new()),
            offenses: Mutex::new(VecDeque::new()),
            _webui: None,
        };
        if webui_port > 0 {
            sysstat::set_assets_dir(dir.join("webui"));
            mind._webui = Some(webui::spawn(webui_port, Arc::clone(&store), dir.join("webui")));
        }
        Ok(Arc::new(mind))
    }

    /// 对我说的消息（被 @ / 私聊）：先给上一轮「自我表达」打分（用户接话=加分，纠正/不满=扣分），
    /// 再按内容调好感度、学黑话、抽关于这个人的事实，最后入账 user 消息。
    /// 学习只看「人打的字」（`nlp::clean` 去掉识图描述 / 表情占位 / 链接 / @），入账保留原文。
    pub fn on_incoming(&self, conv: &str, scope: &str, uid: &str, nick: &str, text: &str) {
        self.ingest(conv, scope, uid, nick, text, true);
    }

    /// 刷屏检测：记下这条，返回是否构成刷屏
    fn note_directed(&self, scope: &str, uid: &str, text: &str) -> bool {
        let now = now_ts();
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        let t = tracks.entry((scope.to_string(), uid.to_string())).or_default();
        t.recent.push_back((now, text.trim().to_string()));
        while t.recent.front().map(|(ts, _)| now - ts > REPEAT_WINDOW).unwrap_or(false) {
            t.recent.pop_front();
        }
        let fast = t.recent.iter().filter(|(ts, _)| now - ts <= BURST_WINDOW).count() >= BURST_MSGS;
        let same = t.recent.iter().filter(|(_, s)| s == text.trim()).count() >= REPEAT_TIMES;
        fast || same
    }

    fn note_offense(&self, scope: &str, uid: &str, why: &'static str) {
        let now = now_ts();
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        tracks.entry((scope.to_string(), uid.to_string())).or_default().offense = Some((now, why));
        drop(tracks);
        let mut log = self.offenses.lock().unwrap_or_else(|e| e.into_inner());
        log.push_back((scope.to_string(), uid.to_string(), now));
        while log.front().is_some_and(|(_, _, t)| now - t > 48 * 3600) {
            log.pop_front();
        }
    }

    /// 某窗口 `within` 秒内对我的冒犯：(次数, 涉及几个人)
    pub fn hostility(&self, scope: &str, within: i64) -> (usize, usize) {
        let now = now_ts();
        let log = self.offenses.lock().unwrap_or_else(|e| e.into_inner());
        let hits: Vec<&String> = log.iter().filter(|(s, _, t)| s == scope && now - t <= within).map(|(_, u, _)| u).collect();
        let people: std::collections::HashSet<&&String> = hits.iter().collect();
        (hits.len(), people.len())
    }

    /// 某人在所有窗口里的最高好感
    pub fn best_affinity(&self, uid: &str) -> Option<f64> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.best_affinity(uid).ok().flatten()
    }

    /// 这个 QQ 号出现过的所有窗口（私聊 + 各群，含旧世代；调用方按当前世代筛）
    pub fn identity_scopes(&self, uid: &str) -> Vec<String> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.identities(uid).unwrap_or_default().into_iter().map(|(s, ..)| s).collect()
    }

    /// 主动小窗候选（私聊窗口里好感够高、聊过、最近 idle_secs 没聊的人）
    pub fn dm_candidates(&self, min_score: f64, min_msgs: i64, idle_secs: i64) -> Vec<store::AffinityRow> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.dm_candidates(min_score, min_msgs, now_ts() - idle_secs).unwrap_or_default()
    }

    /// 最近 10 分钟内这人冒犯过我吗
    fn recent_offense(&self, scope: &str, uid: &str) -> Option<&'static str> {
        let tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        let (ts, why) = tracks.get(&(scope.to_string(), uid.to_string()))?.offense?;
        (now_ts() - ts <= OFFENSE_MEMORY_SECS).then_some(why)
    }

    fn ingest(&self, conv: &str, scope: &str, uid: &str, nick: &str, text: &str, directed: bool) {
        let burst = directed && !uid.is_empty() && self.note_directed(scope, uid, text);
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if self.learn.enabled {
            if directed {
                self.reward_last_turn(&store, conv, text);
            }
            if !uid.is_empty() {
                let (delta, why) = learn::affinity_delta(text, directed, burst, &self.learn);
                let _ = store.bump_affinity(scope, uid, nick, delta, self.learn.affinity_start);
                if let Some(why) = why {
                    tracing::info!("好感度 {delta:+.1}（{why}）[{scope}] {nick}（{uid}）");
                    if directed {
                        self.note_offense(scope, uid, why);
                    }
                }
            }
            let said = nlp::clean(text);
            if !said.is_empty() {
                let _ = learn::learn_definitions(&store, scope, &said);
                let promoted = learn::learn_candidates(&store, scope, uid, &said, &self.learn);
                if !promoted.is_empty() {
                    tracing::info!("学到新黑话 [{scope}]：{}", promoted.join("、"));
                }
                if !uid.is_empty() {
                    let who = if nick.trim().is_empty() { uid } else { nick };
                    for f in nlp::extract_facts(who, &said) {
                        if f.facet.is_empty() {
                            if store.upsert_fact(scope, conv, uid, &f.text, f.strength, f.kind).unwrap_or(false) {
                                tracing::info!("记住了 [{scope}]：{}", f.text);
                            }
                        } else if store
                            .add_facet(scope, uid, f.facet, &f.value, persona::Source::SelfSaid.weight(), persona::Source::SelfSaid.tag())
                            .is_ok()
                        {
                            tracing::info!("画像 [{scope}] {who}：{}={}", f.facet, f.value);
                        }
                    }
                }
            }
        }
        let who = if nick.is_empty() { uid } else { nick };
        let _ = store.log_message(scope, conv, uid, who, text, false, "");
    }

    /// 给同会话上一条风格样本打分：接话 +1，纠正/不满 -1，超时不动。
    fn reward_last_turn(&self, store: &Store, conv: &str, text: &str) {
        let Ok(Some(prev)) = store.last_style_sample(conv) else {
            return;
        };
        let now = now_ts();
        if now - prev.ts > self.learn.pair_window_secs {
            return;
        }
        let delta = if learn::is_negative_feedback(text) { -1.0 } else { 1.0 };
        let _ = store.add_style_score(prev.id, delta);
        let _ = store.trim_style_samples(&prev.scope, self.learn.style_keep);
    }

    /// bot 回复后：记录出站消息 + 存一对风格样本 + 按长度扣精力
    pub fn after_reply(&self, conv: &str, scope: &str, user_text: &str, reply: &str) {
        self.after_reply_to(conv, scope, user_text, reply, "");
    }

    /// 同上，记下这条回复是在回谁（`to_uid`），账本按人串往来时要用
    pub fn after_reply_to(&self, conv: &str, scope: &str, user_text: &str, reply: &str, to_uid: &str) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let _ = store.log_message(scope, conv, "", "bot", reply, true, to_uid);
        let chars = reply.chars().count();
        let _ = heart::charge_reply_energy(&store, conv, chars, &self.heart);
        if self.learn.enabled && !user_text.trim().is_empty() {
            let _ = store.insert_style_sample(scope, conv, user_text, reply);
            let _ = store.trim_style_samples(scope, self.learn.style_keep);
        }
    }

    /// 旁观观察：未被 @ 的全量群消息也进学习层（好感度 / 黑话 / 接话打分 + 消息入账），
    /// 但不触发 DSH 调用——让 bot 在后台默默积累，而不是只在被 @ 时才学。
    pub fn observe(&self, conv: &str, scope: &str, uid: &str, nick: &str, text: &str) {
        self.ingest(conv, scope, uid, nick, text, false);
    }

    /// 近期对话渲染（主动插话时给 DSH 看「群里正在聊什么」）
    pub fn recent_context(&self, scope: &str, limit: usize) -> String {
        let rows = self
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recent_messages(scope, limit)
            .unwrap_or_default();
        rows.iter()
            .map(|(who, text, is_bot)| {
                let who = if *is_bot { "我" } else { who.as_str() };
                format!("{who}: {}", text.replace('\n', " "))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 手动新增一条核心记忆（WebUI / 后续 QQ 命令入口）
    #[allow(dead_code)]
    pub fn add_memory(&self, scope: &str, conv: &str, text: &str, tags: &str, strength: f64, kind: &str) -> Result<i64> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.insert_memory(scope, conv, text, tags, strength, kind)
    }

    /// 夜间压缩的材料：上次压缩以来（最多回看 36 小时）的对话，按「主动参与 / 高兴趣」挑到 `budget` 字。
    /// 新消息不到 `min_msgs` 条返回 None（没什么好压的）。
    pub fn compaction_input(&self, scope: &str, budget: usize, min_msgs: usize) -> Option<(Vec<compact::Line>, Vec<usize>)> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ts();
        let last = store
            .get_kv(&format!("compact:{scope}"))
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let since = (last + 1).max(now - 36 * 3600);
        let rows = store.messages_since(scope, since, 1500).unwrap_or_default();
        drop(store);
        let lines: Vec<compact::Line> = rows
            .into_iter()
            .map(|(who, text, is_bot)| compact::Line { who, text, is_bot })
            .filter(|l| !nlp::clean(&l.text).is_empty() || l.is_bot)
            .collect();
        if lines.len() < min_msgs {
            return None;
        }
        let picked = compact::select(&lines, budget);
        (!picked.is_empty()).then_some((lines, picked))
    }

    /// 写入压缩结果：前情（kind=recap，只留最近 3 条）+ 记忆（kind=episode，强度按兴趣），
    /// 再让没被用到的旧自动记忆慢慢淡掉，裁掉太旧的原始消息，记下这次压缩的时间。
    pub fn apply_compaction(&self, scope: &str, conv: &str, recap: Option<&str>, mems: &[(String, u8)]) -> (usize, usize) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ts();
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let tags = format!("auto:compact,{date}");
        if let Some(r) = recap.filter(|r| !r.trim().is_empty()) {
            let _ = store.insert_memory(scope, conv, r.trim(), &tags, 70.0, "recap");
            let _ = store.fade_memories(scope, "recap", i64::MAX, 0.0, 0.0, 3);
        }
        let existing: Vec<String> = store
            .memories_of_kind(scope, "episode", 200)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.text)
            .collect();
        let mut added = 0;
        for (text, interest) in mems {
            if existing.iter().any(|e| e == text) {
                continue;
            }
            if store.insert_memory(scope, conv, text, &tags, compact::strength_for(*interest), "episode").is_ok() {
                added += 1;
            }
        }
        // 一周没被召回的自动记忆每晚 −4，低于 30 忘掉；每个窗口最多留 80 条
        let faded = store.fade_memories(scope, "episode", now - 7 * 86400, 4.0, 30.0, 80).unwrap_or(0);
        let _ = store.set_kv(&format!("compact:{scope}"), &now.to_string());
        (added, faded)
    }

    /// 这句话是不是聊到了我记得的事：同窗口里强度 ≥60 的记忆（压缩记下的高兴趣记忆、群友档案、手动记忆），
    /// BM25 最高分够高就算。只读，不刷新记忆的「最近使用」。
    pub fn interest_hit(&self, scope: &str, text: &str) -> bool {
        let t = nlp::clean(text);
        if t.chars().count() < 4 {
            return false;
        }
        let mems: Vec<_> = self
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .all_memories()
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.scope == scope && m.kind != "recap" && m.strength >= 60.0)
            .collect();
        memory::bm25_rank(&mems, &t, 1).first().is_some_and(|(_, s)| *s >= INTEREST_MIN_SCORE)
    }

    /// 某人的好感度（衰减前的原始分）
    pub fn affinity_score(&self, scope: &str, uid: &str) -> Option<f64> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.get_affinity(scope, uid).ok().flatten().map(|a| a.score)
    }

    /// 最近 `within` 秒内有消息的窗口
    pub fn active_scopes(&self, within: i64) -> Vec<String> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.scopes_active_since(now_ts() - within).unwrap_or_default()
    }

    /// 话里提到的、我学过的词 → 笔记（最多 2 条）
    fn concepts_in(store: &store::Store, query: &str) -> Vec<String> {
        let q = query.to_lowercase();
        if q.chars().count() < 2 {
            return Vec::new();
        }
        store
            .memories_of_kind("global:concept", "concept", 500)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| {
                m.tags
                    .split(',')
                    .find_map(|t| t.strip_prefix("concept:"))
                    .is_some_and(|term| term.chars().count() >= 2 && q.contains(&term.to_lowercase()))
            })
            .take(2)
            .map(|m| m.text)
            .collect()
    }

    /// 夜间整理的「洞察」落库：模型推断的画像、每人常聊的话题（统计）、她想了解的东西、
    /// 群里常说但还不懂的黑话、队列里的词今天又被提到几次
    pub fn apply_insights(&self, scope: &str, c: &compact::Compacted, lines: &[compact::Line]) -> (usize, usize) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let mut facets = 0;
        for (nick, facet, value) in &c.personas {
            if let Ok(Some(uid)) = store.uid_by_nick(scope, nick) {
                if store.add_facet(scope, &uid, facet, value, persona::Source::Inferred.weight(), persona::Source::Inferred.tag()).is_ok() {
                    facets += 1;
                }
            }
        }
        for (nick, topics) in compact::sender_topics(lines) {
            if let Ok(Some(uid)) = store.uid_by_nick(scope, &nick) {
                for t in topics {
                    let _ = store.add_facet(scope, &uid, "常聊", &t, persona::Source::Stats.weight(), persona::Source::Stats.tag());
                    facets += 1;
                }
            }
        }
        // 好奇心：先给队列里的老词记「又被提到」，再收新的
        let open = store.open_curiosities().unwrap_or_default();
        for (term, n) in compact::count_terms(lines, &open) {
            let _ = store.mention_curiosity(&term, n);
        }
        let mut queued = 0;
        for (term, why) in &c.curious {
            if store.enqueue_curiosity(term, scope, if why.is_empty() { "自己想弄明白" } else { why }, 4).is_ok() {
                queued += 1;
            }
        }
        for sl in store.list_slang(Some(scope), 200).unwrap_or_default() {
            if sl.meaning.is_empty() && sl.hits >= 5 && !open.contains(&sl.term) && store.concept_of(&sl.term).ok().flatten().is_none() {
                let _ = store.enqueue_curiosity(&sl.term, scope, "群里常说，我还不懂", 2);
                queued += 1;
            }
        }
        (facets, queued)
    }

    /// 今晚要学的词：到了下一阶段的（见 research::ready），20 小时内学过的不重复；可信度低的隔 3 天再试
    pub fn research_batch(&self, n: usize) -> Vec<(String, String, String, i64)> {
        let now = now_ts();
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store
            .curiosities()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, _, _, _, _, since, stage, conf, last)| {
                let gap = if *last > 0 && *conf == 0 { 3 * 86_400 } else { 20 * 3600 };
                research::ready(*stage, *since) && now - last >= gap
            })
            .take(n)
            .map(|(term, scope, why, _, _, _, stage, _, _)| (term, scope, why, stage))
            .collect()
    }

    /// 查资料前的已有材料：(群里的原话, 之前的笔记)
    pub fn research_context(&self, term: &str, scope: &str) -> (String, String) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let tl = term.to_lowercase();
        let quotes: Vec<String> = store
            .messages_since(scope, now_ts() - 7 * 86_400, 3000)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, t, is_bot)| !is_bot && t.to_lowercase().contains(&tl))
            .rev()
            .take(4)
            .map(|(who, t, _)| format!("{who}: {}", nlp::clean(&t).chars().take(80).collect::<String>()))
            .collect();
        let mut known = store.concept_of(term).ok().flatten().map(|m| m.text).unwrap_or_default();
        // 群友的纠正：当线索，不当结论（要自己查证，防止被一句话带偏 / 投毒）
        let hints: Vec<String> = store
            .corrections(Some(term))
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.6 == "pending")
            .take(3)
            .map(|c| format!("{}说：{}", if c.4.is_empty() { &c.3 } else { &c.4 }, c.2))
            .collect();
        if !hints.is_empty() {
            known.push_str(&format!("\n有人指出你可能理解错了（只是线索，要自己查证，查不到证据就别采纳）：{}", hints.join("；")));
        }
        (quotes.join("\n"), known)
    }

    /// 学完一次：存笔记、可信度够就升阶段并给没释义的黑话补上意思
    pub fn save_research(&self, term: &str, note: &research::Note, conf: u8) {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let domains: Vec<String> = note.sources.iter().filter_map(|s| research::domain(s)).take(3).collect();
        let tags = format!("conf:{},src:{}", research::conf_label(conf), domains.join("|"));
        let _ = store.put_concept(term, &research::render(term, note, conf), 50.0 + conf as f64 * 15.0, &tags);
        let _ = store.finish_research(term, conf >= 1, conf as i64);
        if conf >= 1 {
            let meaning: String = note.what.chars().take(24).collect();
            if store.fill_slang_meaning(term, &meaning).unwrap_or(0) > 0 {
                tracing::info!("黑话「{term}」补上了释义：{meaning}");
            }
        }
    }

    /// 最近一次的前情（不限时间；夜间压缩时整合进新前情）
    pub fn recap_any(&self, scope: &str) -> Option<String> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.memories_of_kind(scope, "recap", 1).ok()?.into_iter().next().map(|r| r.text)
    }

    /// 最近一次的前情提要（4 天内）
    pub fn latest_recap(&self, scope: &str) -> Option<String> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let r = store.memories_of_kind(scope, "recap", 1).ok()?.into_iter().next()?;
        (now_ts() - r.ts <= 4 * 86400).then_some(r.text)
    }

    /// BM25 召回（按灵魂 recall_depth 决定条数，优先同 scope，不足补全局）
    /// 召回：本窗口 + `also`（私聊时带上对方所在群的当前窗口——群里的事 TA 本来就在场）。
    /// 外窗口的分打七折，本窗口的优先；关于别人的私人事实（uid: 标签）不跨窗口。
    pub fn recall_linked(&self, scope: &str, also: &[String], query: &str) -> Vec<(store::MemoryRow, f64)> {
        let n = (self.heart.soul.recall_depth as usize).clamp(3, 10);
        let all = self
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .all_memories()
            .unwrap_or_default();
        // scope = 「聊天 + 世代号」（如 qq-group-123#0），已归档的旧窗口不参与。
        // 前情提要单独注入（【前情】），不进 BM25 召回
        let scoped: Vec<_> = all.iter().filter(|m| m.scope == scope && m.kind != "recap").cloned().collect();
        let mut ranked = memory::bm25_rank(&scoped, query, n);
        let outer: Vec<_> = all
            .iter()
            .filter(|m| also.contains(&m.scope) && m.kind != "recap" && !m.tags.starts_with("uid:"))
            .cloned()
            .collect();
        if !outer.is_empty() {
            ranked.extend(memory::bm25_rank(&outer, query, n).into_iter().map(|(m, s)| (m, s * 0.7)));
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            ranked.truncate(n);
        }
        // 被召回过的记忆记一下时间：夜间遗忘只淡化「一直没用上」的
        if !ranked.is_empty() {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            for (m, _) in &ranked {
                let _ = store.touch_memory(m.id);
            }
        }
        ranked
    }

    /// 组装注入给 DSH 的「记忆 + 学到的 + 内心」上下文块（追加到系统提示词）。
    /// 以 Heevi 的平静口吻写成「回忆 / 我说话的调子 / 群里的说法 / 和这个人 / 此刻」，
    /// 自然融入人设，不暴露 recall_depth/creativity/好感度数值等内部参数。
    pub fn build_injection(&self, conv: &str, scope: &str, uid: &str, nick: &str, query: &str) -> String {
        self.build_injection_linked(conv, scope, uid, nick, query, &[])
    }

    /// 同上，带上这个人在别处的当前窗口 `linked`（同一个 QQ 号的私聊 / 其它群）：
    /// - 关于这个人的画像、事实、好感跨窗口合并——不管在哪儿遇到，都是同一个人；
    /// - 认得 TA 在不同群里的不同名片；
    /// - 私聊时也能想起 TA 所在群里聊过的事；群里不会带出别的群的聊天内容，
    ///   私聊里知道的个人信息只当心里有数，不在群里说破。
    pub fn build_injection_linked(&self, conv: &str, scope: &str, uid: &str, nick: &str, query: &str, linked: &[String]) -> String {
        let mut parts: Vec<String> = Vec::new();
        let here_private = scope.starts_with("qq-c2c-");
        let linked: Vec<String> = if uid.is_empty() { Vec::new() } else { linked.iter().filter(|s| *s != scope).cloned().collect() };

        // 前情提要：上一次压缩时写下的「之前聊到哪了」（会话已经换新，靠它接上）
        if let Some(r) = self.latest_recap(scope) {
            parts.push(format!("【前情】之前大概聊到这儿——{r}（心里有数就行，别复述。）"));
        }

        // 私聊里可以想起 TA 所在群的事；群里只用本群的
        let also: Vec<String> = if here_private { linked.iter().filter(|s| s.starts_with("qq-group-")).cloned().collect() } else { Vec::new() };
        let mems = self.recall_linked(scope, &also, query);
        if !mems.is_empty() {
            let mut block = String::from("【回忆】我隐约记得这些——");
            block.push_str(
                "（与当前话题相关就自然地带一句，无关就略过；不要生硬罗列，也不要用“记忆/系统”这类词。）\n",
            );
            for (m, _) in mems {
                block.push_str(&format!("· {}\n", m.text.trim()));
            }
            parts.push(block.trim_end().to_string());
        }

        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());

        // 关于正在说话的这个人：多面画像（证据越多越确定）+ 让我记住的事，和【回忆】去重
        if !uid.is_empty() {
            let now = now_ts();
            let mut people = vec![scope.to_string()];
            people.extend(linked.iter().cloned());
            let portrait = persona::render(&merge_facets(people.iter().map(|s| store.facets_of(s, uid).unwrap_or_default())), now);
            // 事实：本窗口的在前；在群里时，私聊里知道的标「私下」
            let mut facts: Vec<(String, bool)> = Vec::new();
            for s in &people {
                let secret = !here_private && s.starts_with("qq-c2c-");
                for f in store.facts_about(s, uid, 5).unwrap_or_default() {
                    let t = f.text.trim().to_string();
                    if !facts.iter().any(|(x, _)| *x == t) && !parts.iter().any(|p| p.contains(&t)) {
                        facts.push((t, secret));
                    }
                }
            }
            facts.truncate(6);
            let alias = identity_line(&store.identities(uid).unwrap_or_default(), scope, &linked, nick);
            if portrait.is_some() || !facts.is_empty() || alias.is_some() {
                let mut block = String::from(
                    "【关于这个人】我对 TA 的了解——（自然地用上就好，别背出来，也别说“我记得你说过”；标了「好像」的是不确定的）\n",
                );
                if let Some(a) = alias {
                    block.push_str(&format!("· {a}\n"));
                }
                if let Some(p) = portrait {
                    block.push_str(&format!("· {p}\n"));
                }
                let mut secret_any = false;
                for (t, secret) in facts {
                    secret_any |= secret;
                    block.push_str(&format!("· {t}{}\n", if secret { "（私下）" } else { "" }));
                }
                if secret_any {
                    block.push_str("（标了「私下」的是 TA 私聊里告诉我的，心里有数就行，在群里别说破。）\n");
                }
                parts.push(block.trim_end().to_string());
            }
        }

        // 聊到我学过的东西：带上学习笔记（是什么 / 为什么 / 怎么做 / 我的看法）
        let concepts = Self::concepts_in(&store, query);
        if !concepts.is_empty() {
            let mut block = String::from("【我了解的】之前自己查过、学过的——（用得上就用，标了「还不确定」的别说得太肯定）\n");
            for c in concepts {
                block.push_str(&format!("· {c}\n"));
            }
            parts.push(block.trim_end().to_string());
        }

        // 学到的表达风格（few-shot）
        if self.learn.enabled {
            if let Ok(rows) = store.top_style_samples(scope, 6) {
                if let Some(b) = learn::render_styles(&rows) {
                    parts.push(b);
                }
            }
            // 群黑话
            if let Ok(rows) = store.list_slang(Some(scope), 8) {
                if let Some(b) = learn::render_slang(&rows) {
                    parts.push(b);
                }
            }
            // 和当前说话人的关系
            if !uid.is_empty() {
                // 好感跨窗口合并（按消息数加权）：在别的群 / 私聊里攒下的关系也算数
                let rows: Vec<_> = std::iter::once(scope)
                    .chain(linked.iter().map(String::as_str))
                    .filter_map(|s| learn::decayed_affinity(&store, s, uid, &self.learn))
                    .collect();
                let msgs: i64 = rows.iter().map(|a| a.msgs).sum();
                let weight: f64 = rows.iter().map(|a| a.msgs.max(1) as f64).sum();
                if !rows.is_empty() {
                    let score = rows.iter().map(|a| a.score * a.msgs.max(1) as f64).sum::<f64>() / weight;
                    let offense = std::iter::once(scope)
                        .chain(linked.iter().map(String::as_str))
                        .find_map(|s| self.recent_offense(s, uid));
                    if msgs >= 2 || offense.is_some() {
                        let mut line = format!("【和这个人】{}。", learn::affinity_label(score));
                        if let Some(why) = offense {
                            line.push_str(&format!(
                                "刚才对方有点过分（{why}），不用陪着、不用讨好，冷淡一句或者直接不接这个茬就好。"
                            ));
                        }
                        line.push_str("（语气按这个来，别说破。）");
                        parts.push(line);
                    }
                }
            }
        }

        let energy = heart::current_energy(&store, conv, &self.heart);
        drop(store);

        let suggest = heart::suggested_len(energy, self.heart.soul.expression_desire);
        // 三无少女的细微内心：精力只影响语调/长度，不写成数字参数
        let emo = if energy >= 80.0 {
            "今天，状态还行。想说的话，可以说多一点。"
        } else if energy >= 50.0 {
            "嗯，很平静。"
        } else if energy >= 20.0 {
            "哈……有点说不动话了。简短就好。"
        } else {
            "……今天，先到这里吧。"
        };
        let mut block = format!("【此刻】{emo}");
        block.push_str(&format!("（少即是多，{suggest} 字以内就够。）"));
        parts.push(block);

        parts.join("\n\n")
    }
}

/// 同一个人在几个窗口里的画像合成一份：同一面的证据相加、取最近时间，按证据排
fn merge_facets(sets: impl Iterator<Item = Vec<persona::FacetRow>>) -> Vec<persona::FacetRow> {
    let mut out: Vec<persona::FacetRow> = Vec::new();
    for f in sets.flatten() {
        match out.iter_mut().find(|x| x.facet == f.facet && x.value == f.value) {
            Some(x) => {
                x.evidence += f.evidence;
                x.hits += f.hits;
                x.last_ts = x.last_ts.max(f.last_ts);
            }
            None => out.push(f),
        }
    }
    out.sort_by(|a, b| b.evidence.partial_cmp(&a.evidence).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// 「这是同一个人」：TA 在别的群 / 私聊里用的名字（和现在的名字不一样才说）
fn identity_line(ids: &[(String, String, i64, i64)], scope: &str, linked: &[String], nick: &str) -> Option<String> {
    let now = ids.iter().find(|(s, ..)| s == scope).map(|(_, n, ..)| n.as_str()).filter(|n| !n.is_empty()).unwrap_or(nick);
    let mut names: Vec<String> = Vec::new();
    let mut private = false;
    let mut groups = 0;
    for (s, n, ..) in ids.iter().filter(|(s, ..)| linked.contains(s)) {
        if s.starts_with("qq-c2c-") {
            private = true;
        } else {
            groups += 1;
        }
        let n = n.trim();
        if !n.is_empty() && n != now && !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    }
    if names.is_empty() && !private && groups == 0 {
        return None;
    }
    let mut where_: Vec<String> = Vec::new();
    if groups > 0 {
        where_.push(format!("另外 {groups} 个群"));
    }
    if private {
        where_.push("私聊".into());
    }
    let mut line = format!("TA 就是我在{}里也遇到的那个人（同一个 QQ 号）", where_.join("和"));
    if !names.is_empty() {
        names.truncate(4);
        line.push_str(&format!("，在别处叫「{}」", names.join("」「")));
    }
    line.push_str("。名字不同但是同一个人，别当成陌生人。");
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_mind(tag: &str) -> Arc<Mind> {
        let dir = std::env::temp_dir().join(format!("mind-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Mind::new(dir.to_str().unwrap(), 0, LearnConfig::default()).expect("open mind")
    }

    #[test]
    fn injection_is_organic_and_contains_recall() {
        let m = tmp_mind("recall");
        m.add_memory("scope1", "c1", "用户偏好猫胜过狗", "偏好,宠物", 80.0, "knowledge")
            .expect("add mem");
        let inj = m.build_injection("c1", "scope1", "u1", "阿猫", "用户喜欢猫");
        assert!(inj.contains("用户偏好猫胜过狗"), "应含召回记忆: {inj}");
        assert!(inj.contains("【回忆】"), "应有回忆段: {inj}");
        assert!(inj.contains("【此刻】"), "应有情绪段: {inj}");
        assert!(!inj.contains("recall_depth"), "不应泄露内部数值: {inj}");
    }

    /// 会话窗口隔离：别的群、以及重置后归档的旧窗口（旧世代号）的记忆 / 消息 / 风格都不会注入新窗口
    #[test]
    fn windows_are_isolated_and_archived_ones_not_recalled() {
        let m = tmp_mind("windows");
        // 群 A 的旧窗口（第 0 代）：有记忆、有对话
        m.add_memory("qq-group-1#0", "qq-group-1#0", "群A旧窗口：喜欢猫", "猫", 80.0, "knowledge").unwrap();
        m.on_incoming("qq-group-1#0", "qq-group-1#0", "u1", "阿猫", "我最喜欢猫了");
        m.after_reply("qq-group-1#0", "qq-group-1#0", "我最喜欢猫了", "猫很可爱。");
        // 群 B 当前窗口
        m.add_memory("qq-group-2#0", "qq-group-2#0", "群B：喜欢狗和猫", "猫,狗", 80.0, "knowledge").unwrap();
        // 群 A 重置后的新窗口（第 1 代）：什么旧东西都不该带进来
        let inj = m.build_injection("qq-group-1#1", "qq-group-1#1", "u1", "阿猫", "猫");
        assert!(!inj.contains("群A旧窗口"), "归档窗口的记忆不应召回：{inj}");
        assert!(!inj.contains("群B"), "别的群的记忆不应召回：{inj}");
        assert!(!inj.contains("猫很可爱"), "归档窗口的风格样本不应注入：{inj}");
        assert!(m.recent_context("qq-group-1#1", 14).is_empty(), "新窗口没有旧对话");
        assert!(m.recent_context("qq-group-1#0", 14).contains("我最喜欢猫了"), "旧窗口的记录仍在库里（可查，不注入）");
        // 群 B 自己的窗口照常召回自己的
        let b = m.build_injection("qq-group-2#0", "qq-group-2#0", "u2", "阿狗", "猫");
        assert!(b.contains("群B：喜欢狗和猫") && !b.contains("群A"), "{b}");
    }

    /// 同一个 QQ 号：私聊和各群的个人了解连通、认得不同名片；群和群之间的聊天内容不串，私聊的事在群里不说破
    #[test]
    fn same_qq_is_one_person_across_windows() {
        let m = tmp_mind("identity");
        let (ga, gb, dm) = ("qq-group-1#0", "qq-group-2#0", "qq-c2c-u1#0");
        m.on_incoming(ga, ga, "u1", "阿猫", "大家好");
        m.on_incoming(gb, gb, "u1", "猫猫队长", "晚上好");
        m.on_incoming(dm, dm, "u1", "小林", "在吗");
        {
            let s = m.store.lock().unwrap();
            s.upsert_fact(dm, dm, "u1", "小林最近在准备考研", 80.0, "profile").unwrap();
            s.add_facet(gb, "u1", "喜欢", "钓鱼", 1.0, "self").unwrap();
        }
        m.add_memory(gb, gb, "群B里大家约好周六一起打灰神", "灰神", 80.0, "episode").unwrap();
        let linked = vec![gb.to_string(), dm.to_string()];

        let a = m.build_injection_linked(ga, ga, "u1", "阿猫", "灰神", &linked);
        assert!(a.contains("猫猫队长") && a.contains("小林") && a.contains("同一个 QQ 号"), "认得别处的名字：{a}");
        assert!(a.contains("钓鱼"), "别的群里了解到的画像也算：{a}");
        assert!(a.contains("考研") && a.contains("（私下）"), "私聊里知道的事带上、标私下：{a}");
        assert!(!a.contains("周六一起打灰神"), "别的群的聊天内容不搬进来：{a}");

        let d = m.build_injection_linked(dm, dm, "u1", "小林", "灰神", &linked);
        assert!(d.contains("周六一起打灰神"), "私聊里能想起 TA 所在群的事：{d}");
        assert!(d.contains("考研") && !d.contains("（私下）"), "{d}");

        // 不传 linked 时还是各窗口独立
        assert!(!m.build_injection(ga, ga, "u1", "阿猫", "灰神").contains("考研"));
    }

    #[test]
    fn style_learning_rewards_continuation() {
        let m = tmp_mind("style");
        // 一轮对话：用户问 → bot 答
        m.on_incoming("c1", "scope1", "u1", "阿猫", "在吗");
        m.after_reply("c1", "scope1", "在吗", "嗯，在。");
        // 用户接话 → 上一轮加分
        m.on_incoming("c1", "scope1", "u1", "阿猫", "帮我看看这个");
        let inj = m.build_injection("c1", "scope1", "u1", "阿猫", "看看");
        assert!(inj.contains("【我说话的调子】"), "应注入风格样本: {inj}");
        assert!(inj.contains("嗯，在。"), "应含学到的回复样本: {inj}");
    }

    #[test]
    fn negative_feedback_lowers_score() {
        let m = tmp_mind("negative");
        m.on_incoming("c1", "scope1", "u1", "阿猫", "在吗");
        m.after_reply("c1", "scope1", "在吗", "嗯，在。");
        m.on_incoming("c1", "scope1", "u1", "阿猫", "不对，你答错了");
        let inj = m.build_injection("c1", "scope1", "u1", "阿猫", "看看");
        assert!(!inj.contains("【我说话的调子】"), "被纠正的样本不应作为示范: {inj}");
    }

    #[test]
    fn slang_and_affinity_get_injected() {
        let m = tmp_mind("slang");
        for i in 0..3 {
            m.on_incoming("c1", "scope1", &format!("u{i}"), "某人", "这把zmd又坑了");
        }
        m.on_incoming("c1", "scope1", "u1", "阿猫", "又来了");
        let inj = m.build_injection("c1", "scope1", "u1", "阿猫", "来了");
        assert!(inj.contains("【群里的说法】"), "应注入黑话: {inj}");
        assert!(inj.contains("zmd"), "应学到高频词: {inj}");
        assert!(inj.contains("【和这个人】"), "应注入好感度语气: {inj}");
        assert!(!inj.contains("affinity"), "不应泄露内部字段: {inj}");
    }

    #[test]
    fn facts_are_learned_and_injected_per_person() {
        let m = tmp_mind("facts");
        m.on_incoming("c1", "scope1", "u1", "阿猫", "我最喜欢猫了，但是家里不让养");
        m.on_incoming("c1", "scope1", "u1", "阿猫", "我最喜欢猫了");
        m.on_incoming("c1", "scope1", "u2", "阿狗", "我在玩星痕共鸣");
        let inj = m.build_injection("c1", "scope1", "u1", "阿猫", "今天干嘛");
        assert!(inj.contains("【关于这个人】") && inj.contains("喜欢：猫"), "{inj}");
        assert!(!inj.contains("星痕共鸣"), "别人的事不该注入给这个人：{inj}");
        let store = m.store.lock().unwrap();
        let f = store.facets_of("scope1", "u1").unwrap();
        assert_eq!(f.len(), 1, "重复说同一件事只加证据不重复");
        assert_eq!(f[0].hits, 2);
        assert!(f[0].evidence >= 2.0);
    }

    #[test]
    fn vision_text_is_not_learned() {
        let m = tmp_mind("vision");
        for i in 0..4 {
            m.on_incoming("c1", "scope1", &format!("u{i}"), "某人", "[图片] 这是一张游戏截图，画面中角色表情包");
        }
        let store = m.store.lock().unwrap();
        assert!(store.list_slang(Some("scope1"), 50).unwrap().is_empty());
        assert!(store.list_candidates(Some("scope1"), 50).unwrap_or_default().is_empty());
    }

    #[test]
    fn harassment_lowers_affinity_and_shows_in_tone() {
        let m = tmp_mind("harass");
        m.on_incoming("c1", "scope1", "u1", "阿猫", "在吗");
        m.on_incoming("c1", "scope1", "u1", "阿猫", "谢谢你");
        let before = m.store.lock().unwrap().get_affinity("scope1", "u1").unwrap().unwrap().score;
        m.on_incoming("c1", "scope1", "u1", "阿猫", "你个傻逼");
        m.on_incoming("c1", "scope1", "u1", "阿猫", "发点涩图");
        let after = m.store.lock().unwrap().get_affinity("scope1", "u1").unwrap().unwrap().score;
        assert!(after <= before - 14.0, "骂人 + 色情骚扰应明显扣分：{before} → {after}");
        let inj = m.build_injection("c1", "scope1", "u1", "阿猫", "嗯");
        assert!(inj.contains("刚才对方有点过分"), "{inj}");
        // 别人不受影响
        m.on_incoming("c1", "scope1", "u2", "阿狗", "在吗");
        m.on_incoming("c1", "scope1", "u2", "阿狗", "今天打本吗");
        assert!(!m.build_injection("c1", "scope1", "u2", "阿狗", "嗯").contains("过分"));
    }

    #[test]
    fn spamming_counts_as_harassment() {
        let m = tmp_mind("spam");
        for _ in 0..3 {
            m.on_incoming("c1", "scope1", "u1", "阿猫", "理我一下");
        }
        let a = m.store.lock().unwrap().get_affinity("scope1", "u1").unwrap().unwrap();
        // 前两条 +2，第三条重复 → -4
        assert!(a.score < 50.0 + 2.0 * 2.0, "刷屏应扣分：{}", a.score);
    }

    #[test]
    fn affinity_never_negative() {
        let m = tmp_mind("floor");
        for i in 0..30 {
            m.on_incoming("c1", "scope1", "u1", "阿猫", &format!("你个傻逼{i}"));
        }
        let a = m.store.lock().unwrap().get_affinity("scope1", "u1").unwrap().unwrap();
        assert!(a.score >= 0.0, "{}", a.score);
        assert!(m.build_injection("c1", "scope1", "u1", "阿猫", "嗯").contains("反感"));
    }

    #[test]
    fn compaction_keeps_participation_and_injects_recap() {
        let m = tmp_mind("compact");
        let s = "qq-group-9#0";
        for (who, t) in [("阿狗", "星痕共鸣新副本好难"), ("阿猫", "星痕共鸣副本我也卡住了"), ("小明", "副本堆什么属性"), ("阿狗", "哈哈哈")] {
            m.observe(s, s, who, who, t);
        }
        m.after_reply(s, s, "副本堆什么属性", "神盾骑士先堆暴击。");
        let (lines, picked) = m.compaction_input(s, 400, 3).expect("有材料");
        let mat = compact::material(&lines, &picked);
        assert!(mat.contains("我: 神盾骑士先堆暴击") && !mat.contains("哈哈哈"), "{mat}");
        let (added, _) = m.apply_compaction(
            s,
            s,
            Some("大家在聊星痕共鸣新副本，我建议先堆暴击。"),
            &[("阿狗卡在星痕共鸣新副本".into(), 5), ("小明在问属性".into(), 2)],
        );
        assert_eq!(added, 2);
        let inj = m.build_injection(s, s, "u1", "阿狗", "副本");
        assert!(inj.contains("【前情】") && inj.contains("先堆暴击"), "{inj}");
        assert!(inj.contains("阿狗卡在星痕共鸣新副本"), "高兴趣记忆能召回：{inj}");
        assert!(m.compaction_input(s, 400, 3).is_none(), "压缩过的不再重复压");
        // 同一条记忆不会重复写
        assert_eq!(m.apply_compaction(s, s, None, &[("阿狗卡在星痕共鸣新副本".into(), 5)]).0, 0);
    }

    #[test]
    fn interest_hit_needs_real_overlap() {
        let m = tmp_mind("interest");
        let s = "qq-group-7#0";
        m.apply_compaction(s, s, None, &[("阿狗卡在星痕共鸣新副本".into(), 5), ("小红喜欢吃火锅".into(), 1)]);
        let store = m.store.lock().unwrap();
        let all = store.all_memories().unwrap();
        drop(store);
        for q in ["星痕共鸣的新副本怎么过", "副本", "今天天气不错", "火锅好吃吗"] {
            let r = memory::bm25_rank(&all.iter().filter(|x| x.strength >= 60.0).cloned().collect::<Vec<_>>(), q, 1);
            println!("BM25 {q} → {:?}", r.first().map(|(m, s)| (m.text.clone(), *s)));
        }
        assert!(m.interest_hit(s, "星痕共鸣的新副本怎么过"));
        assert!(!m.interest_hit(s, "今天天气不错"));
        assert!(!m.interest_hit(s, "火锅好吃吗"), "兴趣低（强度 <60）的记忆不算");
        assert!(!m.interest_hit("qq-group-8#0", "星痕共鸣的新副本怎么过"), "别的窗口不算");
    }

    #[test]
    fn hostility_and_dm_candidates() {
        let m = tmp_mind("social");
        for (i, u) in ["u1", "u2", "u3", "u1"].iter().enumerate() {
            m.on_incoming("g", "qq-group-1#0", u, "某人", &format!("你个傻逼{i}"));
        }
        assert_eq!(m.hostility("qq-group-1#0", 86400), (4, 3));
        assert_eq!(m.hostility("qq-group-2#0", 86400), (0, 0));
        // 私聊窗口：好感高、聊得多
        {
            // 直接写分（连发 12 条会被当成刷屏扣分，这里只测查询）
            let store = m.store.lock().unwrap();
            for _ in 0..12 {
                store.bump_affinity("qq-c2c-9#0", "9", "阿猫", 3.0, 50.0).unwrap();
            }
            store.insert_message("qq-c2c-9#0", "qq-c2c-9#0", "阿猫", "在吗", false).unwrap();
        }
        assert!(m.best_affinity("9").unwrap() > 70.0);
        assert!(m.dm_candidates(65.0, 10, 0).is_empty(), "刚聊过的不算（idle 0 秒也要早于现在）");
        assert_eq!(m.dm_candidates(65.0, 10, -10).len(), 1);
        assert!(m.dm_candidates(65.0, 50, -10).is_empty(), "聊得不够多");
    }

    #[test]
    fn persona_curve_and_contradiction() {
        let m = tmp_mind("persona");
        let s = "qq-group-5#0";
        m.on_incoming(s, s, "u1", "阿猫", "我喜欢香菜");
        let inj = m.build_injection(s, s, "u1", "阿猫", "嗯");
        assert!(inj.contains("喜欢：香菜（好像）"), "只说过一次 → 不太确定：{inj}");
        m.on_incoming(s, s, "u1", "阿猫", "其实我讨厌香菜");
        let inj = m.build_injection(s, s, "u1", "阿猫", "嗯");
        assert!(inj.contains("讨厌：香菜") && !inj.contains("喜欢：香菜"), "新说法顶掉矛盾的旧说法：{inj}");
        // 夜间整理推断的画像 + 统计
        let c = compact::Compacted {
            personas: vec![("阿猫".into(), "性格", "嘴硬心软".into())],
            curious: vec![("灰神".into(), "群里常提到".into())],
            ..Default::default()
        };
        let lines = vec![
            compact::Line { who: "阿猫".into(), text: "灰神好难打".into(), is_bot: false },
            compact::Line { who: "阿狗".into(), text: "灰神又来了".into(), is_bot: false },
        ];
        let (facets, queued) = m.apply_insights(s, &c, &lines);
        assert!(facets >= 1 && queued == 1);
        assert_eq!(m.research_batch(5).len(), 1, "新进队列的词今晚就学「是什么」");
        let note = research::Note { what: "星痕共鸣里的一个Boss".into(), sources: vec!["资料库".into(), "https://a.com".into()], claimed: 2, ..Default::default() };
        m.save_research("灰神", &note, research::validate(&note).unwrap());
        assert!(m.research_batch(5).is_empty(), "学过了，等它再被聊到几次才学下一层");
        let inj = m.build_injection(s, s, "u2", "阿狗", "灰神怎么打");
        assert!(inj.contains("【我了解的】") && inj.contains("星痕共鸣里的一个Boss"), "{inj}");
    }

    #[test]
    fn observe_records_and_renders_recent_context() {
        let m = tmp_mind("observe");
        // 未 @ 的旁观消息：只进学习层，不调用 DSH
        m.observe("c1", "scope1", "u1", "阿猫", "今天好热");
        m.observe("c1", "scope1", "u2", "阿狗", "开空调");
        m.after_reply("c1", "scope1", "", "嗯。");
        let ctx = m.recent_context("scope1", 10);
        assert!(ctx.contains("阿猫: 今天好热"), "应含旁观群消息: {ctx}");
        assert!(ctx.contains("阿狗: 开空调"), "应含旁观群消息: {ctx}");
        assert!(ctx.contains("我: 嗯。"), "bot 发言也应进上下文: {ctx}");
        // 裁剪：每个 scope 只留最近 N 条
        {
            let store = m.store.lock().unwrap_or_else(|e| e.into_inner());
            store.trim_messages("scope1", 2).expect("trim");
        }
        let ctx2 = m.recent_context("scope1", 10);
        assert_eq!(ctx2.lines().count(), 2, "裁剪后应只剩 2 条: {ctx2}");
        assert!(!ctx2.contains("今天好热"), "最早的消息应被裁掉: {ctx2}");
    }
}
