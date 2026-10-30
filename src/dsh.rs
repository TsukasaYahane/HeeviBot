//! DSH 桥接层：把 QQ 消息交给 DeepSeek Harness（headless profile）思考，取回最终回复。
//!
//! 调用方式：每一轮起一个子进程
//! `DeepSeek Harness.exe --expose-internals <cli.js> --profile headless --json [--session-id <id>] -`
//! （等价于 `dsh --profile headless --json …`，绕开 `dsh.cmd` 是为了不经过 cmd.exe 转义），
//! 本轮输入走 **stdin**，不拼进命令行——QQ 消息是任意用户文本，不能进 argv。
//!
//! 输出是逐行 JSON 事件（`session` / `status` / `text` / `final` / `error`），夹杂少量
//! 非 JSON 诊断行（直接忽略）；`step_end` 里的用量累加成本轮 token 数记进日志。
//!
//! **每轮都是无状态的**：不再带 `--session-id` 续 DSH 长会话（长会话每轮要重发全部历史，
//! 实测中位 2.6 万、P90 8.7 万 token）。上下文由记忆账本（mind/ledger.rs）从本地库现场组装，
//! 长度有上限。`<QQ_BOT_LOG_DIR>/dsh_sessions.json` 里旧的会话映射只在重置窗口时归档清理。
//!
//! 并发：同一会话键串行（保证回复和入账顺序），全局再用信号量限制同时运行的子进程数。
//!
//! headless 没有「本轮 system prompt」参数，所以 QQ 侧组装的场景 / 记忆 / 内心上下文以
//! 一个带标记的前置块拼进本轮输入；人格本体由 DSH profile 自己的 persona 提供。

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::config::Config;

pub struct DshBridge {
    /// 可执行文件：`DeepSeek Harness.exe`（配合 cli_js）或任意能接受 dsh 参数的启动器
    exe: PathBuf,
    /// DSH CLI 入口脚本（app.asar 内）；为 None 时直接执行 `exe`
    cli_js: Option<PathBuf>,
    profile: String,
    /// DSH session 的工作目录（agent 的文件操作默认落在这里）
    workdir: PathBuf,
    timeout: Duration,
    sessions: SessionStore,
    /// 会话键 → 串行锁
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    slots: Semaphore,
}

/// 一轮 DSH 调用的结果
struct TurnOutput {
    text: String,
    /// (未命中缓存的输入, 输出, 命中缓存的输入) token，各步相加
    usage: (u64, u64, u64),
}

