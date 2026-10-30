//! B站视频信息解析：从消息文本中识别 B站链接（BV/av/b23.tv），
//! 调官方接口拿标题/UP主/播放量/封面 URL，供“自动回复封面图”使用。

use anyhow::{anyhow, Result};
use reqwest::redirect::Policy;
use std::time::Duration;

pub struct VideoInfo {
    pub title: String,
    pub desc: String,
    pub cover_url: String,
    pub up_name: String,
    pub view: u64,
    pub bvid: String,
}

impl VideoInfo {
    /// 富文本展示文本（含简介）
    pub fn summary(&self) -> String {
        let desc = self
            .desc
            .replace('\n', " ")
            .replace('\r', " ");
        let desc = cap_chars(&desc, 120);
        if desc.is_empty() {
            format!(
                "📺 {}\n👤 {} · ▶️ {}\n🔗 {}",
                self.title, self.up_name, fmt_view(self.view), self.bvid
            )
        } else {
            format!(
                "📺 {}\n📝 {}\n👤 {} · ▶️ {}\n🔗 {}",
                self.title, desc, self.up_name, fmt_view(self.view), self.bvid
            )
        }
    }
}

fn fmt_view(n: u64) -> String {
    if n >= 100_000_000 {
        format!("{:.1}亿", n as f64 / 100_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}万", n as f64 / 10_000.0)
    } else {
        n.to_string()
    }
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0 Safari/537.36",
        )
        .build()?)
}

/// 从文本里提取第一个 B站视频 URL（规范化为主站 video URL）；没有则 None
pub fn extract_bili_url(text: &str) -> Option<String> {
    // 1) 完整链接：bilibili.com / b23.tv / acg.tv
    for raw in text.split_whitespace() {
        let tok = raw
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && !"/:.-_?=&%#".contains(c));
        if tok.contains("b23.tv") || tok.contains("acg.tv") || tok.contains("bilibili.com") {
            if let Some(bv) = id_after(tok, "BV") {
                return Some(format!("https://www.bilibili.com/video/{bv}"));
            }
            for p in ["av", "AV"] {
                if let Some(av) = num_after(tok, p) {
                    return Some(format!("https://www.bilibili.com/video/av{av}"));
                }
            }
            return Some(tok.to_string());
        }
    }
    // 2) 独立成词的 BV / av 号（允许被中文、标点包裹）
    for raw in text.split_whitespace() {
        let tok = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if let Some(bv) = id_after(tok, "BV") {
            if bv.len() == 12 {
                return Some(format!("https://www.bilibili.com/video/{bv}"));
            }
        }
        for p in ["av", "AV"] {
            if let Some(av) = num_after(tok, p) {
                if av.len() <= 12 {
                    return Some(format!("https://www.bilibili.com/video/av{av}"));
                }
            }
        }
    }
    None
}

