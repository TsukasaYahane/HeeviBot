//! 表情包制作插件：对接 [meme-generator](https://github.com/MemeCrafters/meme-generator)（0.1.x，`meme run`）。
//!
//! meme-generator 作为独立进程跑 HTTP 服务（默认 `http://127.0.0.1:3233`），bot 只当客户端：
//! - 启动时 / 定时拉一遍 `GET /memes/keys` + `GET /memes/{key}/info`，建「关键词 → 表情」索引；
//! - 消息以关键词开头（如「摸 @小明」「petpet」「举牌 下班了」）→ 凑素材 → `POST /memes/{key}/`
//!   （multipart：`images` 多个文件、`texts` 多个字段、`args` JSON）→ 把生成的图片 / GIF 发回 QQ。
//!
//! 素材来源（按顺序凑，凑够 `min_images` 为止，不超过 `max_images`）：
//! 消息里的图 → 被引用消息里的图 → 被 @ 的人的头像 → 发送者自己的头像。
//! 需要两张图而只 @ 了一个人时，按 nonebot-plugin-memes 的习惯把「自己」放在第一张（如「亲 @小明」= 我亲小明）。
//!
//! 服务没开、关键词没命中、素材 / 文字数量不满足时都返回 `None`，消息照常走后面的流程，不会吞掉聊天。

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use tracing::{info, warn};

/// 一个表情的参数约束（来自 `/memes/{key}/info` 的 `params_type`）
#[derive(Debug, Clone, PartialEq)]
pub struct MemeInfo {
    pub key: String,
    pub keywords: Vec<String>,
    pub min_images: usize,
    pub max_images: usize,
    pub min_texts: usize,
    pub max_texts: usize,
    pub default_texts: Vec<String>,
    pub tags: Vec<String>,
}

