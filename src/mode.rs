//! 运行模式：正常 / 节能 / 静默（全局，超级用户用「模式 xx」切换，落盘到 `<QQ_BOT_LOG_DIR>/mode.txt`，重启保持）。
//!
//! | | 正常 | 节能 | 静默 |
//! |---|---|---|---|
//! | 被 @ / 私聊时 AI 回复 | ✅ | ✅ 更短（1~2 句、最多 2 条、少调工具） | ❌ |
//! | 注入的上下文 | 完整 | 精简（记忆截短、知识库只给 2 段片段） | — |
//! | 群里主动插话 | ✅ | ❌ | ❌ |
//! | 识图 | 全部 | 只看 @ / 私聊里带的图 | ❌ |
//! | 群日报（要调 AI） | ✅ | ❌ | ❌ |
//! | 不调 AI 的插件（表情包 / B站解析 / 梗图 / 指令） | ✅ | ✅ | ✅ |

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Eco,
    Silent,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Normal => "正常",
            Mode::Eco => "节能",
            Mode::Silent => "静默",
        }
    }

    /// 一句话说明（切换确认 / 查询时用）
    pub fn summary(self) -> &'static str {
        match self {
            Mode::Normal => "所有功能都开着：聊天、识图、主动插话、群日报。",
            Mode::Eco => "只回 @ 和私聊，回得更短；不主动插话，不看群里没 @ 我的图，群日报暂停。",
            Mode::Silent => "不调用 AI，不聊天；只保留表情包、B站解析、梗图和各类指令。",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_lowercase().as_str() {
            "正常" | "普通" | "全开" | "normal" | "full" => Some(Mode::Normal),
            "节能" | "省电" | "省流" | "eco" | "lite" => Some(Mode::Eco),
            "静默" | "安静" | "勿扰" | "silent" | "quiet" => Some(Mode::Silent),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Mode::Normal => 0,
            Mode::Eco => 1,
            Mode::Silent => 2,
        }
    }

    fn from_u8(v: u8) -> Mode {
        match v {
            1 => Mode::Eco,
            2 => Mode::Silent,
            _ => Mode::Normal,
        }
    }

    /// 被 @ / 私聊时是否调用 AI 回复
    pub fn ai_reply(self) -> bool {
        self != Mode::Silent
    }

    /// 是否主动插话
    pub fn chime(self) -> bool {
        self == Mode::Normal
    }

    /// 是否识别这条消息里的图（`directed` = 被 @ 或私聊）
    pub fn vision(self, directed: bool) -> bool {
        match self {
            Mode::Normal => true,
            Mode::Eco => directed,
            Mode::Silent => false,
        }
    }

    /// 是否生成群日报（要调 AI）
    pub fn digest(self) -> bool {
        self == Mode::Normal
    }
}

/// 模式状态（全局一份，原子读写；切换时落盘）
pub struct ModeState {
    cur: AtomicU8,
    path: PathBuf,
}

impl ModeState {
    /// 读落盘的模式；没有就用 `.env` 的默认值
    pub fn load(log_dir: &str, default: Mode) -> Self {
        let path = PathBuf::from(log_dir).join("mode.txt");
        let m = std::fs::read_to_string(&path).ok().and_then(|s| Mode::parse(&s)).unwrap_or(default);
        info!("运行模式：{}（{}）", m.name(), m.summary());
        Self { cur: AtomicU8::new(m.to_u8()), path }
    }

    pub fn get(&self) -> Mode {
        Mode::from_u8(self.cur.load(Ordering::Relaxed))
    }

    /// 切换并落盘，返回旧模式
    pub fn set(&self, m: Mode) -> Mode {
        let old = Mode::from_u8(self.cur.swap(m.to_u8(), Ordering::Relaxed));
        if let Some(p) = self.path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if let Err(e) = std::fs::write(&self.path, m.name()) {
            warn!("运行模式落盘失败（{}）：{e}", self.path.display());
        }
        info!("运行模式切换：{} → {}", old.name(), m.name());
        old
    }
}

