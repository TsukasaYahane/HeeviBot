//! 本地知识库：把 `mind_data/knowledge/*.md` 按标题切块，用户提问时 BM25 检索相关片段注入本轮上下文。
//!
//! - 文档来源：Word 攻略用 `python tools/docx_to_md.py <docx> mind_data/knowledge/<名>.md` 转出来；
//!   目录里改了 / 新增了 .md，下一次检索时自动重载（比对文件修改时间），不用重启 bot。
//! - 切块：每个标题（任意级）到下一个标题之间是一块，块里带「标题路径」（如 `角色养成 > 装备 > 装备养成`），
//!   超长的块按段落再切；只有标题没正文的父级不单独成块。
//! - 检索：复用心智模块的分词（中文单字 + 双字组、英文 / 数字单词），BM25 打分时**只用双字组和单词**——
//!   单字太泛（「的」「是」），会让闲聊也命中；查询里没有双字组时才退回单字。
//! - 命中门槛：至少命中 `min_terms` 个不同的双字组 / 单词，且分数 ≥ `min_score`，避免无关闲聊被塞资料。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;
use tracing::{info, warn};

use crate::mind::memory::tokenize;

/// 单块正文的软上限（字符）：超过就按段落切成多块
const CHUNK_MAX_CHARS: usize = 700;

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// 文档名（文件名去扩展名）
    pub source: String,
    /// 标题路径（`前言 > …`）
    pub path: String,
    /// 正文
    pub text: String,
}

/// 一块的索引数据
struct Indexed {
    chunk: Chunk,
    tf: HashMap<String, u32>,
    len: usize,
}

#[derive(Default)]
struct Inner {
    docs: Vec<Indexed>,
    df: HashMap<String, usize>,
    avg_len: f64,
    /// 各文件修改时间（变化即重载）
    signature: Vec<(PathBuf, Option<SystemTime>)>,
    sources: Vec<String>,
    /// 文档名 → (全文, 文件路径)：小文档命中时整篇注入
    full: HashMap<String, (String, PathBuf)>,
}

pub struct KnowledgeBase {
    dir: PathBuf,
    inner: RwLock<Inner>,
    pub top_k: usize,
    pub min_score: f64,
    pub min_terms: usize,
    pub max_chars: usize,
    /// 不超过这个字数的文档，命中任何一段就整篇注入（片段检索对「新人选什么职业」这类宽泛问题不准）
    pub full_doc_chars: usize,
}

/// 只拿「有区分度」的词：双字组 / 单词（长度 ≥ 2 字符）；没有时退回全部
fn strong_terms(text: &str) -> Vec<String> {
    let all = tokenize(text);
    let strong: Vec<String> = all.iter().filter(|t| t.chars().count() >= 2).cloned().collect();
    if strong.is_empty() {
        all
    } else {
        strong
    }
}

/// 去掉标题开头手打的序号：「4.模组」→「模组」、「5.  20人团本」→「20人团本」、「三、副本」→「副本」。
/// 只认「数字 + `.` / `、`」这种序号形式，标题本身以数字开头（「20人团本」）不受影响。
fn strip_list_number(title: &str) -> &str {
    let t = title.trim();
    let digits = t.chars().take_while(|c| c.is_ascii_digit() || "一二三四五六七八九十".contains(*c)).count();
    if digits == 0 {
        return t;
    }
    let rest: &str = &t[t.char_indices().nth(digits).map_or(t.len(), |(i, _)| i)..];
    match rest.chars().next() {
        Some('.') | Some('、') | Some('．') => rest[rest.chars().next().unwrap().len_utf8()..].trim_start(),
        _ => t,
    }
}

/// 是不是「词：解释」逐行排列的词条表：至少 4 行，且 ≥ 70% 的行在前 20 个字符内有冒号、整行不超过 120 字
fn is_glossary(lines: &[String]) -> bool {
    if lines.len() < 4 {
        return false;
    }
    let entry = lines
        .iter()
        .filter(|l| {
            l.chars().count() <= 120
                && l.chars().take(20).any(|c| c == '：' || c == ':')
        })
        .count();
    entry * 10 >= lines.len() * 7
}

