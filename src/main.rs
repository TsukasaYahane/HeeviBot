//! qqbot-hermes：QQ（LLBot / OneBot 11）↔ DeepSeek Harness（DSH）桥接框架
//!
//! 启动方式：在工程根目录（含 .env）执行 `cargo run --release`；
//! 连上 LLBot 的正向 WebSocket（默认 ws://127.0.0.1:3001）收群聊 / 私聊消息，
//! 交给 DSH headless profile 思考，再把回复经 LLBot 发回 QQ。

mod bili;
mod chat_split;
mod codex;
mod config;
mod digest;
mod dsh;
mod esports;
mod handler;
mod knowledge;
mod leagues;
mod memes;
mod mindcard;
mod mind;
mod mode;
mod moderation;
mod onebot;
mod png8;
mod render;
mod social;
mod stickers;
mod vision;
mod zxcard;
mod zxreport;

use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use config::Config;
use dsh::DshBridge;
use handler::Bot;
use onebot::OneBot;

#[tokio::main]
async fn main() -> Result<()> {
    // 1) 加载 .env（从当前工作目录读取；在工程根目录启动即可）
    if dotenvy::dotenv().is_err() {
        // .env 不存在不致命：也可直接使用系统环境变量
    }

    // 2) 配置（先于日志初始化，日志目录来自配置）
    let cfg = Config::from_env();

    // 3) 日志：控制台按级别着色（ERROR 红 / WARN 黄 / INFO(成功) 绿 / DEBUG 蓝），
    //    同时按日轮转写入文件（<QQ_BOT_LOG_DIR>/qqbot-hermes.YYYY-MM-DD.log）。
    //    级别过滤用 RUST_LOG 覆盖（如 RUST_LOG=qqbot_hermes=debug）
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("qqbot_hermes=info"));

    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("qqbot-hermes")
        .filename_suffix("log")
        .build(&cfg.log_dir)
        .map_err(|e| anyhow::anyhow!("创建日志目录 {} 失败: {e}", cfg.log_dir))?;
    let (file_writer, _file_guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        // 过滤器作为最上层：其下的两个 fmt 层都只收到允许的事件
        .with(filter)
        // 文件层：纯文本（无 ANSI 颜色），每日轮转
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer),
        )
        // 控制台层：按级别着色
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(std::io::stdout),
        )
        .init();

    info!(
        "qqbot-hermes {} 启动：OneBot = {}，DSH profile = {}，日志目录 = {}",
        env!("CARGO_PKG_VERSION"),
        cfg.onebot_ws_url,
        cfg.dsh_profile,
        cfg.log_dir
    );

    // 4) 单实例守护：同一个 LLBot 上挂两个 bot 进程会对同一条消息重复回复。
    //    锁文件放系统临时目录（绝对路径，与启动目录无关）——放工程目录的话，
    //    从别的目录再启一个实例会绕过检查。
    let lock_file = std::env::temp_dir().join("qqbot-hermes-bot.pid");
    acquire_singleton(&lock_file)?;

    // 5) DSH 桥
    let dsh = Arc::new(DshBridge::from_config(&cfg)?);
    info!("DSH 桥已就绪：{}", dsh.describe());

    // 6) LLBot（OneBot 11 正向 WS）：后台连接 + 自动重连
    let ob = OneBot::new(cfg.onebot_ws_url.clone(), cfg.onebot_access_token.clone());
    let mut events = ob.start();

    // 7) 事件处理器 + 后台任务（日报 / 战报轮询 / 早报）
    let bot = Arc::new(Bot::new(dsh, ob, Arc::new(cfg.clone())));
    bot.start_background();
    tokio::spawn(bot.clone().social_loop());

    // 8) 事件循环：每个事件独立 spawn，DSH 慢不会卡住后续消息
    while let Some(ev) = events.recv().await {
        let bot = bot.clone();
        tokio::spawn(async move {
            bot.on_event(ev).await;
        });
    }
    error!("OneBot 事件通道已关闭（不应发生），进程退出");
    Ok(())
}

/// 单实例守护（Windows）：
/// 若锁文件中记录的进程还活着，则拒绝启动；否则把自身 PID 写入该文件。
/// 非 Windows 平台跳过（尽力而为，不阻断）。
fn acquire_singleton(pid_file: &std::path::Path) -> Result<()> {
    if !cfg!(target_os = "windows") {
        return Ok(());
    }
    if let Ok(raw) = std::fs::read_to_string(pid_file) {
        if let Ok(existing) = raw.trim().parse::<u32>() {
            if existing != std::process::id() && process_alive(existing) {
                return Err(anyhow::anyhow!(
                    "检测到已有 bot 实例在运行（PID {existing}）。\
                     两个实例连同一个 LLBot 会对每条消息重复回复，请先关闭旧实例再启动（锁文件: {}）。",
                    pid_file.display()
                ));
            }
            warn!("发现过期 PID 文件（{existing} 已不存在），忽略");
        }
    }
    if let Some(parent) = pid_file.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::write(pid_file, std::process::id().to_string());
    Ok(())
}

#[cfg(target_os = "windows")]
fn process_alive(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match out {
        Ok(o) => {
            // 不光看 PID 在不在：Windows 会复用 PID（实测旧 bot 的 PID 被 chrome.exe 接手，导致误判「已有实例」起不来），
            // 还要求那个进程的映像名就是本程序
            let s = String::from_utf8_lossy(&o.stdout).to_lowercase();
            let me = std::env::current_exe()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
                .unwrap_or_else(|| "qqbot-hermes.exe".to_string());
            s.contains(&format!(" {pid} ")) && s.contains(&me)
        }
        Err(_) => false,
    }
}

#[cfg(not(target_os = "windows"))]
fn process_alive(_pid: u32) -> bool {
    // 非 Windows 暂不强制单实例（跳过守护，不阻断启动）
    false
}