/// 解析模式指令：`模式` / `#模式` / `当前模式`（查询，返回 Ok(None)）；`模式 节能` / `切换模式 静默` / `节能模式`（切换）。
/// 不是模式指令返回 None；是指令但模式名不认识返回 Some(Err(原文))。
pub fn parse_cmd(text: &str) -> Option<Result<Option<Mode>, String>> {
    let t = text.trim().trim_start_matches(['#', '/', '!', '！']).trim();
    if matches!(t, "模式" | "当前模式" | "运行模式" | "查看模式") {
        return Some(Ok(None));
    }
    for head in ["切换模式", "运行模式", "模式切换", "模式"] {
        if let Some(after) = t.strip_prefix(head) {
            if after.trim().is_empty() {
                return Some(Ok(None));
            }
            // 「模式」后面得是分隔符（空格 / 冒号 / 为 / 到）或直接就是模式名；否则是普通聊天（「模式化地回答」）
            if after.starts_with([' ', ':', '：', '为', '到']) {
                let rest = after.trim_start_matches([' ', ':', '：', '为', '到']).trim();
                return Some(Mode::parse(rest).map(Some).ok_or_else(|| rest.to_string()));
            }
            return Mode::parse(after).map(|m| Ok(Some(m)));
        }
    }
    // 「节能模式」「静默模式」「正常模式」
    if let Some(name) = t.strip_suffix("模式") {
        if let Some(m) = Mode::parse(name) {
            return Some(Ok(Some(m)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 指令解析() {
        assert_eq!(parse_cmd("模式"), Some(Ok(None)));
        assert_eq!(parse_cmd("#模式"), Some(Ok(None)));
        assert_eq!(parse_cmd("模式 节能"), Some(Ok(Some(Mode::Eco))));
        assert_eq!(parse_cmd("切换模式：静默"), Some(Ok(Some(Mode::Silent))));
        assert_eq!(parse_cmd("模式切换为正常"), Some(Ok(Some(Mode::Normal))));
        assert_eq!(parse_cmd("节能模式"), Some(Ok(Some(Mode::Eco))));
        assert_eq!(parse_cmd("模式 eco"), Some(Ok(Some(Mode::Eco))));
        assert_eq!(parse_cmd("模式 飞行"), Some(Err("飞行".to_string())));
        assert_eq!(parse_cmd("今天什么模式的天气"), None, "普通聊天不拦截");
        assert_eq!(parse_cmd("游戏模式怎么开"), None);
        assert_eq!(parse_cmd("模式化地回答我"), None, "模式后面不是分隔符也不是模式名：不拦截");
        assert_eq!(parse_cmd("模式静默"), Some(Ok(Some(Mode::Silent))));
    }

    #[test]
    fn 各模式开关() {
        assert!(Mode::Normal.ai_reply() && Mode::Normal.chime() && Mode::Normal.vision(false) && Mode::Normal.digest());
        assert!(Mode::Eco.ai_reply() && !Mode::Eco.chime() && Mode::Eco.vision(true) && !Mode::Eco.vision(false) && !Mode::Eco.digest());
        assert!(!Mode::Silent.ai_reply() && !Mode::Silent.chime() && !Mode::Silent.vision(true) && !Mode::Silent.digest());
    }

    #[test]
    fn 落盘与读回() {
        let dir = std::env::temp_dir().join(format!("mode_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = ModeState::load(dir.to_str().unwrap(), Mode::Normal);
        assert_eq!(st.get(), Mode::Normal);
        assert_eq!(st.set(Mode::Silent), Mode::Normal);
        let again = ModeState::load(dir.to_str().unwrap(), Mode::Normal);
        assert_eq!(again.get(), Mode::Silent, "重启后保持");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
