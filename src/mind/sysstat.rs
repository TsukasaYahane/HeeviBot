//! 进程/系统运行指标：供心智后台展示「已启动时间 / CPU / 内存占用」。
//!
//! 采样器常驻（全局单例）。sysinfo 的 CPU 占用靠两次刷新之间的时间差计算，
//! 所以必须复用同一个 `System` 实例，不能每次请求都新建。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use sysinfo::{Pid, ProcessesToUpdate, System};

static ASSETS: OnceLock<PathBuf> = OnceLock::new();

/// 一次采样的结果（展示用，已换算成人类可读单位）
#[derive(Debug, Clone)]
pub struct SysStat {
    pub pid: u32,
    /// 进程已运行秒数
    pub uptime_secs: u64,
    /// 进程启动的 Unix 时间戳（秒）
    pub started_at: i64,
    /// 本进程 CPU 占用（%，多核可 >100）
    pub cpu_percent: f32,
    /// 本进程常驻内存（MB）
    pub mem_mb: f64,
    /// 系统内存占用（%）
    pub sys_mem_percent: f64,
    pub sys_mem_used_mb: f64,
    pub sys_mem_total_mb: f64,
}

struct Sampler {
    sys: System,
    pid: Pid,
    started_at: i64,
}

impl Sampler {
    fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let mut sys = System::new_all();
        sys.refresh_all();
        // 进程真实启动时刻（sysinfo 给 Unix 秒）。取不到就退回当前时间——
        // 注意不能用采样器初始化时间：WebUI 首次被打开时才会 lazy 初始化，
        // 那样「运行时长」会从第一次访问算起，而不是 bot 启动算起。
        let started_at = sys
            .process(pid)
            .map(|p| p.start_time() as i64)
            .filter(|t| *t > 0)
            .unwrap_or_else(crate::mind::now_ts);
        Self {
            sys,
            pid,
            started_at,
        }
    }

    fn sample(&mut self) -> SysStat {
        self.sys
            .refresh_processes(ProcessesToUpdate::Some(&[self.pid]), true);
        self.sys.refresh_memory();
        let (cpu, mem) = self
            .sys
            .process(self.pid)
            .map(|p| (p.cpu_usage(), p.memory()))
            .unwrap_or((0.0, 0));
        let total = self.sys.total_memory() as f64;
        let used = self.sys.used_memory() as f64;
        SysStat {
            pid: self.pid.as_u32(),
            uptime_secs: crate::mind::now_ts().saturating_sub(self.started_at).max(0) as u64,
            started_at: self.started_at,
            cpu_percent: (cpu * 10.0).round() / 10.0,
            mem_mb: (mem as f64 / 1_048_576.0 * 10.0).round() / 10.0,
            sys_mem_percent: if total > 0.0 {
                (used / total * 1000.0).round() / 10.0
            } else {
                0.0
            },
            sys_mem_used_mb: (used / 1_048_576.0).round(),
            sys_mem_total_mb: (total / 1_048_576.0).round(),
        }
    }
}

fn shared() -> &'static Mutex<Sampler> {
    static S: OnceLock<Mutex<Sampler>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Sampler::new()))
}

/// 采样一次（首次调用会初始化采样器）
pub fn snapshot() -> SysStat {
    let mut s = shared().lock().unwrap_or_else(|e| e.into_inner());
    s.sample()
}

/// 登记背景图目录（`<mind_data>/webui`），由 Mind 启动时调用
pub fn set_assets_dir(dir: PathBuf) {
    let _ = ASSETS.set(dir);
}

/// 背景图目录
pub fn assets_dir() -> PathBuf {
    ASSETS
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("mind_data/webui"))
}

/// 背景图文件名（1..=3）：优先用生图结果 `bgN.png`，没有则回退到人设图派生的 `fallbackN.png`
pub fn bg_candidates(n: u8) -> [String; 2] {
    [format!("bg{n}.png"), format!("fallback{n}.png")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_has_pid_and_memory() {
        let a = snapshot();
        assert_eq!(a.pid, std::process::id());
        assert!(a.mem_mb > 0.0, "本进程内存占用应大于 0: {a:?}");
        assert!(a.sys_mem_total_mb > 0.0, "系统总内存应大于 0: {a:?}");
        // 运行时长必须锚在进程真实启动时刻，而不是首次采样时刻
        let now = crate::mind::now_ts();
        assert!(a.started_at <= now, "started_at 不应在未来: {a:?}");
        let expect = (now - a.started_at).max(0) as u64;
        assert!(
            a.uptime_secs + 2 >= expect && a.uptime_secs <= expect + 2,
            "uptime_secs({}) 与 started_at 不一致: {a:?}",
            a.uptime_secs
        );
        // 第二次采样也应正常（CPU 差值路径）
        let b = snapshot();
        assert_eq!(b.pid, a.pid);
        assert!(b.uptime_secs >= a.uptime_secs);
    }

    #[test]
    fn bg_candidates_prefer_generated() {
        assert_eq!(bg_candidates(1), ["bg1.png".to_string(), "fallback1.png".to_string()]);
    }
}