impl MemeInfo {
    /// 从 info JSON 解析；缺字段时用最宽松的默认值
    pub fn from_json(v: &Value) -> Option<MemeInfo> {
        let key = v.get("key")?.as_str()?.to_string();
        let p = v.get("params_type").cloned().unwrap_or(Value::Null);
        let num = |k: &str, d: usize| p.get(k).and_then(Value::as_u64).map(|n| n as usize).unwrap_or(d);
        let strs = |val: Option<&Value>| -> Vec<String> {
            val.and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        let mut keywords = strs(v.get("keywords"));
        if keywords.is_empty() {
            keywords.push(key.clone());
        }
        Some(MemeInfo {
            key,
            keywords,
            min_images: num("min_images", 0),
            max_images: num("max_images", 0),
            min_texts: num("min_texts", 0),
            max_texts: num("max_texts", 0),
            default_texts: strs(p.get("default_texts")),
            tags: strs(v.get("tags")),
        })
    }
}

/// 一次表情制作请求（已经从消息里解析出来的）
#[derive(Debug, Clone, PartialEq)]
pub struct MemeRequest {
    pub key: String,
    /// 命中的关键词（日志用）
    pub keyword: String,
    /// 关键词后面的文字参数（按空白切分）
    pub texts: Vec<String>,
}

/// 消息里能当素材的东西
#[derive(Debug, Clone, Default)]
pub struct Materials {
    /// 消息本身 + 被引用消息里的图片 URL（按出现顺序）
    pub image_urls: Vec<String>,
    /// 被 @ 的人（QQ 号）
    pub ats: Vec<i64>,
    /// 发送者 QQ 号
    pub sender: i64,
}

impl Materials {
    /// 按表情需要的张数凑图片来源（URL）。凑不够 `min_images` 返回 None。
    pub fn pick_images(&self, info: &MemeInfo) -> Option<Vec<String>> {
        if info.max_images == 0 {
            return Some(Vec::new());
        }
        let mut out: Vec<String> = self.image_urls.iter().take(info.max_images).cloned().collect();
        let at_urls: Vec<String> = self.ats.iter().map(|q| avatar_url(*q)).collect();
        // 需要两张以上、但用户只提供了一个目标：自己当第一张（「亲 @小明」= 我亲小明）
        if out.is_empty() && at_urls.len() == 1 && info.min_images >= 2 && self.sender > 0 {
            out.push(avatar_url(self.sender));
        }
        for u in at_urls {
            if out.len() >= info.max_images {
                break;
            }
            out.push(u);
        }
        // 还不够：用发送者头像补
        if out.len() < info.min_images && self.sender > 0 && !out.contains(&avatar_url(self.sender)) {
            out.insert(0, avatar_url(self.sender));
        }
        if out.len() < info.min_images {
            return None;
        }
        Some(out)
    }
}

/// QQ 头像直链（640px）
pub fn avatar_url(qq: i64) -> String {
    format!("https://q1.qlogo.cn/g?b=qq&nk={qq}&s=640")
}

/// 关键词索引：最长关键词优先匹配（「摸摸头」先于「摸」）
#[derive(Default)]
struct Index {
    infos: HashMap<String, MemeInfo>,
    /// (关键词小写, key)，按关键词长度降序
    keywords: Vec<(String, String)>,
}

pub struct MemeClient {
    base: String,
    http: reqwest::Client,
    index: RwLock<Index>,
    /// 关键词前缀（如设为「/」则只认「/摸」）；空 = 直接说关键词就行
    prefix: String,
}

impl MemeClient {
    pub fn new(base: &str, prefix: &str, timeout_secs: u64) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs.max(5)))
                .build()?,
            index: RwLock::new(Index::default()),
            prefix: prefix.trim().to_string(),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn count(&self) -> usize {
        self.index.read().unwrap_or_else(|e| e.into_inner()).infos.len()
    }

    pub fn info(&self, key: &str) -> Option<MemeInfo> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).infos.get(key).cloned()
    }

    /// 全部表情 key（render_list 用）
    pub fn keys(&self) -> Vec<String> {
        let mut v: Vec<String> = self.index.read().unwrap_or_else(|e| e.into_inner()).infos.keys().cloned().collect();
        v.sort();
        v
    }

    /// 拉一遍表情列表与每个表情的参数，重建关键词索引；返回表情数量
    pub async fn refresh(&self) -> Result<usize> {
        let keys: Vec<String> = self
            .http
            .get(format!("{}/memes/keys", self.base))
            .send()
            .await
            .map_err(|e| anyhow!("连不上 meme-generator（{}）：{e}", self.base))?
            .error_for_status()?
            .json()
            .await?;
        let mut infos = HashMap::new();
        for key in keys {
            let url = format!("{}/memes/{key}/info", self.base);
            match self.http.get(&url).send().await {
                Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                    Ok(v) => {
                        if let Some(info) = MemeInfo::from_json(&v) {
                            infos.insert(info.key.clone(), info);
                        }
                    }
                    Err(e) => warn!("表情 {key} 的 info 解析失败：{e}"),
                },
                Ok(r) => warn!("表情 {key} 的 info 返回 HTTP {}", r.status()),
                Err(e) => warn!("表情 {key} 的 info 请求失败：{e}"),
            }
        }
        let n = infos.len();
        let idx = build_index(infos);
        *self.index.write().unwrap_or_else(|e| e.into_inner()) = idx;
        Ok(n)
    }

    /// 解析一条消息是不是表情制作指令（只认「关键词 [文字…]」开头的消息）
    pub fn parse(&self, bare_text: &str) -> Option<MemeRequest> {
        let idx = self.index.read().unwrap_or_else(|e| e.into_inner());
        parse_with(&idx, &self.prefix, bare_text)
    }

    /// 生成一张表情：下载素材图 → multipart 交给 meme-generator → 返回图片字节
    pub async fn generate(&self, key: &str, image_urls: &[String], texts: &[String]) -> Result<Vec<u8>> {
        let mut form = reqwest::multipart::Form::new();
        for (i, url) in image_urls.iter().enumerate() {
            let bytes = self.download(url).await?;
            let part = reqwest::multipart::Part::bytes(bytes).file_name(format!("image{i}.png"));
            form = form.part("images", part);
        }
        for t in texts {
            form = form.text("texts", t.clone());
        }
        // 不传 args：服务端会用该表情自己的默认参数（传 "{}" 遇到有必填参数的表情会 552）
        let resp = self
            .http
            .post(format!("{}/memes/{key}/", self.base))
            .multipart(form)
            .send()
            .await
            .map_err(|e| anyhow!("请求 meme-generator 失败：{e}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await?.to_vec();
        if !status.is_success() {
            return Err(anyhow!("meme-generator 返回 HTTP {status}：{}", error_detail(&bytes)));
        }
        Ok(bytes)
    }

    /// 表情列表大图（`POST /memes/render_list`）
    pub async fn render_list(&self) -> Result<Vec<u8>> {
        let list: Vec<Value> = self
            .keys()
            .into_iter()
            .map(|k| json!({ "meme_key": k, "disabled": false, "labels": [] }))
            .collect();
        let resp = self
            .http
            .post(format!("{}/memes/render_list", self.base))
            .json(&json!({ "meme_list": list }))
            .send()
            .await?;
        let status = resp.status();
        let bytes = resp.bytes().await?.to_vec();
        if !status.is_success() {
            return Err(anyhow!("render_list 返回 HTTP {status}：{}", error_detail(&bytes)));
        }
        Ok(bytes)
    }

    /// 单个表情的示例图（`GET /memes/{key}/preview`）
    pub async fn preview(&self, key: &str) -> Result<Vec<u8>> {
        let resp = self.http.get(format!("{}/memes/{key}/preview", self.base)).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?.to_vec();
        if !status.is_success() {
            return Err(anyhow!("preview 返回 HTTP {status}：{}", error_detail(&bytes)));
        }
        Ok(bytes)
    }

    /// 按关键词 / key 查表情（「表情详情 摸」用）
    pub fn find(&self, name: &str) -> Option<MemeInfo> {
        let idx = self.index.read().unwrap_or_else(|e| e.into_inner());
        let n = name.trim().to_lowercase();
        idx.infos.get(&n).cloned().or_else(|| {
            idx.keywords
                .iter()
                .find(|(k, _)| *k == n)
                .and_then(|(_, key)| idx.infos.get(key).cloned())
        })
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let r = self
            .http
            .get(url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send()
            .await
            .map_err(|e| anyhow!("下载素材图失败：{e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("下载素材图失败：{e}"))?;
        let b = r.bytes().await?.to_vec();
        if b.len() > 10 * 1024 * 1024 {
            return Err(anyhow!("素材图超过 10MB"));
        }
        Ok(b)
    }
}

fn build_index(infos: HashMap<String, MemeInfo>) -> Index {
    let mut keywords: Vec<(String, String)> = Vec::new();
    for info in infos.values() {
        for kw in &info.keywords {
            let kw = kw.trim().to_lowercase();
            if !kw.is_empty() && !keywords.iter().any(|(k, _)| *k == kw) {
                keywords.push((kw, info.key.clone()));
            }
        }
    }
    keywords.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()).then(a.0.cmp(&b.0)));
    Index { infos, keywords }
}

/// 关键词匹配：消息 = [前缀]关键词 + (结尾 | 空白 + 文字参数)。
/// 关键词后面必须是结尾或空白，避免「摸鱼去了」被当成「摸」。
fn parse_with(idx: &Index, prefix: &str, bare_text: &str) -> Option<MemeRequest> {
    let t = bare_text.trim();
    let t = if prefix.is_empty() { t } else { t.strip_prefix(prefix)?.trim_start() };
    if t.is_empty() {
        return None;
    }
    let lower = t.to_lowercase();
    for (kw, key) in &idx.keywords {
        let Some(rest) = lower.strip_prefix(kw.as_str()) else {
            continue;
        };
        if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
            continue;
        }
        // 用原文（保留大小写）取参数：lower 与 t 的字节长度在 to_lowercase 后可能不同，按字符数切
        let kw_chars = kw.chars().count();
        let rest_orig: String = t.chars().skip(kw_chars).collect();
        let texts: Vec<String> = rest_orig.split_whitespace().map(str::to_string).collect();
        return Some(MemeRequest {
            key: key.clone(),
            keyword: kw.clone(),
            texts,
        });
    }
    None
}

/// 把用户给的文字按表情要求整理：多余的丢掉，不够用 default_texts 补；补不够 min_texts 返回 Err（给用户的提示）
pub fn fit_texts(info: &MemeInfo, given: &[String]) -> std::result::Result<Vec<String>, String> {
    if info.max_texts == 0 {
        return Ok(Vec::new());
    }
    let mut texts: Vec<String> = given.iter().take(info.max_texts).cloned().collect();
    if texts.is_empty() && !info.default_texts.is_empty() {
        texts = info.default_texts.iter().take(info.max_texts).cloned().collect();
    }
    if texts.len() < info.min_texts {
        return Err(if info.min_texts == info.max_texts {
            format!("「{}」需要 {} 段文字（用空格隔开）", info.keywords[0], info.min_texts)
        } else {
            format!("「{}」需要 {}~{} 段文字（用空格隔开）", info.keywords[0], info.min_texts, info.max_texts)
        });
    }
    Ok(texts)
}

/// 从错误响应里抠出可读信息（FastAPI 的 `{"detail": ...}`）
fn error_detail(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    serde_json::from_str::<Value>(&s)
        .ok()
        .and_then(|v| v.get("detail").map(|d| d.as_str().map(str::to_string).unwrap_or_else(|| d.to_string())))
        .unwrap_or_else(|| s.chars().take(200).collect())
}

/// 一句话描述表情用法（「表情详情」用）
pub fn describe(info: &MemeInfo) -> String {
    let mut out = format!("🖼 {}（{}）\n关键词：{}", info.keywords[0], info.key, info.keywords.join(" / "));
    let imgs = match (info.min_images, info.max_images) {
        (0, 0) => "不需要图片".to_string(),
        (a, b) if a == b => format!("需要 {a} 张图"),
        (a, b) => format!("需要 {a}~{b} 张图"),
    };
    let txts = match (info.min_texts, info.max_texts) {
        (0, 0) => "不需要文字".to_string(),
        (a, b) if a == b => format!("需要 {a} 段文字"),
        (a, b) => format!("需要 {a}~{b} 段文字"),
    };
    out.push_str(&format!("\n{imgs}，{txts}"));
    if !info.default_texts.is_empty() {
        out.push_str(&format!("\n默认文字：{}", info.default_texts.join(" ")));
    }
    if !info.tags.is_empty() {
        out.push_str(&format!("\n标签：{}", info.tags.join("、")));
    }
    out.push_str("\n用法：发「关键词 [文字]」，图片可以直接附上、引用一张图，或者 @ 某人用 TA 的头像。");
    out
}

/// 后台刷新：启动时拉一次（服务没开就每分钟重试），之后每 6 小时刷新一次
pub async fn refresh_loop(client: std::sync::Arc<MemeClient>) {
    let mut first_ok = false;
    loop {
        match client.refresh().await {
            Ok(n) => {
                if !first_ok {
                    info!("表情包制作已就绪：{n} 个表情（meme-generator：{}）", client.base());
                }
                first_ok = true;
            }
            Err(e) => {
                if first_ok {
                    warn!("刷新表情列表失败（沿用旧列表）：{e:#}");
                } else {
                    warn!("meme-generator 暂不可用，1 分钟后重试：{e:#}");
                }
            }
        }
        let wait = if first_ok { 6 * 3600 } else { 60 };
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(key: &str, kws: &[&str], imgs: (usize, usize), texts: (usize, usize), defaults: &[&str]) -> MemeInfo {
        MemeInfo {
            key: key.into(),
            keywords: kws.iter().map(|s| s.to_string()).collect(),
            min_images: imgs.0,
            max_images: imgs.1,
            min_texts: texts.0,
            max_texts: texts.1,
            default_texts: defaults.iter().map(|s| s.to_string()).collect(),
            tags: vec![],
        }
    }

    fn sample_index() -> Index {
        let mut m = HashMap::new();
        for i in [
            info("petpet", &["摸", "摸摸", "rua"], (1, 1), (0, 0), &[]),
            info("pat", &["拍"], (1, 1), (0, 0), &[]),
            info("kiss", &["亲", "亲亲"], (2, 2), (0, 0), &[]),
            info("raise_sign", &["举牌"], (0, 0), (1, 1), &["大佬带带我"]),
            info("petpet_head", &["摸摸头"], (1, 1), (0, 0), &[]),
        ] {
            m.insert(i.key.clone(), i);
        }
        build_index(m)
    }

    #[test]
    fn 关键词匹配_最长优先且要求边界() {
        let idx = sample_index();
        assert_eq!(parse_with(&idx, "", "摸").unwrap().key, "petpet");
        assert_eq!(parse_with(&idx, "", "摸摸头").unwrap().key, "petpet_head", "长关键词优先");
        assert_eq!(parse_with(&idx, "", "RUA").unwrap().key, "petpet", "不区分大小写");
        assert!(parse_with(&idx, "", "摸鱼去了").is_none(), "关键词后必须是空白或结尾");
        assert!(parse_with(&idx, "", "今天摸了").is_none(), "必须以关键词开头");
        let r = parse_with(&idx, "", "举牌 下班了  好耶").unwrap();
        assert_eq!(r.key, "raise_sign");
        assert_eq!(r.texts, vec!["下班了", "好耶"]);
        // 前缀模式
        assert!(parse_with(&idx, "/", "摸").is_none());
        assert_eq!(parse_with(&idx, "/", "/ 摸").unwrap().key, "petpet");
    }

    #[test]
    fn 素材凑图() {
        let one = info("petpet", &["摸"], (1, 1), (0, 0), &[]);
        let two = info("kiss", &["亲"], (2, 2), (0, 0), &[]);
        // 只 @ 一个人：单图表情用对方头像
        let m = Materials { image_urls: vec![], ats: vec![222], sender: 111 };
        assert_eq!(m.pick_images(&one).unwrap(), vec![avatar_url(222)]);
        // 双图表情：自己在前，对方在后
        assert_eq!(m.pick_images(&two).unwrap(), vec![avatar_url(111), avatar_url(222)]);
        // 什么都没给：单图用自己头像
        let me = Materials { image_urls: vec![], ats: vec![], sender: 111 };
        assert_eq!(me.pick_images(&one).unwrap(), vec![avatar_url(111)]);
        // 双图只能凑出自己一张 → 不够
        assert!(me.pick_images(&two).is_none());
        // 附图优先于头像，超出上限截断
        let img = Materials { image_urls: vec!["https://x/a.png".into(), "https://x/b.png".into()], ats: vec![222], sender: 111 };
        assert_eq!(img.pick_images(&one).unwrap(), vec!["https://x/a.png".to_string()]);
        assert_eq!(img.pick_images(&two).unwrap(), vec!["https://x/a.png".to_string(), "https://x/b.png".to_string()]);
    }

    #[test]
    fn 文字参数整理() {
        let sign = info("raise_sign", &["举牌"], (0, 0), (1, 1), &["大佬带带我"]);
        assert_eq!(fit_texts(&sign, &[]).unwrap(), vec!["大佬带带我"], "没给文字用默认");
        assert_eq!(fit_texts(&sign, &["a".into(), "b".into()]).unwrap(), vec!["a"], "多余截断");
        let need2 = info("x", &["两句"], (0, 0), (2, 2), &[]);
        assert!(fit_texts(&need2, &["只有一句".into()]).unwrap_err().contains("需要 2 段文字"));
        let pet = info("petpet", &["摸"], (1, 1), (0, 0), &[]);
        assert!(fit_texts(&pet, &["多余".into()]).unwrap().is_empty(), "不收文字的表情忽略文字");
    }

    /// 需要本机 meme-generator（meme_server\start.bat）：走 bot 的真实代码路径拉列表、解析关键词、生成。
    /// 跑法：cargo test -- --ignored meme_live --nocapture
    #[tokio::test]
    #[ignore]
    async fn meme_live_端到端() {
        let base = std::env::var("QQ_BOT_MEME_API").unwrap_or_else(|_| "http://127.0.0.1:3233".into());
        let c = MemeClient::new(&base, "", 60).unwrap();
        let n = c.refresh().await.expect("连不上 meme-generator");
        println!("表情数量：{n}");
        assert!(n > 100);
        let out = std::env::temp_dir();
        for msg in ["摸", "举牌 端到端测试", "亲"] {
            let req = c.parse(msg).unwrap_or_else(|| panic!("「{msg}」应命中关键词"));
            let info = c.info(&req.key).unwrap();
            let texts = fit_texts(&info, &req.texts).unwrap();
            // 发送者 10001、@ 了 10000（腾讯官方号，头像稳定存在）
            let mats = Materials { image_urls: vec![], ats: if msg == "亲" { vec![10000] } else { vec![] }, sender: 10001 };
            let imgs = mats.pick_images(&info).expect("素材应够");
            let bytes = c.generate(&info.key, &imgs, &texts).await.expect("生成失败");
            let path = out.join(format!("meme_live_{}.bin", info.key));
            std::fs::write(&path, &bytes).unwrap();
            println!("「{msg}」→ {}（{} 张图 / {} 段字）→ {} 字节，{}", info.key, imgs.len(), texts.len(), bytes.len(), path.display());
            assert!(bytes.len() > 1000);
        }
        assert!(c.parse("摸鱼去了").is_none());
        let list = c.render_list().await.expect("表情列表");
        println!("表情列表大图：{} 字节", list.len());
        assert!(c.find("摸").is_some());
    }

    #[test]
    fn info解析() {
        let v = json!({
            "key": "petpet",
            "params_type": {"min_images": 1, "max_images": 1, "min_texts": 0, "max_texts": 0, "default_texts": [], "args_type": null},
            "keywords": ["摸", "摸摸"], "shortcuts": [], "tags": ["动图"]
        });
        let i = MemeInfo::from_json(&v).unwrap();
        assert_eq!((i.min_images, i.max_images, i.max_texts), (1, 1, 0));
        assert_eq!(i.keywords, vec!["摸", "摸摸"]);
        assert_eq!(error_detail(r#"{"detail":"图片数量不符"}"#.as_bytes()), "图片数量不符");
    }
}
