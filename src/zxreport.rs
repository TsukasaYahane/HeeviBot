//! 每日早报插件：每天 09:00（可配）向已订阅的群/单聊推送一张浅色早报卡片。
//!
//! 数据源（全部免费公开接口）：
//! - 60s API（vikiboss/60s）`/v2/60s` 每日 60 秒读懂世界、`/v2/weibo` 微博热搜、
//!   `/v2/today_in_history` 历史上的今天
//! - Bangumi API `/calendar` 今日放送番剧
//!
//! 排版参考 nonebot-plugin-zxreport（早报），但输出为本地绘制 PNG（无外部渲染服务）。
//!
//! 指令：`早报`（立即预览）/ `早报订阅` / `取消早报` / `早报订阅列表` / `早报帮助`

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

use crate::config::Config;
use crate::onebot::{Chat, Image, OneBot, Target};
use crate::zxcard::{self, ZxCard};

const NEWS_API: &str = "https://60s.viki.moe/v2/60s";
const HOT_API: &str = "https://60s.viki.moe/v2/weibo";
const HIST_API: &str = "https://60s.viki.moe/v2/today_in_history";
const BANGUMI_API: &str = "https://api.bgm.tv/calendar";
const GAME_RSS: &str = "https://www.gcores.com/rss";

const MAX_NEWS: usize = 15;
const MAX_HOT: usize = 8;
const MAX_ANIME: usize = 6;
const MAX_HIST: usize = 3;
const MAX_GAME: usize = 5;
/// 游戏条目标题最大字数（超出裁剪）
const GAME_TITLE_MAX: usize = 44;
/// 描述截断长度（早报里过长的事件描述只保留开头）
const HIST_DESC_MAX: usize = 96;

// ==================== 数据 ====================

#[derive(Default, Clone)]
pub struct ZxData {
    pub date_line: String,
    pub news: Vec<String>,
    pub hot: Vec<(String, u64)>,
    pub anime: Vec<(String, f32, u32)>,
    pub game: Vec<(String, String)>, // 游戏专区（标题，类型标签）
    pub history: Vec<(String, String)>,
    pub tip: String,
}

fn client() -> reqwest::Client {
    static CL: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CL.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("HeeviBot/1.0 (qqbot zxreport; low-rate)")
            .gzip(true)
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default()
    })
    .clone()
}

/// 北京时间的“今天”（UTC+8，避免依赖系统时区）
pub fn now_cn() -> chrono::DateTime<chrono::FixedOffset> {
    let tz = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8");
    chrono::Utc::now().with_timezone(&tz)
}

fn weekday_cn(w: u32) -> &'static str {
    match w {
        1 => "星期一",
        2 => "星期二",
        3 => "星期三",
        4 => "星期四",
        5 => "星期五",
        6 => "星期六",
        _ => "星期日",
    }
}

