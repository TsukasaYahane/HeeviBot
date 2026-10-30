//! 心智系统的本地存储层（SQLite，rusqlite bundled，零外部依赖）。
//! 数据落在 bot 根目录 `<mind_data>/mind.db`，与日志/订阅文件分离。
//!
//! Phase 2 新增三张学习表：`style_samples`（表达风格样本）、`slang`/`slang_cand`
//! （群黑话及候选）、`affinity`（按人好感度）。

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// 一条「核心记忆」行
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: i64,
    pub scope: String,
    pub conv: String,
    pub text: String,
    pub tags: String,
    pub strength: f64,
    pub kind: String,
    pub ts: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NoteRow {
    pub id: i64,
    pub title: String,
    pub content: String,
    pub ts: i64,
}

/// 一条表达风格样本：用户说了什么 → 当时怎么回的
#[derive(Debug, Clone, serde::Serialize)]
pub struct StyleRow {
    pub id: i64,
    pub scope: String,
    pub conv: String,
    pub user_text: String,
    pub bot_text: String,
    pub score: f64,
    pub ts: i64,
}

/// 一条群黑话
#[derive(Debug, Clone, serde::Serialize)]
pub struct SlangRow {
    pub id: i64,
    pub scope: String,
    pub term: String,
    pub meaning: String,
    pub hits: i64,
    pub users: i64,
    pub ts: i64,
}

/// 某人在某会话的好感度
#[derive(Debug, Clone, serde::Serialize)]
pub struct AffinityRow {
    pub scope: String,
    pub uid: String,
    pub nick: String,
    pub score: f64,
    pub msgs: i64,
    pub last_ts: i64,
}

/// 未压缩区的一条消息
#[derive(Debug, Clone)]
pub struct RawLine {
    pub scope: String,
    pub sender: String,
    pub text: String,
    pub is_bot: bool,
    pub ts: i64,
}

impl RawLine {
    /// 列顺序：scope, uid, sender, text, is_bot, ts（uid 只用于筛选，不读）
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            scope: r.get(0)?,
            sender: r.get(2)?,
            text: r.get(3)?,
            is_bot: r.get::<_, i64>(4)? != 0,
            ts: r.get(5)?,
        })
    }
}

