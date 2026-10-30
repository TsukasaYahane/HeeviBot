//! 群聊日报：采集 → 每日 00:01 自动汇总 → PNG 渲染 → 图片发回群。
//!
//! 记录：收到群消息即追加 `<QQ_BOT_LOG_DIR>/digest/<群号>/<YYYY-MM-DD>.jsonl`。
//! 生成：00:01 对“昨天”有数据的群跑 DSH 总结（话题 + 热榜 + 总结），渲染成 PNG，
//! 经 LLBot 以 `base64://` 图片发回对应群（同一群一天只发一次，.done 标记防重）。
//!
//! 数据范围：LLBot 登录的是普通 QQ 账号，能收到所在群的全部消息，日报统计的是全群聊天。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::Utc;

/// 中国标准时间（UTC+8）。Windows 上 chrono 的 Local::now() 有双偏移 bug（比真实时间快 8h），
/// 统一用 Utc+8 显式计算，避免日报/调度错 8 小时。
fn now_cn() -> chrono::DateTime<chrono::FixedOffset> {
    Utc::now().with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8"))
}
use image::{DynamicImage, ImageFormat};
use tokio::time::sleep;

use crate::config::Config;
use crate::dsh::DshBridge;
use crate::onebot::{Chat, Image, OneBot};
use crate::render::{render_card, save_png, DigestCard};

const MIN_LINES_FOR_SUMMARY: usize = 2;

/// 把一条群消息落盘（日报数据源）。失败仅警告不阻断。
pub fn record_group_message(cfg: &Config, group_id: &str, member_id: &str, nick: &str, text: &str) {
    if !cfg.digest_enabled {
        return;
    }
    let now = now_cn();
    let path = jsonl_path(cfg, group_id, &now.format("%Y-%m-%d").to_string());
    let line = format!(
        "{{\"ts\":{},\"group\":\"{}\",\"member\":\"{}\",\"nick\":{},\"text\":{}}}\n",
        now.timestamp(),
        group_id.replace('"', ""),
        member_id.replace('"', ""),
        serde_json::to_string(nick).unwrap_or_else(|_| "\"\"".to_string()),
        serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
    );
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    if let Err(e) = f.write_all(line.as_bytes()) {
        tracing::warn!("群消息记录写入失败（{}）: {e}", path.display());
    }
}

/// 常驻调度：启动后先跑一次测试日期（若配置），然后每天 00:01 汇总“昨天”。
pub async fn run_daily_loop(dsh: Arc<DshBridge>, cfg: Arc<Config>, ob: OneBot, mode: Arc<crate::mode::ModeState>) {
    // 调试用：QQ_BOT_DIGEST_TEST_DATE=YYYY-MM-DD 时启动立即生成一次
    if let Some(date) = cfg.digest_test_date.clone() {
        ob.wait_ready().await;
        tracing::info!("日报测试模式：立即为 {date} 生成");
        generate_for_date(&dsh, &cfg, &ob, &date).await;
    }
    if !cfg.digest_enabled {
        return;
    }
    loop {
        let now = now_cn();
        let next = next_midnight_plus(now);
        let wait_ms = (next - now).num_milliseconds().max(1_000) as u64;
        tracing::info!("下次群日报调度: {}（{:.0} 分钟后）", next.format("%Y-%m-%d %H:%M:%S"), wait_ms as f64 / 60_000.0);
        sleep(Duration::from_millis(wait_ms)).await;
        let yesterday = (now_cn() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        if !mode.get().digest() {
            tracing::info!("{} 模式：跳过 {yesterday} 的群日报（要调用 AI）", mode.get().name());
            continue;
        }
        tracing::info!("开始生成 {yesterday} 的群日报");
        generate_for_date(&dsh, &cfg, &ob, &yesterday).await;
    }
}

/// 为某个日期生成所有有数据且未生成的群日报
async fn generate_for_date(dsh: &DshBridge, cfg: &Config, ob: &OneBot, date: &str) {
    let root = PathBuf::from(&cfg.log_dir).join("digest");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let group_dir = entry.path();
        if !group_dir.is_dir() {
            continue;
        }
        let Some(group_id) = group_dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // 目录名就是群号；旧版 QQ 官方机器人留下的 openid 目录（以及 render/）无法发送，跳过
        let Some(chat) = Chat::group_str(group_id) else {
            continue;
        };
        let jsonl = group_dir.join(format!("{date}.jsonl"));
        if !jsonl.exists() {
            continue;
        }
        let done = group_dir.join(format!("{date}.done"));
        if done.exists() {
            continue;
        }
        match generate_one(dsh, cfg, ob, chat, group_id, date).await {
            Ok(summary) => {
                tracing::info!("群日报已生成 [{group_id}] {date}: {summary}");
                let _ = std::fs::write(&done, "ok");
            }
            Err(e) => tracing::warn!("群日报生成失败 [{group_id}] {date}: {e:#}"),
        }
    }
}

