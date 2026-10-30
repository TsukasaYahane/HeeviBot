//! OneBot 11 正向 WebSocket 客户端：对接 LLBot（LuckyLilliaBot）。
//!
//! 一条 WS 连接同时承载两件事：
//! - **事件**：LLBot 主动推送的 `post_type` 帧（消息 / 通知 / 元事件），转进 mpsc 交给 handler；
//! - **API 调用**：`{"action","params","echo"}` 发出去，按 `echo` 匹配回包。
//!
//! 断线后指数退避自动重连（2s → 30s 封顶），重连期间的 API 调用直接返回错误，
//! 不排队重放——QQ 消息过时再发没有意义。
//!
//! LLBot 侧配置：启用「正向 WebSocket」（默认端口 3001），消息格式建议选 `array`
//! （`string`/CQ 码格式也能解析，见 [`parse_cq`]）。

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};

/// 单次 API 调用超时（发图时 LLBot 要先上传到 QQ，给足时间）
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// OneBot 客户端句柄（可廉价 clone，所有 clone 共享同一条连接）
#[derive(Clone)]
pub struct OneBot {
    inner: Arc<Inner>,
}

struct Inner {
    url: String,
    token: Option<String>,
    /// 当前连接的发送队列；未连接时为 None
    out: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// echo → 等待回包的调用方
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    seq: AtomicU64,
    /// 机器人自己的 QQ 号（连上后由 get_login_info / 事件里的 self_id 回填）
    self_id: AtomicI64,
    /// 首次就绪信号（拿到 self_id 之后置 true）
    ready: watch::Sender<bool>,
}