async fn get_json(url: &str) -> Option<serde_json::Value> {
    let resp = client().get(url).send().await.ok()?;
    if !resp.status().is_success() {
        warn!("早报数据源 {} 返回 {}", url, resp.status());
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

/// 抓纯文本（RSS 用）
async fn get_text(url: &str) -> Option<String> {
    let resp = client().get(url).send().await.ok()?;
    if !resp.status().is_success() {
        warn!("早报数据源 {} 返回 {}", url, resp.status());
        return None;
    }
    resp.text().await.ok()
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn strip_cdata(s: &str) -> String {
    let s = s.trim();
    let inner = s
        .strip_prefix("<![CDATA[")
        .and_then(|x| x.strip_suffix("]]>"))
        .unwrap_or(s);
    xml_unescape(inner)
}

/// 取 <tag>...</tag> 的文本（自动去 CDATA / 反转义空白）
fn tag_text(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let i = block.find(&open)? + open.len();
    let j = block[i..].find(&close)? + i;
    let s = strip_cdata(&block[i..j]);
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// 极简 RSS 解析：只取 <item> 里的 title/link/pubDate，不引第三方 XML 依赖
pub fn parse_rss_items(xml: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<item>") {
        let after = &rest[i + "<item>".len()..];
        let j = match after.find("</item>") {
            Some(j) => j,
            None => break,
        };
        let block = &after[..j];
        rest = &after[j + "</item>".len()..];
        let title = match tag_text(block, "title") {
            Some(t) => t,
            None => continue,
        };
        let link = match tag_text(block, "link") {
            Some(t) => t,
            None => continue,
        };
        let date = tag_text(block, "pubDate").unwrap_or_default();
        out.push((title, link, date));
    }
    out
}

/// 从链接判断条目类型（机核：/articles /radios /videos /gamenews）
fn rss_kind(link: &str) -> &'static str {
    if link.contains("/radios/") {
        "电台"
    } else if link.contains("/videos/") {
        "视频"
    } else if link.contains("/gamenews/") {
        "快讯"
    } else if link.contains("/articles/") {
        "文章"
    } else {
        "资讯"
    }
}

/// 站务 / 会员推广类条目（GPASS 优惠、节目单、续费通知…）不进早报
fn is_site_notice(title: &str) -> bool {
    ["GPASS", "优惠", "续费", "节目单", "补发", "招募", "招聘"]
        .iter()
        .any(|k| title.contains(k))
}

/// 标题裁剪（按字符数，不按字节）
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

/// 挑「最热门」的游戏条目：机核 RSS 没有热度字段，只有首页顺位（越靠前＝编辑部越推荐），
/// 因此按顺位取前 N 条；电台/视频这类长节目最多 1 条，避免整屏都是播客；站务推广条目跳过。
pub fn pick_game_items(items: &[(String, String)], max: usize) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut long_form = 0usize;
    for (title, kind) in items {
        if out.len() >= max {
            break;
        }
        if is_site_notice(title) {
            continue;
        }
        if kind == "电台" || kind == "视频" {
            if long_form >= 1 {
                continue;
            }
            long_form += 1;
        }
        out.push((clip(title, GAME_TITLE_MAX), kind.clone()));
    }
    out
}

/// 抓取全部数据源；单个源失败只影响对应板块（不会整体失败）
pub async fn fetch() -> ZxData {
    let now = now_cn();
    let mut d = ZxData {
        date_line: format!(
            "{} · {}",
            now.format("%Y 年 %m 月 %d 日"),
            weekday_cn(now.format("%u").to_string().parse().unwrap_or(1))
        ),
        ..Default::default()
    };

    // —— 60 秒读懂世界 ——
    if let Some(v) = get_json(NEWS_API).await {
        let data = &v["data"];
        if let Some(arr) = data["news"].as_array() {
            d.news = arr
                .iter()
                .filter_map(|x| x.as_str())
                .take(MAX_NEWS)
                .map(|s| s.trim().to_string())
                .collect();
        }
        if let Some(t) = data["tip"].as_str() {
            d.tip = t.trim().to_string();
        }
        let dow = data["day_of_week"].as_str().unwrap_or("");
        let lunar = data["lunar_date"].as_str().unwrap_or("");
        if !dow.is_empty() || !lunar.is_empty() {
            d.date_line = format!("{} · {} · {}", now.format("%Y 年 %m 月 %d 日"), dow, lunar);
        }
    }

    // —— 微博热搜 ——
    if let Some(v) = get_json(HOT_API).await {
        if let Some(arr) = v["data"].as_array() {
            d.hot = arr
                .iter()
                .filter_map(|x| {
                    let t = x["title"].as_str()?.trim().to_string();
                    if t.is_empty() {
                        return None;
                    }
                    Some((t, x["hot_value"].as_u64().unwrap_or(0)))
                })
                .take(MAX_HOT)
                .collect();
        }
    }

    // —— 历史上的今天 ——
    if let Some(v) = get_json(HIST_API).await {
        if let Some(arr) = v["data"]["items"].as_array() {
            d.history = arr
                .iter()
                .filter_map(|x| {
                    let title = x["title"].as_str()?.trim().to_string();
                    let year = x["year"].as_str().unwrap_or("").trim().to_string();
                    let desc = x["description"]
                        .as_str()
                        .unwrap_or("")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let desc = if desc.chars().count() > HIST_DESC_MAX {
                        format!("{}…", desc.chars().take(HIST_DESC_MAX).collect::<String>())
                    } else {
                        desc
                    };
                    let head = if year.is_empty() {
                        title
                    } else {
                        format!("{year} 年 · {title}")
                    };
                    Some((head, desc))
                })
                .take(MAX_HIST)
                .collect();
        }
    }

    // —— 今日番剧（Bangumi 每日放送） ——
    if let Some(v) = get_json(BANGUMI_API).await {
        let today: u32 = now.format("%u").to_string().parse().unwrap_or(1);
        if let Some(days) = v.as_array() {
            for day in days {
                if day["weekday"]["id"].as_u64() != Some(today as u64) {
                    continue;
                }
                let mut items: Vec<(String, f32, u32)> = day["items"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|it| {
                                let name = it["name_cn"]
                                    .as_str()
                                    .filter(|s| !s.trim().is_empty())
                                    .unwrap_or_else(|| it["name"].as_str().unwrap_or(""));
                                if name.trim().is_empty() {
                                    return None;
                                }
                                let score = it["rating"]["score"].as_f64().unwrap_or(0.0) as f32;
                                let doing = it["collection"]["doing"].as_u64().unwrap_or(0) as u32;
                                Some((name.trim().to_string(), score, doing))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                items.sort_by(|a, b| b.2.cmp(&a.2));
                items.truncate(MAX_ANIME);
                d.anime = items;
                break;
            }
        }
    }

    // —— 游戏专区（机核 GCORES RSS） ——
    if let Some(xml) = get_text(GAME_RSS).await {
        let items: Vec<(String, String)> = parse_rss_items(&xml)
            .into_iter()
            .map(|(t, l, _)| (t, rss_kind(&l).to_string()))
            .collect();
        d.game = pick_game_items(&items, MAX_GAME);
        if d.game.is_empty() {
            warn!("早报：机核 RSS 没解析出可用条目（{} 字节）", xml.len());
        }
    }

    d
}

pub fn load_icon(path: &str) -> Option<image::RgbaImage> {
    let bytes = std::fs::read(path).ok()?;
    image::load_from_memory(&bytes).ok().map(|d| d.to_rgba8())
}

/// 生成早报 PNG（含页脚来源说明）；返回 (png 字节, 附带文字)
pub fn render_png(cfg: &Config, d: &ZxData) -> anyhow::Result<(Vec<u8>, String)> {
    let icon = load_icon(&cfg.zx_icon);
    let card = ZxCard {
        date_line: d.date_line.clone(),
        news: d.news.clone(),
        hot: d.hot.clone(),
        anime: d.anime.clone(),
        game: d.game.clone(),
        history: d.history.clone(),
        tip: d.tip.clone(),
        icon,
        icon_note: "Heevi 早报 · 数据来源：60s API / 微博 / Bangumi / 机核 · 每日 09:00 推送".to_string(),
    };
    let img = zxcard::render(&card);
    // 先试 256 色索引 PNG（~180KB）；失败再退回真彩 PNG（最高压缩，~600KB）
    if let Some(bytes) = crate::png8::encode_indexed_png(&img) {
        return Ok((bytes, text_digest(d)));
    }
    // 用最高压缩 + 自适应滤波：同样的画面比默认设置小 ~25%（1080 宽长图省下约 200KB）
    let mut buf = Vec::new();
    {
        use image::ImageEncoder;
        let enc = image::codecs::png::PngEncoder::new_with_quality(
            std::io::Cursor::new(&mut buf),
            image::codecs::png::CompressionType::Best,
            image::codecs::png::FilterType::Adaptive,
        );
        enc.write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )?;
    }
    let caption = text_digest(d);
    Ok((buf, caption))
}

/// 纯文本兜底版（图片发送失败或用户在纯文本场景要求时使用）
pub fn text_digest(d: &ZxData) -> String {
    let mut s = format!("📰 Heevi 早报 · {}\n", d.date_line);
    if !d.news.is_empty() {
        s.push_str("\n【60 秒读懂世界】\n");
        for (i, n) in d.news.iter().take(MAX_NEWS).enumerate() {
            s.push_str(&format!("{}. {}\n", i + 1, n));
        }
    }
    if !d.hot.is_empty() {
        s.push_str("\n【微博热搜】\n");
        for (i, (t, _)) in d.hot.iter().enumerate() {
            s.push_str(&format!("{}. {}\n", i + 1, t));
        }
    }
    if !d.anime.is_empty() {
        s.push_str("\n【今日番剧】\n");
        for (n, score, doing) in &d.anime {
            s.push_str(&format!("· {}（{:.1} / {} 人在看）\n", n, score, doing));
        }
    }
    if !d.game.is_empty() {
        s.push_str("\n【游戏专区 · 机核】\n");
        for (t, k) in &d.game {
            s.push_str(&format!("· [{}] {}\n", k, t));
        }
    }
    if !d.tip.trim().is_empty() {
        s.push_str(&format!("\n「{}」", d.tip.trim()));
    }
    s
}

// ==================== 订阅存储 ====================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Kind {
    #[serde(rename = "group")]
    Group,
    #[serde(rename = "c2c")]
    C2C,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sub {
    pub kind: Kind,
    pub id: String,
    pub at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    subs: Vec<Sub>,
}

pub struct Store {
    path: PathBuf,
    state: State,
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, state }
    }

    fn save(&self) {
        if let Some(p) = self.path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if let Ok(s) = serde_json::to_string_pretty(&self.state) {
            let _ = std::fs::write(&self.path, s);
        }
    }

    pub fn has(&self, kind: &Kind, id: &str) -> bool {
        self.state
            .subs
            .iter()
            .any(|s| &s.kind == kind && s.id == id)
    }

    /// 返回 true 表示新增成功（false = 此前已订阅）
    pub fn add(&mut self, kind: Kind, id: &str) -> bool {
        if self.has(&kind, id) {
            return false;
        }
        self.state.subs.push(Sub {
            kind,
            id: id.to_string(),
            at: chrono::Utc::now().timestamp(),
        });
        self.save();
        true
    }

    pub fn remove(&mut self, kind: &Kind, id: &str) -> bool {
        let before = self.state.subs.len();
        self.state
            .subs
            .retain(|s| !(&s.kind == kind && s.id == id));
        let changed = self.state.subs.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    pub fn count(&self) -> usize {
        self.state.subs.len()
    }

    pub fn ids_of(&self, kind: &Kind) -> Vec<String> {
        self.state
            .subs
            .iter()
            .filter(|s| &s.kind == kind)
            .map(|s| s.id.clone())
            .collect()
    }
}

/// 订阅文件路径：<log_dir>/zxreport/subs.json
pub fn store_path(cfg: &Config) -> PathBuf {
    PathBuf::from(&cfg.log_dir).join("zxreport").join("subs.json")
}

// ==================== 指令 ====================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// 立即生成一张（预览 / 手动推送）
    Preview,
    Subscribe,
    Unsubscribe,
    List,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Where {
    Group,
    C2C,
}

pub fn parse(text: &str) -> Option<Cmd> {
    let t = text
        .trim()
        .trim_start_matches(|c: char| matches!(c, '/' | '!' | '！'))
        .trim_start_matches(|c: char| c.is_whitespace() || c == '。' || c == '.');
    if t.is_empty() {
        return None;
    }
    let eq = |kw: &str| t == kw;
    if eq("早报帮助") || eq("早报菜单") {
        return Some(Cmd::Help);
    }
    if eq("早报订阅列表") || eq("早报列表") {
        return Some(Cmd::List);
    }
    if eq("早报订阅") || eq("订阅早报") || eq("开启早报") || eq("开早报") {
        return Some(Cmd::Subscribe);
    }
    if eq("取消早报") || eq("早报取消") || eq("关闭早报") || eq("退订早报") {
        return Some(Cmd::Unsubscribe);
    }
    if eq("早报") || eq("今日早报") || eq("早报预览") {
        return Some(Cmd::Preview);
    }
    None
}

pub fn help_text(cfg: &Config) -> String {
    format!(
        "📰 早报（{} 推送）\n\
         早报 —— 立即生成一张今日早报\n\
         早报订阅 —— 订阅每日推送（当前会话）\n\
         取消早报 —— 退订\n\
         早报列表 —— 查看订阅状态\n\
         内容：60 秒读懂世界 / 微博热搜 / 今日番剧 / 历史上的今天",
        push_time_desc(cfg)
    )
}

pub fn push_time_desc(cfg: &Config) -> String {
    format!("每天 {:02}:{:02}", cfg.zx_hour, cfg.zx_minute)
}

/// 处理一条早报指令（预览/订阅/退订/列表/帮助）
pub async fn run_cmd(
    ob: OneBot,
    msg: Target,
    ch: Where,
    id: String,
    store: Arc<Mutex<Store>>,
    cfg: Arc<Config>,
    cmd: Cmd,
) {
    let kind = match ch {
        Where::Group => Kind::Group,
        Where::C2C => Kind::C2C,
    };
    match cmd {
        Cmd::Help => {
            let out = help_text(&cfg);
            if let Err(e) = msg.reply(&ob, &out).await {
                warn!("早报帮助回复失败：{e}");
            }
        }
        Cmd::Subscribe => {
            let added = store
                .lock()
                .ok()
                .map(|mut s| s.add(kind, &id))
                .unwrap_or(false);
            let out = if added {
                format!(
                    "✅ 已订阅早报。{}\n内容：60 秒读懂世界 / 微博热搜 / 今日番剧 / 历史上的今天。\n发「早报」可随时预览，「取消早报」退订。",
                    push_time_desc(&cfg)
                )
            } else {
                "已经在订阅列表里了，不用重复订阅。".to_string()
            };
            if let Err(e) = msg.reply(&ob, &out).await {
                warn!("早报订阅回复失败：{e}");
            }
        }
        Cmd::Unsubscribe => {
            let removed = store
                .lock()
                .ok()
                .map(|mut s| s.remove(&kind, &id))
                .unwrap_or(false);
            let out = if removed {
                "已取消早报推送。想再看时发「早报订阅」就行。"
            } else {
                "本会话并没有订阅早报。"
            };
            if let Err(e) = msg.reply(&ob, out).await {
                warn!("早报退订回复失败：{e}");
            }
        }
        Cmd::List => {
            let (mine, total) = {
                let lock = store.lock();
                match lock {
                    Ok(s) => (s.has(&kind, &id), s.count()),
                    Err(_) => (false, 0),
                }
            };
            let out = format!(
                "{}本会话订阅状态：{}\n全局订阅会话：{} 个\n推送时间：{}",
                "📋 ", 
                if mine { "已订阅" } else { "未订阅" },
                total,
                push_time_desc(&cfg)
            );
            if let Err(e) = msg.reply(&ob, &out).await {
                warn!("早报列表回复失败：{e}");
            }
        }
        Cmd::Preview => {
            info!("早报预览：id={id}");
            send_report(&ob, &msg, &cfg).await;
        }
    }
}

/// 立即生成并“回复”一张早报；失败时回退纯文本。返回是否成功发出了图片
pub async fn send_report(ob: &OneBot, msg: &Target, cfg: &Config) -> bool {
    let data = fetch().await;
    let (png, caption) = match render_png(cfg, &data) {
        Ok(v) => v,
        Err(e) => {
            warn!("早报渲染失败：{e}");
            let _ = msg.reply(ob, "早报生成失败了，稍后再试…").await;
            return false;
        }
    };
    match msg.reply_media(ob, &Image::Bytes(png), greeting(&data)).await {
        Ok(_) => true,
        Err(e) => {
            warn!("早报图文回复失败，回退纯文本：{e:#}");
            let _ = msg.reply(ob, &caption).await;
            false
        }
    }
}

fn greeting(d: &ZxData) -> &'static str {
    let _ = d;
    "早安。今日份早报。"
}