/// 一条群消息记录
struct Line {
    ts: i64,
    member: String,
    nick: String,
    text: String,
}

/// AI 给出的日报内容
#[derive(Debug, Default, PartialEq)]
struct AiDigest {
    topics: Vec<String>,
    keywords: Vec<String>,
    quotes: Vec<(String, String)>,
    summary: String,
}

/// 单个群日报：读数据 → 生成卡片 → 发回群
async fn generate_one(
    dsh: &DshBridge,
    cfg: &Config,
    ob: &OneBot,
    chat: Chat,
    group_id: &str,
    date: &str,
) -> Result<String> {
    let Some((card, msg_count, members)) = build_card(dsh, cfg, Some(ob), group_id, date).await? else {
        return Ok("消息过少，跳过".to_string());
    };
    let img = render_card(&card);
    // 本地留一份副本便于检查渲染效果
    let local_png = PathBuf::from(&cfg.log_dir)
        .join("digest")
        .join("render")
        .join(format!("{group_id}_{date}.png"));
    if let Err(e) = save_png(&img, &local_png) {
        tracing::warn!("日报 PNG 本地副本保存失败: {e}");
    }
    let png_bytes = encode_png(img)?;
    ob.send_media(chat, &Image::Bytes(png_bytes), "")
        .await
        .map_err(|e| anyhow!("发送群图片失败: {e:#}"))?;
    Ok(format!("{msg_count} 条消息，{members} 位成员"))
}

/// 256 色索引 PNG（体积约为真彩的 1/3）；失败退回普通 PNG
fn encode_png(img: image::RgbImage) -> Result<Vec<u8>> {
    if let Some(b) = crate::png8::encode_indexed_png(&img) {
        return Ok(b);
    }
    let mut png_bytes: Vec<u8> = Vec::new();
    DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut png_bytes), ImageFormat::Png)
        .map_err(|e| anyhow!("PNG 编码失败: {e}"))?;
    Ok(png_bytes)
}

