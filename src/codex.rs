//! ChatGPT / Codex 通路：让主人在 QQ 里查看 Codex 的实时额度和工程进度。
//!
//! 走的是本机官方 Codex 客户端自带的 `codex app-server`（stdio JSON-RPC，Codex 自己的 /status 也用它），
//! 只调三个只读方法：
//! - `account/rateLimits/read`：5 小时窗口 / 每周窗口的已用百分比、重置时间、套餐、是否已用完；
//! - `thread/list`：最近的 Codex 任务（标题、工作目录、更新时间、运行状态）；
//! - `thread/goal/get`：任务挂的长期目标（进行中 / 暂停 / 额度不足 / 完成、已用 token、已用时长）。
//!
//! 每次查询临时起一个 app-server 子进程，查完就关；bot 不读、不存任何 ChatGPT 凭据（登录态由 Codex 自己管）。
//! 只有超级用户能查；工程进度含项目名和路径，群里只给概要，详情私聊看。

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const TIMEOUT: Duration = Duration::from_secs(25);

/// 查询指令
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    Quota,
    Progress,
    Both,
}

/// 「codex额度」「/codex 进度」「chatgpt 用量」「codex」→ 指令；别的话返回 None。
/// 必须以 codex / chatgpt / gpt 开头，后面要么为空、要么是额度 / 进度类关键词，避免把「codex 是什么」当指令。
pub fn parse_cmd(text: &str) -> Option<Query> {
    let t = text.trim().trim_start_matches(['/', '／', '#']).trim().to_lowercase();
    let rest = ["chatgpt", "codex", "gpt"].iter().find_map(|p| t.strip_prefix(p))?;
    let rest = rest.trim().trim_start_matches(['的', ' ']).trim();
    if rest.is_empty() || ["状态", "status"].contains(&rest) {
        return Some(Query::Both);
    }
    if ["额度", "用量", "限额", "余量", "quota", "usage", "limit", "limits"].contains(&rest) {
        return Some(Query::Quota);
    }
    if ["进度", "工程", "工程进度", "项目", "任务", "progress", "tasks"].contains(&rest) {
        return Some(Query::Progress);
    }
    None
}

/// 找 codex 可执行文件：CODEX_BIN → 官方桌面版自带（%LOCALAPPDATA%\OpenAI\Codex\bin\<版本>\codex.exe，取最新）→ PATH 里的 codex
pub fn locate(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| !p.trim().is_empty()) {
        return Some(PathBuf::from(p));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let bin = PathBuf::from(local).join("OpenAI").join("Codex").join("bin");
        let newest = std::fs::read_dir(&bin)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("codex.exe"))
            .filter(|p| p.is_file())
            .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
        if newest.is_some() {
            return newest;
        }
    }
    Some(PathBuf::from("codex"))
}

/// 正在运行的 codex.exe 的路径（有多个就取最晚启动的）
fn running_codex() -> Option<PathBuf> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::Always));
    sys.processes()
        .values()
        .filter(|p| p.name().eq_ignore_ascii_case("codex.exe") || p.name().eq_ignore_ascii_case("codex"))
        .filter_map(|p| Some((p.start_time(), p.exe()?.to_path_buf())))
        .filter(|(_, exe)| exe.is_file())
        .max_by_key(|(t, _)| *t)
        .map(|(_, exe)| exe)
}

