//! 纯本地 BM25 检索（CJK bigram + 拉丁/数字单词），无外部向量/嵌入依赖。
//! 忠实移植 angel_memory 的「CJK bigram + BM25」主流检索，用字符双字组处理中文。

use crate::mind::store::MemoryRow;
use std::collections::HashMap;

/// 中文统一码范围
fn is_cjk(c: char) -> bool {
    let u = c as u32;
    (0x4E00..=0x9FFF).contains(&u)
        || (0x3400..=0x4DBF).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
}

/// 把文本切成语义 term：CJK 连续段 → 单字+双字组；拉丁/数字连续段 → 小写单词。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cjk: Vec<char> = Vec::new();
    let mut word = String::new();

    let flush_cjk = |cjk: &mut Vec<char>, terms: &mut Vec<String>| {
        if cjk.is_empty() {
            return;
        }
        // 单字词
        for ch in cjk.iter() {
            terms.push(ch.to_string());
        }
        // 双字组（bigram）
        for w in cjk.windows(2) {
            terms.push(w.iter().collect::<String>());
        }
        cjk.clear();
    };
    let flush_word = |word: &mut String, terms: &mut Vec<String>| {
        if !word.is_empty() {
            terms.push(word.clone().to_lowercase());
            word.clear();
        }
    };

    for c in text.chars() {
        if is_cjk(c) {
            flush_word(&mut word, &mut terms);
            cjk.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut terms);
            word.push(c);
        } else {
            flush_cjk(&mut cjk, &mut terms);
            flush_word(&mut word, &mut terms);
        }
    }
    flush_cjk(&mut cjk, &mut terms);
    flush_word(&mut word, &mut terms);
    terms
}

/// BM25 参数
const K1: f64 = 1.5;
const B: f64 = 0.75;

/// 对一批记忆按 query 做 BM25 打分排序，返回前 `limit` 条（含分值）。
pub fn bm25_rank(memories: &[MemoryRow], query: &str, limit: usize) -> Vec<(MemoryRow, f64)> {
    let q_terms = tokenize(query);
    if q_terms.is_empty() {
        return Vec::new();
    }

    let n = memories.len() as f64;
    if n == 0.0 {
        return Vec::new();
    }

    // 每篇文档的 term 频率 + 文档长度
    let mut doc_tf: Vec<HashMap<String, usize>> = Vec::with_capacity(memories.len());
    let mut doc_len: Vec<usize> = Vec::with_capacity(memories.len());
    let mut df: HashMap<String, usize> = HashMap::new();
    let mut total_len: f64 = 0.0;

    for m in memories {
        let terms = tokenize(&m.text);
        let mut tf: HashMap<String, usize> = HashMap::new();
        for t in &terms {
            *tf.entry(t.clone()).or_insert(0) += 1;
        }
        for t in tf.keys() {
            *df.entry(t.clone()).or_insert(0) += 1;
        }
        total_len += terms.len() as f64;
        doc_len.push(terms.len());
        doc_tf.push(tf);
    }
    let avgdl = if n > 0.0 { total_len / n } else { 1.0 };

    // 计算每篇文档的得分
    let mut scored: Vec<(MemoryRow, f64)> = Vec::with_capacity(memories.len());
    let mut idf_cache: HashMap<&str, f64> = HashMap::new();
    for (i, m) in memories.iter().enumerate() {
        let mut score = 0.0;
        for term in &q_terms {
            let df_v = *df.get(term).unwrap_or(&0) as f64;
            if df_v == 0.0 {
                continue;
            }
            let idf = *idf_cache.entry(term.as_str()).or_insert_with(|| {
                ((n - df_v + 0.5) / (df_v + 0.5) + 1.0).ln()
            });
            let tf_v = *doc_tf[i].get(term).unwrap_or(&0) as f64;
            if tf_v == 0.0 {
                continue;
            }
            let denom = tf_v + K1 * (1.0 - B + B * (doc_len[i] as f64 / avgdl.max(0.5)));
            score += idf * (tf_v * (K1 + 1.0)) / denom;
        }
        // 强度加成：记忆越强，排序越靠前
        score *= 0.7 + (m.strength / 100.0).min(1.0) * 0.6;
        if score > 0.0 {
            scored.push((m.clone(), score));
        }
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mind::store::MemoryRow;

    fn row(text: &str, strength: f64) -> MemoryRow {
        MemoryRow {
            id: 1,
            scope: "s".into(),
            conv: "c".into(),
            text: text.into(),
            tags: String::new(),
            strength,
            kind: "knowledge".into(),
            ts: 0,
        }
    }

    #[test]
    fn cjk_bigram_tokens() {
        let t = tokenize("我喜欢吃宫保鸡丁");
        assert!(t.contains(&"宫保".to_string()), "应有 bigram 宫保, got {t:?}");
        assert!(t.contains(&"鸡丁".to_string()), "应有 bigram 鸡丁, got {t:?}");
    }

    #[test]
    fn bm25_ranks_relevant_first() {
        let ms = vec![
            row("用户偏好猫而不是狗", 80.0),
            row("如何修电脑蓝屏", 60.0),
            row("今天天气很好适合出门", 50.0),
        ];
        let r = bm25_rank(&ms, "用户喜欢猫", 3);
        assert!(!r.is_empty());
        assert!(r[0].0.text.contains("猫"), "最相关应排第一: {:?}", r[0].0.text);
    }
}