/// 读当天记录、本地统计、AI 提炼，组装日报卡片。消息太少返回 None。
/// `ob` 为 None 时不查群名（预览 / 测试用）。
async fn build_card(
    dsh: &DshBridge,
    cfg: &Config,
    ob: Option<&OneBot>,
    group_id: &str,
    date: &str,
) -> Result<Option<(DigestCard, usize, usize)>> {
    let lines = read_lines(cfg, group_id, date)?;
    if lines.len() < MIN_LINES_FOR_SUMMARY {
        return Ok(None);
    }

    // 1) 本地统计：发言排行、参与人数、最活跃时段
    let mut counts: Vec<(String, u32)> = Vec::new();
    for l in &lines {
        match counts.iter_mut().find(|(k, _)| *k == l.member) {
            Some((_, c)) => *c += 1,
            None => counts.push((l.member.clone(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    let label = |member: &str| -> String {
        // 用这个人最后一次的群名片 / 昵称
        lines
            .iter()
            .rev()
            .find(|l| l.member == member && !l.nick.is_empty())
            .map(|l| display_name(&l.nick, member))
            .unwrap_or_else(|| short_label(member))
    };
    let ranking: Vec<(String, u32)> = counts.iter().take(10).map(|(m, c)| (label(m), *c)).collect();
    let peak_hour = peak_hour(&lines);

    // 2) AI 提炼：话题 / 热词 / 金句 / 总结
    let transcript = sample_transcript(&lines, 14000);
    let system = "\
你是群聊日报编辑。根据给定的 QQ 群聊天记录（每行「[昵称] 内容」，可能经过抽样），输出：
1. topics：当天聊到的主要话题，3-6 个，每个不超过 14 字，按热度排序；
2. keywords：当天的话题热词 / 梗 / 高频说法，6-12 个，每个 2-6 字，不要「哈哈」「什么」这类虚词；
3. quotes：2-4 句群友金句——有趣、有梗或有道理的**原话**，必须从记录里一字不差地摘出来（可以只摘一句里最精彩的部分，
   但不能改字），附上说话人的昵称；不要选图片描述、表情、广告、涉及色情或人身攻击的话；
4. summary：80-150 字的中文总结，概括讨论内容和群里氛围，语气轻松。
必须只输出一个 JSON 对象：{\"topics\":[...],\"keywords\":[...],\"quotes\":[{\"who\":\"昵称\",\"text\":\"原话\"}],\"summary\":\"...\"}，
不要任何其它文字，不要用 <reply> 标签，不要调用工具。";
    let reply = dsh
        .chat_oneshot(system, &format!("群内共 {} 条消息。记录如下：\n{}", lines.len(), transcript))
        .await?;
    let mut ai = parse_digest_json(&reply);
    // 金句必须真的出现在记录里（防模型编造），说话人以记录为准；再过一遍内容审查
    ai.quotes = verify_quotes(&ai.quotes, &lines);
    ai.topics.retain(|t| crate::moderation::scan(t).0 != crate::moderation::Level::Explicit);
    ai.keywords.retain(|k| crate::moderation::scan(k).0 == crate::moderation::Level::Clean);
    if ai.keywords.is_empty() {
        ai.keywords = local_keywords(&lines, 10);
    }
    if ai.summary.is_empty() {
        ai.summary = reply.lines().filter(|l| !l.trim().is_empty()).take(3).collect::<Vec<_>>().join(" ");
    }

    // 3) 群名、星期、图标
    let group_name = match ob {
        Some(ob) => group_name(ob, group_id).await.unwrap_or_else(|| format!("群 {group_id}")),
        None => format!("群 {group_id}"),
    };
    let weekday = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| weekday_cn(chrono::Datelike::weekday(&d).number_from_monday()))
        .unwrap_or("")
        .to_string();
    let card = DigestCard {
        group_name,
        date: date.to_string(),
        weekday,
        msg_count: lines.len(),
        member_count: counts.len(),
        peak_hour,
        topics: ai.topics,
        keywords: ai.keywords,
        ranking,
        quotes: ai.quotes,
        summary: ai.summary,
        icon: crate::zxreport::load_icon(&cfg.zx_icon),
    };
    Ok(Some((card, lines.len(), counts.len())))
}

fn read_lines(cfg: &Config, group_id: &str, date: &str) -> Result<Vec<Line>> {
    let raw = std::fs::read_to_string(jsonl_path(cfg, group_id, date))?;
    Ok(raw
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let text = s("text");
            if text.trim().is_empty() {
                return None;
            }
            Some(Line { ts: v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0), member: s("member"), nick: s("nick"), text })
        })
        .collect())
}

/// 最活跃的小时（北京时间）及条数
fn peak_hour(lines: &[Line]) -> Option<(u32, usize)> {
    let mut hist = [0usize; 24];
    for l in lines.iter().filter(|l| l.ts > 0) {
        let h = ((l.ts + 8 * 3600).rem_euclid(86400) / 3600) as usize;
        hist[h] += 1;
    }
    let (h, n) = hist.iter().enumerate().max_by_key(|(_, n)| **n)?;
    (*n > 0).then_some((h as u32, *n))
}

/// 拼成「[昵称] 内容」交给模型：每行截到 80 字，总长超了就**均匀抽样**（覆盖全天，而不是只看前半天）
fn sample_transcript(lines: &[Line], max_chars: usize) -> String {
    let rows: Vec<String> = lines
        .iter()
        .map(|l| {
            let who = if l.nick.is_empty() { short_label(&l.member) } else { l.nick.clone() };
            format!("[{}] {}", cap_chars(&who, 16), cap_chars(&l.text.replace('\n', " "), 80))
        })
        .collect();
    let total: usize = rows.iter().map(|r| r.chars().count() + 1).sum();
    if total <= max_chars {
        return rows.join("\n");
    }
    let keep = (rows.len() * max_chars / total).max(1);
    let step = rows.len() as f64 / keep as f64;
    (0..keep)
        .map(|i| rows[((i as f64 * step) as usize).min(rows.len() - 1)].clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析模型给的 JSON（容忍前后多出来的文字 / 代码块 / <reply> 标签）
fn parse_digest_json(reply: &str) -> AiDigest {
    let (Some(s), Some(e)) = (reply.find('{'), reply.rfind('}')) else {
        return AiDigest::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&reply[s..=e]) else {
        return AiDigest::default();
    };
    let strs = |k: &str, max_len: usize, max_n: usize| -> Vec<String> {
        v.get(k)
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && s.chars().count() <= max_len)
                    .take(max_n)
                    .collect()
            })
            .unwrap_or_default()
    };
    let quotes = v
        .get("quotes")
        .and_then(|q| q.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|q| {
                    let who = q.get("who").and_then(|x| x.as_str())?.trim().to_string();
                    let text = q.get("text").and_then(|x| x.as_str())?.trim().to_string();
                    (!text.is_empty()).then_some((who, text))
                })
                .take(4)
                .collect()
        })
        .unwrap_or_default();
    AiDigest {
        topics: strs("topics", 24, 6),
        keywords: strs("keywords", 10, 12),
        quotes,
        summary: v.get("summary").and_then(|x| x.as_str()).unwrap_or("").trim().to_string(),
    }
}