/// 参与打分的文本：自己的小标题算两遍（标题最能说明这块讲什么）+ 正文。
/// 上级标题**不参与打分**（只在注入时作为上下文展示）：否则「生活职业 > 烹饪」这种一句话的小块
/// 会因为父标题里的「职业」被「新人选什么职业」排到第一。
fn index_text(chunk: &Chunk) -> String {
    let own = chunk.path.rsplit(" > ").next().unwrap_or("");
    format!("{own} {own} {}", chunk.text)
}

/// Markdown → 块（按标题切）
pub fn chunk_markdown(source: &str, md: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut buf: Vec<String> = Vec::new();

    let flush = |stack: &Vec<(usize, String)>, buf: &mut Vec<String>, out: &mut Vec<Chunk>| {
        let body: Vec<String> = buf
            .drain(..)
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && l != "[图片]")
            .collect();
        if body.is_empty() {
            return;
        }
        let path = stack.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" > ");
        // 词条式小节（术语库这类「词：解释」逐行列出的）：一行一块，否则单个词条会被整段稀释
        if is_glossary(&body) {
            for line in body {
                out.push(Chunk { source: source.to_string(), path: path.clone(), text: line });
            }
            return;
        }
        // 超长按行聚合切块
        let mut cur = String::new();
        for line in body {
            if !cur.is_empty() && cur.chars().count() + line.chars().count() > CHUNK_MAX_CHARS {
                out.push(Chunk { source: source.to_string(), path: path.clone(), text: std::mem::take(&mut cur) });
            }
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(&line);
        }
        if !cur.is_empty() {
            out.push(Chunk { source: source.to_string(), path, text: cur });
        }
    };

    for line in md.lines() {
        let t = line.trim_start();
        let level = t.chars().take_while(|c| *c == '#').count();
        if level > 0 && t[level..].starts_with(' ') {
            flush(&stack, &mut buf, &mut out);
            let title = strip_list_number(t[level..].trim().trim_end_matches(['：', ':'])).to_string();
            while stack.last().is_some_and(|(l, _)| *l >= level) {
                stack.pop();
            }
            stack.push((level, title));
        } else {
            buf.push(line.to_string());
        }
    }
    flush(&stack, &mut buf, &mut out);
    out
}

impl KnowledgeBase {
    pub fn new(dir: &str, top_k: usize, min_score: f64, min_terms: usize, max_chars: usize) -> Self {
        let kb = Self {
            dir: PathBuf::from(dir),
            inner: RwLock::new(Inner::default()),
            top_k: top_k.max(1),
            min_score,
            min_terms: min_terms.max(1),
            max_chars: max_chars.max(200),
            full_doc_chars: 0,
        };
        kb.reload_if_changed();
        kb
    }

    /// 已加载的文档名与块数（启动日志用）
    pub fn summary(&self) -> (Vec<String>, usize) {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        (g.sources.clone(), g.docs.len())
    }