/// 一次性 app-server 会话（查完即关）
struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Session {
    async fn open(bin: &PathBuf) -> Result<Self> {
        let mut cmd = Command::new(bin);
        cmd.arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：不闪黑框
        let mut child = cmd.spawn().with_context(|| format!("启动 Codex 失败（{}）", bin.display()))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("拿不到 Codex stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("拿不到 Codex stdout"))?;
        let mut s = Self { child, stdin, lines: BufReader::new(stdout).lines(), next_id: 1 };
        s.call("initialize", json!({ "clientInfo": { "name": "qqbot-hermes", "version": env!("CARGO_PKG_VERSION") } }))
            .await?;
        s.send(json!({ "jsonrpc": "2.0", "method": "initialized" })).await?;
        Ok(s)
    }

    async fn send(&mut self, v: Value) -> Result<()> {
        self.stdin.write_all(format!("{v}\n").as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// 发请求、等同 id 的响应（中间的通知一律跳过）
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
        loop {
            let line = self.lines.next_line().await?.ok_or_else(|| anyhow!("Codex 提前退出了"))?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                bail!("Codex {method} 出错：{}", e.get("message").and_then(Value::as_str).unwrap_or("未知错误"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn close(mut self) {
        let _ = self.child.kill().await;
    }
}

/// 查到的原始数据
pub struct Snapshot {
    pub limits: Option<Value>,
    /// (thread, goal)
    pub threads: Vec<(Value, Option<Value>)>,
}

pub struct Codex {
    bin: std::sync::Mutex<PathBuf>,
    /// CODEX_BIN（没配就自动找）
    configured: Option<String>,
}

impl Codex {
    pub fn new(bin: PathBuf, configured: Option<String>) -> Self {
        Self { bin: std::sync::Mutex::new(bin), configured }
    }

    pub fn describe(&self) -> String {
        self.bin.lock().unwrap_or_else(|e| e.into_inner()).display().to_string()
    }

    /// 这次查询用哪个 codex：配了 CODEX_BIN 就用它；否则优先用正在运行的那个（手动重启 / 自动更新后
    /// 版本目录会变），没在运行再退回启动时找到的、或重新扫一遍安装目录
    fn current_bin(&self) -> PathBuf {
        let mut bin = self.bin.lock().unwrap_or_else(|e| e.into_inner());
        if self.configured.as_deref().is_none_or(|p| p.trim().is_empty()) {
            if let Some(found) = running_codex() {
                if found != *bin {
                    tracing::info!("Codex：换用正在运行的 {}（原 {}）", found.display(), bin.display());
                    *bin = found;
                }
                return bin.clone();
            }
        }
        if bin.is_absolute() && !bin.is_file() {
            if let Some(found) = locate(self.configured.as_deref()) {
                tracing::info!("Codex 已更新，换用 {}（原 {} 已不存在）", found.display(), bin.display());
                *bin = found;
            }
        }
        bin.clone()
    }

    pub async fn fetch(&self, q: Query, threads: usize) -> Result<Snapshot> {
        tokio::time::timeout(TIMEOUT, self.fetch_inner(q, threads))
            .await
            .map_err(|_| anyhow!("Codex 没在 {} 秒内响应", TIMEOUT.as_secs()))?
    }

    async fn fetch_inner(&self, q: Query, n: usize) -> Result<Snapshot> {
        let mut s = Session::open(&self.current_bin()).await?;
        let res = async {
            let limits = if q != Query::Progress { Some(s.call("account/rateLimits/read", Value::Null).await?) } else { None };
            let mut threads = Vec::new();
            if q != Query::Quota {
                let list = s.call("thread/list", json!({ "limit": n })).await?;
                for t in list.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let goal = match t.get("id").and_then(Value::as_str) {
                        Some(id) => s
                            .call("thread/goal/get", json!({ "threadId": id }))
                            .await
                            .ok()
                            .and_then(|g| g.get("goal").cloned())
                            .filter(|g| !g.is_null()),
                        None => None,
                    };
                    threads.push((t, goal));
                }
            }
            Ok::<_, anyhow::Error>(Snapshot { limits, threads })
        }
        .await;
        s.close().await;
        res
    }
}

/* ---------------- 渲染（纯文本，QQ 不渲染 Markdown） ---------------- */

fn bar(pct: i64, w: usize) -> String {
    let n = ((pct.clamp(0, 100) as f64 / 100.0) * w as f64).round() as usize;
    format!("[{}{}]", "#".repeat(n), ".".repeat(w - n))
}

fn when(ts: i64, now: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(t) = Local.timestamp_opt(ts, 0).single() else { return "?".into() };
    let today = Local.timestamp_opt(now, 0).single().map(|n| n.date_naive());
    if Some(t.date_naive()) == today {
        format!("今天 {}", t.format("%H:%M"))
    } else if Some(t.date_naive()) == today.and_then(|d| d.succ_opt()) {
        format!("明天 {}", t.format("%H:%M"))
    } else {
        t.format("%m/%d %H:%M").to_string()
    }
}

fn ago(ts: i64, now: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=59 => "刚刚".into(),
        60..=3599 => format!("{} 分钟前", d / 60),
        3600..=86399 => format!("{} 小时前", d / 3600),
        _ => format!("{} 天前", d / 86400),
    }
}

fn window_name(mins: Option<i64>) -> String {
    match mins {
        Some(300) => "5 小时".into(),
        Some(10080) => "本周".into(),
        Some(m) if m % 1440 == 0 => format!("{} 天", m / 1440),
        Some(m) if m % 60 == 0 => format!("{} 小时", m / 60),
        Some(m) => format!("{m} 分钟"),
        None => "额度".into(),
    }
}

fn plan_name(p: &str) -> String {
    match p {
        "plus" => "Plus".into(),
        "pro" => "Pro".into(),
        "team" => "Team".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        "free" => "Free".into(),
        other => other.to_string(),
    }
}

pub fn render_quota(v: &Value, now: i64) -> String {
    let rl = v.get("rateLimits").unwrap_or(&Value::Null);
    let plan = rl.get("planType").and_then(Value::as_str).map(plan_name).unwrap_or_else(|| "?".into());
    let mut out = format!("Codex 额度（ChatGPT {plan}）");
    let mut reset_at: Option<i64> = None;
    for key in ["primary", "secondary"] {
        let Some(w) = rl.get(key).filter(|w| !w.is_null()) else { continue };
        let pct = w.get("usedPercent").and_then(Value::as_i64).unwrap_or(0);
        let name = window_name(w.get("windowDurationMins").and_then(Value::as_i64));
        let reset = w.get("resetsAt").and_then(Value::as_i64);
        // 和 Codex 客户端一致：条形图和主数字都是「剩余」，已用的放括号里
        let left = (100 - pct).clamp(0, 100);
        out.push_str(&format!("\n{:<6}{} 剩 {:>3}%（已用 {pct}%）", name, bar(left, 10), left));
        if let Some(r) = reset {
            out.push_str(&format!("  {} 重置", when(r, now)));
            if pct >= 100 {
                reset_at = Some(reset_at.map_or(r, |x: i64| x.max(r)));
            }
        }
    }
    let credits = rl.get("credits").unwrap_or(&Value::Null);
    if credits.get("unlimited").and_then(Value::as_bool) == Some(true) {
        out.push_str("\n点数：不限");
    } else if credits.get("hasCredits").and_then(Value::as_bool) == Some(true) {
        out.push_str(&format!("\n点数余额：{}", credits.get("balance").and_then(Value::as_str).unwrap_or("?")));
    }
    let reached = rl.get("rateLimitReachedType").and_then(Value::as_str).is_some()
        || v.get("ordinaryUsageAllowed").and_then(Value::as_bool) == Some(false);
    out.push_str(&match (reached, reset_at) {
        (true, Some(r)) => format!("\n状态：额度用完了，{} 后恢复", when(r, now)),
        (true, None) => "\n状态：额度用完了".into(),
        (false, _) => "\n状态：可以用".into(),
    });
    out
}

fn goal_status(s: &str) -> &'static str {
    match s {
        "active" => "进行中",
        "paused" => "已暂停",
        "blocked" => "卡住了",
        "usageLimited" => "额度不足暂停",
        "budgetLimited" => "预算用完",
        "complete" => "已完成",
        _ => "未知",
    }
}

fn thread_state(t: &Value) -> &'static str {
    match t.pointer("/status/type").and_then(Value::as_str) {
        Some("active") => "运行中",
        Some("systemError") => "出错",
        _ => "",
    }
}

fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n {
        s
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// 长路径保留结尾（项目文件夹名比盘符有用）
fn clip_left(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= n {
        s.to_string()
    } else {
        "…".to_string() + &c[c.len() - n..].iter().collect::<String>()
    }
}

fn fmt_tokens(n: i64) -> String {
    if n >= 10_000 {
        format!("{:.0} 万", n as f64 / 10_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_dur(secs: i64) -> String {
    let h = secs as f64 / 3600.0;
    if h >= 1.0 {
        format!("{h:.1} 小时")
    } else {
        format!("{} 分钟", (secs / 60).max(1))
    }
}

/// 工程进度。`detail = false`（群里）只给概要，不露项目名和路径
pub fn render_progress(threads: &[(Value, Option<Value>)], now: i64, detail: bool) -> String {
    if threads.is_empty() {
        return "Codex 工程进度：最近没有任务。".into();
    }
    if !detail {
        let running = threads.iter().filter(|(t, _)| thread_state(t) == "运行中").count();
        let goals: Vec<&str> = threads
            .iter()
            .filter_map(|(_, g)| g.as_ref()?.get("status")?.as_str())
            .collect();
        let active = goals.iter().filter(|s| **s == "active").count();
        let limited = goals.iter().filter(|s| **s == "usageLimited" || **s == "budgetLimited").count();
        let done = goals.iter().filter(|s| **s == "complete").count();
        return format!(
            "Codex 工程进度：最近 {} 个任务，{running} 个正在跑；目标 {active} 个进行中、{limited} 个因额度暂停、{done} 个已完成。\n项目详情私聊我发「codex进度」看。",
            threads.len()
        );
    }
    let mut out = format!("Codex 工程进度（最近 {} 个）", threads.len());
    for (i, (t, goal)) in threads.iter().enumerate() {
        let title = t
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| t.get("preview").and_then(Value::as_str))
            .unwrap_or("（无标题）");
        let state = thread_state(t);
        out.push_str(&format!("\n\n{}. {}{}", i + 1, clip(title, 28), if state.is_empty() { String::new() } else { format!(" [{state}]") }));
        let cwd = t.get("cwd").and_then(Value::as_str).unwrap_or("");
        let upd = t.get("updatedAt").and_then(Value::as_i64).map(|ts| ago(ts, now)).unwrap_or_default();
        out.push_str(&format!("\n   {} · {}", clip_left(cwd, 36), upd));
        if let Some(g) = goal {
            let st = goal_status(g.get("status").and_then(Value::as_str).unwrap_or(""));
            let obj = clip(g.get("objective").and_then(Value::as_str).unwrap_or(""), 30);
            let tokens = g.get("tokensUsed").and_then(Value::as_i64).unwrap_or(0);
            let secs = g.get("timeUsedSeconds").and_then(Value::as_i64).unwrap_or(0);
            let budget = g
                .get("tokenBudget")
                .and_then(Value::as_i64)
                .filter(|b| *b > 0)
                .map(|b| format!(" {}", bar(tokens * 100 / b, 8)))
                .unwrap_or_default();
            out.push_str(&format!("\n   目标：{obj}\n   {st} · 已用 {} token{budget} · {}", fmt_tokens(tokens), fmt_dur(secs)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 指令解析() {
        assert_eq!(parse_cmd("codex额度"), Some(Query::Quota));
        assert_eq!(parse_cmd("/Codex 用量"), Some(Query::Quota));
        assert_eq!(parse_cmd("chatgpt额度"), Some(Query::Quota));
        assert_eq!(parse_cmd("codex进度"), Some(Query::Progress));
        assert_eq!(parse_cmd("codex 工程进度"), Some(Query::Progress));
        assert_eq!(parse_cmd("/codex"), Some(Query::Both));
        assert_eq!(parse_cmd("codex是什么"), None, "普通提问不当指令");
        assert_eq!(parse_cmd("gpt5好用吗"), None);
        assert_eq!(parse_cmd("今天 codex 额度"), None, "必须以 codex 开头");
    }

    fn sample_limits() -> Value {
        json!({"ordinaryUsageAllowed": false, "rateLimits": {"planType": "plus",
            "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1_000_3600},
            "secondary": {"usedPercent": 16, "windowDurationMins": 10080, "resetsAt": 1_060_0000},
            "credits": {"hasCredits": false, "unlimited": false, "balance": "0"},
            "rateLimitReachedType": "rate_limit_reached"}})
    }

    #[test]
    fn 额度渲染() {
        let s = render_quota(&sample_limits(), 1_000_0000);
        assert!(s.contains("ChatGPT Plus"), "{s}");
        assert!(s.contains("5 小时") && s.contains("[..........] 剩   0%（已用 100%）"), "用完 = 空条：{s}");
        assert!(s.contains("本周") && s.contains("[########..] 剩  84%（已用 16%）"), "{s}");
        assert!(s.contains("额度用完了") && s.contains("后恢复"), "{s}");
        assert!(!s.contains("点数余额"), "没点数就不显示：{s}");
    }

    #[test]
    fn 进度渲染_私聊详情_群里概要() {
        let threads = vec![
            (
                json!({"name": "确认 R1 是否完成", "cwd": "D:\\demo-project", "updatedAt": 1_000_0000 - 7200, "status": {"type": "notLoaded"}}),
                Some(json!({"objective": "可发布设计，直到满足 R2 标准", "status": "usageLimited", "tokensUsed": 19_671_691, "timeUsedSeconds": 201_315})),
            ),
            (json!({"name": null, "preview": "推进剩余任务", "cwd": "D:\\proj", "updatedAt": 1_000_0000 - 90, "status": {"type": "active"}}), None),
        ];
        let d = render_progress(&threads, 1_000_0000, true);
        assert!(d.contains("1. 确认 R1 是否完成") && d.contains("D:\\demo-project · 2 小时前"), "{d}");
        assert!(d.contains("额度不足暂停 · 已用 1967 万 token · 55.9 小时"), "{d}");
        assert!(d.contains("2. 推进剩余任务 [运行中]"), "没标题用 preview：{d}");
        let g = render_progress(&threads, 1_000_0000, false);
        assert!(!g.contains("Rust-base-browser") && !g.contains("R1"), "群里不露项目：{g}");
        assert!(g.contains("1 个正在跑") && g.contains("1 个因额度暂停"), "{g}");
    }

    /// 实机：连本机 Codex 查一次（`cargo test codex_live -- --ignored --nocapture`）
    #[test]
    #[ignore]
    fn running_codex_found() {
        println!("{:?}", running_codex());
    }

    #[tokio::test]
    #[ignore]
    async fn codex_live() {
        let c = Codex::new(locate(None).unwrap(), None);
        let snap = c.fetch(Query::Both, 5).await.expect("fetch");
        let now = crate::mind::now_ts();
        println!("{}

{}

{}", render_quota(snap.limits.as_ref().unwrap(), now), render_progress(&snap.threads, now, true), render_progress(&snap.threads, now, false));
    }
}
