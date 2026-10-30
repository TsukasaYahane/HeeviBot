//! 视觉模块：识别群里的图片 / 表情包，交给多模态模型「看懂」，产出中文描述。
//!
//! 通道说明：
//! - 视觉识别**直连** OpenAI 兼容的 OneHop `/v1/chat/completions`，图片以 `data:` URL
//!   内联，不依赖上游拉外网（早期走 Hermes `/v1/responses` 带图时图片到不了上游，
//!   DSH headless 也只收文本，所以这条独立通道保留）；
//! - 聊天 / 记忆 / 学习走 DSH，二者互不影响；
//! - 为什么不用 `google/gemini-3.1-flash-image`：该模型是**图像生成**模型，
//!   不能用 openai_chat 协议调用（OneHop 直接返回 400），识图请用多模态对话模型。

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tracing::{info, warn};

use crate::config::Config;

/// 单条消息最多识别的图片数（防刷屏、防超时）
const MAX_IMAGES_PER_MSG: usize = 2;
/// 描述文本最大字符数（喂给聊天 / 学习层前截断）
const MAX_DESC_CHARS: usize = 220;
/// 缓存条目上限（进程内，按内容哈希去重）
const CACHE_CAP: usize = 256;

/// 识图提示词：只描述，不寒暄、不调工具
const VISION_PROMPT: &str = "看这张图，用一到两句中文说明：这是什么（照片/截图/表情包/梗图）、\
画面里有什么、图上有什么文字、情绪或梗是什么。只输出描述本身，不要寒暄，不要解释你在做什么。";

/// QQ 消息里的一张图片（来自 OneBot 11 的 `image` / `mface` 消息段）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub url: String,
    pub content_type: String,
    pub filename: String,
    pub size: u64,
    /// QQ 标记为表情包（image 段 `sub_type=1`、商城表情 `mface`、或摘要是「动画表情」）
    pub sticker: bool,
}

impl ImageRef {
    /// 从一个 OneBot 11 消息段提取图片；不是图片 / 没有可下载的 URL 时返回 None。
    ///
    /// LLBot 的图片段形如
    /// `{"type":"image","data":{"file":"xxx.jpg","url":"https://multimedia.nt.qq.com.cn/...","sub_type":0,"summary":"","file_size":"20480"}}`，
    /// `url` 是带 rkey 的临时直链，直接 GET 即可。
    pub fn from_segment(seg: &Value) -> Option<ImageRef> {
        let kind = seg.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "image" && kind != "mface" {
            return None;
        }
        let data = seg.get("data")?;
        let s = |k: &str| -> String {
            match data.get(k) {
                Some(Value::String(v)) => v.trim().to_string(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            }
        };
        let file = s("file");
        let mut url = s("url");
        if url.is_empty() && file.starts_with("http") {
            url = file.clone();
        }
        if !url.starts_with("http") {
            return None;
        }
        let summary = s("summary");
        let sticker = kind == "mface" || s("sub_type") == "1" || summary.contains("动画表情");
        let probe = format!("{file} {url}").to_ascii_lowercase();
        let content_type = if probe.contains(".gif") {
            "image/gif"
        } else if probe.contains(".png") {
            "image/png"
        } else if probe.contains(".webp") {
            "image/webp"
        } else {
            "image/jpeg"
        };
        Some(ImageRef {
            url,
            content_type: content_type.to_string(),
            filename: file,
            size: s("file_size").parse().unwrap_or(0),
            sticker,
        })
    }

    /// 从整条消息（段数组）提取图片：按 URL 去重，最多 `MAX_IMAGES_PER_MSG` 张
    pub fn from_segments(segs: &[Value]) -> Vec<ImageRef> {
        let mut out: Vec<ImageRef> = Vec::new();
        for seg in segs {
            let Some(img) = Self::from_segment(seg) else {
                continue;
            };
            if out.iter().any(|x| x.url == img.url) {
                continue;
            }
            out.push(img);
            if out.len() >= MAX_IMAGES_PER_MSG {
                break;
            }
        }
        out
    }

    /// 中文标签：QQ 标记的表情包、动图或文件名带表情语义的算「表情包」，其余算「图片」
    pub fn label(&self) -> &'static str {
        let ct = self.content_type.to_ascii_lowercase();
        let name = self.filename.to_ascii_lowercase();
        if self.sticker
            || ct.contains("gif")
            || name.contains("emoji")
            || name.contains("sticker")
            || name.contains("meme")
            || name.contains("表情")
        {
            "表情包"
        } else {
            "图片"
        }
    }
}