    fn scan(&self) -> Vec<(PathBuf, Option<SystemTime>)> {
        let mut v: Vec<(PathBuf, Option<SystemTime>)> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md") || x.eq_ignore_ascii_case("txt")))
                    .map(|p| {
                        let m = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                        (p, m)
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// 目录有变化就重建索引
    pub fn reload_if_changed(&self) {
        let sig = self.scan();
        if self.inner.read().unwrap_or_else(|e| e.into_inner()).signature == sig {
            return;
        }
        let mut docs = Vec::new();
        let mut sources = Vec::new();
        let mut full = HashMap::new();
        for (path, _) in &sig {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("资料").to_string();
            match std::fs::read_to_string(path) {
                Ok(md) => {
                    let chunks = chunk_markdown(&name, &md);
                    sources.push(format!("{name}（{} 块）", chunks.len()));
                    full.insert(name.clone(), (md.replace("[图片]\n", "").replace("[图片]", ""), path.clone()));
                    for chunk in chunks {
                        let mut tf = HashMap::new();
                        let terms = strong_terms(&index_text(&chunk));
                        let len = terms.len().max(1);
                        for t in terms {
                            *tf.entry(t).or_insert(0) += 1;
                        }
                        docs.push(Indexed { chunk, tf, len });
                    }
                }
                Err(e) => warn!("知识库文件读取失败（{}）：{e}", path.display()),
            }
        }
        let mut df: HashMap<String, usize> = HashMap::new();
        for d in &docs {
            for t in d.tf.keys() {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
        }
        let avg_len = if docs.is_empty() { 1.0 } else { docs.iter().map(|d| d.len).sum::<usize>() as f64 / docs.len() as f64 };
        if !sources.is_empty() {
            info!("知识库已加载：{}，共 {} 块（{}）", sources.join("、"), docs.len(), self.dir.display());
        }
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = Inner { docs, df, avg_len, signature: sig, sources, full };
    }

    /// BM25 检索：返回 (块, 分数, 命中的强词数)，按分数降序，已过门槛
    pub fn search(&self, query: &str) -> Vec<(Chunk, f64, usize)> {
        self.reload_if_changed();
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if g.docs.is_empty() {
            return Vec::new();
        }
        let q: HashSet<String> = strong_terms(query).into_iter().collect();
        let n = g.docs.len() as f64;
        let (k1, b) = (1.2, 0.75);
        let mut scored: Vec<(usize, f64, usize)> = Vec::new();
        for (i, d) in g.docs.iter().enumerate() {
            let mut score = 0.0;
            let mut hits = 0;
            for t in &q {
                let Some(&f) = d.tf.get(t) else { continue };
                hits += 1;
                let df = *g.df.get(t).unwrap_or(&1) as f64;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let f = f as f64;
                score += idf * f * (k1 + 1.0) / (f + k1 * (1.0 - b + b * d.len as f64 / g.avg_len));
            }
            if hits >= self.min_terms && score >= self.min_score {
                scored.push((i, score, hits));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(self.top_k)
            .map(|(i, s, h)| (g.docs[i].chunk.clone(), s, h))
            .collect()
    }

    /// 设置「整篇注入」的文档字数上限（0 = 关闭，只注入片段）
    pub fn with_full_doc_chars(mut self, n: usize) -> Self {
        self.full_doc_chars = n;
        self
    }

    /// 组装注入本轮上下文的「资料库」块；没有相关内容返回 None。
    /// 命中的文档不超过 `full_doc_chars` 字时整篇给出（模型自己找对应小节），否则只给命中的片段。
    #[cfg(test)]
    pub fn context_block(&self, query: &str) -> Option<String> {
        self.context_block_opts(query, false)
    }

    /// `lite = true`（节能模式）：不整篇注入、最多 2 段片段、总长约 800 字
    pub fn context_block_opts(&self, query: &str, lite: bool) -> Option<String> {
        let mut hits = self.search(query);
        let max_chars = if lite { self.max_chars.min(800) } else { self.max_chars };
        if lite {
            hits.truncate(2);
        }
        if hits.is_empty() {
            return None;
        }
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut body = String::new();
        let mut used: Vec<String> = Vec::new();
        let mut whole: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        for (c, score, _) in &hits {
            if whole.contains(&c.source) {
                continue;
            }
            let doc = g.full.get(&c.source);
            if let Some((p, path)) = doc {
                let f = path.display().to_string();
                if !files.contains(&f) {
                    files.push(f);
                }
                if !lite && self.full_doc_chars > 0 && p.chars().count() <= self.full_doc_chars {
                    body.push_str(&format!("〔{}（全文）〕\n{}\n\n", c.source, p.trim()));
                    whole.push(c.source.clone());
                    used.push(format!("{}（全文，因命中 {}，{score:.1}）", c.source, c.path));
                    continue;
                }
            }
            let piece = format!("〔{}｜{}〕\n{}\n\n", c.source, c.path, c.text);
            if !body.is_empty() && body.chars().count() + piece.chars().count() > max_chars {
                break;
            }
            used.push(format!("{}（{score:.1}）", c.path));
            body.push_str(&piece);
        }
        drop(g);
        info!("📚 知识库命中：{}", used.join("；"));
        let what = if whole.is_empty() { "和这条消息相关的片段" } else { "相关的攻略文档" };
        Some(format!(
            "【资料库】下面是本地攻略资料里{what}。回答相关问题时**优先依据这些资料**，\
用自己的话说，挑和问题有关的部分讲，不用逐字照搬，也不用把整篇复述一遍；资料没写到的可以再查网上，\
但要说清楚哪些来自资料、哪些是你查到或推测的；资料可能过时（作者注明了「未完成」），涉及版本数值时提醒一句以游戏内为准。\
资料原文：{}\n\n{}",
            files.join("、"),
            body.trim_end()
        ))
    }
}

/// 只给测试用：直接从文本建库
#[cfg(test)]
fn kb_from(md: &str) -> KnowledgeBase {
    // 每次调用一个独立目录：测试并行跑，同名目录会被另一个测试删掉
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("kb_test_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("测试攻略.md"), md).unwrap();
    KnowledgeBase::new(dir.to_str().unwrap(), 3, 1.0, 1, 1800)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD: &str = "# 攻略\n\n## 前言：\n\n本文帮助新人。\n\n## 职业选择：\n\n### 转职相关：\n\n转职需要使用勇者的辉芒，在开拓局找米莉亚接任务。\n[图片]\n勇者的辉芒可以在蔷薇宝石商店花费680绑定蔷薇宝石购买。\n\n## 货币体系\n\n#### 绑定蔷薇宝石：\n\n绑定蔷薇宝石可以通过每日委托和活动获取。\n\n### 5.  20人团本：\n\n每周开放，平原 6龙 9龙。\n\n## 术语库：\n\nsdm：斯蒂门遗迹\n3+2：3次每日委托+2次不稳定空间\n";

    #[test]
    fn 按标题切块并带路径() {
        let chunks = chunk_markdown("测试攻略", MD);
        let paths: Vec<&str> = chunks.iter().map(|c| c.path.as_str()).collect();
        assert!(paths.contains(&"攻略 > 职业选择 > 转职相关"), "{paths:?}");
        assert!(paths.contains(&"攻略 > 货币体系 > 绑定蔷薇宝石"), "跳级标题也挂在正确父级下：{paths:?}");
        assert!(paths.contains(&"攻略 > 货币体系 > 20人团本"), "手打序号被去掉：{paths:?}");
        assert!(!paths.contains(&"攻略 > 职业选择"), "只有标题没正文的父级不单独成块");
        let zz = chunks.iter().find(|c| c.path.ends_with("转职相关")).unwrap();
        assert!(!zz.text.contains("[图片]"), "图片占位不进正文");
    }

    #[test]
    fn 检索命中相关块_闲聊不命中() {
        let kb = kb_from(MD);
        let hits = kb.search("怎么转职啊");
        assert!(!hits.is_empty() && hits[0].0.path.ends_with("转职相关"), "{hits:?}");
        let hits = kb.search("sdm是什么");
        assert!(hits[0].0.path.ends_with("术语库"), "英文缩写也能命中：{hits:?}");
        assert!(kb.search("今天天气不错").is_empty(), "无关闲聊不应命中");
        assert!(kb.search("哈哈").is_empty());
        let block = kb.context_block("勇者的辉芒在哪买").unwrap();
        assert!(block.starts_with("【资料库】") && block.contains("680绑定蔷薇宝石"), "{block}");
    }

    /// 用真实攻略调门槛：相关问题要命中、日常闲聊不能命中（没有本地知识库时跳过）
    #[test]
    fn 真实攻略_门槛校准() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mind_data").join("knowledge");
        if !dir.join("星痕共鸣从新人到星怒.md").exists() {
            return;
        }
        let probe = KnowledgeBase::new(dir.to_str().unwrap(), 3, 0.0, 1, 1800);
        let should_hit = [
            ("神盾骑士堆什么属性", "职业流派属性"),
            ("怎么转职", "转职相关"),
            ("蔷薇宝石怎么获得", "蔷薇宝石"),
            ("sdm是什么意思", "术语库"),
            ("每天必做什么", "每日"),
            ("钓鱼怎么玩", "钓鱼"),
            ("20人团本", "20人团本"),
            ("协会币能换什么", "协会"),
        ];
        let should_miss = [
            "今天吃什么",
            "你好呀",
            "发个表情包",
            "上海天气怎么样",
            "DeepSeek 最新版本是多少",
            "我刚下班好累",
            "这个视频好好笑",
        ];
        // 门槛必须和 config.rs 里 QQ_BOT_KB_MIN_SCORE / QQ_BOT_KB_MIN_TERMS 的默认值一致
        let (cfg_score, cfg_terms) = (crate::config::DEFAULT_KB_MIN_SCORE, crate::config::DEFAULT_KB_MIN_TERMS);
        let mut fails = Vec::new();
        for (q, want) in should_hit {
            let hits = probe.search(q);
            let top: Vec<String> = hits.iter().map(|(c, s, h)| format!("{}（{s:.1}，{h}词）", c.path.rsplit(" > ").next().unwrap_or(""))).collect();
            println!("命中？{q} → {}", top.join(" | "));
            // 注入的是前 3 段：目标小节在前 3 段里、且分数过门槛就算命中
            match hits.iter().find(|(c, _, _)| c.path.contains(want)) {
                None => fails.push(format!(
                    "「{q}」前 3 段里没有「{want}」：{:?}",
                    hits.iter().map(|(c, _, _)| c.path.as_str()).collect::<Vec<_>>()
                )),
                Some((_, s, h)) => {
                    if *s < cfg_score || *h < cfg_terms {
                        fails.push(format!("「{q}」{s:.1} 分 / {h} 词，过不了门槛 {cfg_score} / {cfg_terms}"));
                    }
                }
            }
        }
        let kb = KnowledgeBase::new(dir.to_str().unwrap(), 3, cfg_score, cfg_terms, 1800);
        for q in should_miss {
            let raw = probe.search(q);
            let top = raw.first().map(|(c, s, h)| format!("{}（{s:.1}，{h}词）", c.path)).unwrap_or("—".into());
            println!("闲聊 {q} → 无门槛最高：{top}");
            if !kb.search(q).is_empty() {
                fails.push(format!("闲聊「{q}」不应过门槛：{top}"));
            }
        }
        // 宽泛问题：片段排序不准，但只要过门槛，小文档就整篇注入，模型能自己找到对应小节
        let full = KnowledgeBase::new(dir.to_str().unwrap(), 3, cfg_score, cfg_terms, 1800).with_full_doc_chars(12000);
        for (q, must) in [("新人选什么职业好", "职业概览"), ("星痕共鸣新手入门攻略", "前言")] {
            match full.context_block(q) {
                Some(b) if b.contains(must) => println!("宽泛 {q} → 整篇注入（{} 字）", b.chars().count()),
                Some(_) => fails.push(format!("宽泛「{q}」注入内容里没有「{must}」")),
                None => fails.push(format!("宽泛「{q}」没过门槛")),
            }
        }
        // 节能模式：同一个宽泛问题只给片段、不给全文
        let lite = full.context_block_opts("新人选什么职业好", true).unwrap_or_default();
        if lite.contains("（全文）") || lite.chars().count() > 1200 {
            fails.push(format!("节能模式不应整篇注入（{} 字）", lite.chars().count()));
        }
        for q in should_miss {
            if full.context_block(q).is_some() {
                fails.push(format!("闲聊「{q}」在整篇模式下也不应注入"));
            }
        }
        assert!(fails.is_empty(), "
{}", fails.join("
"));
    }

    #[test]
    fn 文件变化自动重载() {
        let kb = kb_from(MD);
        assert!(kb.search("钓鱼技巧").is_empty());
        let dir = &kb.dir;
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(dir.join("新增.md"), "# 生活\n\n## 钓鱼：\n\n钓鱼技巧：选好鱼竿和鱼饵。\n").unwrap();
        // 部分文件系统 mtime 精度不高：新增文件本身就会改变签名
        let hits = kb.search("钓鱼技巧");
        assert!(!hits.is_empty() && hits[0].0.source == "新增", "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