/// 总览统计（WebUI）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Counts {
    pub messages: i64,
    pub memories: i64,
    pub notes: i64,
    pub scopes: i64,
    pub styles: i64,
    pub slang: i64,
    pub affinity: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open(dir.join("mind.db"))?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS messages(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT, conv TEXT, sender TEXT, text TEXT, is_bot INTEGER DEFAULT 0, ts INTEGER);
             CREATE TABLE IF NOT EXISTS memories(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT, conv TEXT, text TEXT, tags TEXT DEFAULT '',
                 strength REAL DEFAULT 80, kind TEXT DEFAULT 'knowledge',
                 ts INTEGER, last_used INTEGER DEFAULT 0);
             CREATE TABLE IF NOT EXISTS notes(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 title TEXT, content TEXT, ts INTEGER);
             CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE IF NOT EXISTS style_samples(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT, conv TEXT, user_text TEXT, bot_text TEXT,
                 score REAL DEFAULT 0, ts INTEGER);
             CREATE INDEX IF NOT EXISTS idx_style_conv ON style_samples(conv, ts);
             CREATE TABLE IF NOT EXISTS slang(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT, term TEXT, meaning TEXT DEFAULT '',
                 hits INTEGER DEFAULT 1, users INTEGER DEFAULT 1, ts INTEGER,
                 UNIQUE(scope, term));
             CREATE TABLE IF NOT EXISTS slang_cand(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT, term TEXT, hits INTEGER DEFAULT 1, users TEXT DEFAULT '', ts INTEGER,
                 UNIQUE(scope, term));
             CREATE TABLE IF NOT EXISTS persona(
                 scope TEXT, uid TEXT, facet TEXT, value TEXT,
                 evidence REAL DEFAULT 0, hits INTEGER DEFAULT 0, source TEXT DEFAULT '',
                 first_ts INTEGER, last_ts INTEGER,
                 PRIMARY KEY(scope, uid, facet, value));
             CREATE TABLE IF NOT EXISTS curiosity(
                 term TEXT PRIMARY KEY, scope TEXT, why TEXT DEFAULT '', interest INTEGER DEFAULT 3,
                 mentions INTEGER DEFAULT 1, since_stage INTEGER DEFAULT 0, stage INTEGER DEFAULT 0,
                 conf INTEGER DEFAULT 0, last_research INTEGER DEFAULT 0, created INTEGER);
             CREATE TABLE IF NOT EXISTS concept_log(
                 id INTEGER PRIMARY KEY AUTOINCREMENT, term TEXT, ts INTEGER, event TEXT,
                 stage INTEGER, conf INTEGER, mentions INTEGER, detail TEXT DEFAULT '');
             CREATE INDEX IF NOT EXISTS idx_concept_log ON concept_log(term, ts);
             CREATE TABLE IF NOT EXISTS persona_log(
                 id INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT, uid TEXT, facet TEXT, value TEXT,
                 ts INTEGER, weight REAL, source TEXT);
             CREATE INDEX IF NOT EXISTS idx_persona_log ON persona_log(scope, uid, ts);
             CREATE TABLE IF NOT EXISTS corrections(
                 id INTEGER PRIMARY KEY AUTOINCREMENT, term TEXT, text TEXT, by_uid TEXT, by_nick TEXT,
                 ts INTEGER, status TEXT DEFAULT 'pending');
             CREATE TABLE IF NOT EXISTS affinity(
                 scope TEXT, uid TEXT, nick TEXT DEFAULT '',
                 score REAL DEFAULT 50, msgs INTEGER DEFAULT 0, last_ts INTEGER,
                 PRIMARY KEY(scope, uid));
             CREATE TABLE IF NOT EXISTS digest(
                 kind TEXT, key TEXT, text TEXT, updated INTEGER,
                 PRIMARY KEY(kind, key));",
        )?;
        // 记忆账本：messages 就是「未压缩区」，每条带说话人的 QQ 号（uid）；
        // bot 的回复记下是在回谁（reply_uid），这样能按人把各窗口的往来串起来
        for col in ["uid TEXT DEFAULT ''", "reply_uid TEXT DEFAULT ''"] {
            let _ = self.conn.execute(&format!("ALTER TABLE messages ADD COLUMN {col}"), []);
        }
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_msg_scope ON messages(scope, ts);
             CREATE INDEX IF NOT EXISTS idx_msg_uid ON messages(uid, ts);
             CREATE INDEX IF NOT EXISTS idx_msg_reply ON messages(reply_uid, ts);",
        )?;
        // 老消息只有昵称：按同窗口的好感表（scope + 昵称 → uid）补一次
        if self.get_kv("ledger_v1")?.is_none() {
            self.conn.execute(
                "UPDATE messages SET uid=COALESCE((SELECT a.uid FROM affinity a
                     WHERE a.scope=messages.scope AND a.nick=messages.sender ORDER BY a.last_ts DESC LIMIT 1), '')
                 WHERE uid='' AND is_bot=0",
                [],
            )?;
            self.set_kv("ledger_v1", "1")?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn insert_message(&self, scope: &str, conv: &str, sender: &str, text: &str, is_bot: bool) -> Result<()> {
        self.log_message(scope, conv, "", sender, text, is_bot, "")
    }

    /// 入账一条消息（未压缩区）。`uid` = 说话人 QQ 号（bot 为空），`reply_uid` = bot 这条在回谁
    #[allow(clippy::too_many_arguments)]
    pub fn log_message(&self, scope: &str, conv: &str, uid: &str, sender: &str, text: &str, is_bot: bool, reply_uid: &str) -> Result<()> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO messages(scope,conv,uid,sender,text,is_bot,reply_uid,ts) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![scope, conv, uid, sender, text, is_bot as i64, reply_uid, now],
        )?;
        let id = self.conn.last_insert_rowid();
        // 全量观察下消息量增长快：每 256 条顺手裁一次（每 scope 保留最近 3000 条）
        if id % 256 == 0 {
            let _ = self.trim_messages(scope, 3000);
        }
        Ok(())
    }

    /// 某 scope 最近 N 条消息（按时间正序返回 `(发送者, 内容, 是否 bot)`）
    pub fn recent_messages(&self, scope: &str, limit: usize) -> Result<Vec<(String, String, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT sender,text,is_bot FROM messages WHERE scope=?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt.query_map(rusqlite::params![scope, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        let mut out: Vec<(String, String, bool)> = rows.collect::<Result<Vec<_>, _>>()?;
        out.reverse();
        Ok(out)
    }

    /// 每个 scope 只保留最近 keep 条消息（防全量观察把库撑爆）
    pub fn trim_messages(&self, scope: &str, keep: usize) -> Result<()> {
        self.conn.execute(
            "DELETE FROM messages WHERE scope=?1 AND id NOT IN \
             (SELECT id FROM messages WHERE scope=?1 ORDER BY id DESC LIMIT ?2)",
            rusqlite::params![scope, keep as i64],
        )?;
        Ok(())
    }

    /// 新增一条核心记忆（记忆系统入口；tags 以逗号分隔）
    pub fn insert_memory(&self, scope: &str, conv: &str, text: &str, tags: &str, strength: f64, kind: &str) -> Result<i64> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO memories(scope,conv,text,tags,strength,kind,ts) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![scope, conv, text, tags, strength, kind, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn all_memories(&self) -> Result<Vec<MemoryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,scope,conv,text,tags,strength,kind,ts FROM memories ORDER BY ts DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                scope: r.get(1)?,
                conv: r.get(2)?,
                text: r.get(3)?,
                tags: r.get(4)?,
                strength: r.get(5)?,
                kind: r.get(6)?,
                ts: r.get(7)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 关键词兜底检索（WebUI/召回兜底，LIKE 匹配）；BM25 主召回在 memory.rs
    #[allow(dead_code)]
    pub fn search_memories_like(&self, q: &str, limit: usize) -> Result<Vec<MemoryRow>> {
        let pat = format!("%{}%", q);
        let mut stmt = self.conn.prepare(
            "SELECT id,scope,conv,text,tags,strength,kind,ts FROM memories
             WHERE text LIKE ?1 OR tags LIKE ?1 ORDER BY ts DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![pat, limit as i64], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                scope: r.get(1)?,
                conv: r.get(2)?,
                text: r.get(3)?,
                tags: r.get(4)?,
                strength: r.get(5)?,
                kind: r.get(6)?,
                ts: r.get(7)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 自动抽到的「关于某人」事实：同 scope 同人同句 → 加强（strength +10，上限 100）并刷新时间，否则新增。
    /// tags 记 `uid:<uid>`，便于按人取。返回 true = 新增。
    pub fn upsert_fact(&self, scope: &str, conv: &str, uid: &str, text: &str, strength: f64, kind: &str) -> Result<bool> {
        let now = crate::mind::now_ts();
        let tag = format!("uid:{uid}");
        let n = self.conn.execute(
            "UPDATE memories SET strength=MIN(100, strength+10), ts=?1
             WHERE scope=?2 AND tags=?3 AND text=?4",
            rusqlite::params![now, scope, tag, text],
        )?;
        if n > 0 {
            return Ok(false);
        }
        self.insert_memory(scope, conv, text, &tag, strength, kind)?;
        // 每人最多留 30 条，挤掉最弱最旧的
        self.conn.execute(
            "DELETE FROM memories WHERE scope=?1 AND tags=?2 AND id NOT IN (
                 SELECT id FROM memories WHERE scope=?1 AND tags=?2 ORDER BY strength DESC, ts DESC LIMIT 30)",
            rusqlite::params![scope, tag],
        )?;
        Ok(true)
    }

    /// 某人在本 scope 的事实（强度高、较新的在前）
    pub fn facts_about(&self, scope: &str, uid: &str, limit: usize) -> Result<Vec<MemoryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,scope,conv,text,tags,strength,kind,ts FROM memories
             WHERE scope=?1 AND tags=?2 ORDER BY strength DESC, ts DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![scope, format!("uid:{uid}"), limit as i64], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                scope: r.get(1)?,
                conv: r.get(2)?,
                text: r.get(3)?,
                tags: r.get(4)?,
                strength: r.get(5)?,
                kind: r.get(6)?,
                ts: r.get(7)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 有释义的黑话 (id, term, meaning)（迁移时重新校验用）
    pub fn explained_slang(&self) -> Result<Vec<(i64, String, String)>> {
        let mut st = self.conn.prepare("SELECT id, term, meaning FROM slang WHERE meaning<>''")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 清空自动统计出来的黑话（没释义的）和全部候选，返回删掉的黑话数
    pub fn clear_counted_slang(&self) -> Result<usize> {
        let n = self.conn.execute("DELETE FROM slang WHERE meaning=''", [])?;
        self.conn.execute("DELETE FROM slang_cand", [])?;
        Ok(n)
    }

    /// 全部群友消息（不含 bot），按时间正序：(scope, sender, text)
    pub fn user_messages(&self) -> Result<Vec<(String, String, String)>> {
        let mut st = self
            .conn
            .prepare("SELECT scope, sender, text FROM messages WHERE is_bot=0 ORDER BY ts, id")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 某 scope 自某时刻以来的消息（正序）：(发送者, 文本, 是否 bot)
    pub fn messages_since(&self, scope: &str, since: i64, limit: usize) -> Result<Vec<(String, String, bool)>> {
        let mut st = self.conn.prepare(
            "SELECT sender, text, is_bot FROM (
                 SELECT id, sender, text, is_bot FROM messages WHERE scope=?1 AND ts>=?2 ORDER BY id DESC LIMIT ?3
             ) ORDER BY id",
        )?;
        let rows = st.query_map(rusqlite::params![scope, since, limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? != 0))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 某 scope 某类记忆（新的在前）
    pub fn memories_of_kind(&self, scope: &str, kind: &str, limit: usize) -> Result<Vec<MemoryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,scope,conv,text,tags,strength,kind,ts FROM memories
             WHERE scope=?1 AND kind=?2 ORDER BY ts DESC, id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![scope, kind, limit as i64], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                scope: r.get(1)?,
                conv: r.get(2)?,
                text: r.get(3)?,
                tags: r.get(4)?,
                strength: r.get(5)?,
                kind: r.get(6)?,
                ts: r.get(7)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 自动记忆的遗忘：`stale_before` 之前写入、且之后没被召回过的，强度 −step；低于 floor 的删掉。
    /// 再按强度只留前 keep 条。返回删掉的条数。
    pub fn fade_memories(&self, scope: &str, kind: &str, stale_before: i64, step: f64, floor: f64, keep: usize) -> Result<usize> {
        self.conn.execute(
            "UPDATE memories SET strength=strength-?4
             WHERE scope=?1 AND kind=?2 AND ts<?3 AND COALESCE(last_used,0)<?3",
            rusqlite::params![scope, kind, stale_before, step],
        )?;
        let mut n = self.conn.execute(
            "DELETE FROM memories WHERE scope=?1 AND kind=?2 AND strength<?3",
            rusqlite::params![scope, kind, floor],
        )?;
        n += self.conn.execute(
            "DELETE FROM memories WHERE scope=?1 AND kind=?2 AND id NOT IN (
                 SELECT id FROM memories WHERE scope=?1 AND kind=?2 ORDER BY strength DESC, ts DESC LIMIT ?3)",
            rusqlite::params![scope, kind, keep as i64],
        )?;
        Ok(n)
    }

    pub fn delete_memory(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM memories WHERE id=?1", rusqlite::params![id])?;
        Ok(())
    }

    pub fn touch_memory(&self, id: i64) -> Result<()> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "UPDATE memories SET last_used=?1 WHERE id=?2",
            rusqlite::params![now, id],
        )?;
        Ok(())
    }

    pub fn insert_note(&self, title: &str, content: &str) -> Result<i64> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO notes(title,content,ts) VALUES(?1,?2,?3)",
            rusqlite::params![title, content, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn all_notes(&self) -> Result<Vec<NoteRow>> {
        let mut stmt = self.conn.prepare("SELECT id,title,content,ts FROM notes ORDER BY ts DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(NoteRow { id: r.get(0)?, title: r.get(1)?, content: r.get(2)?, ts: r.get(3)? })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// kv 读写（内部状态/精力等持久化）
    pub fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    pub fn get_kv(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM kv WHERE key=?1")?;
        let mut rows = stmt.query(rusqlite::params![key])?;
        Ok(rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?)
    }

    // ---------------- Phase 2：表达风格样本 ----------------

    /// 记一对「用户说了什么 → 我回了什么」（初始 0 分，等下一轮接话再打分）
    pub fn insert_style_sample(&self, scope: &str, conv: &str, user_text: &str, bot_text: &str) -> Result<i64> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO style_samples(scope,conv,user_text,bot_text,score,ts) VALUES(?1,?2,?3,?4,0,?5)",
            rusqlite::params![scope, conv, user_text, bot_text, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 某会话最近一条风格样本
    pub fn last_style_sample(&self, conv: &str) -> Result<Option<StyleRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,scope,conv,user_text,bot_text,score,ts FROM style_samples
             WHERE conv=?1 ORDER BY ts DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![conv])?;
        match rows.next()? {
            Some(r) => Ok(Some(StyleRow {
                id: r.get(0)?,
                scope: r.get(1)?,
                conv: r.get(2)?,
                user_text: r.get(3)?,
                bot_text: r.get(4)?,
                score: r.get(5)?,
                ts: r.get(6)?,
            })),
            None => Ok(None),
        }
    }

    pub fn add_style_score(&self, id: i64, delta: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE style_samples SET score=score+?1 WHERE id=?2",
            rusqlite::params![delta, id],
        )?;
        Ok(())
    }

    /// 高分风格样本（注入用）：只取正分、回复不太长的，按分值与时间排序
    pub fn top_style_samples(&self, scope: &str, limit: usize) -> Result<Vec<StyleRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,scope,conv,user_text,bot_text,score,ts FROM style_samples
             WHERE scope=?1 AND score>0 AND length(bot_text)<=160
             ORDER BY score DESC, ts DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![scope, limit as i64], |r| {
            Ok(StyleRow {
                id: r.get(0)?,
                scope: r.get(1)?,
                conv: r.get(2)?,
                user_text: r.get(3)?,
                bot_text: r.get(4)?,
                score: r.get(5)?,
                ts: r.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn all_style_samples(&self, limit: usize) -> Result<Vec<StyleRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,scope,conv,user_text,bot_text,score,ts FROM style_samples
             ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |r| {
            Ok(StyleRow {
                id: r.get(0)?,
                scope: r.get(1)?,
                conv: r.get(2)?,
                user_text: r.get(3)?,
                bot_text: r.get(4)?,
                score: r.get(5)?,
                ts: r.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn delete_style_sample(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM style_samples WHERE id=?1", rusqlite::params![id])?;
        Ok(())
    }

    /// 只保留该 scope 分值最高的 `keep` 条（其余删除），避免无限增长
    pub fn trim_style_samples(&self, scope: &str, keep: usize) -> Result<()> {
        self.conn.execute(
            "DELETE FROM style_samples WHERE scope=?1 AND id NOT IN (
                 SELECT id FROM style_samples WHERE scope=?1 ORDER BY score DESC, ts DESC LIMIT ?2)",
            rusqlite::params![scope, keep as i64],
        )?;
        Ok(())
    }

    // ---------------- Phase 2：群黑话 ----------------

    /// 写入/更新一条黑话（释义非空则覆盖旧释义）
    pub fn upsert_slang(&self, scope: &str, term: &str, meaning: &str) -> Result<()> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO slang(scope,term,meaning,hits,users,ts) VALUES(?1,?2,?3,1,1,?4)
             ON CONFLICT(scope,term) DO UPDATE SET
                 hits=hits+1,
                 meaning=CASE WHEN excluded.meaning<>'' THEN excluded.meaning ELSE meaning END,
                 ts=excluded.ts",
            rusqlite::params![scope, term, meaning, now],
        )?;
        Ok(())
    }

    /// 候选词计数 +1（users 存逗号分隔的去重 uid 列表），返回是否已达门槛
    pub fn bump_candidate(&self, scope: &str, term: &str, uid: &str, min_hits: i64, min_users: i64) -> Result<bool> {
        let now = crate::mind::now_ts();
        let existing: Option<(i64, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT hits,users FROM slang_cand WHERE scope=?1 AND term=?2")?;
            let mut rows = stmt.query(rusqlite::params![scope, term])?;
            match rows.next()? {
                Some(r) => Some((r.get(0)?, r.get(1)?)),
                None => None,
            }
        };
        let (hits, users) = match existing {
            Some((h, u)) => {
                let mut list: Vec<&str> = u.split(',').filter(|s| !s.is_empty()).collect();
                if !list.contains(&uid) {
                    list.push(uid);
                }
                (h + 1, list.join(","))
            }
            None => (1, uid.to_string()),
        };
        let user_count = users.split(',').filter(|s| !s.is_empty()).count() as i64;
        self.conn.execute(
            "INSERT INTO slang_cand(scope,term,hits,users,ts) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(scope,term) DO UPDATE SET hits=excluded.hits, users=excluded.users, ts=excluded.ts",
            rusqlite::params![scope, term, hits, users, now],
        )?;
        Ok(hits >= min_hits && user_count >= min_users)
    }

    /// 候选词达标 → 提升为黑话并清掉候选。返回是否真的提升了。
    pub fn promote_candidate(&self, scope: &str, term: &str) -> Result<bool> {
        let now = crate::mind::now_ts();
        let cand: Option<(i64, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT hits,users FROM slang_cand WHERE scope=?1 AND term=?2")?;
            let mut rows = stmt.query(rusqlite::params![scope, term])?;
            match rows.next()? {
                Some(r) => Some((r.get(0)?, r.get(1)?)),
                None => None,
            }
        };
        let Some((hits, users)) = cand else { return Ok(false) };
        let user_count = users.split(',').filter(|s| !s.is_empty()).count() as i64;
        self.conn.execute(
            "INSERT INTO slang(scope,term,meaning,hits,users,ts) VALUES(?1,?2,'',?3,?4,?5)
             ON CONFLICT(scope,term) DO UPDATE SET hits=hits+excluded.hits, users=MAX(users,excluded.users)",
            rusqlite::params![scope, term, hits, user_count, now],
        )?;
        self.conn.execute(
            "DELETE FROM slang_cand WHERE scope=?1 AND term=?2",
            rusqlite::params![scope, term],
        )?;
        Ok(true)
    }

    pub fn list_slang(&self, scope: Option<&str>, limit: usize) -> Result<Vec<SlangRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,scope,term,meaning,hits,users,ts FROM slang
             WHERE (?1 IS NULL OR scope=?1)
             ORDER BY CASE WHEN meaning<>'' THEN 0 ELSE 1 END, hits DESC, ts DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![scope, limit as i64], |r| {
            Ok(SlangRow {
                id: r.get(0)?,
                scope: r.get(1)?,
                term: r.get(2)?,
                meaning: r.get(3)?,
                hits: r.get(4)?,
                users: r.get(5)?,
                ts: r.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn delete_slang(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM slang WHERE id=?1", rusqlite::params![id])?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn set_slang_meaning(&self, id: i64, meaning: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE slang SET meaning=?1 WHERE id=?2",
            rusqlite::params![meaning, id],
        )?;
        Ok(())
    }

    /// 候选词列表（WebUI 观察用）
    pub fn list_candidates(&self, scope: Option<&str>, limit: usize) -> Result<Vec<(String, String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT scope,term,hits,users FROM slang_cand
             WHERE (?1 IS NULL OR scope=?1) ORDER BY hits DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![scope, limit as i64], |r| {
            let users: String = r.get(3)?;
            let uc = users.split(',').filter(|s| !s.is_empty()).count() as i64;
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, uc))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // ---------------- Phase 2：社交好感度 ----------------

    /// 互动一次：加分 + 记昵称 + 消息数 +1
    pub fn bump_affinity(&self, scope: &str, uid: &str, nick: &str, delta: f64, start: f64) -> Result<()> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO affinity(scope,uid,nick,score,msgs,last_ts) VALUES(?1,?2,?3,?4,1,?5)
             ON CONFLICT(scope,uid) DO UPDATE SET
                 score=MAX(0.0, MIN(100.0, score+?6)),
                 msgs=msgs+1,
                 nick=CASE WHEN excluded.nick<>'' THEN excluded.nick ELSE nick END,
                 last_ts=excluded.last_ts",
            rusqlite::params![scope, uid, nick, (start + delta).clamp(0.0, 100.0), now, delta],
        )?;
        Ok(())
    }

    pub fn get_affinity(&self, scope: &str, uid: &str) -> Result<Option<AffinityRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT scope,uid,nick,score,msgs,last_ts FROM affinity WHERE scope=?1 AND uid=?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![scope, uid])?;
        match rows.next()? {
            Some(r) => Ok(Some(AffinityRow {
                scope: r.get(0)?,
                uid: r.get(1)?,
                nick: r.get(2)?,
                score: r.get(3)?,
                msgs: r.get(4)?,
                last_ts: r.get(5)?,
            })),
            None => Ok(None),
        }
    }

    pub fn set_affinity_score(&self, scope: &str, uid: &str, score: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE affinity SET score=?1 WHERE scope=?2 AND uid=?3",
            rusqlite::params![score, scope, uid],
        )?;
        Ok(())
    }

    // ---------------- 人物画像 ----------------

    /// 加一份证据：同一个面累加证据和次数、刷新时间；顶掉矛盾的面；每个维度只留证据最多的 5 个
    pub fn add_facet(&self, scope: &str, uid: &str, facet: &str, value: &str, weight: f64, source: &str) -> Result<()> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO persona(scope,uid,facet,value,evidence,hits,source,first_ts,last_ts) VALUES(?1,?2,?3,?4,?5,1,?6,?7,?7)
             ON CONFLICT(scope,uid,facet,value) DO UPDATE SET
                 evidence=evidence+excluded.evidence, hits=hits+1, last_ts=excluded.last_ts,
                 source=CASE WHEN instr(source, excluded.source)>0 THEN source ELSE source||','||excluded.source END",
            rusqlite::params![scope, uid, facet, value, weight, source, now],
        )?;
        self.conn.execute(
            "INSERT INTO persona_log(scope,uid,facet,value,ts,weight,source) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![scope, uid, facet, value, now, weight, source],
        )?;
        if let Some(opp) = crate::mind::persona::opposite(facet) {
            self.conn.execute(
                "DELETE FROM persona WHERE scope=?1 AND uid=?2 AND facet=?3 AND value=?4",
                rusqlite::params![scope, uid, opp, value],
            )?;
        }
        self.conn.execute(
            "DELETE FROM persona WHERE scope=?1 AND uid=?2 AND facet=?3 AND value NOT IN (
                 SELECT value FROM persona WHERE scope=?1 AND uid=?2 AND facet=?3 ORDER BY evidence DESC, last_ts DESC LIMIT 5)",
            rusqlite::params![scope, uid, facet],
        )?;
        Ok(())
    }

    pub fn facets_of(&self, scope: &str, uid: &str) -> Result<Vec<crate::mind::persona::FacetRow>> {
        let mut st = self.conn.prepare(
            "SELECT facet,value,evidence,hits,last_ts FROM persona WHERE scope=?1 AND uid=?2 ORDER BY evidence DESC",
        )?;
        let rows = st.query_map(rusqlite::params![scope, uid], |r| {
            Ok(crate::mind::persona::FacetRow {
                facet: r.get(0)?,
                value: r.get(1)?,
                evidence: r.get(2)?,
                hits: r.get(3)?,
                last_ts: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 全部画像（WebUI）：(scope, uid, 昵称, 面…)
    pub fn all_personas(&self, limit: usize) -> Result<Vec<(String, String, String, Vec<crate::mind::persona::FacetRow>)>> {
        let mut st = self.conn.prepare(
            "SELECT p.scope, p.uid, COALESCE((SELECT nick FROM affinity a WHERE a.scope=p.scope AND a.uid=p.uid), ''), SUM(p.evidence) e
             FROM persona p GROUP BY p.scope, p.uid ORDER BY e DESC LIMIT ?1",
        )?;
        let heads: Vec<(String, String, String)> = st
            .query_map(rusqlite::params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(heads
            .into_iter()
            .map(|(s, u, n)| {
                let f = self.facets_of(&s, &u).unwrap_or_default();
                (s, u, n, f)
            })
            .collect())
    }

    /// 窗口里按昵称找人（夜间整理的画像行只有昵称）
    pub fn uid_by_nick(&self, scope: &str, nick: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare("SELECT uid FROM affinity WHERE scope=?1 AND nick=?2 ORDER BY last_ts DESC LIMIT 1")?;
        let mut rows = st.query(rusqlite::params![scope, nick])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    // ---------------- 好奇心队列 ----------------

    /// 进队列（已有就算又提到一次）
    pub fn enqueue_curiosity(&self, term: &str, scope: &str, why: &str, interest: i64) -> Result<()> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO curiosity(term,scope,why,interest,created) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(term) DO UPDATE SET mentions=mentions+1, since_stage=since_stage+1,
                 interest=MAX(interest, excluded.interest), why=CASE WHEN why='' THEN excluded.why ELSE why END",
            rusqlite::params![term, scope, why, interest, now],
        )?;
        self.log_concept(term, "queued", why)?;
        Ok(())
    }

    /// 认知曲线的一个点：记下这一刻的阶段 / 可信度 / 被提到的次数
    pub fn log_concept(&self, term: &str, event: &str, detail: &str) -> Result<()> {
        let now = crate::mind::now_ts();
        let (stage, conf, mentions): (i64, i64, i64) = self
            .conn
            .query_row("SELECT stage, conf, mentions FROM curiosity WHERE term=?1", rusqlite::params![term], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap_or((0, 0, 0));
        self.conn.execute(
            "INSERT INTO concept_log(term,ts,event,stage,conf,mentions,detail) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![term, now, event, stage, conf, mentions, detail],
        )?;
        Ok(())
    }

    /// 某个词的认知曲线：(时间, 事件, 阶段, 可信度, 被提到次数, 说明)
    #[allow(clippy::type_complexity)]
    pub fn concept_history(&self, term: &str) -> Result<Vec<(i64, String, i64, i64, i64, String)>> {
        let mut st = self.conn.prepare("SELECT ts,event,stage,conf,mentions,detail FROM concept_log WHERE term=?1 ORDER BY ts, id")?;
        let rows = st.query_map(rusqlite::params![term], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 某人某个面的证据时间线：(时间, 权重)
    pub fn facet_history(&self, scope: &str, uid: &str, facet: &str, value: &str) -> Result<Vec<(i64, f64)>> {
        let mut st = self.conn.prepare(
            "SELECT ts, weight FROM persona_log WHERE scope=?1 AND uid=?2 AND facet=?3 AND value=?4 ORDER BY ts, id",
        )?;
        let rows = st.query_map(rusqlite::params![scope, uid, facet, value], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 删掉某人的某些面（value 为空 = 这个维度全删）
    pub fn delete_facet(&self, scope: &str, uid: &str, facet: Option<&str>, value: &str) -> Result<usize> {
        Ok(match facet {
            Some(f) if value.is_empty() => self.conn.execute(
                "DELETE FROM persona WHERE scope=?1 AND uid=?2 AND facet=?3",
                rusqlite::params![scope, uid, f],
            )?,
            Some(f) => self.conn.execute(
                "DELETE FROM persona WHERE scope=?1 AND uid=?2 AND facet=?3 AND value=?4",
                rusqlite::params![scope, uid, f, value],
            )?,
            None => self.conn.execute(
                "DELETE FROM persona WHERE scope=?1 AND uid=?2 AND value=?3",
                rusqlite::params![scope, uid, value],
            )?,
        })
    }

    // ---------------- 纠正 ----------------

    pub fn add_correction(&self, term: &str, text: &str, by_uid: &str, by_nick: &str) -> Result<i64> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "INSERT INTO corrections(term,text,by_uid,by_nick,ts) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![term, text, by_uid, by_nick, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 纠正记录：(id, 词, 内容, QQ, 昵称, 时间, 状态)；term 为 None 时列全部待处理的
    #[allow(clippy::type_complexity)]
    pub fn corrections(&self, term: Option<&str>) -> Result<Vec<(i64, String, String, String, String, i64, String)>> {
        let mut st = self.conn.prepare(
            "SELECT id,term,text,by_uid,by_nick,ts,status FROM corrections
             WHERE (?1 IS NULL AND status='pending') OR term=?1 ORDER BY ts DESC LIMIT 50",
        )?;
        let rows = st.query_map(rusqlite::params![term], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn set_correction_status(&self, id: i64, status: &str) -> Result<Option<(String, String)>> {
        self.conn.execute("UPDATE corrections SET status=?2 WHERE id=?1", rusqlite::params![id, status])?;
        Ok(self
            .conn
            .query_row("SELECT term, text FROM corrections WHERE id=?1", rusqlite::params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok())
    }

    /// 主人纠正后：可信度直接拉满、至少算学过「是什么」
    pub fn mark_corrected(&self, term: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE curiosity SET conf=2, stage=MAX(stage,1), since_stage=0, last_research=?2 WHERE term=?1",
            rusqlite::params![term, crate::mind::now_ts()],
        )?;
        Ok(())
    }

    /// 让某个词下一晚重新学（有人指出了问题，或主人点了「重新学」）
    pub fn requeue_research(&self, term: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE curiosity SET last_research=0, since_stage=MAX(since_stage,3) WHERE term=?1",
            rusqlite::params![term],
        )?;
        Ok(())
    }

    /// 忘掉一个词（笔记 + 队列 + 曲线）
    pub fn forget_concept(&self, term: &str) -> Result<()> {
        let tag = format!("concept:{term}");
        self.conn.execute("DELETE FROM memories WHERE kind='concept' AND (tags=?1 OR tags LIKE ?1 || ',%')", rusqlite::params![tag])?;
        self.conn.execute("DELETE FROM curiosity WHERE term=?1", rusqlite::params![term])?;
        self.conn.execute("DELETE FROM concept_log WHERE term=?1", rusqlite::params![term])?;
        Ok(())
    }

    /// 主人直接改释义：黑话表里有这个词就覆盖
    pub fn set_slang_meaning_by_term(&self, term: &str, meaning: &str) -> Result<usize> {
        Ok(self.conn.execute("UPDATE slang SET meaning=?2 WHERE term=?1", rusqlite::params![term, meaning])?)
    }

    /// 队列里还没学完的词（统计今天又被提到几次用）
    pub fn open_curiosities(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT term FROM curiosity WHERE stage<3")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn mention_curiosity(&self, term: &str, n: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE curiosity SET mentions=mentions+?2, since_stage=since_stage+?2 WHERE term=?1",
            rusqlite::params![term, n],
        )?;
        self.log_concept(term, "mentioned", &format!("+{n}"))?;
        Ok(())
    }

    /// 全部好奇心条目：(term, scope, why, interest, mentions, since_stage, stage, conf, last_research)
    #[allow(clippy::type_complexity)]
    pub fn curiosities(&self) -> Result<Vec<(String, String, String, i64, i64, i64, i64, i64, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT term,scope,why,interest,mentions,since_stage,stage,conf,last_research FROM curiosity
             ORDER BY stage<3 DESC, interest*2+mentions DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 学完一次：升阶段（可信度够才升）、清零「又提到」计数、记时间
    pub fn finish_research(&self, term: &str, advance: bool, conf: i64) -> Result<()> {
        let now = crate::mind::now_ts();
        self.conn.execute(
            "UPDATE curiosity SET stage=stage+?2, since_stage=0, conf=?3, last_research=?4 WHERE term=?1",
            rusqlite::params![term, advance as i64, conf, now],
        )?;
        self.log_concept(term, if advance { "learned" } else { "unsure" }, "")?;
        Ok(())
    }

    /// 学习笔记（kind=concept，scope=global:concept，tags=concept:<词>）：同一个词只留最新一条
    pub fn put_concept(&self, term: &str, text: &str, strength: f64, extra_tags: &str) -> Result<()> {
        let tag = format!("concept:{term}");
        self.conn.execute(
            "DELETE FROM memories WHERE kind='concept' AND (tags=?1 OR tags LIKE ?1 || ',%')",
            rusqlite::params![tag],
        )?;
        let tags = if extra_tags.is_empty() { tag } else { format!("{tag},{extra_tags}") };
        self.insert_memory("global:concept", "", text, &tags, strength, "concept")?;
        Ok(())
    }

    /// 某个词的学习笔记
    pub fn concept_of(&self, term: &str) -> Result<Option<MemoryRow>> {
        let tag = format!("concept:{term}");
        Ok(self
            .memories_of_kind("global:concept", "concept", 500)?
            .into_iter()
            .find(|m| m.tags == tag || m.tags.starts_with(&format!("{tag},"))))
    }

    /// 没释义的黑话补上释义
    pub fn fill_slang_meaning(&self, term: &str, meaning: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE slang SET meaning=?2 WHERE term=?1 AND meaning=''",
            rusqlite::params![term, meaning],
        )?)
    }

    /// 某人在所有窗口里的最高好感（入群邀请 / 好友申请时看「她喜不喜欢这个人」）
    pub fn best_affinity(&self, uid: &str) -> Result<Option<f64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(score) FROM affinity WHERE uid=?1", rusqlite::params![uid], |r| r.get::<_, Option<f64>>(0))?)
    }

    // ---------------- 记忆账本：未压缩区 / 已压缩区 ----------------

    /// 这个人在 `scopes` 这些窗口里的往来（TA 说的 + 我回 TA 的），`since` 之后，最新的 `limit` 条（正序）
    pub fn trail_of(&self, uid: &str, scopes: &[String], since: i64, limit: usize) -> Result<Vec<RawLine>> {
        if scopes.is_empty() || uid.is_empty() {
            return Ok(Vec::new());
        }
        // 显式编号：裸 `?` 会按出现顺序编成 ?3、?4…，和后面的 LIMIT ?3 撞号
        let marks = (0..scopes.len()).map(|i| format!("?{}", i + 4)).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT scope,uid,sender,text,is_bot,ts FROM (
                 SELECT id,scope,uid,sender,text,is_bot,ts FROM messages
                 WHERE (uid=?1 OR reply_uid=?1) AND ts>=?2 AND scope IN ({marks})
                 ORDER BY id DESC LIMIT ?3
             ) ORDER BY id"
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(uid.to_string()), Box::new(since), Box::new(limit as i64)];
        for s in scopes {
            params.push(Box::new(s.clone()));
        }
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())), RawLine::from_row)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 这个人在所有窗口里的往来（夜间整理印象用），`since` 之后最新的 `limit` 条（正序）
    pub fn person_lines(&self, uid: &str, since: i64, limit: usize) -> Result<Vec<RawLine>> {
        let mut st = self.conn.prepare(
            "SELECT scope,uid,sender,text,is_bot,ts FROM (
                 SELECT id,scope,uid,sender,text,is_bot,ts FROM messages
                 WHERE (uid=?1 OR reply_uid=?1) AND ts>=?2 ORDER BY id DESC LIMIT ?3
             ) ORDER BY id",
        )?;
        let rows = st.query_map(rusqlite::params![uid, since, limit as i64], RawLine::from_row)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 某窗口最近的未压缩消息（正序）
    pub fn raw_recent(&self, scope: &str, limit: usize) -> Result<Vec<RawLine>> {
        let mut st = self.conn.prepare(
            "SELECT scope,uid,sender,text,is_bot,ts FROM (
                 SELECT id,scope,uid,sender,text,is_bot,ts FROM messages WHERE scope=?1 ORDER BY id DESC LIMIT ?2
             ) ORDER BY id",
        )?;
        let rows = st.query_map(rusqlite::params![scope, limit as i64], RawLine::from_row)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// `since` 以来说过话的人：(uid, 最近的昵称, 条数)，话多的在前
    pub fn active_people(&self, since: i64, limit: usize) -> Result<Vec<(String, String, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT uid, (SELECT sender FROM messages m2 WHERE m2.uid=m.uid ORDER BY id DESC LIMIT 1), COUNT(*) c
             FROM messages m WHERE uid<>'' AND is_bot=0 AND ts>=?1 GROUP BY uid ORDER BY c DESC LIMIT ?2",
        )?;
        let rows = st.query_map(rusqlite::params![since, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 已压缩区：取一条摘要（kind = person / …）
    pub fn digest(&self, kind: &str, key: &str) -> Result<Option<(String, i64)>> {
        let mut st = self.conn.prepare("SELECT text, updated FROM digest WHERE kind=?1 AND key=?2")?;
        let mut rows = st.query(rusqlite::params![kind, key])?;
        Ok(match rows.next()? {
            Some(r) => Some((r.get(0)?, r.get(1)?)),
            None => None,
        })
    }

    pub fn set_digest(&self, kind: &str, key: &str, text: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO digest(kind,key,text,updated) VALUES(?1,?2,?3,?4)
             ON CONFLICT(kind,key) DO UPDATE SET text=excluded.text, updated=excluded.updated",
            rusqlite::params![kind, key, text, crate::mind::now_ts()],
        )?;
        Ok(())
    }

    /// 释放未压缩区：删掉某窗口 `upto` 之前（已压缩进记忆）的消息，只留最近 `keep_tail` 条接话用
    pub fn free_raw(&self, scope: &str, upto: i64, keep_tail: usize) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM messages WHERE scope=?1 AND ts<=?2 AND id NOT IN
                 (SELECT id FROM messages WHERE scope=?1 ORDER BY id DESC LIMIT ?3)",
            rusqlite::params![scope, upto, keep_tail as i64],
        )?)
    }

    /// 同一个 QQ 号出现过的所有窗口：(scope, 昵称 / 群名片, 消息数, 最后出现)，最近的在前
    pub fn identities(&self, uid: &str) -> Result<Vec<(String, String, i64, i64)>> {
        let mut st = self.conn.prepare("SELECT scope,nick,msgs,last_ts FROM affinity WHERE uid=?1 ORDER BY last_ts DESC")?;
        let rows = st.query_map(rusqlite::params![uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// 主动小窗候选：私聊窗口里好感 ≥ min、聊过 ≥ min_msgs 条、且 idle_before 之后没再聊过的人
    pub fn dm_candidates(&self, min_score: f64, min_msgs: i64, idle_before: i64) -> Result<Vec<AffinityRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.scope,a.uid,a.nick,a.score,a.msgs,a.last_ts FROM affinity a
             WHERE a.scope LIKE 'qq-c2c-%' AND a.score>=?1 AND a.msgs>=?2
               AND COALESCE((SELECT MAX(ts) FROM messages m WHERE m.scope=a.scope), 0) < ?3
             ORDER BY a.score DESC LIMIT 20",
        )?;
        let rows = stmt.query_map(rusqlite::params![min_score, min_msgs, idle_before], |r| {
            Ok(AffinityRow {
                scope: r.get(0)?,
                uid: r.get(1)?,
                nick: r.get(2)?,
                score: r.get(3)?,
                msgs: r.get(4)?,
                last_ts: r.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn list_affinity(&self, scope: Option<&str>, limit: usize) -> Result<Vec<AffinityRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT scope,uid,nick,score,msgs,last_ts FROM affinity
             WHERE (?1 IS NULL OR scope=?1) ORDER BY score DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![scope, limit as i64], |r| {
            Ok(AffinityRow {
                scope: r.get(0)?,
                uid: r.get(1)?,
                nick: r.get(2)?,
                score: r.get(3)?,
                msgs: r.get(4)?,
                last_ts: r.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // ---------------- 总览 ----------------

    /// 总览统计（WebUI）
    pub fn counts(&self) -> Result<Counts> {
        let one = |sql: &str| -> i64 { self.conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        Ok(Counts {
            messages: one("SELECT COUNT(*) FROM messages"),
            memories: one("SELECT COUNT(*) FROM memories"),
            notes: one("SELECT COUNT(*) FROM notes"),
            scopes: one("SELECT COUNT(DISTINCT scope) FROM messages"),
            styles: one("SELECT COUNT(*) FROM style_samples"),
            slang: one("SELECT COUNT(*) FROM slang"),
            affinity: one("SELECT COUNT(*) FROM affinity"),
        })
    }

    /// 各对话渠道（scope）消息数，供 WebUI 展示
    /// 某时刻之后有消息的 scope
    pub fn scopes_active_since(&self, since: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT scope FROM messages WHERE ts>=?1")?;
        let rows = stmt.query_map(rusqlite::params![since], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn scopes_preview(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT scope, COUNT(*) FROM messages GROUP BY scope ORDER BY COUNT(*) DESC")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}