// ==================== 定时推送 ====================

/// 下一个推送时刻（北京时间当天/次日 hh:mm）
fn next_push_at(
    now: chrono::DateTime<chrono::FixedOffset>,
    hour: u32,
    minute: u32,
) -> chrono::DateTime<chrono::FixedOffset> {
    use chrono::{Datelike, TimeZone};
    let tz = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8");
    let today = tz
        .with_ymd_and_hms(now.year(), now.month(), now.day(), hour, minute, 0)
        .single()
        .unwrap_or(now);
    if today <= now {
        today + chrono::Duration::days(1)
    } else {
        today
    }
}

/// 常驻调度：每天 hh:mm 向订阅会话推送早报
pub async fn run_loop(cfg: Arc<Config>, ob: OneBot, store: Arc<Mutex<Store>>) {
    // 调试：QQ_BOT_ZX_TEST=true 时启动即推一次（不必等到 09:00）
    if cfg.zx_test {
        ob.wait_ready().await;
        info!("早报测试模式：LLBot 已就绪，5 秒后推送一次");
        tokio::time::sleep(Duration::from_secs(5)).await;
        push_to_all(cfg.clone(), ob.clone(), store.clone()).await;
    }
    if !cfg.zx_enabled {
        info!("早报推送未启用（QQ_BOT_ZX_ENABLED=false），仅支持手动「早报」");
        return;
    }
    loop {
        let now = now_cn();
        let next = next_push_at(now, cfg.zx_hour, cfg.zx_minute);
        let wait_ms = (next - now).num_milliseconds().max(1_000) as u64;
        let subs = store.lock().map(|s| s.count()).unwrap_or(0);
        info!(
            "下次早报推送: {}（{:.0} 分钟后，{} 个订阅会话）",
            next.format("%Y-%m-%d %H:%M:%S"),
            wait_ms as f64 / 60_000.0,
            subs
        );
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        push_to_all(cfg.clone(), ob.clone(), store.clone()).await;
    }
}