impl DshBridge {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let workdir = PathBuf::from(&cfg.dsh_workdir);
        std::fs::create_dir_all(&workdir)
            .map_err(|e| anyhow!("创建 DSH 工作目录 {} 失败：{e}", workdir.display()))?;
        // 转绝对路径（不用 canonicalize：Windows 上会带 `\\?\` 前缀）
        let workdir = if workdir.is_absolute() {
            workdir
        } else {
            std::env::current_dir().map(|d| d.join(&workdir)).unwrap_or(workdir)
        };
        let exe = PathBuf::from(&cfg.dsh_exe);
        let cli_js = match &cfg.dsh_cli_js {
            Some(p) => Some(PathBuf::from(p)),
            None => default_cli_js(&exe),
        };
        if cli_js.is_none() && is_electron_exe(&exe) {
            return Err(anyhow!(
                "找不到 DSH CLI 入口（{} 旁边没有 resources\\app.asar\\…\\cli.js），请在 .env 设置 DSH_CLI_JS",
                exe.display()
            ));
        }
        Ok(Self {
            exe,
            cli_js,
            profile: cfg.dsh_profile.clone(),
            workdir,
            timeout: Duration::from_secs(cfg.dsh_timeout_secs),
            sessions: SessionStore::load(PathBuf::from(&cfg.log_dir).join("dsh_sessions.json")),
            locks: Mutex::new(HashMap::new()),
            slots: Semaphore::new(cfg.dsh_max_concurrency.max(1)),
        })
    }

    /// 启动日志用的一行描述
    pub fn describe(&self) -> String {
        format!(
            "profile={}，工作目录={}，超时={}s，已有会话 {} 个",
            self.profile,
            self.workdir.display(),
            self.timeout.as_secs(),
            self.sessions.len()
        )
    }

    /// 归档一个会话窗口（重置时）：把旧版留下的会话映射移除，追加一行到
    /// `<log_dir>/dsh_sessions_archive.jsonl`。DSH 自己的 session 文件不动。没有映射返回 None。
    pub fn archive(&self, conversation: &str) -> Option<String> {
        let sid = self.sessions.get(conversation)?;
        self.sessions.remove(conversation);
        let line = serde_json::json!({
            "conversation": conversation,
            "session_id": sid,
            "reason": "reset",
            "archived_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        let path = self.sessions.path.with_file_name("dsh_sessions_archive.jsonl");
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()));
        if let Err(e) = res {
            warn!("会话归档记录写入失败（{}）：{e}", path.display());
        }
        Some(sid)
    }

    /// 无状态的一轮对话（记忆账本模式）：上下文全部由本地记忆库组装进 `system`，
    /// 不续 DSH 会话——每轮的输入长度有上限，不会像长会话那样越聊越贵。
    /// 同一个窗口仍然串行，保证回复和入账的顺序。
    pub async fn chat_window(&self, conversation: &str, system: &str, user_text: &str) -> Result<String> {
        let lock = self.lock_for(conversation);
        let _serial = lock.lock().await;
        let _slot = self.slots.acquire().await.map_err(|e| anyhow!("DSH 并发槽已关闭：{e}"))?;
        let out = self.run_turn(&compose_input(system, user_text)).await?;
        info!("DSH 用量 [{conversation}]：输入 {}（缓存 {}）/ 输出 {} token", out.usage.0 + out.usage.2, out.usage.2, out.usage.1);
        Ok(out.text)
    }

    /// 一次性任务（不续上下文、不记会话映射），如群日报总结
    pub async fn chat_oneshot(&self, system: &str, user_text: &str) -> Result<String> {
        let _slot = self.slots.acquire().await.map_err(|e| anyhow!("DSH 并发槽已关闭：{e}"))?;
        let out = self.run_turn(&compose_input(system, user_text)).await?;
        info!("DSH 用量 [一次性]：输入 {}（缓存 {}）/ 输出 {} token", out.usage.0 + out.usage.2, out.usage.2, out.usage.1);
        Ok(out.text)
    }

    fn lock_for(&self, conversation: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(conversation.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn run_turn(&self, input: &str) -> Result<TurnOutput> {
        let mut cmd = match &self.cli_js {
            Some(js) => {
                let mut c = tokio::process::Command::new(&self.exe);
                c.arg("--expose-internals").arg(js).env("ELECTRON_RUN_AS_NODE", "1");
                c
            }
            None => tokio::process::Command::new(&self.exe),
        };
        // 让全局 DSH 补丁认出「这是 QQ 机器人」：不挂 agent-bridge 委托工具（见 ~/.dsh/cordis.patch.yml）
        cmd.env("QQBOT_DSH", "1");
        cmd.arg("--profile").arg(&self.profile).arg("--json");
        cmd.arg("-")
            .current_dir(&self.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW：子进程不弹控制台窗口
            cmd.creation_flags(0x0800_0000);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("启动 DSH 失败（{}）：{e}。请检查 DSH_EXE / DSH_CLI_JS", self.exe.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input.as_bytes())
                .await
                .map_err(|e| anyhow!("向 DSH 写入本轮输入失败：{e}"))?;
            // drop 关闭管道 → DSH 读到 EOF 才开始处理
        }
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(r) => r.map_err(|e| anyhow!("等待 DSH 结束失败：{e}"))?,
            // 超时：future 被丢弃时 kill_on_drop 会结束子进程
            Err(_) => return Err(anyhow!("DSH 本轮超时（{}s）", self.timeout.as_secs())),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed = parse_events(&stdout);

        if !output.status.success() || !parsed.errors.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let why = if parsed.errors.is_empty() {
                last_lines(&stderr, 3)
            } else {
                parsed.errors.join("；")
            };
            return Err(anyhow!("DSH 返回错误（退出码 {:?}）：{why}", output.status.code()));
        }
        match parsed.final_text {
            Some(t) if !t.trim().is_empty() => Ok(TurnOutput {
                text: t.trim().to_string(),
                usage: parsed.usage,
            }),
            _ => Err(anyhow!(
                "DSH 没有给出最终回复（结束原因：{}）",
                parsed.end_reason.unwrap_or_else(|| "未知".into())
            )),
        }
    }
}