impl OneBot {
    pub fn new(url: String, token: Option<String>) -> Self {
        let (ready, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                url,
                token,
                out: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                seq: AtomicU64::new(1),
                self_id: AtomicI64::new(0),
                ready,
            }),
        }
    }

    /// 启动连接循环（后台任务，永不返回）；返回事件接收端
    pub fn start(&self) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        let me = self.clone();
        tokio::spawn(async move { me.run(tx).await });
        rx
    }

    /// 机器人自己的 QQ 号（未就绪时为 0）
    pub fn self_id(&self) -> i64 {
        self.inner.self_id.load(Ordering::Relaxed)
    }

    /// 等到第一次连上 LLBot 并拿到登录信息
    pub async fn wait_ready(&self) {
        let mut rx = self.inner.ready.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    async fn run(&self, events: mpsc::UnboundedSender<Value>) {
        let mut backoff = 2u64;
        loop {
            let (connected, res) = self.session(&events).await;
            self.on_disconnect();
            if connected {
                backoff = 2;
            }
            match res {
                Ok(()) => warn!("与 LLBot 的 OneBot 连接已断开，{backoff}s 后重连"),
                Err(e) => warn!(
                    "OneBot 连接失败（{}）：{e:#}；{backoff}s 后重连。请确认 SnowLuma（或其它 OneBot 实现）已启动、已连上 QQ，并开启了正向 WebSocket",
                    self.inner.url
                ),
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    }

    /// 一次完整的连接生命周期。返回（是否曾连上，断开原因）
    async fn session(&self, events: &mpsc::UnboundedSender<Value>) -> (bool, Result<()>) {
        let mut req = match self.inner.url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => return (false, Err(anyhow!("OneBot 地址无效：{e}"))),
        };
        if let Some(t) = &self.inner.token {
            match format!("Bearer {t}").parse() {
                Ok(h) => {
                    req.headers_mut().insert("Authorization", h);
                }
                Err(e) => return (false, Err(anyhow!("ONEBOT_ACCESS_TOKEN 含非法字符：{e}"))),
            }
        }
        let ws = match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => ws,
            Err(e) => return (false, Err(e.into())),
        };
        info!("已连接 OneBot（正向 WS：{}）", self.inner.url);
        let (mut sink, mut stream) = ws.split();
        let (otx, mut orx) = mpsc::unbounded_channel::<String>();
        *self.inner.out.lock().unwrap_or_else(|e| e.into_inner()) = Some(otx);

        // 拉登录信息（发出的请求先进队列，下面的循环开始后才真正写出去）
        let me = self.clone();
        tokio::spawn(async move {
            match me.call("get_login_info", json!({})).await {
                Ok(v) => {
                    let id = v.get("user_id").map(value_i64).unwrap_or(0);
                    if id != 0 {
                        me.inner.self_id.store(id, Ordering::Relaxed);
                    }
                    let nick = v.get("nickname").and_then(Value::as_str).unwrap_or("");
                    info!("OneBot 已就绪：{nick}（QQ {id}）");
                    me.inner.ready.send_replace(true);
                }
                Err(e) => warn!("get_login_info 失败：{e:#}"),
            }
        });

        let res: Result<()> = loop {
            tokio::select! {
                out = orx.recv() => match out {
                    Some(s) => {
                        if let Err(e) = sink.send(WsMessage::Text(s)).await {
                            break Err(e.into());
                        }
                    }
                    None => break Ok(()),
                },
                msg = stream.next() => match msg {
                    Some(Ok(WsMessage::Text(t))) => self.on_text(&t, events),
                    Some(Ok(WsMessage::Binary(b))) => {
                        if let Ok(t) = String::from_utf8(b) {
                            self.on_text(&t, events);
                        }
                    }
                    Some(Ok(WsMessage::Ping(p))) => {
                        if let Err(e) = sink.send(WsMessage::Pong(p)).await {
                            break Err(e.into());
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(e.into()),
                },
            }
        };
        (true, res)
    }

    fn on_disconnect(&self) {
        *self.inner.out.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // 丢掉所有等待中的调用：发送端被 drop，调用方立刻收到“连接中断”
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn on_text(&self, text: &str, events: &mpsc::UnboundedSender<Value>) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            debug!("OneBot 收到无法解析的帧：{}", clip(text, 120));
            return;
        };
        if v.get("post_type").is_none() {
            // API 回包
            let echo = v.get("echo").map(value_string).unwrap_or_default();
            let waiter = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&echo);
            if let Some(w) = waiter {
                let _ = w.send(v);
            }
            return;
        }
        if let Some(id) = v.get("self_id").map(value_i64).filter(|i| *i != 0) {
            self.inner.self_id.store(id, Ordering::Relaxed);
        }
        if v.get("post_type").and_then(Value::as_str) == Some("meta_event") {
            // 心跳 / 生命周期：不往下传
            if v.get("meta_event_type").and_then(Value::as_str) == Some("lifecycle") {
                debug!("OneBot 生命周期事件：{}", clip(text, 200));
            }
            return;
        }
        let _ = events.send(v);
    }

    /// 调用一个 OneBot API，返回 `data` 字段
    pub async fn call(&self, action: &str, params: Value) -> Result<Value> {
        let echo = format!("qb-{}", self.inner.seq.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(echo.clone(), tx);
        let payload = json!({ "action": action, "params": params, "echo": echo }).to_string();
        let sent = self
            .inner
            .out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.send(payload).is_ok())
            .unwrap_or(false);
        if !sent {
            self.forget(&echo);
            return Err(anyhow!("LLBot 未连接，无法调用 {action}"));
        }
        let resp = match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => return Err(anyhow!("调用 {action} 时与 LLBot 的连接中断")),
            Err(_) => {
                self.forget(&echo);
                return Err(anyhow!("调用 {action} 超时（{}s）", CALL_TIMEOUT.as_secs()));
            }
        };
        let ok = resp.get("status").and_then(Value::as_str) == Some("ok")
            || resp.get("retcode").map(value_i64) == Some(0);
        if !ok {
            let retcode = resp.get("retcode").map(value_i64).unwrap_or(-1);
            let why = resp
                .get("wording")
                .or_else(|| resp.get("message"))
                .or_else(|| resp.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(anyhow!("OneBot {action} 失败（retcode={retcode}）：{why}"));
        }
        Ok(resp.get("data").cloned().unwrap_or(Value::Null))
    }

    fn forget(&self, echo: &str) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(echo);
    }

    /// 发一条消息（段数组）到群 / 私聊
    pub async fn send(&self, chat: Chat, message: Vec<Value>) -> Result<()> {
        let (action, params) = match chat {
            Chat::Group(g) => ("send_group_msg", json!({ "group_id": g, "message": message })),
            Chat::Private(u) => ("send_private_msg", json!({ "user_id": u, "message": message })),
        };
        self.call(action, params).await.map(|_| ())
    }

    /// 主动发纯文本
    pub async fn send_text(&self, chat: Chat, text: &str) -> Result<()> {
        self.send(chat, vec![seg_text(text)]).await
    }

    /// 主动发「图 + 文」（文字可为空）
    pub async fn send_media(&self, chat: Chat, img: &Image, text: &str) -> Result<()> {
        let mut msg = vec![img.segment()];
        if !text.trim().is_empty() {
            msg.push(seg_text(&format!("\n{text}")));
        }
        self.send(chat, msg).await
    }
}