/// 金句校验：原话（去掉引号 / 首尾空白后至少 4 个字）必须是某条记录的子串；说话人换成那条记录的昵称；
/// 去重、过内容审查（必须完全干净）、去掉图片描述
fn verify_quotes(quotes: &[(String, String)], lines: &[Line]) -> Vec<(String, String)> {
    let clean = |s: &str| s.trim().trim_matches(|c| matches!(c, '「' | '」' | '"' | '“' | '”' | '\'')).trim().to_string();
    let mut out: Vec<(String, String)> = Vec::new();
    for (_, text) in quotes {
        let t = clean(text);
        if t.chars().count() < 4 || t.chars().count() > 120 {
            continue;
        }
        if crate::moderation::scan(&t).0 != crate::moderation::Level::Clean {
            continue;
        }
        let Some(src) = lines
            .iter()
            .find(|l| l.text.contains(&t) && !l.text.starts_with("[图片]") && !l.text.starts_with("[表情包]"))
        else {
            continue;
        };
        if out.iter().any(|(_, x)| x == &t) {
            continue;
        }
        let who = display_name(&src.nick, &src.member);
        out.push((who, t));
    }
    out
}

/// 本地兜底热词：2~4 字中文片段的频次，要求至少 3 个不同的人说过，去掉虚词和被更长片段覆盖的子串
fn local_keywords(lines: &[Line], n: usize) -> Vec<String> {
    const STOP: &[&str] = &[
        "什么", "这个", "那个", "我们", "你们", "他们", "没有", "可以", "就是", "一个", "不是", "真的", "怎么", "然后",
        "还是", "自己", "知道", "现在", "感觉", "觉得", "这样", "哈哈", "哈哈哈", "图片", "表情", "表情包", "这是",
        "画面", "一张", "已经", "因为", "所以", "但是", "如果", "时候", "东西", "今天", "是不是", "有没有", "不会",
        "不要", "还有", "应该", "一下", "这么", "那么", "为什么", "出来", "起来", "看看", "一样",
    ];
    let mut freq: std::collections::HashMap<String, (usize, std::collections::HashSet<String>)> = Default::default();
    for l in lines.iter().filter(|l| !l.text.starts_with('[')) {
        let cjk: Vec<char> = l.text.chars().collect();
        let mut seen = std::collections::HashSet::new();
        for len in 2..=4 {
            for w in cjk.windows(len) {
                if !w.iter().all(|c| ('\u{4e00}'..='\u{9fff}').contains(c)) {
                    continue;
                }
                let s: String = w.iter().collect();
                if STOP.contains(&s.as_str()) || !seen.insert(s.clone()) {
                    continue;
                }
                let e = freq.entry(s).or_default();
                e.0 += 1;
                e.1.insert(l.member.clone());
            }
        }
    }
    let mut cands: Vec<(String, usize)> = freq
        .into_iter()
        .filter(|(_, (c, who))| *c >= 3 && who.len() >= 3)
        .map(|(k, (c, _))| (k, c))
        .collect();
    cands.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.chars().count().cmp(&a.0.chars().count())));
    let mut out: Vec<(String, usize)> = Vec::new();
    for (k, c) in cands {
        // 已选的更长片段包含它、且次数相近 → 跳过（「星痕共鸣」选了就不要「星痕」「共鸣」）
        if out.iter().any(|(o, oc)| o.contains(&k) && *oc * 10 >= c * 7) {
            continue;
        }
        if crate::moderation::scan(&k).0 != crate::moderation::Level::Clean {
            continue;
        }
        out.push((k, c));
        if out.len() >= n {
            break;
        }
    }
    out.into_iter().map(|(k, _)| k).collect()
}