/// 把场景 / 记忆上下文与用户消息拼成本轮输入
fn compose_input(system: &str, user_text: &str) -> String {
    let system = system.trim();
    if system.is_empty() {
        return user_text.to_string();
    }
    format!(
        "<qq_bridge_context>\n{system}\n</qq_bridge_context>\n\
         （以上是 QQ 桥接层提供的本轮上下文，只用于理解场景，不要复述或提及它。）\n\n{user_text}"
    )
}

#[derive(Debug, Default)]
struct ParsedEvents {
    session_id: Option<String>,
    final_text: Option<String>,
    errors: Vec<String>,
    end_reason: Option<String>,
    usage: (u64, u64, u64),
}

/// 解析 `--json` 输出：逐行 JSON 事件，非 JSON 行跳过
fn parse_events(stdout: &str) -> ParsedEvents {
    let mut out = ParsedEvents::default();
    let mut streamed = String::new();
    for line in stdout.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "session" => out.session_id = s("sessionId"),
            "final" => out.final_text = s("text"),
            "text" => streamed.push_str(&s("text").unwrap_or_default()),
            "error" => out.errors.push(s("message").unwrap_or_else(|| line.to_string())),
            "status" if s("phase").as_deref() == Some("turn_end") => {
                out.end_reason = v.pointer("/reason/kind").and_then(Value::as_str).map(str::to_string);
            }
            "status" if s("phase").as_deref() == Some("step_end") => {
                let n = |k: &str| v.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
                out.usage.0 += n("inputTokens");
                out.usage.1 += n("outputTokens");
                out.usage.2 += n("cacheReadTokens");
            }
            _ => {}
        }
    }
    // 没有 final 事件但有流式文本（异常结束）时，退回用流式文本
    if out.final_text.is_none() && !streamed.trim().is_empty() && out.errors.is_empty() {
        out.final_text = Some(streamed);
    }
    out
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    let joined = lines[start..].join(" | ");
    if joined.is_empty() {
        "（无输出）".to_string()
    } else {
        joined.chars().take(400).collect()
    }
}

fn is_electron_exe(exe: &Path) -> bool {
    exe.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("DeepSeek Harness.exe"))
        .unwrap_or(false)
}

/// `DeepSeek Harness.exe` 旁边的 CLI 入口（与官方 `dsh.cmd` 指向同一个文件）
fn default_cli_js(exe: &Path) -> Option<PathBuf> {
    if !is_electron_exe(exe) {
        return None;
    }
    let p = exe
        .parent()?
        .join("resources")
        .join("app.asar")
        .join("dsh")
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh-desktop-host")
        .join("lib")
        .join("cli.js");
    // app.asar 是归档文件，普通 fs 看不到里面的路径；只校验归档本身存在
    let asar = exe.parent()?.join("resources").join("app.asar");
    asar.exists().then_some(p)
}

/// QQ 会话键 → DSH sessionId（持久化，重启后续上同一个 session）
struct SessionStore {
    path: PathBuf,
    map: Mutex<HashMap<String, String>>,
}