/// 一个 QQ 会话：群或私聊（好友 / 群临时会话）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chat {
    Group(i64),
    Private(i64),
}

impl Chat {
    /// 从持久化的字符串 id 还原（订阅数据里存的是字符串）。
    /// 旧版 QQ 官方机器人留下的 openid 不是数字，会返回 None，调用方跳过即可。
    pub fn group_str(id: &str) -> Option<Chat> {
        id.trim().parse().ok().filter(|v: &i64| *v > 0).map(Chat::Group)
    }

    pub fn private_str(id: &str) -> Option<Chat> {
        id.trim().parse().ok().filter(|v: &i64| *v > 0).map(Chat::Private)
    }
}

/// 要发的图片：本地字节（`base64://` 内联）或远端 URL（由 LLBot 自己下载）
#[derive(Debug, Clone)]
pub enum Image {
    Bytes(Vec<u8>),
    Url(String),
}

impl Image {
    pub fn segment(&self) -> Value {
        let file = match self {
            Image::Bytes(b) => format!("base64://{}", crate::vision::base64_encode(b)),
            Image::Url(u) => u.clone(),
        };
        json!({ "type": "image", "data": { "file": file } })
    }
}

/// 回复目标：会话 + 可选的「引用回复」消息 id + 可选的「@某人」（只在群里生效）
#[derive(Debug, Clone)]
pub struct Target {
    pub chat: Chat,
    pub reply_to: Option<String>,
    pub at: Option<i64>,
}

impl Target {
    pub fn new(chat: Chat, reply_to: Option<String>) -> Self {
        Self { chat, reply_to, at: None }
    }

    /// 消息开头 @ 这个人（主动找人说话）
    pub fn with_at(mut self, uid: i64) -> Self {
        self.at = Some(uid);
        self
    }

    fn head(&self) -> Vec<Value> {
        let mut h = match &self.reply_to {
            Some(id) if !id.is_empty() => vec![json!({ "type": "reply", "data": { "id": id } })],
            _ => Vec::new(),
        };
        if let (Some(uid), Chat::Group(_)) = (self.at, self.chat) {
            h.push(json!({ "type": "at", "data": { "qq": uid.to_string() } }));
            h.push(seg_text(" "));
        }
        h
    }

    /// 回复纯文本
    pub async fn reply(&self, bot: &OneBot, text: &str) -> Result<()> {
        let mut msg = self.head();
        msg.push(seg_text(text));
        bot.send(self.chat, msg).await
    }

    /// 回复「图 + 文」合并的一条消息（文字可为空）
    pub async fn reply_media(&self, bot: &OneBot, img: &Image, text: &str) -> Result<()> {
        let mut msg = self.head();
        msg.push(img.segment());
        if !text.trim().is_empty() {
            msg.push(seg_text(&format!("\n{text}")));
        }
        bot.send(self.chat, msg).await
    }
}

pub fn seg_text(t: &str) -> Value {
    json!({ "type": "text", "data": { "text": t } })
}

/// 一条入站消息解析后的结果
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Parsed {
    /// 可读文本：表情映射成 `[表情:微笑]`，卡片渲染成 `[卡片消息] …`，@ 机器人的段去掉
    pub text: String,
    /// 是否 @ 了机器人
    pub at_me: bool,
    /// 消息里的图片 / 表情包段（原样保留，交给视觉模块）
    pub segments: Vec<Value>,
    /// 被 @ 的其他人（QQ 号，昵称；不含机器人自己和 @全体）——表情包插件拿来取头像
    pub ats: Vec<(i64, String)>,
    /// 引用回复的消息 id（`reply` 段）——表情包插件可以从被引用的消息里取图
    pub reply_id: Option<String>,
    /// 去掉所有 @ 段之后的纯文本（表情包指令解析用，避免把「@小明」当成文字参数）
    pub bare_text: String,
}

