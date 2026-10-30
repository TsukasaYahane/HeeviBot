//! 表情差分贴纸库：AI 生成的 Heevi 表情包（scripts/gen_stickers.py 产出），
//! 运行时按消息情绪挑一张，在合适时机随回复发出。
//!
//! 数据布局（与生成脚本共享）：
//! ```text
//! mind_data/stickers/
//!   index.json     { "version":1, "items": { "<id>": {file,label,mood,desc,source,created_at,uses,last_used} } }
//!   happy.jpg      # 白底 JPG（512px，约 55KB；旧库是透明 PNG，原图备份在 mind_data/stickers_png_backup）
//!   ...
//! ```
//! 发送通道：直接读本地图片字节，以 `base64://` 图片段交给 LLBot（OneBot 11）发出，不再经过图床。

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 一条贴纸元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sticker {
    /// 索引键（= 文件名去扩展名），由 index.json 的 key 回填
    #[serde(default, skip_deserializing)]
    pub id: String,
    pub file: String,
    pub label: String,
    pub mood: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub uses: u64,
    #[serde(default)]
    pub last_used: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    items: HashMap<String, Sticker>,
}

#[derive(Default)]
struct Inner {
    items: Vec<Sticker>,
    mtime: Option<SystemTime>,
    /// 上一次挑中的贴纸 id（避免同一张连着发）
    last_pick: Option<String>,
}

/// 表情贴纸库
pub struct StickerStore {
    dir: PathBuf,
    index_path: PathBuf,
    inner: Mutex<Inner>,
}

impl StickerStore {
    /// 打开（不存在则创建空库；不报错，调用方按空库降级）
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let me = Self {
            dir: dir.to_path_buf(),
            index_path: dir.join("index.json"),
            inner: Mutex::new(Inner::default()),
        };
        me.reload();
        Ok(me)
    }

    /// 重新读盘（index.json 变化时才真正解析）
    pub fn reload(&self) {
        let mtime = std::fs::metadata(&self.index_path).ok().and_then(|m| m.modified().ok());
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if mtime.is_some() && mtime == inner.mtime && !inner.items.is_empty() {
            return;
        }
        let mut items: Vec<Sticker> = Vec::new();
        if let Ok(raw) = std::fs::read_to_string(&self.index_path) {
            match serde_json::from_str::<Index>(&raw) {
                Ok(mut idx) => {
                    for (id, mut st) in idx.items.drain() {
                        st.id = id;
                        if st.file.is_empty() {
                            // 没写文件名：按现有文件推断（贴纸库现在是 JPG，旧库是 PNG）
                            st.file = ["jpg", "png"]
                                .iter()
                                .map(|ext| format!("{}.{ext}", st.id))
                                .find(|f| self.dir.join(f).exists())
                                .unwrap_or_else(|| format!("{}.jpg", st.id));
                        }
                        if !self.dir.join(&st.file).exists() {
                            warn!("贴纸文件缺失，跳过：{}", self.dir.join(&st.file).display());
                            continue;
                        }
                        items.push(st);
                    }
                }
                Err(e) => warn!("表情包索引解析失败（{}）：{e}", self.index_path.display()),
            }
        }
        items.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
        inner.items = items;
        inner.mtime = mtime;
        info!("表情包库已加载：{} 张（{}）", inner.items.len(), self.dir.display());
    }

    pub fn all(&self) -> Vec<Sticker> {
        self.reload();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .clone()
    }

    pub fn count(&self) -> usize {
        self.reload();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// 按消息文本挑一张贴纸：优先情绪命中，其次随机；尽量不与上一次重复
    pub fn pick(&self, hint: &str) -> Option<Sticker> {
        let items = self.all();
        if items.is_empty() {
            return None;
        }
        let moods = moods_for_hint(hint);
        let mut pool: Vec<Sticker> = if moods.is_empty() {
            items.clone()
        } else {
            let mut best = 0usize;
            let mut scored: Vec<(usize, &Sticker)> = items
                .iter()
                .map(|s| {
                    let sc = moods
                        .iter()
                        .position(|m| *m == s.mood)
                        .map(|i| 100 - i * 5)
                        .unwrap_or(0);
                    best = best.max(sc);
                    (sc, s)
                })
                .collect();
            if best == 0 {
                items.clone()
            } else {
                scored.retain(|(sc, _)| *sc == best);
                scored.into_iter().map(|(_, s)| s.clone()).collect()
            }
        };
        if pool.len() > 1 {
            let last = self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_pick
                .clone();
            let filtered: Vec<Sticker> =
                pool.iter().filter(|s| Some(&s.id) != last.as_ref()).cloned().collect();
            if !filtered.is_empty() {
                pool = filtered;
            }
        }
        let idx = (now_nanos() as usize) % pool.len();
        let picked = pool[idx].clone();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_pick = Some(picked.id.clone());
        Some(picked)
    }

    /// 读取贴纸图片字节（路径做防穿越校验）
    pub fn bytes(&self, id: &str) -> Result<Vec<u8>> {
        let items = self.all();
        let st = items
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| anyhow!("贴纸不存在：{id}"))?;
        let path = self.safe_path(&st.file)?;
        Ok(std::fs::read(path)?)
    }

    /// 贴纸文件的绝对路径（WebUI 提供图片用）
    pub fn safe_path(&self, file: &str) -> Result<PathBuf> {
        if file.contains("..") || file.contains('/') || file.contains('\\') {
            return Err(anyhow!("非法贴纸文件名：{file}"));
        }
        Ok(self.dir.join(file))
    }

    /// 记一次使用（写回 index.json）
    pub fn mark_used(&self, id: &str) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(st) = inner.items.iter_mut().find(|s| s.id == id) {
                st.uses += 1;
                st.last_used = now_ts();
            }
        }
        self.persist();
    }

    /// 删除一张贴纸（文件 + 索引）
    #[allow(dead_code)]
    pub fn delete(&self, id: &str) -> Result<bool> {
        let items = self.all();
        let Some(st) = items.iter().find(|s| s.id == id) else {
            return Ok(false);
        };
        let path = self.safe_path(&st.file)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.items.retain(|s| s.id != id);
            inner.mtime = None; // 强制下次读盘
        }
        self.persist();
        Ok(true)
    }

    fn persist(&self) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut idx = Index {
            version: 1,
            items: HashMap::new(),
        };
        for st in &inner.items {
            idx.items.insert(st.id.clone(), st.clone());
        }
        drop(inner);
        match serde_json::to_string_pretty(&idx) {
            Ok(raw) => {
                let tmp = self.index_path.with_extension("json.tmp");
                if let Err(e) = std::fs::write(&tmp, raw).and_then(|_| std::fs::rename(&tmp, &self.index_path)) {
                    warn!("表情包索引写盘失败：{e}");
                }
            }
            Err(e) => warn!("表情包索引序列化失败：{e}"),
        }
    }

    /// WebUI 用：索引快照（数组，已排序）
    #[allow(dead_code)]
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({ "items": self.all(), "count": self.count() })
    }
}