impl SessionStore {
    fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            map: Mutex::new(map),
        }
    }

    fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn get(&self, key: &str) -> Option<String> {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
    }

    #[cfg(test)]
    fn set(&self, key: &str, sid: &str) {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        m.insert(key.to_string(), sid.to_string());
        self.persist(&m);
    }

    fn remove(&self, key: &str) {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if m.remove(key).is_some() {
            self.persist(&m);
        }
    }

    fn persist(&self, m: &HashMap<String, String>) {
        if let Some(p) = self.path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let sorted: std::collections::BTreeMap<_, _> = m.iter().collect();
        match serde_json::to_string_pretty(&sorted) {
            Ok(raw) => {
                let tmp = self.path.with_extension("json.tmp");
                if let Err(e) = std::fs::write(&tmp, raw).and_then(|_| std::fs::rename(&tmp, &self.path)) {
                    warn!("DSH 会话映射写盘失败（{}）：{e}", self.path.display());
                }
            }
            Err(e) => warn!("DSH 会话映射序列化失败：{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析headless事件流() {
        let out = "\u{feff}{\"type\":\"session\",\"sessionId\":\"session-abc\",\"cwd\":\"I:\\\\x\"}\n\
            {\"type\":\"status\",\"phase\":\"turn_start\",\"turn\":1}\n\
            [llm-deepseek-account] model listing loaded 2 model(s)\n\
            {\"type\":\"text\",\"text\":\"收\"}\n\
            {\"type\":\"text\",\"text\":\"到\"}\n\
            {\"type\":\"status\",\"phase\":\"turn_end\",\"turn\":1,\"reason\":{\"kind\":\"completed\"}}\n\
            {\"type\":\"final\",\"text\":\"收到\"}\n";
        let p = parse_events(out);
        assert_eq!(p.session_id.as_deref(), Some("session-abc"));
        assert_eq!(p.final_text.as_deref(), Some("收到"));
        assert_eq!(p.end_reason.as_deref(), Some("completed"));
        assert!(p.errors.is_empty());
    }

    #[test]
    fn 解析错误事件() {
        let p = parse_events("{\"type\":\"error\",\"message\":\"session \\\"x\\\" does not exist\"}\n");
        assert_eq!(p.errors.len(), 1);
        assert!(p.errors[0].contains("does not exist"));
        assert!(p.final_text.is_none());
    }

    #[test]
    fn 累加各步用量() {
        let p = parse_events(
            "{\"type\":\"status\",\"phase\":\"step_end\",\"usage\":{\"inputTokens\":2600,\"outputTokens\":40}}\n\
             {\"type\":\"status\",\"phase\":\"step_end\",\"usage\":{\"inputTokens\":3100,\"outputTokens\":60}}\n\
             {\"type\":\"final\",\"text\":\"好\"}\n",
        );
        assert_eq!(p.usage, (5700, 100, 0));
    }

    #[test]
    fn 没有final时退回流式文本() {
        let p = parse_events("{\"type\":\"text\",\"text\":\"半截\"}\n");
        assert_eq!(p.final_text.as_deref(), Some("半截"));
    }

    #[test]
    fn 上下文块拼接() {
        assert_eq!(compose_input("", "你好"), "你好");
        let s = compose_input("【场景】群聊", "你好");
        assert!(s.starts_with("<qq_bridge_context>\n【场景】群聊\n</qq_bridge_context>"));
        assert!(s.ends_with("\n\n你好"));
    }

    #[test]
    fn 归档移出映射并记录() {
        let dir = std::env::temp_dir().join(format!("dsh_archive_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::from_env();
        cfg.log_dir = dir.to_string_lossy().to_string();
        cfg.dsh_workdir = dir.join("ws").to_string_lossy().to_string();
        let b = DshBridge::from_config(&cfg).expect("构造");
        b.sessions.set("qq-group-1#0", "session-old");
        b.sessions.set("qq-group-2#0", "session-other");
        assert_eq!(b.archive("qq-group-1#0").as_deref(), Some("session-old"));
        assert!(b.archive("qq-group-1#0").is_none(), "已归档的不会重复归档");
        assert!(b.archive("qq-group-9#0").is_none(), "没聊过的窗口没有可归档的");
        assert!(b.sessions.get("qq-group-1#0").is_none(), "归档后不再续用");
        assert_eq!(b.sessions.get("qq-group-2#0").as_deref(), Some("session-other"), "别的群不受影响");
        let log = std::fs::read_to_string(dir.join("dsh_sessions_archive.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 1);
        assert!(log.contains("\"session_id\":\"session-old\"") && log.contains("qq-group-1#0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 会话映射落盘() {
        let dir = std::env::temp_dir().join(format!("dsh_sessions_{}", std::process::id()));
        let path = dir.join("dsh_sessions.json");
        let st = SessionStore::load(path.clone());
        st.set("qq-group-1#0", "session-1");
        st.set("qq-c2c-2#0", "session-2");
        st.remove("qq-c2c-2#0");
        let again = SessionStore::load(path);
        assert_eq!(again.get("qq-group-1#0").as_deref(), Some("session-1"));
        assert_eq!(again.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