/// 把事件里的 `message` 字段（段数组，或 CQ 码字符串）解析成 [`Parsed`]
pub fn parse_message(message: &Value, self_id: i64) -> Parsed {
    let segs: Vec<Value> = match message {
        Value::Array(a) => a.clone(),
        Value::String(s) => parse_cq(s),
        _ => Vec::new(),
    };
    let me = self_id.to_string();
    let mut out = Parsed::default();
    let mut text = String::new();
    let mut bare = String::new();
    for seg in &segs {
        let kind = seg.get("type").and_then(Value::as_str).unwrap_or("");
        let data = seg.get("data").cloned().unwrap_or(Value::Null);
        let field = |k: &str| data.get(k).map(value_string).unwrap_or_default();
        match kind {
            "text" => {
                text.push_str(&field("text"));
                bare.push_str(&field("text"));
            }
            "reply" => {
                let id = field("id");
                if !id.is_empty() {
                    out.reply_id = Some(id);
                }
            }
            "at" => {
                let qq = field("qq");
                if self_id != 0 && qq == me {
                    out.at_me = true;
                } else if qq == "all" {
                    text.push_str("@全体成员 ");
                } else {
                    let name = field("name").trim_start_matches('@').to_string();
                    if !name.is_empty() {
                        text.push_str(&format!("@{name} "));
                    }
                    let id: i64 = qq.trim().parse().unwrap_or(0);
                    if id > 0 && !out.ats.iter().any(|(q, _)| *q == id) {
                        out.ats.push((id, name));
                    }
                }
            }
            "face" => match crate::vision::face_name(&field("id")) {
                Some(name) => text.push_str(&format!("[表情:{name}]")),
                None => text.push_str("[表情]"),
            },
            "mface" => {
                // 商城表情：有 url 时交给视觉模块看图，否则只留摘要
                if field("url").starts_with("http") {
                    out.segments.push(seg.clone());
                } else {
                    let s = field("summary");
                    text.push_str(if s.is_empty() { "[表情包]" } else { &s });
                }
            }
            "image" => out.segments.push(seg.clone()),
            "json" => text.push_str(&card_text(&field("data"))),
            "forward" => text.push_str("[合并转发]"),
            "dice" => text.push_str(&format!("[骰子:{}]", field("result"))),
            "rps" => text.push_str("[猜拳]"),
            "poke" => text.push_str("[戳一戳]"),
            _ => {}
        }
    }
    out.text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    out.bare_text = bare.split_whitespace().collect::<Vec<_>>().join(" ");
    out
}

/// QQ 小程序 / 链接卡片（`json` 段）→ 一行文本。
///
/// - 卡片里有 B站跳转链接（`qqdocurl` / `jumpUrl`）时**只输出链接**：后续按「纯分享」走
///   B站解析，回封面 + 标题 + UP + 播放量；
/// - 否则渲染成 `[卡片消息] 小程序 摘要: … title: … preview: … source: …`，
///   与 `bili::parse_share_card` 的格式对齐。
pub fn card_text(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return "[卡片消息]".to_string();
    };
    let meta = v.get("meta").and_then(Value::as_object);
    let detail = meta.and_then(|m| m.values().next()).cloned().unwrap_or(Value::Null);
    let get = |k: &str| {
        detail
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let url = [get("qqdocurl"), get("jumpUrl"), get("url")]
        .into_iter()
        .find(|u| u.starts_with("http"))
        .unwrap_or_default();
    if !url.is_empty() && crate::bili::extract_bili_url(&url).is_some() {
        return url;
    }
    let preview = {
        let p = get("preview");
        if p.is_empty() || p.starts_with("http") {
            p
        } else {
            format!("https://{p}")
        }
    };
    // 小程序卡片：title 是来源（如「哔哩哔哩」），desc 才是内容标题；链接卡片：title 是标题
    let is_app = v.get("app").and_then(Value::as_str).unwrap_or("").contains("miniapp")
        || !get("appid").is_empty();
    let (title, source) = if is_app {
        (get("desc"), get("title"))
    } else {
        (get("title"), get("tag"))
    };
    let summary = v.get("prompt").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let kind = if is_app { "小程序" } else { "链接" };
    let mut s = format!("[卡片消息] {kind} 摘要: {summary} title: {title} preview: {preview} source: {source}");
    if !url.is_empty() {
        s.push_str(&format!(" {url}"));
    }
    s
}

