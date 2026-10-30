//! 电竞赛事数据层：Liquipedia CS2（官方 HLTV 全站 Cloudflare 拦截，Liquipedia 为
//! 志愿者实时维护的 CS 赛事维基，比赛结束数分钟内更新，API 需 gzip 编码）。
//!
//! 换回 HLTV 时只需重写本文件内部实现（保持对外函数签名不变）。

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};

const API: &str = "https://liquipedia.net/counterstrike/api.php";
const UA: &str = "HeeviBot/1.0 (QQ esports digest; contact: bot owner)";

fn client() -> reqwest::Client {
    static CL: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CL.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(UA)
            .gzip(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client")
    })
    .clone()
}

/// 调 MediaWiki API（自动 gzip），返回 JSON
async fn api_get(params: &[(&str, &str)]) -> Result<Value> {
    let mut pairs = params.to_vec();
    pairs.push(("format", "json"));
    let resp = client()
        .get(API)
        .query(&pairs)
        .header(reqwest::header::ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .context("liquipedia 请求失败")?;
    if !resp.status().is_success() {
        return Err(anyhow!("liquipedia HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.context("liquipedia JSON 解析失败")?;
    if v.get("error").is_some() {
        let info = v["error"]["info"].as_str().unwrap_or("未知错误");
        return Err(anyhow!("liquipedia API 错误: {info}"));
    }
    Ok(v)
}

/// 取页面 wikitext
async fn wikitext(title: &str) -> Result<String> {
    let v = api_get(&[
        ("action", "parse"),
        ("page", title),
        ("prop", "wikitext"),
    ])
    .await?;
    v.pointer("/parse/wikitext/*")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("页面 {title} 无内容"))
}

/// 取页面渲染后的 HTML（表格数据靠 Cargo 服务端渲染，只能读 HTML）
async fn rendered_html(title: &str) -> Result<String> {
    let v = api_get(&[("action", "parse"), ("page", title), ("prop", "text")]).await?;
    v.pointer("/parse/text/*")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("页面 {title} 渲染失败"))
}

fn strip_tags(s: &str) -> String {
    let s = s.replace("<br", "\n<br");
    let s = s.replace("</p>", "\n").replace("</div>", "\n").replace("</li>", "\n");
    let no_scripts = match regex::Regex::new(r"(?s)<(script|style)[^>]*>.*?</\1>")
        .map(|re| re.replace_all(&s, ""))
    {
        Ok(cow) => cow.into_owned(),
        Err(_) => s.clone(),
    };
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re.replace_all(&no_scripts, "");
    let mut out = String::new();
    for ch in text.chars() {
        if ch == '\u{a0}' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    let mut out = out.replace('»', "").replace('«', "");
    // 压缩多余空白
    let re = regex::Regex::new(r"[ \t]+").unwrap();
    out = re.replace_all(&out, " ").to_string();
    out
}

/// 条目类型（按信息框模板判定）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Team,
    Player,
    Event,
    Other,
}

/// 通过信息框模板判定页面类型
pub fn kind_of_wikitext(w: &str) -> EntryKind {
    let head = &w[..w.len().min(3000)];
    if head.contains("{{Infobox team") || head.contains("{{Infobox team2") {
        EntryKind::Team
    } else if head.contains("{{Infobox player") {
        EntryKind::Player
    } else if head.contains("{{Infobox league") {
        EntryKind::Event
    } else {
        EntryKind::Other
    }
}

#[allow(dead_code)]
pub async fn detect_kind(title: &str) -> Result<EntryKind> {
    Ok(kind_of_wikitext(&wikitext(title).await?))
}

/// 搜索并返回指定类型的条目（前 `limit` 个候选里筛）
pub async fn search_kind(query: &str, want: EntryKind, limit: usize) -> Result<Vec<SearchHit>> {
    let v = api_get(&[
        ("action", "query"),
        ("list", "search"),
        ("srsearch", query),
        ("srlimit", &(limit * 3).to_string()),
    ])
    .await?;
    let titles: Vec<String> = v
        .pointer("/query/search")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|h| h.get("title").and_then(|t| t.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut hits = Vec::new();
    for t in titles {
        if hits.len() >= limit {
            break;
        }
        if let Ok(w) = wikitext(&t).await {
            if kind_of_wikitext(&w) == want {
                let url = page_url(&t);
                hits.push(SearchHit { title: t, url });
            }
        }
    }
    Ok(hits)
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
}

pub fn page_url(title: &str) -> String {
    format!("https://liquipedia.net/counterstrike/{}", title.replace(' ', "_"))
}

/// 战队的一场记录（以页面所属战队为“我方”）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchRow {
    pub date_str: String,
    pub tier: String,
    pub kind: String,
    pub tournament: String,
    /// Some((我方比分, 对手比分))；None=尚未开赛
    pub score: Option<(u8, u8)>,
    pub opponent: String,
    pub self_team: String,
}

impl MatchRow {
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.date_str, self.tournament, self.opponent)
    }
    pub fn finished(&self) -> bool {
        self.score.is_some()
    }
}