/// 推送一次早报到所有订阅会话（主动消息）
pub async fn push_to_all(cfg: Arc<Config>, ob: OneBot, store: Arc<Mutex<Store>>) {
    let (groups, c2cs) = match store.lock() {
        Ok(s) => (s.ids_of(&Kind::Group), s.ids_of(&Kind::C2C)),
        Err(_) => (Vec::new(), Vec::new()),
    };
    // 订阅里存的是 QQ 号字符串；旧版官方机器人的 openid 解析不成数字，直接跳过
    let targets: Vec<(Chat, String)> = groups
        .iter()
        .filter_map(|g| Chat::group_str(g).map(|c| (c, format!("群 {g}"))))
        .chain(
            c2cs.iter()
                .filter_map(|u| Chat::private_str(u).map(|c| (c, format!("私聊 {u}")))),
        )
        .collect();
    if targets.is_empty() {
        info!("早报推送：当前没有订阅会话，跳过");
        return;
    }

    let data = fetch().await;
    let (png, caption) = match render_png(&cfg, &data) {
        Ok(v) => v,
        Err(e) => {
            warn!("早报渲染失败，本次推送取消：{e}");
            return;
        }
    };
    let img = Image::Bytes(png);
    let greeting = greeting(&data);

    for (chat, label) in targets {
        let r = match ob.send_media(chat, &img, greeting).await {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!("早报图片发送失败（{label}），改发纯文本：{e:#}");
                ob.send_text(chat, &caption).await
            }
        };
        match r {
            Ok(_) => info!("早报已推送到{label}"),
            Err(e) => warn!("早报推送失败（{label}）：{e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 指令解析：订阅/退订/列表/预览/帮助互不串味
    #[test]
    fn 指令解析() {
        assert_eq!(parse("早报"), Some(Cmd::Preview));
        assert_eq!(parse(" 早报订阅 "), Some(Cmd::Subscribe));
        assert_eq!(parse("取消早报"), Some(Cmd::Unsubscribe));
        assert_eq!(parse("早报列表"), Some(Cmd::List));
        assert_eq!(parse("help 早报"), None); // 帮助走 handler 的 help 指令
        assert_eq!(parse("早报帮助"), Some(Cmd::Help));
        assert_eq!(parse("早报订阅列表"), Some(Cmd::List));
        assert_eq!(parse("今天天气"), None);
    }

    /// 订阅存储：加/查/去重/落盘
    #[test]
    fn 订阅增删去重() {
        let dir = std::env::temp_dir().join(format!("zxstore_{}", std::process::id()));
        let path = dir.join("subs.json");
        let mut st = Store::load(path.clone());
        assert!(st.add(Kind::Group, "G1"));
        assert!(!st.add(Kind::Group, "G1"), "重复订阅应返回 false");
        assert!(st.add(Kind::C2C, "U1"));
        assert_eq!(st.count(), 2);
        assert!(st.remove(&Kind::Group, "G1"));
        assert!(st.ids_of(&Kind::Group).is_empty());
        assert_eq!(st.ids_of(&Kind::C2C), vec!["U1".to_string()]);
        let reload = Store::load(path);
        assert_eq!(reload.count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RSS 解析：CDATA / 实体反转义 / 缺字段容错
    #[test]
    fn rss解析() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?><rss><channel><title>机核</title>
        <item><title><![CDATA[评测：A &amp; B]]></title><link>https://www.gcores.com/articles/1</link><pubDate>Thu, 10 Sep 2026 22:59:00 +0800</pubDate></item>
        <item><title>电台节目</title><link>https://www.gcores.com/radios/2</link></item>
        <item><title>坏条目（没有 link）</title></item>
        </channel></rss>"#;
        let items = parse_rss_items(xml);
        assert_eq!(items.len(), 2, "缺 link 的条目应被跳过");
        assert_eq!(items[0].0, "评测：A & B");
        assert_eq!(rss_kind(&items[0].1), "文章");
        assert_eq!(rss_kind(&items[1].1), "电台");
        assert_eq!(items[1].2, "", "缺 pubDate 不应 panic");
    }

    /// 游戏专区挑条目：跳站务、长节目限 1 条、长标题裁剪
    #[test]
    fn 游戏专区挑选() {
        let items: Vec<(String, String)> = vec![
            ("GPASS 新一季优惠说明：价格与续费".to_string(), "文章".to_string()),
            ("长".repeat(60), "电台".to_string()),
            ("第二条电台不该出现".to_string(), "电台".to_string()),
            ("C".to_string(), "文章".to_string()),
            ("D".to_string(), "文章".to_string()),
        ];
        let picked = pick_game_items(&items, 3);
        assert_eq!(picked.len(), 3);
        assert_eq!(picked[0].1, "电台");
        assert!(picked[0].0.ends_with('…'), "超长标题应被裁剪");
        assert_eq!(picked[1].0, "C", "第二条电台应被限流、站务条目应被跳过");
        assert_eq!(clip("短标题", 44), "短标题");
        assert_eq!(picked[2].0, "D");
    }

    /// 联网渲染一张样例（验收排版用）：
    /// cargo test --release 渲染早报样例 -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn 渲染早报样例() {
        let cfg = Config::from_env();
        let data = fetch().await;
        println!(
            "数据：新闻 {} 条 / 热搜 {} 条 / 番剧 {} 部 / 游戏 {} 条 / 历史 {} 条",
            data.news.len(),
            data.hot.len(),
            data.anime.len(),
            data.game.len(),
            data.history.len()
        );
        println!("游戏专区：{:?}", data.game);
        println!("日期行：{}", data.date_line);
        assert!(!data.news.is_empty(), "新闻为空，接口可能挂了");
        let (png, caption) = render_png(&cfg, &data).expect("渲染失败");
        let dir = PathBuf::from(&cfg.log_dir).join("zxreport");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("preview.png");
        std::fs::write(&path, &png).unwrap();
        println!("样例已写出：{}（{} KB）", path.display(), png.len() / 1024);
        println!("配文：{caption}");
    }
}