/// 视觉识别器（可选组件：未启用 / 无 key 时为 None，调用方自动降级）
pub struct Vision {
    http: reqwest::Client,
    base: String,
    api_key: String,
    /// 模型链：`models[0]` 是主模型，其余为备用（同 base/key）
    models: Vec<String>,
    /// 当前“粘住”的模型下标
    idx: AtomicUsize,
    /// 上次成功的时间戳（秒）
    last_ok: AtomicI64,
    /// 每隔多少秒回头试一次主模型
    retry_secs: i64,
    max_bytes: usize,
    cache: Mutex<HashMap<u64, String>>,
}

impl Vision {
    /// 从配置构造；未启用或缺少 key 时返回 None（不报错，仅降级）
    pub fn from_config(cfg: &Config) -> Option<Self> {
        if !cfg.vision_enabled {
            return None;
        }
        let api_key = cfg.vision_api_key.clone()?;
        if api_key.trim().is_empty() {
            return None;
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.vision_timeout_secs.max(5)))
            .build()
            .ok()?;
        // 模型链：主模型 + 备用（去重，保持顺序）
        let mut models = vec![cfg.vision_model.clone()];
        for m in &cfg.vision_fallbacks {
            let m = m.trim();
            if !m.is_empty() && !models.iter().any(|x| x == m) {
                models.push(m.to_string());
            }
        }
        Some(Self {
            http,
            base: cfg.vision_base.trim_end_matches('/').to_string(),
            api_key,
            models,
            idx: AtomicUsize::new(0),
            last_ok: AtomicI64::new(0),
            retry_secs: cfg.vision_primary_retry_secs.max(30) as i64,
            max_bytes: cfg.vision_max_bytes.max(64 * 1024),
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// 完整模型链（主 + 备用），启动日志用
    pub fn model_chain(&self) -> &[String] {
        &self.models
    }

    /// 本次从第几个模型开始试：距上次成功超过 retry_secs → 回主模型探一次
    fn start_index(&self) -> usize {
        if now_ts() - self.last_ok.load(Ordering::Relaxed) >= self.retry_secs {
            0
        } else {
            self.idx.load(Ordering::Relaxed).min(self.models.len() - 1)
        }
    }

    fn note_ok(&self, i: usize) {
        self.idx.store(i, Ordering::Relaxed);
        self.last_ok.store(now_ts(), Ordering::Relaxed);
    }

    /// 识别一批图片，返回可直接拼进消息文本的片段，例如：
    /// `[表情包] 一只橘猫趴在键盘上，配文「我裂开了」（崩溃/搞笑）`
    /// 全部失败时返回 None。
    pub async fn describe(&self, imgs: &[ImageRef]) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        for img in imgs.iter().take(MAX_IMAGES_PER_MSG) {
            match self.describe_one(img).await {
                Ok(d) if !d.is_empty() => {
                    info!("👁 视觉识别（{}）：{}", img.label(), d);
                    parts.push(format!("[{}] {}", img.label(), d));
                }
                Ok(_) => {}
                Err(e) => warn!("视觉识别失败（{}）：{e:#}", short_url(&img.url)),
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }

    async fn describe_one(&self, img: &ImageRef) -> Result<String> {
        let bytes = self.download(&img.url).await?;
        let hash = hash_bytes(&bytes);
        if let Some(hit) = self.cache_get(hash) {
            info!("👁 视觉识别命中缓存（{} 字节）", bytes.len());
            return Ok(hit);
        }
        // 按文件头判断真实格式（QQ 常把 JPG 叫 .png；Anthropic 对 media_type 和内容不符会直接 400）
        let mime = sniff_mime(&bytes).map(str::to_string).unwrap_or_else(|| normalize_mime(&img.content_type));
        let b64 = base64_encode(&bytes);

        // 模型链逐个尝试：主模型不可用（上游 5xx / 报错）自动切备用
        let start = self.start_index();
        let mut last_err: Option<anyhow::Error> = None;
        for i in start..self.models.len() {
            match self.describe_with(&mime, &b64, i).await {
                Ok(desc) => {
                    if i != start {
                        info!(
                            "🔁 视觉主模型 {} 不可用，已切到 {}（本次起沿用）",
                            self.models[start], self.models[i]
                        );
                    }
                    self.note_ok(i);
                    self.cache_put(hash, desc.clone());
                    return Ok(desc);
                }
                Err(e) => {
                    if i + 1 < self.models.len() {
                        warn!(
                            "视觉模型 {} 失败，自动改用备用 {}：{e:#}",
                            self.models[i],
                            self.models[i + 1]
                        );
                    } else {
                        warn!("视觉模型 {} 失败（已无备用模型）：{e:#}", self.models[i]);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("视觉模型链为空")))
    }

    /// 用链上第 `idx` 个模型识别一张图（describe_one 的重试循环逐个调用它）
    /// 用链上第 `idx` 个模型识别一张图：`anthropic/*` 走 Anthropic Messages，其余走 OpenAI chat/completions
    async fn describe_with(&self, mime: &str, b64: &str, idx: usize) -> Result<String> {
        let model = &self.models[idx];
        let content = if is_anthropic_model(model) {
            self.describe_anthropic(model, mime, b64).await?
        } else {
            self.describe_openai(model, mime, b64).await?
        };
        if content.is_empty() {
            return Err(anyhow!("视觉模型没有返回文本"));
        }
        Ok(sanitize_desc(&content))
    }

    /// OpenAI 兼容：`POST {base}/chat/completions`，图片以 data: URL 内联
    async fn describe_openai(&self, model: &str, mime: &str, b64: &str) -> Result<String> {
        let body = json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": VISION_PROMPT},
                    {"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{b64}")}},
                ],
            }],
            "max_tokens": 300,
            "temperature": 0.3,
        });
        let url = format!("{}/chat/completions", self.base);
        let v = self.post_json(&url, &body, false).await?;
        Ok(v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("").trim().to_string())
    }

    /// Anthropic Messages：`POST {根}/v1/messages`（OneHop 上 Claude 系列只开放这个协议）
    async fn describe_anthropic(&self, model: &str, mime: &str, b64: &str) -> Result<String> {
        let body = json!({
            "model": model,
            "max_tokens": 400,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": mime, "data": b64}},
                    {"type": "text", "text": VISION_PROMPT},
                ],
            }],
        });
        let url = format!("{}/v1/messages", anthropic_root(&self.base));
        let v = self.post_json(&url, &body, true).await?;
        let text: Vec<&str> = v
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        Ok(text.join(" ").trim().to_string())
    }

    /// 发请求 + 统一处理 HTTP / JSON / 错误对象（两种协议的错误都是 `{"error":{"message":…}}`）
    async fn post_json(&self, url: &str, body: &Value, anthropic: bool) -> Result<Value> {
        let mut req = self.http.post(url).json(body);
        req = if anthropic {
            req.header("x-api-key", &self.api_key).header("anthropic-version", "2023-06-01")
        } else {
            req.bearer_auth(&self.api_key)
        };
        let resp = req.send().await.map_err(|e| anyhow!("请求视觉模型失败（{url}）：{e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("视觉模型 HTTP {status}：{}", clip(&text, 200)));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| anyhow!("解析视觉模型响应失败：{e}"))?;
        if let Some(msg) = v.pointer("/error/message").and_then(Value::as_str) {
            return Err(anyhow!("视觉模型返回错误：{msg}"));
        }
        Ok(v)
    }

    /// 下载图片（QQ 附件 URL 自带 rkey，无需额外鉴权；限制体积防爆内存）
    async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let mut resp = self
            .http
            .get(url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .header("Accept", "image/*,*/*;q=0.8")
            .send()
            .await
            .map_err(|e| anyhow!("下载图片失败：{e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("下载图片 HTTP {status}"));
        }
        if let Some(len) = resp.content_length() {
            if len as usize > self.max_bytes {
                return Err(anyhow!("图片 {} 字节，超过上限 {}", len, self.max_bytes));
            }
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| anyhow!("读取图片失败：{e}"))? {
            buf.extend_from_slice(&chunk);
            if buf.len() > self.max_bytes {
                return Err(anyhow!("图片超过上限 {} 字节", self.max_bytes));
            }
        }
        if buf.is_empty() {
            return Err(anyhow!("图片内容为空"));
        }
        Ok(buf)
    }

    fn cache_get(&self, h: u64) -> Option<String> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&h)
            .cloned()
    }

    fn cache_put(&self, h: u64, desc: String) {
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if c.len() >= CACHE_CAP {
            c.clear();
        }
        c.insert(h, desc);
    }
}