/// 拉取某战队的比赛表（CS2）。列：日期|等级|类型|赛事|比分|对手
pub async fn team_matches(team_title: &str) -> Result<Vec<MatchRow>> {
    let page = format!("{team_title}/Matches");
    let html = rendered_html(&page).await?;
    parse_matches_html(&html, team_title)
}

fn parse_matches_html(html: &str, team_title: &str) -> Result<Vec<MatchRow>> {
    let re = regex::Regex::new(r"(?s)<tr[^>]*>(.*?)</tr>").unwrap();
    let cell_re = regex::Regex::new(r"(?s)<t[dh][^>]*>(.*?)</t[dh]>").unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let row_html = &cap[1];
        let cells: Vec<String> = cell_re
            .captures_iter(row_html)
            .map(|c| {
                strip_tags(&c[1])
                    .trim()
                    .to_string()
            })
            .collect();
        if cells.len() < 8 {
            continue;
        }
        let date_str = cells[0].clone();
        if !date_str.contains(',') || date_str == "Date" {
            continue;
        }
        let tier = cells[1].trim().to_string();
        let kind = cells[2].trim().to_string();
        let tournament = cells[4].trim().to_string();
        let score_raw = cells[6].trim();
        let opponent = cells[7].trim().to_string();
        if opponent.is_empty() {
            continue;
        }
        let score = parse_score(score_raw);
        out.push(MatchRow {
            date_str,
            tier,
            kind,
            tournament,
            score,
            opponent,
            self_team: team_title.to_string(),
        });
    }
    Ok(out)
}

fn parse_score(raw: &str) -> Option<(u8, u8)> {
    let re = regex::Regex::new(r"(\d+)\s*:\s*(\d+)").unwrap();
    re.captures(raw).and_then(|c| {
        Some((c[1].parse().ok()?, c[2].parse().ok()?))
    })
}

