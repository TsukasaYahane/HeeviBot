//! 拟人化发送：把一段回复拆成几条 QQ 消息，像真人打字那样一条条发。
//!
//! 拆分规则（`split_for_chat`）：
//! 1. 短回复（≤ `SHORT_CHARS` 字）不拆；
//! 2. 先按模型留的空行分段（人设里要求「想说的多就分成几条，用空行隔开」）；
//! 3. 「结构化」段落整段保留，不拆散：带网址、列表（`1.` `-` `·` 开头的行）、表格、多行「键：值」数据；
//! 4. 普通段落按句末（。！？!?；~ 和省略号）切句，再把短句贪心拼成不超过 `TARGET_CHARS` 字的一条；
//!    单句本身很长也不硬切（不在逗号中间断开）；
//! 5. 只剩标点 / 一两个字的碎片并进前一条；
//! 6. 超过 `max_parts` 条时，反复合并「合起来最短」的相邻两条，直到够数。
//!
//! 发送（`send_split`）：只有第一条引用原消息；表情包和最后一条合成图文；后面每条之前按字数停顿一下。

use anyhow::Result;
use std::time::Duration;

use crate::onebot::{Image, OneBot, Target};

/// 不超过这个字数的回复不拆
const SHORT_CHARS: usize = 28;
/// 拼句时每条的目标长度
const TARGET_CHARS: usize = 36;
/// 小于这个字数（且不是独立段落）的碎片并进前一条
const MIN_PIECE_CHARS: usize = 3;