/// 取 `key` 之后紧跟的 BV 号（BV + 10 位字母数字），要求左侧是边界
fn id_after(s: &str, key: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut from = 0usize;
    while let Some(p) = s[from..].find(key) {
        let abs = from + p;
        let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let seg: String = s[abs..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if left_ok && seg.len() >= 12 && seg.starts_with(key) {
            return Some(seg.chars().take(12).collect());
        }
        from = abs + key.len();
    }
    None
}

/// 取 `key` 之后紧跟的数字串（av 号），要求左侧是边界
fn num_after(s: &str, key: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut from = 0usize;
    while let Some(p) = s[from..].find(key) {
        let abs = from + p;
        let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let num: String = s[abs + key.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if left_ok && !num.is_empty() {
            return Some(num);
        }
        from = abs + key.len();
    }
    None
}

/// 消息内容“基本只有链接/分享装饰”时为 true（用于判断要不要跳过 DSH 回答）
pub fn is_pure_share(text: &str) -> bool {
    let Some(url) = extract_bili_url(text) else {
        return false;
    };
    let rest = text.replace(&url, "");
    // 去掉分享装饰：方括号【】[] 内容、复制提示、纯标点空白
    let mut out = String::with_capacity(rest.len());
    let mut in_bracket = 0u32;
    for ch in rest.chars() {
        match ch {
            '【' | '[' | '{' | '(' => in_bracket += 1,
            '】' | ']' | '}' | ')' => in_bracket = in_bracket.saturating_sub(1),
            _ if in_bracket == 0 => {
                if ch.is_alphanumeric() {
                    out.push(ch);
                }
            }
            _ => {}
        }
    }
    let cleaned = out
        .replace("复制打开", "")
        .replace("bilibili", "")
        .replace("B站", "");
    cleaned.trim().is_empty()
}

/// 解析（自动处理 b23.tv 短链跳转）并返回视频信息
pub async fn fetch_video(url: &str) -> Result<VideoInfo> {
    let client = http()?;
    let canonical = if url.contains("b23.tv") {
        resolve_short(&client, url).await?
    } else {
        url.to_string()
    };

    let api_url = if let Some(pos) = canonical.find("BV") {
        let bv: String = canonical[pos..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .take(12)
            .collect();
        format!("https://api.bilibili.com/x/web-interface/view?bvid={bv}")
    } else if let Some(pos) = canonical.find("av") {
        let num: String = canonical[pos + 2..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        format!("https://api.bilibili.com/x/web-interface/view?aid={num}")
    } else {
        return Err(anyhow!("无法从链接中解析 B站视频 id: {url}"));
    };

    let resp = client.get(&api_url).send().await?;
    let body = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&body)?;
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        return Err(anyhow!("B站接口返回错误: {}", v.get("message").map(|m| m.to_string()).unwrap_or_default()));
    }
    let d = v
        .get("data")
        .ok_or_else(|| anyhow!("B站接口无 data"))?;
    let s = d.get("stat").unwrap_or(&serde_json::Value::Null);
    Ok(VideoInfo {
        title: d.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        desc: d.get("desc").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        cover_url: d.get("pic").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        up_name: d
            .pointer("/owner/name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        view: s.get("view").and_then(|x| x.as_u64()).unwrap_or(0),
        bvid: d.get("bvid").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

/// b23.tv 短链 → 真实视频页 URL（不跟随，手动读 Location，最多 4 跳）
async fn resolve_short(client: &reqwest::Client, url: &str) -> Result<String> {
    let no_redirect = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("Mozilla/5.0")
        .redirect(Policy::none())
        .build()?;
    let mut cur = url.to_string();
    for _ in 0..4 {
        let resp = no_redirect.get(&cur).send().await?;
        if let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) {
            let loc = if loc.starts_with("http") {
                loc.to_string()
            } else {
                format!("https://www.bilibili.com{loc}")
            };
            cur = loc;
            if cur.contains("video/") {
                return Ok(cur);
            }
            continue;
        }
        break;
    }
    let _ = client;
    if cur.contains("video/") || cur.contains("BV") {
        Ok(cur)
    } else {
        Err(anyhow!("b23.tv 短链解析失败: {cur}"))
    }
}

/// QQ 分享卡片（小程序 / 链接卡片）解析结果。
///
/// QQ 不把卡片 JSON 原样交给机器人，而是渲染成一行文本，形如：
/// `[卡片消息] 小程序 摘要: … title: … preview: <封面> source: 哔哩哔哩 source_logo: …`
/// B站小程序卡片里没有可直接解析的视频链接，只能靠来源 + 封面判断后回标题。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShareCard {
    pub kind: String,
    pub title: String,
    pub preview: String,
    pub source: String,
    pub summary: String,
}

impl ShareCard {
    /// 是否是 B站卡片
    pub fn is_bilibili(&self) -> bool {
        let hay = format!("{} {} {}", self.source, self.title, self.summary).to_lowercase();
        hay.contains("哔哩哔哩")
            || hay.contains("bilibili")
            || hay.contains("b站")
            || self.preview.contains("hdslb.com")
            || self.preview.contains("bilivideo.com")
    }

    /// 回复正文（纯文本，随封面一起发）
    pub fn reply_text(&self) -> String {
        let t = self.title.trim();
        if t.is_empty() {
            "📺 收到一张 B站小程序卡片，但没解析出标题。".to_string()
        } else {
            format!("📺 {t}\n🔗 来自 B站小程序卡片")
        }
    }
}

/// 解析 QQ 分享卡片文本；不是卡片返回 None
pub fn parse_share_card(text: &str) -> Option<ShareCard> {
    let t = text.trim();
    let rest = t.strip_prefix("[卡片消息]")?.trim_start();
    let (kind, body) = match rest.split_once(char::is_whitespace) {
        Some((k, r)) => (k.trim().to_string(), r),
        None => (String::new(), rest),
    };
    let title = card_field(body, "title: ", &[" preview: ", " source: ", " source_logo: "]);
    let preview = card_field(body, "preview: ", &[" source: ", " source_logo: "]);
    let source = card_field(body, "source: ", &[" source_logo: "]);
    let summary = card_field(body, "摘要: ", &[" title: ", " preview: ", " source: "]);
    if title.is_empty() && preview.is_empty() {
        return None;
    }
    Some(ShareCard {
        kind,
        title,
        preview,
        source,
        summary,
    })
}

/// 取卡片文本里 `key` 之后、下一个标记之前的内容
fn card_field(raw: &str, key: &str, next: &[&str]) -> String {
    let Some(p) = raw.find(key) else {
        return String::new();
    };
    let after = &raw[p + key.len()..];
    let mut end = after.len();
    for n in next {
        if let Some(q) = after.find(n) {
            end = end.min(q);
        }
    }
    after[..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "[卡片消息] 小程序 摘要: [QQ小程序]【时隔12年重启】电击文库《漆黑的子弹 新世界秩序》宣传PV公开！梶裕贵×日高里菜再度献声 title: 【时隔12年重启】电击文库《漆黑的子弹 新世界秩序》宣传PV公开！梶裕贵×日高里菜再度献声 preview: https://qq.ugcimg.cn/v1/sel87lus18rmfngp0td7vnkvrjg5t2ako044ltetf8u8841r2ruakle716ef0grc5q0ukb4jduoerd269a8uv7dk50g5alsmkas4anci361d9k9uverndi2shaho4p1a97vrpbrhr0ae9at50svuav133s/e9vf2tsqr6j29hl2kodb3vahlc source: 哔哩哔哩 source_logo: http://miniapp.gtimg.cn/public/appicon/1109937557.jpg";

    /// 卡片封面地址里的 `...av133...` 不能被当成 av 号（历史 bug：解析 av133 失败）
    #[test]
    fn card_preview_av133_is_not_a_video_id() {
        assert_eq!(extract_bili_url(CARD), None);
    }

    #[test]
    fn parses_share_card_fields() {
        let card = parse_share_card(CARD).expect("应当解析出卡片");
        assert_eq!(card.kind, "小程序");
        assert_eq!(
            card.title,
            "【时隔12年重启】电击文库《漆黑的子弹 新世界秩序》宣传PV公开！梶裕贵×日高里菜再度献声"
        );
        assert!(card.preview.starts_with("https://qq.ugcimg.cn/"));
        assert_eq!(card.source, "哔哩哔哩");
        assert!(card.is_bilibili());
        assert!(card.reply_text().contains("B站小程序卡片"));
    }

    #[test]
    fn still_finds_links_and_bare_ids() {
        assert_eq!(
            extract_bili_url("看看 https://www.bilibili.com/video/BV1xx411c7mD?p=1 这个").unwrap(),
            "https://www.bilibili.com/video/BV1xx411c7mD"
        );
        assert_eq!(
            extract_bili_url("BV1xx411c7mD 这个").unwrap(),
            "https://www.bilibili.com/video/BV1xx411c7mD"
        );
        assert_eq!(
            extract_bili_url("av12345").unwrap(),
            "https://www.bilibili.com/video/av12345"
        );
        assert_eq!(
            extract_bili_url("https://b23.tv/abcDEF").unwrap(),
            "https://b23.tv/abcDEF"
        );
        assert_eq!(extract_bili_url("[卡片消息] 小程序 摘要: 你好"), None);
    }
}