/// 卡片上显示的名字：群名片 / 昵称；昵称过不了内容审查（擦边、色情）就换成「群友」+ QQ 号后 4 位
fn display_name(nick: &str, member: &str) -> String {
    let nick = nick.trim();
    if nick.is_empty() {
        return short_label(member);
    }
    if crate::moderation::scan(nick).0 != crate::moderation::Level::Clean {
        let tail: String = member.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        return format!("群友 {tail}");
    }
    nick.to_string()
}

/// 群名（OneBot get_group_info）
async fn group_name(ob: &OneBot, group_id: &str) -> Option<String> {
    let gid: i64 = group_id.parse().ok()?;
    let v = ob.call("get_group_info", serde_json::json!({ "group_id": gid })).await.ok()?;
    v.get("group_name").and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
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

/// 下一天 00:01（本地时区）
fn next_midnight_plus(now: chrono::DateTime<chrono::FixedOffset>) -> chrono::DateTime<chrono::FixedOffset> {
    let today = now.date_naive();
    let tomorrow = today + chrono::Duration::days(1);
    match tomorrow.and_hms_opt(0, 1, 0) {
        Some(naive) => naive
            .and_local_timezone(*now.offset())
            .single()
            .unwrap_or(now + chrono::Duration::days(1)),
        None => now + chrono::Duration::days(1),
    }
}

fn jsonl_path(cfg: &Config, group_id: &str, date: &str) -> PathBuf {
    PathBuf::from(&cfg.log_dir)
        .join("digest")
        .join(sanitize(group_id))
        .join(format!("{date}.jsonl"))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 长 id（旧版 openid）显示末 6 位；QQ 号原样显示
pub fn short_label(id: &str) -> String {
    if id.len() > 12 {
        format!("…{}", &id[id.len() - 6..])
    } else {
        id.to_string()
    }
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 日报json解析() {
        let r = parse_digest_json("好的：{\"topics\":[\"游戏\",\"天气\"],\"keywords\":[\"摸鱼\"],\"quotes\":[{\"who\":\"A\",\"text\":\"人生苦短\"}],\"summary\":\"大家在聊天\"} 完");
        assert_eq!(r.topics, vec!["游戏".to_string(), "天气".to_string()]);
        assert_eq!(r.keywords, vec!["摸鱼".to_string()]);
        assert_eq!(r.quotes, vec![("A".to_string(), "人生苦短".to_string())]);
        assert_eq!(r.summary, "大家在聊天");
        assert_eq!(parse_digest_json("没有 json"), AiDigest::default());
    }

    fn line(m: &str, nick: &str, text: &str, ts: i64) -> Line {
        Line { ts, member: m.into(), nick: nick.into(), text: text.into() }
    }

    #[test]
    fn 金句必须是原话() {
        let lines = vec![
            line("1", "阿猫", "人生就像打副本，掉落全看脸", 0),
            line("2", "阿狗", "[图片] 一张截图，配文「我裂开了」", 0),
            line("3", "", "今天也是摸鱼的一天", 0),
        ];
        let q = vec![
            ("错名字".into(), "「人生就像打副本，掉落全看脸」".into()),
            ("阿狗".into(), "我裂开了".into()),
            ("编的".into(), "这句话没人说过".into()),
            ("x".into(), "摸鱼".into()),
            ("阿猫".into(), "人生就像打副本，掉落全看脸".into()),
        ];
        let v = verify_quotes(&q, &lines);
        assert_eq!(v, vec![("阿猫".to_string(), "人生就像打副本，掉落全看脸".to_string())], "说话人以记录为准、图片描述 / 编造 / 太短 / 重复都丢掉");
    }

    #[test]
    fn 最活跃时段按北京时间() {
        // 2026-10-23 13:05 UTC = 21:05 北京时间
        let base = 1792674300;
        let lines: Vec<Line> = (0..5).map(|i| line("1", "", "x", base + i * 60)).chain([line("2", "", "y", base - 6 * 3600)]).collect();
        assert_eq!(peak_hour(&lines).map(|p| p.0), Some(((base + 8 * 3600).rem_euclid(86400) / 3600) as u32));
    }

    #[test]
    fn 抽样覆盖全天() {
        let lines: Vec<Line> = (0..1000).map(|i| line(&format!("{}", i % 7), "", &format!("第{i}条消息内容"), i)).collect();
        let t = sample_transcript(&lines, 3000);
        assert!(t.chars().count() <= 3300);
        assert!(t.contains("第0条") && t.contains("第99"), "开头和后段都要有");
    }

    /// 用真实记录渲染一张日报预览（调 DSH，不发群）。
    /// 跑法：DIGEST_PREVIEW=<群号>:<日期> cargo test -- --ignored 渲染日报样例 --nocapture
    #[tokio::test]
    #[ignore]
    async fn 渲染日报样例() {
        dotenvy::from_filename(".env").ok();
        let spec = std::env::var("DIGEST_PREVIEW").expect("DIGEST_PREVIEW=<群号>:<日期>");
        let (gid, date) = spec.split_once(':').expect("格式 群号:日期");
        let cfg = Config::from_env();
        let dsh = DshBridge::from_config(&cfg).expect("构造 DSH 桥");
        let t = std::time::Instant::now();
        let (card, n, m) = build_card(&dsh, &cfg, None, gid, date).await.expect("生成失败").expect("消息太少");
        println!("{n} 条消息，{m} 人；AI 用时 {:.1}s", t.elapsed().as_secs_f32());
        println!("话题：{:?}\n热词：{:?}\n金句：{:?}\n总结：{}", card.topics, card.keywords, card.quotes, card.summary);
        let img = render_card(&card);
        println!("尺寸 {}x{}", img.width(), img.height());
        let bytes = encode_png(img).unwrap();
        let out = PathBuf::from(&cfg.log_dir).join("digest").join("render").join(format!("preview_{gid}.png"));
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, &bytes).unwrap();
        println!("预览：{}（{} KB）", out.display(), bytes.len() / 1024);
    }

    #[test]
    fn 不当昵称打码() {
        assert_eq!(display_name("今天在床上表现不佳", "100001918"), "群友 1918");
        assert_eq!(display_name("阿猫（摸鱼中......）", "100002"), "阿猫（摸鱼中......）");
        assert_eq!(display_name("", "123"), "123");
    }

    #[test]
    fn 本地热词() {
        let mut lines = Vec::new();
        for (m, t) in [("1", "星痕共鸣今天开新副本了"), ("2", "星痕共鸣新副本好难"), ("3", "星痕共鸣副本掉落太少"), ("4", "哈哈哈什么啊")] {
            lines.push(line(m, "", t, 0));
        }
        let k = local_keywords(&lines, 5);
        assert!(k.iter().any(|w| w == "星痕共鸣"), "{k:?}");
        assert!(!k.iter().any(|w| w == "星痕" || w == "哈哈"), "{k:?}");
    }

    #[test]
    fn 短标签() {
        assert_eq!(short_label("123456789"), "123456789");
        assert_eq!(short_label("3411572DD065226596CE2E0712EB81D9"), "…EB81D9");
    }
}