#[derive(Debug, Clone)]
struct Piece {
    text: String,
    /// 来自第几个段落（合并时同段直接拼，跨段用换行）
    para: usize,
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// 结构化段落：拆开会散架，整段保留
fn is_block(p: &str) -> bool {
    if p.contains("http://") || p.contains("https://") || p.contains('|') {
        return true;
    }
    let lines: Vec<&str> = p.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() < 2 {
        return false;
    }
    let listy = lines
        .iter()
        .filter(|l| {
            l.starts_with(['-', '·', '•', '*'])
                || l.chars().next().is_some_and(|c| c.is_ascii_digit())
                    && l.chars().nth(1).is_some_and(|c| matches!(c, '.' | '、' | ')' | '）'))
        })
        .count();
    let kv = lines.iter().filter(|l| l.chars().take(12).any(|c| c == '：' || c == ':')).count();
    listy >= 2 || kv >= 3
}

/// 一行切成句子（句末标点、紧跟的右引号 / 右括号都留在句尾）
fn sentences(line: &str) -> Vec<String> {
    let cs: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        cur.push(c);
        let is_end = matches!(c, '。' | '！' | '？' | '!' | '?' | '；' | '~' | '～')
            || (c == '…' && cs.get(i + 1) != Some(&'…'));
        if is_end {
            // 连续的句末符号（「？！」「……」「~~」）和右引号 / 右括号吃进同一句
            while let Some(&n) = cs.get(i + 1) {
                if matches!(n, '。' | '！' | '？' | '!' | '?' | '~' | '～' | '…' | '”' | '」' | '』' | '）' | ')' | '"') {
                    cur.push(n);
                    i += 1;
                } else {
                    break;
                }
            }
            // 英文句点只在后面是空白 / 结尾时才算句末（避免切开 0.2.1、v1.0）
            out.push(std::mem::take(&mut cur));
        } else if c == '.' {
            let next = cs.get(i + 1);
            if next.is_none() || next.is_some_and(|n| n.is_whitespace()) {
                out.push(std::mem::take(&mut cur));
            }
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// 把短句贪心拼成不超过 `target` 字的若干条
fn group(sents: Vec<String>, target: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in sents {
        match out.last_mut() {
            Some(last) if chars(last) + chars(&s) <= target => last.push_str(&s),
            _ => out.push(s),
        }
    }
    out
}

fn is_only_punct(s: &str) -> bool {
    s.chars().all(|c| !c.is_alphanumeric() && !('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// 把一段回复拆成几条消息（见模块文档）。`max_parts <= 1` 时原样返回一条。
pub fn split_for_chat(text: &str, max_parts: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    // 短回复不拆——除非模型自己用空行分了条（「在的。\n\n有什么事吗？」就该是两条）
    let model_split = text.lines().any(|l| l.trim().is_empty());
    if max_parts <= 1 || (chars(text) <= SHORT_CHARS && !model_split) {
        return vec![text.to_string()];
    }
    // 1) 按空行分段
    let mut paras: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                paras.push(cur.join("\n"));
                cur.clear();
            }
        } else {
            cur.push(line.trim_end());
        }
    }
    if !cur.is_empty() {
        paras.push(cur.join("\n"));
    }
    // 2) 段内切句、拼句
    let mut pieces: Vec<Piece> = Vec::new();
    for (pi, p) in paras.iter().enumerate() {
        if is_block(p) || chars(p) <= TARGET_CHARS {
            pieces.push(Piece { text: p.clone(), para: pi });
            continue;
        }
        for line in p.lines() {
            for g in group(sentences(line), TARGET_CHARS) {
                pieces.push(Piece { text: g, para: pi });
            }
        }
    }
    // 3) 碎片并进前一条（同段）
    let mut merged: Vec<Piece> = Vec::new();
    for pc in pieces {
        let tiny = chars(&pc.text) < MIN_PIECE_CHARS || is_only_punct(&pc.text);
        match merged.last_mut() {
            Some(last) if tiny && last.para == pc.para => last.text.push_str(&pc.text),
            _ => merged.push(pc),
        }
    }
    // 4) 超过上限：合并「合起来最短」的相邻两条
    while merged.len() > max_parts {
        let i = (0..merged.len() - 1)
            .min_by_key(|&i| chars(&merged[i].text) + chars(&merged[i + 1].text))
            .unwrap_or(0);
        let next = merged.remove(i + 1);
        let sep = if merged[i].para == next.para { "" } else { "\n" };
        merged[i].text = format!("{}{sep}{}", merged[i].text, next.text);
    }
    merged.into_iter().map(|p| p.text).collect()
}

/// 下一条消息前的停顿：像在打字，按字数算，0.9~3 秒
pub fn typing_delay(next: &str) -> Duration {
    Duration::from_millis((500 + 70 * chars(next) as u64).clamp(900, 3000))
}

/// 拆开逐条发送。返回（发出的条数，表情包是否随最后一条发出）。
/// 第一条失败直接返回错误（调用方走兜底）；后面某条失败只记日志、停止继续发。
pub async fn send_split(
    ob: &OneBot,
    target: &Target,
    text: &str,
    sticker: Option<&Image>,
    max_parts: usize,
) -> Result<(usize, bool)> {
    let parts = split_for_chat(text, max_parts);
    let n = parts.len();
    let mut sticker_sent = false;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(typing_delay(part)).await;
        }
        // 只有第一条引用原消息
        let t = if i == 0 { target.clone() } else { Target::new(target.chat, None) };
        let last = i + 1 == n;
        let res = match (last, sticker) {
            (true, Some(img)) => match t.reply_media(ob, img, part).await {
                Ok(()) => {
                    sticker_sent = true;
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!("图文发送失败，这一条改发纯文本：{e:#}");
                    t.reply(ob, part).await
                }
            },
            _ => t.reply(ob, part).await,
        };
        if let Err(e) = res {
            if i == 0 {
                return Err(e);
            }
            tracing::warn!("分条发送第 {} / {n} 条失败，后面不再发：{e:#}", i + 1);
            return Ok((i, sticker_sent));
        }
    }
    Ok((n, sticker_sent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 短回复不拆() {
        assert_eq!(split_for_chat("我是 Heevi。你有事可以直接说，我在。", 4).len(), 1);
        assert_eq!(split_for_chat("嗯。", 4), vec!["嗯。"]);
        assert!(split_for_chat("   ", 4).is_empty());
    }

    #[test]
    fn 模型用空行分条时短回复也拆() {
        assert_eq!(split_for_chat("在的。\n\n有什么事吗？", 4), vec!["在的。", "有什么事吗？"]);
        assert_eq!(
            split_for_chat("没干什么特别的……在待机。\n\n有人来聊天，就接着聊。", 4),
            vec!["没干什么特别的……在待机。", "有人来聊天，就接着聊。"]
        );
        assert_eq!(split_for_chat("在的。\n\n有什么事吗？", 1).len(), 1, "关闭拆分时仍是一条");
    }

    #[test]
    fn 按空行分条() {
        let r = split_for_chat("上海现在小雨，21℃，体感 20℃，东北风 2 级。\n\n今天夜里转大雨，明天还在下，出门记得带伞。\n\n……这两天湿漉漉的，别忘收衣服。", 4);
        assert_eq!(r.len(), 3, "{r:?}");
        assert!(r[2].starts_with("……"));
    }

    #[test]
    fn 长段落按句子拆并拼成适中长度() {
        let p = "挺平静的。没什么特别的事，就这样待着……你呢？今天还顺利吗？刚才群里在聊团本的事，我看了一会儿，没插话。晚点要是有空，可以跟我说说你那边的情况。";
        let r = split_for_chat(p, 4);
        assert!(r.len() >= 2 && r.len() <= 4, "{r:?}");
        assert_eq!(r.concat(), p, "拆开再拼回去应和原文一致（同段直接相连）");
        assert!(r.iter().all(|s| !is_only_punct(s)));
    }

    #[test]
    fn 结构化段落不拆() {
        let list = "每天必做：\n1. 两次不稳定空间\n2. 三次每日委托\n3. 刷满活跃度";
        let r = split_for_chat(&format!("给你列一下。\n\n{list}"), 4);
        assert!(r.iter().any(|s| s == list), "列表应整段保留：{r:?}");
        let url = "官方仓库最新是 v0.2.1-rc.2，地址 https://github.com/deepseek-ai/deepseek-harness/releases 可以看更新说明，本机还是 0.2.0-rc.2。";
        assert_eq!(split_for_chat(url, 4), vec![url], "带网址的段落不拆");
    }

    #[test]
    fn 版本号和小数点不被当成句末() {
        let s = sentences("本机装的是 0.2.0-rc.2，落后一个小版本。官方是 v0.2.1。");
        assert_eq!(s, vec!["本机装的是 0.2.0-rc.2，落后一个小版本。", "官方是 v0.2.1。"]);
    }

    #[test]
    fn 省略号和连续标点留在句尾() {
        let s = sentences("嗯……好吧？！那就这样「说定了」。");
        assert_eq!(s, vec!["嗯……", "好吧？！", "那就这样「说定了」。"]);
    }

    #[test]
    fn 超过上限会合并() {
        let t = "第一段话说一点内容。\n\n第二段话说一点内容。\n\n第三段话说一点内容。\n\n第四段话说一点内容。\n\n第五段话说一点内容。\n\n第六段。";
        let r = split_for_chat(t, 3);
        assert_eq!(r.len(), 3, "{r:?}");
        assert!(r.iter().any(|s| s.contains('\n')), "跨段合并用换行连接");
        assert_eq!(split_for_chat(t, 1).len(), 1, "max_parts=1 等于关闭拆分");
    }

    #[test]
    fn 打字停顿随长度变化() {
        assert_eq!(typing_delay("嗯"), Duration::from_millis(900));
        assert_eq!(typing_delay(&"字".repeat(100)), Duration::from_millis(3000));
        assert!(typing_delay(&"字".repeat(20)) > typing_delay(&"字".repeat(5)));
    }
}