/// 消息文本 → 情绪键（按匹配到的关键词数量/顺序给优先级）
pub fn moods_for_hint(hint: &str) -> Vec<&'static str> {
    let h = hint.to_lowercase();
    let mut out: Vec<&'static str> = Vec::new();
    for (mood, keys) in MOOD_HINTS {
        if keys.iter().any(|k| h.contains(*k)) {
            out.push(mood);
        }
    }
    out
}

/// 情绪 → 触发关键词（顺序即优先级：越靠前越"强"）
const MOOD_HINTS: &[(&str, &[&str])] = &[
    ("哭", &["哭", "呜呜", "泪", "裂开", "绷不住", "惨"]),
    ("生气", &["生气", "气死", "滚", "烦死", "讨厌", "骂"]),
    ("害羞", &["害羞", "不好意思", "脸红", "羞", "喜欢你", "表白"]),
    ("惊讶", &["卧槽", "我靠", "震惊", "真的假的", "居然", "竟然", "天呐"]),
    ("无语", &["无语", "……", "沉默", "服了", "麻了", "离谱", "什么鬼"]),
    ("疑惑", &["?", "？", "疑惑", "为啥", "为什么", "怎么", "啥"]),
    ("委屈", &["委屈", "难受", "不开心", "emo", "心累", "唉"]),
    ("困", &["困", "睡觉", "晚安", "熬夜", "累了"]),
    ("加油", &["加油", "厉害", "好强", "牛", "顶", "赞", "好活", "nb"]),
    ("得意", &["得意", "嘿嘿", "哼", "赢了", "简单", "稳了"]),
    ("开心", &["哈哈", "笑死", "hhh", "开心", "高兴", "好耶", "太好了", "草"]),
    ("打招呼", &["你好", "早上好", "在吗", "来了", "hi", "hello"]),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(dir: &Path, items: &[(&str, &str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut map = serde_json::Map::new();
        for (id, label, mood) in items {
            std::fs::write(dir.join(format!("{id}.png")), b"PNG").unwrap();
            map.insert(
                (*id).to_string(),
                serde_json::json!({
                    "file": format!("{id}.png"),
                    "label": label,
                    "mood": mood,
                    "desc": "",
                    "source": "ai",
                    "created_at": 0,
                    "uses": 0,
                    "last_used": 0
                }),
            );
        }
        let idx = serde_json::json!({ "version": 1, "items": map });
        std::fs::write(dir.join("index.json"), serde_json::to_string(&idx).unwrap()).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("heevi_stickers_test_{tag}_{}", now_nanos()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// JPG 贴纸：显式写 .jpg 的能读；没写文件名的按现有文件推断扩展名
    #[test]
    fn jpg_stickers_load_and_infer_extension() {
        let dir = temp_dir("jpg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("happy.jpg"), b"JPG").unwrap();
        std::fs::write(dir.join("sad.jpg"), b"JPG").unwrap();
        std::fs::write(dir.join("old.png"), b"PNG").unwrap();
        let idx = serde_json::json!({ "version": 1, "items": {
            "happy": {"file": "happy.jpg", "label": "开心", "mood": "开心"},
            "sad": {"file": "", "label": "难过", "mood": "难过"},
            "old": {"file": "", "label": "旧", "mood": "旧"},
            "gone": {"file": "gone.png", "label": "缺", "mood": "缺"}
        }});
        std::fs::write(dir.join("index.json"), idx.to_string()).unwrap();
        let s = StickerStore::open(&dir).unwrap();
        assert_eq!(s.count(), 3, "缺文件的那张应被跳过");
        let files: HashMap<String, String> = s.all().into_iter().map(|st| (st.id, st.file)).collect();
        assert_eq!(files["happy"], "happy.jpg");
        assert_eq!(files["sad"], "sad.jpg", "空文件名应推断出 .jpg");
        assert_eq!(files["old"], "old.png", "只有 PNG 时仍能回退");
        assert_eq!(s.bytes("happy").unwrap(), b"JPG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实贴纸库自检：index.json 里每一项的文件都在、能解码、确实是 JPG
    #[test]
    fn real_sticker_library_is_valid_jpg() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("mind_data").join("stickers");
        if !dir.join("index.json").exists() {
            return; // 没有本地贴纸库（如别的机器上）就跳过
        }
        let s = StickerStore::open(&dir).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("index.json")).unwrap()).unwrap();
        let declared = raw["items"].as_object().map(|m| m.len()).unwrap_or(0);
        assert_eq!(s.count(), declared, "index.json 里有贴纸找不到文件");
        for st in s.all() {
            assert!(st.file.ends_with(".jpg"), "{} 还不是 JPG：{}", st.id, st.file);
            let bytes = s.bytes(&st.id).unwrap();
            assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF], "{} 文件头不是 JPEG", st.file);
            let img = image::load_from_memory(&bytes).expect("JPG 能解码");
            assert!(img.width() > 0 && img.height() > 0);
        }
    }

    #[test]
    fn loads_and_counts() {
        let dir = temp_dir("load");
        write_fixture(&dir, &[("happy", "开心", "开心"), ("cry", "哭", "哭")]);
        let s = StickerStore::open(&dir).unwrap();
        assert_eq!(s.count(), 2);
        assert_eq!(s.all()[0].id, "cry", "按 label 排序");
        assert_eq!(s.bytes("happy").unwrap(), b"PNG");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mood_matching_prefers_hint() {
        assert_eq!(moods_for_hint("笑死我了哈哈"), vec!["开心"]);
        assert!(moods_for_hint("？？？").contains(&"疑惑"));
        assert!(moods_for_hint("今天心情不错").is_empty());

        let dir = temp_dir("mood");
        write_fixture(&dir, &[("happy", "开心", "开心"), ("cry", "哭", "哭"), ("shy", "害羞", "害羞")]);
        let s = StickerStore::open(&dir).unwrap();
        for _ in 0..8 {
            let st = s.pick("呜呜呜我裂开了").expect("应能挑到");
            assert_eq!(st.mood, "哭", "情绪命中优先：{}", st.label);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pick_avoids_repeat_when_possible() {
        let dir = temp_dir("repeat");
        write_fixture(&dir, &[("a", "开心", "开心"), ("b", "加油", "加油")]);
        let s = StickerStore::open(&dir).unwrap();
        let first = s.pick("").unwrap();
        let second = s.pick("").unwrap();
        assert_ne!(first.id, second.id, "两次随机不应连发同一张");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mark_used_and_delete() {
        let dir = temp_dir("used");
        write_fixture(&dir, &[("happy", "开心", "开心")]);
        let s = StickerStore::open(&dir).unwrap();
        s.mark_used("happy");
        let st = s.all().into_iter().find(|x| x.id == "happy").unwrap();
        assert_eq!(st.uses, 1);
        assert!(st.last_used > 0);
        assert!(s.delete("happy").unwrap());
        assert_eq!(s.count(), 0);
        assert!(!s.delete("happy").unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = temp_dir("traversal");
        let s = StickerStore::open(&dir).unwrap();
        assert!(s.safe_path("../evil.png").is_err());
        assert!(s.safe_path("a/b.png").is_err());
        assert!(s.safe_path("ok.png").is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