/// 解析 CQ 码字符串（LLBot 消息格式选 `string` 时）成段数组
pub fn parse_cq(s: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("[CQ:") {
        if start > 0 {
            out.push(seg_text(&cq_unescape(&rest[..start])));
        }
        let after = &rest[start + 4..];
        let Some(end) = after.find(']') else {
            out.push(seg_text(&cq_unescape(&rest[start..])));
            return out;
        };
        let body = &after[..end];
        let mut parts = body.split(',');
        let kind = parts.next().unwrap_or("").trim().to_string();
        let mut data = serde_json::Map::new();
        for kv in parts {
            if let Some((k, v)) = kv.split_once('=') {
                data.insert(k.trim().to_string(), Value::String(cq_unescape(v)));
            }
        }
        out.push(json!({ "type": kind, "data": Value::Object(data) }));
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        out.push(seg_text(&cq_unescape(rest)));
    }
    out
}

fn cq_unescape(s: &str) -> String {
    s.replace("&#91;", "[")
        .replace("&#93;", "]")
        .replace("&#44;", ",")
        .replace("&amp;", "&")
}

/// OneBot 实现对数字 / 字符串类型不统一（message_id、user_id 都可能是字符串），统一宽松读取
pub fn value_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

pub fn value_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
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
    fn 段数组解析_at机器人与表情() {
        let msg = json!([
            {"type": "at", "data": {"qq": "10001"}},
            {"type": "text", "data": {"text": " 你好 "}},
            {"type": "face", "data": {"id": "0"}},
            {"type": "at", "data": {"qq": "20002", "name": "小明"}},
            {"type": "image", "data": {"url": "https://x/a.jpg"}},
        ]);
        let p = parse_message(&msg, 10001);
        assert!(p.at_me);
        assert_eq!(p.text, "你好 [表情:微笑]@小明");
        assert_eq!(p.segments.len(), 1);
    }

    #[test]
    fn 记录at对象与引用回复() {
        let msg = json!([
            {"type": "reply", "data": {"id": "123456"}},
            {"type": "at", "data": {"qq": "10001"}},
            {"type": "text", "data": {"text": "摸 "}},
            {"type": "at", "data": {"qq": "20002", "name": "@小明"}},
            {"type": "at", "data": {"qq": "20002", "name": "小明"}},
            {"type": "at", "data": {"qq": "all"}},
            {"type": "text", "data": {"text": " 好可爱"}},
        ]);
        let p = parse_message(&msg, 10001);
        assert!(p.at_me);
        assert_eq!(p.reply_id.as_deref(), Some("123456"));
        assert_eq!(p.ats, vec![(20002, "小明".to_string())], "去重、不含自己和全体");
        assert_eq!(p.bare_text, "摸 好可爱", "bare_text 不含任何 @");
        assert!(p.text.contains("@小明"));
    }

    #[test]
    fn 没at机器人() {
        let p = parse_message(&json!([{"type": "at", "data": {"qq": "20002"}}, {"type": "text", "data": {"text": "hi"}}]), 10001);
        assert!(!p.at_me);
        assert_eq!(p.text, "hi");
    }

    #[test]
    fn cq码字符串也能解析() {
        let p = parse_message(&json!("[CQ:at,qq=10001] 订阅战队 RA&#44;好 [CQ:face,id=6]"), 10001);
        assert!(p.at_me);
        assert_eq!(p.text, "订阅战队 RA,好 [表情:害羞]");
        let segs = parse_cq("[CQ:image,file=a.jpg,url=https://x/a.jpg?a=1&amp;b=2]");
        assert_eq!(segs[0]["data"]["url"], "https://x/a.jpg?a=1&b=2");
        assert_eq!(parse_cq("坏的 [CQ:at,qq=1").len(), 2, "未闭合 CQ 码按文本保留");
    }

    #[test]
    fn b站小程序卡片输出链接() {
        let raw = json!({
            "app": "com.tencent.miniapp_01",
            "prompt": "[QQ小程序]哔哩哔哩",
            "meta": {"detail_1": {
                "appid": "1109937557",
                "title": "哔哩哔哩",
                "desc": "【测试】一个视频",
                "preview": "pubminishare-30161.picsz.qpic.cn/abc",
                "qqdocurl": "https://b23.tv/AbCdEf?share_medium=android"
            }}
        })
        .to_string();
        let t = card_text(&raw);
        assert!(t.starts_with("https://b23.tv/AbCdEf"), "{t}");
    }

    #[test]
    fn 普通链接卡片渲染成卡片文本() {
        let raw = json!({
            "app": "com.tencent.structmsg",
            "prompt": "[分享]一篇文章",
            "meta": {"news": {"title": "标题A", "desc": "描述", "preview": "https://p/1.jpg", "jumpUrl": "https://example.com/a", "tag": "某网站"}}
        })
        .to_string();
        let t = card_text(&raw);
        let card = crate::bili::parse_share_card(&t).expect("应能被 parse_share_card 识别");
        assert_eq!(card.title, "标题A");
        assert_eq!(card.preview, "https://p/1.jpg");
        assert!(!card.is_bilibili());
    }

    #[test]
    fn 字符串数字宽松读取() {
        assert_eq!(value_i64(&json!("123")), 123);
        assert_eq!(value_i64(&json!(456)), 456);
        assert_eq!(value_string(&json!(789)), "789");
        assert_eq!(Chat::group_str("123"), Some(Chat::Group(123)));
        assert_eq!(Chat::group_str("3411572DD065226596CE2E0712EB81D9"), None, "旧 openid 跳过");
    }

    /// 本地起一个假 OneBot 服务端，走一遍：鉴权头 → get_login_info → 推群消息事件 → send_group_msg 回包
    #[tokio::test]
    async fn 假onebot服务端全链路() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sent_tx, mut sent_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let check = |req: &Request, resp: Response| {
                let auth = req.headers().get("Authorization").and_then(|v| v.to_str().ok());
                assert_eq!(auth, Some("Bearer tok"), "应带 access token");
                Ok(resp)
            };
            let ws = tokio_tungstenite::accept_hdr_async(tcp, check).await.unwrap();
            let (mut sink, mut stream) = ws.split();
            while let Some(Ok(WsMessage::Text(t))) = stream.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let echo = req["echo"].clone();
                match req["action"].as_str().unwrap() {
                    "get_login_info" => {
                        let resp = json!({"status": "ok", "retcode": 0, "data": {"user_id": 10001, "nickname": "Heevi"}, "echo": echo});
                        sink.send(WsMessage::Text(resp.to_string())).await.unwrap();
                        let ev = json!({
                            "post_type": "message", "message_type": "group", "self_id": 10001,
                            "group_id": 777, "user_id": 42, "message_id": 9,
                            "message": [{"type": "at", "data": {"qq": "10001"}}, {"type": "text", "data": {"text": " 早报"}}],
                            "sender": {"nickname": "小明", "card": "", "role": "admin"}
                        });
                        sink.send(WsMessage::Text(ev.to_string())).await.unwrap();
                    }
                    _ => {
                        sent_tx.send(req.clone()).unwrap();
                        let resp = json!({"status": "ok", "retcode": 0, "data": {"message_id": 1}, "echo": echo});
                        sink.send(WsMessage::Text(resp.to_string())).await.unwrap();
                    }
                }
            }
        });

        let ob = OneBot::new(format!("ws://{addr}"), Some("tok".into()));
        let mut events = ob.start();
        tokio::time::timeout(Duration::from_secs(5), ob.wait_ready()).await.expect("应在 5 秒内就绪");
        assert_eq!(ob.self_id(), 10001);
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
        let p = parse_message(&ev["message"], ob.self_id());
        assert!(p.at_me);
        assert_eq!(p.text, "早报");

        let t = Target::new(Chat::Group(777), Some("9".into()));
        t.reply(&ob, "你好").await.expect("send_group_msg 应成功");
        let sent = sent_rx.recv().await.unwrap();
        assert_eq!(sent["action"], "send_group_msg");
        assert_eq!(sent["params"]["group_id"], 777);
        assert_eq!(sent["params"]["message"][0]["type"], "reply");
        assert_eq!(sent["params"]["message"][1]["data"]["text"], "你好");
    }

    #[tokio::test]
    async fn 未连接时调用直接报错() {
        let ob = OneBot::new("ws://127.0.0.1:9".into(), None);
        let e = ob.send_text(Chat::Private(1), "x").await.unwrap_err();
        assert!(format!("{e:#}").contains("未连接"), "{e:#}");
    }

    #[test]
    fn 回复目标带引用段() {
        let t = Target::new(Chat::Group(1), Some("42".into()));
        let head = t.head();
        assert_eq!(head[0]["type"], "reply");
        assert_eq!(head[0]["data"]["id"], "42");
        assert!(Target::new(Chat::Private(1), None).head().is_empty());
    }
}
