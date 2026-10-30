//! 「内心」层：单会话精力（能量）系统 + 四维灵魂参数。
//! 忠实移植 angel_heart 的精力公式：`消耗 = 基础(14) + 有效字符数 × 每字符(0.12)`，
//! 每秒恢复 0.6，上下限 [-100, 100]。精力按会话独立持久化（重启不丢）。

use crate::mind::store::Store;

/// 四维灵魂参数（映射到 LLM 注入行为；angel_memory 的灵魂槽）
#[derive(Debug, Clone)]
pub struct Soul {
    pub recall_depth: f64,       // 1..=20，控制 RAG 检索条数
    #[allow(dead_code)]
    pub impression_depth: f64,   // 1..=10，控制记忆生成倾向
    pub expression_desire: f64,  // 0..=1，控制表达长度
    pub creativity: f64,         // 0..=1，控制温度
}

impl Default for Soul {
    fn default() -> Self {
        Self { recall_depth: 7.0, impression_depth: 3.0, expression_desire: 0.5, creativity: 0.7 }
    }
}

/// 精力系统参数
#[derive(Debug, Clone)]
pub struct HeartConfig {
    pub initial: f64,
    pub energy_max: f64,
    pub energy_min: f64,
    pub recover_per_sec: f64,
    pub base_cost: f64,
    pub per_char: f64,
    pub soul: Soul,
}

impl Default for HeartConfig {
    fn default() -> Self {
        Self {
            initial: 100.0,
            energy_max: 100.0,
            energy_min: -100.0,
            recover_per_sec: 0.6,
            base_cost: 14.0,
            per_char: 0.12,
            soul: Soul::default(),
        }
    }
}

fn key(conv: &str) -> (String, String) {
    (format!("energy:{conv}"), format!("energy_ts:{conv}"))
}

/// 取当前精力（先按经过时间恢复，再持久化）。无记录则回初始值。
pub fn current_energy(store: &Store, conv: &str, cfg: &HeartConfig) -> f64 {
    let (ek, tk) = key(conv);
    let now = crate::mind::now_ts();
    let last: f64 = store
        .get_kv(&ek)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.initial);
    let last_ts: i64 = store
        .get_kv(&tk)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(now);
    let elapsed = (now - last_ts).max(0) as f64;
    let recovered = (last + elapsed * cfg.recover_per_sec).min(cfg.energy_max);
    let _ = store.set_kv(&ek, &format!("{recovered:.2}"));
    let _ = store.set_kv(&tk, &format!("{now}"));
    recovered
}

/// 回复后按实际长度扣精力：`消耗 = base + 有效字符 × per_char`，扣到下限后不再扣。
/// 返回扣后精力。
pub fn charge_reply_energy(store: &Store, conv: &str, chars: usize, cfg: &HeartConfig) -> f64 {
    let (ek, _) = key(conv);
    let before = current_energy(store, conv, cfg);
    let cost = cfg.base_cost + chars as f64 * cfg.per_char;
    let after = (before - cost).max(cfg.energy_min).min(cfg.energy_max);
    let _ = store.set_kv(&ek, &format!("{after:.2}"));
    after
}

/// 精力对应的情绪标签
pub fn mood_label(energy: f64) -> &'static str {
    if energy >= 70.0 {
        "活力充沛"
    } else if energy >= 30.0 {
        "平稳"
    } else if energy >= -20.0 {
        "有点疲惫"
    } else {
        "非常疲惫（想休息一下）"
    }
}

/// 根据精力 + 表达欲望折算的「建议回复长度上限」（字）
pub fn suggested_len(energy: f64, expression_desire: f64) -> usize {
    // 精力越足、表达欲望越高 → 允许更长回复
    let e = ((energy + 100.0) / 200.0).clamp(0.0, 1.0);
    let base = (e * expression_desire + 0.15).clamp(0.15, 1.0);
    (base * 600.0) as usize // 上限 90~600 字，配合简洁提示词
}