/// QQ 经典表情（faceId → 名称）。未收录的返回 None，调用方显示为「表情」。
/// 只覆盖常见的 0~90，够群聊用；新版大表情 / 表情回应拿不到名字就不猜。
pub fn face_name(id: &str) -> Option<&'static str> {
    const NAMES: [&str; 91] = [
        "微笑", "撇嘴", "色", "发呆", "得意", "流泪", "害羞", "闭嘴", "睡", "大哭", // 0-9
        "尴尬", "发怒", "调皮", "呲牙", "惊讶", "难过", "酷", "冷汗", "抓狂", "吐", // 10-19
        "偷笑", "可爱", "白眼", "傲慢", "饥饿", "困", "惊恐", "流汗", "憨笑", "大兵", // 20-29
        "奋斗", "咒骂", "疑问", "嘘", "晕", "折磨", "抠鼻", "鼓掌", "糗大了", "坏笑", // 30-39
        "左哼哼", "右哼哼", "哈欠", "鄙视", "委屈", "快哭了", "阴险", "亲亲", "吓", "可怜", // 40-49
        "菜刀", "西瓜", "啤酒", "篮球", "乒乓", "咖啡", "饭", "猪头", "玫瑰", "凋谢", // 50-59
        "示爱", "爱心", "心碎", "蛋糕", "闪电", "炸弹", "刀", "足球", "瓢虫", "便便", // 60-69
        "月亮", "太阳", "礼物", "拥抱", "强", "弱", "握手", "胜利", "抱拳", "勾引", // 70-79
        "拳头", "差劲", "爱你", "NO", "OK", "爱情", "飞吻", "跳跳", "发抖", "怄火", // 80-89
        "转圈", // 90
    ];
    id.trim().parse::<usize>().ok().and_then(|i| NAMES.get(i).copied())
}