/// 提取 {{Infobox ...}} 的键值对（处理嵌套花括号与 [[链接]]）
pub fn infobox_kv(w: &str) -> Vec<(String, String)> {
    let start = match w.find("{{Infobox") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut depth = 0i32;
    let mut end = w.len();
    for (i, ch) in w[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &w[start..end];
    let mut kv = Vec::new();
    for line in body.split('\n') {
        let line = line.trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim().to_lowercase();
        if k.is_empty() || k.contains("name") || k.contains("image") || k.contains("logo")
            || k.contains("caption") || k.contains("imagedark") || k.contains("premier")
            || k.contains("achievement") || k.contains("history")
        {
            continue;
        }
        let v = clean_wikitext(v);
        if !v.is_empty() {
            kv.push((k, v));
        }
    }
    kv
}

/// wikitext → 纯文本（去链接/模板残留）
fn clean_wikitext(s: &str) -> String {
    let mut s = s.to_string();
    // 多行模板参数值只取首行（如日期范围第二行）
    s = s.lines().next().unwrap_or("").to_string();
    s = s.replace("{{", "").replace("}}", "");
    // 去 [[a|b]] → b，[[a]] → a
    let re = regex::Regex::new(r"\[\[([^\]|]*\|)?([^\]]*)\]\]").unwrap();
    s = re.replace_all(&s, "$2").to_string();
    s = s.replace('|', ",").replace("&nbsp;", " ").replace("&amp;", "&");
    s.trim().to_string()
}

// ============ 信息查询 ============

#[derive(Debug)]
#[allow(dead_code)]
pub struct TeamInfo {
    pub title: String,
    pub url: String,
    pub region: String,
    pub location: String,
}

pub async fn team_info(title: &str) -> Result<TeamInfo> {
    let w = wikitext(title).await?;
    let kv: std::collections::HashMap<String, String> = infobox_kv(&w).into_iter().collect();
    Ok(TeamInfo {
        title: title.to_string(),
        url: page_url(title),
        region: kv.get("region").cloned().unwrap_or_default(),
        location: kv.get("location").cloned().unwrap_or_default(),
    })
}

#[derive(Debug)]
pub struct EventInfo {
    pub title: String,
    pub url: String,
    pub dates: String,
    pub organizer: String,
    pub prize: String,
    pub tier: String,
}

pub async fn event_info(title: &str) -> Result<EventInfo> {
    let w = wikitext(title).await?;
    let kv: std::collections::HashMap<String, String> = infobox_kv(&w).into_iter().collect();
    Ok(EventInfo {
        title: title.to_string(),
        url: page_url(title),
        dates: kv.get("date").or_else(|| kv.get("dates")).cloned().unwrap_or_default(),
        organizer: kv.get("organizer").cloned().unwrap_or_default(),
        prize: kv
            .get("prizepool")
            .or_else(|| kv.get("prizepoolusd"))
            .cloned()
            .unwrap_or_default(),
        tier: kv.get("liquipediatier").cloned().unwrap_or_default(),
    })
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct PlayerInfo {
    pub title: String,
    pub url: String,
    pub name: String,
    pub team: String,
    pub role: String,
    pub ids: String,
}

pub async fn player_info(title: &str) -> Result<PlayerInfo> {
    let w = wikitext(title).await?;
    let kv: std::collections::HashMap<String, String> = infobox_kv(&w).into_iter().collect();
    Ok(PlayerInfo {
        title: title.to_string(),
        url: page_url(title),
        name: title.to_string(),
        team: kv.get("team").cloned().unwrap_or_default(),
        role: kv.get("role").cloned().unwrap_or_default(),
        ids: kv
            .get("ids")
            .cloned()
            .filter(|s| !s.is_empty())
            .or_else(|| kv.get("steam").cloned())
            .unwrap_or_default(),
    })
}

/// 抓取速率保护（每次 API 请求间隔）
pub struct Throttle {
    last: Instant,
    min_gap: Duration,
}

impl Throttle {
    pub fn new(min_gap_ms: u64) -> Self {
        Self { last: Instant::now() - Duration::from_millis(min_gap_ms), min_gap: Duration::from_millis(min_gap_ms) }
    }
    pub async fn wait(&mut self) {
        let elapsed = self.last.elapsed();
        if elapsed < self.min_gap {
            tokio::time::sleep(self.min_gap - elapsed).await;
        }
        self.last = Instant::now();
    }
}

/// 便捷文本：把日期串换成更短的展示（保留原文）
pub fn fmt_date(raw: &str) -> String {
    raw.replace(" CEST", "").replace(" CET", "").replace(" - ", " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn score_parse() {
        assert_eq!(parse_score("0 : 2"), Some((0, 2)));
        assert_eq!(parse_score("2 : 0"), Some((2, 0)));
        assert_eq!(parse_score("W"), None);
    }
    #[test]
    fn kv_clean() {
        assert_eq!(clean_wikitext("[[Team Spirit]]"), "Team Spirit");
        assert_eq!(clean_wikitext("{{flag|ua}} [[Ukraine]]"), "flag,ua Ukraine");
    }
}

// ================= 订阅存储（日志目录下 JSON，随 bot 重启保留） =================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubChannel {
    /// 群订阅：战报推送到群（主动）
    Group(String),
    /// 单聊订阅：战报尝试主动推送到该用户（QQ 对 C2C 主动消息有限制）
    C2C(String),
}

#[allow(dead_code)]
impl SubChannel {
    pub fn as_group(&self) -> Option<&str> {
        match self {
            SubChannel::Group(g) => Some(g),
            _ => None,
        }
    }
    pub fn as_c2c(&self) -> Option<&str> {
        match self {
            SubChannel::C2C(u) => Some(u),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub channel: SubChannel,
    pub team_title: String,
}

/// 订阅 + 已推送去重 的持久化状态
pub struct Subs {
    path: std::path::PathBuf,
    list: Vec<Subscription>,
    pushed: Vec<String>,
}

impl Subs {
    pub fn load(path: std::path::PathBuf) -> Self {
        let (list, pushed) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<(Vec<Subscription>, Vec<String>)>(&s).ok())
            .unwrap_or_default();
        Subs { path, list, pushed }
    }

    fn save(&self) {
        let _ = self.path.parent().map(|p| std::fs::create_dir_all(p));
        if let Ok(s) = serde_json::to_string(&(&self.list, &self.pushed)) {
            let _ = std::fs::write(&self.path, s);
        }
    }

    /// 某频道订阅了某战队？
    pub fn subscribed(&self, channel: &SubChannel, team: &str) -> bool {
        self.list
            .iter()
            .any(|s| s.channel == *channel && s.team_title.eq_ignore_ascii_case(team))
    }

    pub fn add(&mut self, channel: SubChannel, team: String) -> bool {
        if self.subscribed(&channel, &team) {
            return false;
        }
        self.list.push(Subscription { channel, team_title: team });
        self.save();
        true
    }

    pub fn remove(&mut self, channel: &SubChannel, team: &str) -> bool {
        let before = self.list.len();
        self.list.retain(|s| !(s.channel == *channel && s.team_title.eq_ignore_ascii_case(team)));
        let removed = self.list.len() < before;
        if removed {
            self.save();
        }
        removed
    }

    pub fn list_channel(&self, channel: &SubChannel) -> Vec<String> {
        self.list
            .iter()
            .filter(|s| s.channel == *channel)
            .map(|s| s.team_title.clone())
            .collect()
    }

    /// 全部去重后的战队名单（轮询用）
    pub fn all_teams(&self) -> Vec<String> {
        let mut v: Vec<String> = self.list.iter().map(|s| s.team_title.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// 某战队的所有订阅频道
    pub fn channels_of(&self, team: &str) -> Vec<SubChannel> {
        self.list
            .iter()
            .filter(|s| s.team_title.eq_ignore_ascii_case(team))
            .map(|s| s.channel.clone())
            .collect()
    }

    /// 是否已推送过该场
    pub fn peek(&self, key: &str) -> bool {
        self.pushed.iter().any(|k| k == key)
    }

    /// 标记已推送（返回是否“首次”）
    pub fn mark_pushed(&mut self, key: &str) -> bool {
        if self.pushed.contains(&key.to_string()) {
            return false;
        }
        self.pushed.push(key.to_string());
        if self.pushed.len() > 4000 {
            let keep = self.pushed.len() - 2000;
            self.pushed.drain(..keep);
        }
        self.save();
        true
    }
}

/// 战报文本
pub fn report_text(row: &MatchRow) -> String {
    let (a, b) = row.score.unwrap_or((0, 0));
    let winner = if a > b { row.self_team.as_str() } else { row.opponent.as_str() };
    let date = fmt_date(&row.date_str);
    format!(
        "🏆 战报 · {}\n{}\n🆚 {} {} : {} {}（胜者 {}）\n📅 {} · {} · {}\n🔗 {}",
        row.tournament,
        row.self_team,
        row.self_team, a, b, row.opponent,
        winner,
        date,
        if row.tier.is_empty() { "—" } else { &row.tier },
        if row.kind.is_empty() { "—" } else { &row.kind },
        page_url(&row.self_team)
    )
}

/// 下一场/最近一场的简短介绍文本
#[allow(dead_code)]
pub fn upcoming_text(row: &MatchRow) -> String {
    format!(
        "⏳ 赛程 · {}\n🆚 {} vs {}（{}）\n📅 {} · {}",
        row.tournament,
        row.self_team,
        row.opponent,
        if row.score.is_some() { "已结束" } else { "待开赛" },
        fmt_date(&row.date_str),
        if row.tier.is_empty() { "—" } else { &row.tier }
    )
}