/// 模型走不走 Anthropic 协议：OneHop 的 Claude 系列（`anthropic/claude-*`）只开放 anthropic_messages
fn is_anthropic_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("anthropic/") || m.starts_with("claude-")
}

/// OpenAI 风格的 base（`https://api.onehop.ai/v1`）→ Anthropic 根地址（`https://api.onehop.ai`）
fn anthropic_root(base: &str) -> &str {
    let b = base.trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b)
}

/// 按文件头识别图片格式；认不出返回 None（交给 content_type 推断）
fn sniff_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn normalize_mime(ct: &str) -> String {
    let ct = ct.trim().to_ascii_lowercase();
    if ct.starts_with("image/") {
        if ct == "image/jpg" {
            "image/jpeg".to_string()
        } else {
            ct
        }
    } else {
        "image/jpeg".to_string()
    }
}

/// 描述清洗：换行折叠为空格、去掉成对引号、压缩空白、截断
fn sanitize_desc(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    out = out.trim_matches(|c: char| c == '"' || c == '“' || c == '”' || c == '\'' || c == '`').trim().to_string();
    if out.chars().count() > MAX_DESC_CHARS {
        out = out.chars().take(MAX_DESC_CHARS).collect::<String>() + "…";
    }
    out
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

fn short_url(u: &str) -> String {
    clip(u, 80)
}

/// 标准 base64（带 padding）。只为一处用途引入，不额外加依赖。
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_image_segments_only() {
        let segs = vec![
            serde_json::json!({"type": "text", "data": {"text": "看这个"}}),
            serde_json::json!({"type": "image", "data": {"file": "photo.jpg", "url": "https://multimedia.nt.qq.com.cn/download?fileid=1", "sub_type": 0, "file_size": "20480"}}),
            serde_json::json!({"type": "record", "data": {"file": "voice.amr", "url": "https://x/voice.amr"}}),
            serde_json::json!({"type": "image", "data": {"file": "abc.jpg", "url": "https://multimedia.nt.qq.com.cn/download?fileid=2", "sub_type": 1, "summary": "[动画表情]"}}),
            serde_json::json!({"type": "image", "data": {"file": "photo.jpg", "url": "https://multimedia.nt.qq.com.cn/download?fileid=1"}}),
        ];
        let imgs = ImageRef::from_segments(&segs);
        assert_eq!(imgs.len(), 2, "只取图片、按 URL 去重");
        assert_eq!(imgs[0].label(), "图片");
        assert_eq!(imgs[1].label(), "表情包", "sub_type=1 视为表情包");
        assert_eq!(imgs[0].size, 20480);
    }

    #[test]
    fn non_image_segments_yield_empty() {
        assert!(ImageRef::from_segments(&[serde_json::json!({"type": "text", "data": {"text": "纯文本"}})]).is_empty());
        assert!(ImageRef::from_segments(&[]).is_empty());
        assert!(ImageRef::from_segments(&[serde_json::json!({"type": "image", "data": {"file": "local.jpg"}})]).is_empty(), "没有可下载 URL 时跳过");
    }

    #[test]
    fn mface_counts_as_sticker() {
        let seg = serde_json::json!({"type": "mface", "data": {"url": "https://gxh.vip.qq.com/club/item/parcel/item/ab/abcd/raw300.gif", "summary": "[开心]"}});
        let img = ImageRef::from_segment(&seg).expect("mface 带 url 应识别为图片");
        assert_eq!(img.label(), "表情包");
        assert_eq!(img.content_type, "image/gif");
    }

    #[test]
    fn caps_images_per_message() {
        let segs: Vec<Value> = (0..5)
            .map(|i| serde_json::json!({"type": "image", "data": {"url": format!("https://x/{i}.png")}}))
            .collect();
        assert_eq!(ImageRef::from_segments(&segs).len(), MAX_IMAGES_PER_MSG);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn desc_is_sanitized_and_truncated() {
        assert_eq!(sanitize_desc("  “一只猫\n趴在键盘上”  "), "一只猫 趴在键盘上");
        let long = "喵".repeat(400);
        let d = sanitize_desc(&long);
        assert_eq!(d.chars().count(), MAX_DESC_CHARS + 1, "截断并加省略号");
    }

    #[test]
    fn face_names_lookup() {
        assert_eq!(face_name("0"), Some("微笑"));
        assert_eq!(face_name("6"), Some("害羞"));
        assert_eq!(face_name("489"), None, "未收录的大表情不猜名字");
        assert_eq!(face_name("abc"), None);
    }

    #[test]
    fn 协议选择与地址() {
        assert!(is_anthropic_model("anthropic/claude-sonnet-5.5"));
        assert!(is_anthropic_model("claude-sonnet-5-5"));
        assert!(!is_anthropic_model("google/gemini-3.1-flash-lite"));
        assert!(!is_anthropic_model("openai/gpt-5.5"));
        assert_eq!(anthropic_root("https://api.onehop.ai/v1"), "https://api.onehop.ai");
        assert_eq!(anthropic_root("https://api.onehop.ai/v1/"), "https://api.onehop.ai");
        assert_eq!(anthropic_root("https://x.example/anthropic"), "https://x.example/anthropic");
    }

    #[test]
    fn 按文件头识别格式() {
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff_mime(b"GIF89a......"), Some("image/gif"));
        assert_eq!(sniff_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"<html>"), None);
    }

    #[test]
    fn mime_normalized() {
        assert_eq!(normalize_mime("image/jpg"), "image/jpeg");
        assert_eq!(normalize_mime("IMAGE/PNG"), "image/png");
        assert_eq!(normalize_mime(""), "image/jpeg");
    }

    /// 真实联网识图（默认忽略，手动跑）：`cargo test vision_live -- --ignored --nocapture`
    /// 走的是生产代码路径：下载 → base64 内联 → OneHop 多模态 → 解析描述。
    #[tokio::test]
    #[ignore = "需要网络 + .env 里的 QQ_BOT_VISION_API_KEY"]
    async fn vision_live_reads_public_image() {
        use crate::config::Config;
        let _ = dotenvy::from_path(".env");
        let cfg = Config::from_env();
        let v = Vision::from_config(&cfg).expect("视觉模块未配置（检查 QQ_BOT_VISION_API_KEY）");
        let img = ImageRef {
            url: "https://www.baidu.com/img/flexible/logo/pc/result.png".into(),
            content_type: "image/png".into(),
            filename: "logo.png".into(),
            size: 0,
            sticker: false,
        };
        let desc = v.describe(&[img]).await.expect("识图失败");
        println!("识图结果：{desc}");
        assert!(desc.starts_with("[图片]"), "应带 [图片] 标签：{desc}");
        assert!(desc.chars().count() > 6, "描述太短：{desc}");
    }
}
