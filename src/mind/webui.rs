//! 内嵌 axum WebUI：bot 进程内起一个本地 HTTP 服务（默认 127.0.0.1:8090），
//! 提供心智系统的可视化管理（总览/记忆/笔记/内心 + Phase 2 学习/图谱）。自包含，零额外服务。

use crate::mind::{heart, store::Store, HeartConfig, LearnConfig};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type SharedStore = Arc<Mutex<Store>>;
type AppState = SharedStore;

/// 启动 WebUI 服务器（后台任务）。绑 127.0.0.1 保证仅本机可访问。
pub fn spawn(port: u16, store: SharedStore, assets_dir: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        crate::mind::sysstat::set_assets_dir(assets_dir);
        let app = Router::new()
            .route("/", get(index))
            .route("/api/overview", get(overview))
            .route("/api/memories", get(list_memories))
            .route("/api/memory", post(add_memory))
            .route("/api/memory/:id", delete(del_memory))
            .route("/api/notes", get(list_notes))
            .route("/api/note", post(add_note))
            .route("/api/heart", get(heart_status))
            .route("/api/search", get(search))
            // Phase 2：学习层
            .route("/api/styles", get(list_styles))
            .route("/api/style/:id", delete(del_style))
            .route("/api/slang", get(list_slang).post(add_slang))
            .route("/api/slang/:id", delete(del_slang))
            .route("/api/candidates", get(list_candidates))
            .route("/api/affinity", get(list_affinity))
            .route("/api/graph", get(graph))
            .route("/api/system", get(system_stats))
            .route("/assets/bg/:n", get(asset_bg))
            .route("/api/stickers", get(list_stickers))
            .route("/assets/sticker/:file", get(asset_sticker))
            .route("/api/sticker/delete", post(del_sticker))
            // 认知（她对一件事的了解程度 / 自己的理解）与人物画像：可视化 + 纠错
            .route("/api/concepts", get(list_concepts))
            .route("/api/concept", post(correct_concept))
            .route("/api/concept/relearn", post(relearn_concept))
            .route("/api/concept/forget", post(forget_concept))
            .route("/api/corrections", get(list_corrections))
            .route("/api/correction", post(review_correction))
            .route("/api/personas", get(list_personas))
            .route("/api/persona/facet", post(add_facet))
            .route("/api/persona/facet/delete", post(del_facet))
            .with_state(store);

        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        // 重启时旧进程可能还没完全退出、端口还被占着（Windows 报 10013 / 10048）：等一会儿重试，别直接放弃
        for attempt in 1..=12u32 {
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    tracing::info!("心智 WebUI 已启动：http://{addr}");
                    let _ = axum::serve(listener, app).await;
                    return;
                }
                Err(e) if attempt < 12 => {
                    if attempt == 1 {
                        tracing::warn!("心智 WebUI 端口 {port} 暂时绑不上（{e}），每 5 秒重试一次，最多 1 分钟");
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                Err(e) => tracing::error!(
                    "心智 WebUI 启动失败（端口 {port} 一直被占用或在系统保留范围内）: {e}；可在 .env 改 QQ_BOT_MIND_WEBUI_PORT"
                ),
            }
        }
    })
}

async fn index() -> impl IntoResponse {
    Html(include_str!("dashboard.html"))
}

/// 进程/系统运行指标：已启动时间、CPU、内存占用、背景图来源
async fn system_stats() -> impl IntoResponse {
    let s = crate::mind::sysstat::snapshot();
    let dir = crate::mind::sysstat::assets_dir();
    let assets: Vec<&str> = (1..=3u8)
        .map(|n| {
            match crate::mind::sysstat::bg_candidates(n)
                .iter()
                .find(|f| dir.join(f).exists())
            {
                Some(f) if f.starts_with("bg") => "generated",
                Some(_) => "fallback",
                None => "missing",
            }
        })
        .collect();
    Json(json!({
        "pid": s.pid,
        "uptime_secs": s.uptime_secs,
        "started_at": s.started_at,
        "cpu_percent": s.cpu_percent,
        "mem_mb": s.mem_mb,
        "sys_mem_percent": s.sys_mem_percent,
        "sys_mem_used_mb": s.sys_mem_used_mb,
        "sys_mem_total_mb": s.sys_mem_total_mb,
        "version": env!("CARGO_PKG_VERSION"),
        "assets": assets,
    }))
}

/// 背景图：优先生图结果 `bgN.png`，回退人设图派生的 `fallbackN.png`
async fn asset_bg(Path(n): Path<u8>) -> impl IntoResponse {
    if !(1..=3).contains(&n) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let dir = crate::mind::sysstat::assets_dir();
    for name in crate::mind::sysstat::bg_candidates(n) {
        let path = dir.join(&name);
        if let Ok(bytes) = std::fs::read(&path) {
            return (
                [
                    (header::CONTENT_TYPE, "image/png"),
                    (header::CACHE_CONTROL, "public, max-age=60"),
                ],
                bytes,
            )
                .into_response();
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn overview(State(store): State<AppState>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let counts = store.counts().unwrap_or_default();
    let mut v = serde_json::to_value(&counts).unwrap_or_else(|_| json!({}));
    v["scope_list"] = json!(store.scopes_preview().unwrap_or_default());
    Json(v)
}

#[derive(Deserialize)]
struct MemQuery {
    q: Option<String>,
    limit: Option<usize>,
    scope: Option<String>,
}

async fn list_memories(State(store): State<AppState>, Query(q): Query<MemQuery>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let limit = q.limit.unwrap_or(50).min(500);
    let rows = match (&q.q, &q.scope) {
        (Some(needle), _) if !needle.trim().is_empty() => {
            let mut r = crate::mind::memory::bm25_rank(&store.all_memories().unwrap_or_default(), needle, limit);
            if let Some(sc) = &q.scope {
                r.retain(|(m, _)| m.scope == *sc);
            }
            r.into_iter().map(|(m, s)| payload_memory(&m, s)).collect::<Vec<_>>()
        }
        _ => store
            .all_memories()
            .unwrap_or_default()
            .into_iter()
            .filter(|m| q.scope.as_deref().map(|s| m.scope == s).unwrap_or(true))
            .take(limit)
            .map(|m| payload_memory(&m, 0.0))
            .collect::<Vec<_>>(),
    };
    Json(json!({ "memories": rows }))
}

fn payload_memory(m: &crate::mind::store::MemoryRow, score: f64) -> Value {
    json!({
        "id": m.id, "scope": m.scope, "conv": m.conv, "text": m.text, "tags": m.tags,
        "strength": m.strength, "kind": m.kind, "ts": m.ts, "score": (score * 100.0).round() as i64
    })
}

#[derive(Deserialize)]
struct NewMemory {
    text: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    conv: String,
    #[serde(default)]
    tags: String,
    #[serde(default = "default_strength")]
    strength: f64,
    #[serde(default = "default_kind")]
    kind: String,
}
fn default_strength() -> f64 { 80.0 }
fn default_kind() -> String { "knowledge".to_string() }

async fn add_memory(State(store): State<AppState>, Json(body): Json<NewMemory>) -> impl IntoResponse {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return Json(json!({ "ok": false, "error": "text 不能为空" }));
    }
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let id = store
        .insert_memory(&body.scope, &body.conv, &text, &body.tags, body.strength, &body.kind)
        .unwrap_or(-1);
    Json(json!({ "ok": true, "id": id }))
}

async fn del_memory(State(store): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let _ = store.delete_memory(id);
    Json(json!({ "ok": true }))
}

async fn list_notes(State(store): State<AppState>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let rows = store.all_notes().unwrap_or_default();
    Json(json!({ "notes": rows }))
}

#[derive(Deserialize)]
struct NewNote {
    title: String,
    content: String,
}

async fn add_note(State(store): State<AppState>, Json(body): Json<NewNote>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let id = store.insert_note(&body.title, &body.content).unwrap_or(-1);
    Json(json!({ "ok": true, "id": id }))
}

#[derive(Deserialize)]
struct HeartQuery {
    conv: Option<String>,
}

async fn heart_status(State(store): State<AppState>, Query(q): Query<HeartQuery>) -> impl IntoResponse {
    let cfg = HeartConfig::default();
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let conv = q.conv.unwrap_or_else(|| "default".into());
    let energy = heart::current_energy(&store, &conv, &cfg);
    let mood = heart::mood_label(energy);
    let suggest = heart::suggested_len(energy, cfg.soul.expression_desire);
    Json(json!({
        "conv": conv, "energy": energy.round(), "mood": mood,
        "recall_depth": cfg.soul.recall_depth, "expression_desire": cfg.soul.expression_desire,
        "creativity": cfg.soul.creativity, "suggested_len": suggest
    }))
}

async fn search(State(store): State<AppState>, Query(q): Query<MemQuery>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let needle = q.q.unwrap_or_default();
    let ranked = crate::mind::memory::bm25_rank(&store.all_memories().unwrap_or_default(), &needle, q.limit.unwrap_or(20).min(100));
    Json(json!({ "results": ranked.iter().map(|(m, s)| payload_memory(m, *s)).collect::<Vec<_>>() }))
}

// ---------------- Phase 2：学习层 API ----------------

#[derive(Deserialize)]
struct ScopeQuery {
    scope: Option<String>,
    limit: Option<usize>,
}

async fn list_styles(State(store): State<AppState>, Query(q): Query<ScopeQuery>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let limit = q.limit.unwrap_or(60).min(300);
    let mut rows = store.all_style_samples(limit).unwrap_or_default();
    if let Some(sc) = &q.scope {
        rows.retain(|r| &r.scope == sc);
    }
    Json(json!({ "styles": rows }))
}

async fn del_style(State(store): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let _ = store.delete_style_sample(id);
    Json(json!({ "ok": true }))
}

async fn list_slang(State(store): State<AppState>, Query(q): Query<ScopeQuery>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let rows = store.list_slang(q.scope.as_deref(), q.limit.unwrap_or(100).min(500)).unwrap_or_default();
    Json(json!({ "slang": rows }))
}

#[derive(Deserialize)]
struct NewSlang {
    term: String,
    #[serde(default)]
    meaning: String,
    #[serde(default)]
    scope: String,
}

async fn add_slang(State(store): State<AppState>, Json(body): Json<NewSlang>) -> impl IntoResponse {
    let term = body.term.trim().to_string();
    if term.is_empty() {
        return Json(json!({ "ok": false, "error": "term 不能为空" }));
    }
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let ok = store.upsert_slang(&body.scope, &term, body.meaning.trim()).is_ok();
    Json(json!({ "ok": ok }))
}

async fn del_slang(State(store): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let _ = store.delete_slang(id);
    Json(json!({ "ok": true }))
}

async fn list_candidates(State(store): State<AppState>, Query(q): Query<ScopeQuery>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let rows: Vec<Value> = store
        .list_candidates(q.scope.as_deref(), q.limit.unwrap_or(60).min(200))
        .unwrap_or_default()
        .into_iter()
        .map(|(scope, term, hits, users)| json!({ "scope": scope, "term": term, "hits": hits, "users": users }))
        .collect();
    Json(json!({ "candidates": rows }))
}

async fn list_affinity(State(store): State<AppState>, Query(q): Query<ScopeQuery>) -> impl IntoResponse {
    let cfg = LearnConfig::default();
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let rows: Vec<Value> = store
        .list_affinity(q.scope.as_deref(), q.limit.unwrap_or(100).min(500))
        .unwrap_or_default()
        .into_iter()
        .map(|a| {
            // 展示时套用时间衰减，和实际注入的口径一致
            let days = ((crate::mind::now_ts() - a.last_ts).max(0) as f64) / 86400.0;
            let score = (a.score - days * cfg.affinity_decay_per_day).max(0.0);
            json!({
                "scope": a.scope, "uid": a.uid, "nick": a.nick,
                "score": score.round(), "msgs": a.msgs, "last_ts": a.last_ts,
                "label": crate::mind::learn::affinity_label(score)
            })
        })
        .collect();
    Json(json!({ "affinity": rows }))
}

/// 知识图谱：scope / 用户 / 黑话 / 记忆 作为节点，归属关系作为边
async fn graph(State(store): State<AppState>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let mut nodes: Vec<Value> = Vec::new();
    let mut links: Vec<Value> = Vec::new();
    // 空 scope（例如 WebUI 手记的全局记忆）统一挂到「全局」节点，避免图谱出现孤岛
    let scope_id = |s: &str| -> String {
        if s.trim().is_empty() {
            "scope:全局".to_string()
        } else {
            format!("scope:{s}")
        }
    };

    let scopes = store.scopes_preview().unwrap_or_default();
    for (scope, n) in &scopes {
        nodes.push(json!({
            "id": format!("scope:{scope}"),
            "label": short_label(scope, 14),
            "kind": "scope",
            "weight": n,
        }));
    }
    for a in store.list_affinity(None, 200).unwrap_or_default() {
        let id = format!("user:{}|{}", a.scope, a.uid);
        let label = if a.nick.trim().is_empty() {
            short_label(&a.uid, 6)
        } else {
            a.nick.clone()
        };
        nodes.push(json!({
            "id": id, "label": label, "kind": "user",
            "weight": a.msgs, "detail": format!("好感度 {} · {} 条消息", a.score.round(), a.msgs),
        }));
        links.push(json!({ "source": id, "target": scope_id(&a.scope), "rel": "在" }));
    }
    for s in store.list_slang(None, 120).unwrap_or_default() {
        let id = format!("slang:{}|{}", s.scope, s.term);
        let detail = if s.meaning.trim().is_empty() {
            format!("{} 次 · 还没搞懂意思", s.hits)
        } else {
            format!("{} = {}（{} 次）", s.term, s.meaning, s.hits)
        };
        nodes.push(json!({
            "id": id, "label": s.term, "kind": "slang",
            "weight": s.hits, "detail": detail,
        }));
        links.push(json!({ "source": id, "target": scope_id(&s.scope), "rel": "出现" }));
    }
    for m in store.all_memories().unwrap_or_default().into_iter().take(50) {
        let id = format!("mem:{}", m.id);
        nodes.push(json!({
            "id": id, "label": short_label(&m.text, 16), "kind": "memory",
            "weight": m.strength, "detail": m.text,
        }));
        links.push(json!({ "source": id, "target": scope_id(&m.scope), "rel": "关于" }));
    }

    // 补齐只被边引用、还没有消息的 scope 节点（含「全局」）
    let mut need: Vec<String> = links
        .iter()
        .filter_map(|l| l["target"].as_str().map(String::from))
        .collect();
    need.sort();
    need.dedup();
    for id in need {
        if !nodes.iter().any(|n| n["id"].as_str() == Some(id.as_str())) {
            let label = id.trim_start_matches("scope:").to_string();
            nodes.push(json!({ "id": id, "label": label, "kind": "scope", "weight": 0 }));
        }
    }

    // 丢掉指向不存在节点的边（例如只有记忆、还没有消息的 scope）
    let ids: HashSet<&str> = nodes.iter().filter_map(|n| n["id"].as_str()).collect();
    links.retain(|l| {
        l["source"].as_str().map(|s| ids.contains(s)).unwrap_or(false)
            && l["target"].as_str().map(|s| ids.contains(s)).unwrap_or(false)
    });

    Json(json!({ "nodes": nodes, "links": links }))
}

fn short_label(s: &str, max: usize) -> String {
    let t: String = s.trim().chars().take(max).collect();
    if s.trim().chars().count() > max {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 0");
        l.local_addr().expect("addr").port()
    }

    #[test]
    fn sticker_mime_follows_extension() {
        assert_eq!(super::sticker_mime("happy.jpg"), "image/jpeg");
        assert_eq!(super::sticker_mime("HAPPY.JPEG"), "image/jpeg");
        assert_eq!(super::sticker_mime("old.png"), "image/png");
        assert_eq!(super::sticker_mime("a.gif"), "image/gif");
    }

    /// 起一个真实的 axum 服务，逐个打 Phase 2 新接口，确认 JSON 形状与数据。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn webui_serves_phase2_endpoints() {
        let dir = std::env::temp_dir().join(format!("mind-webui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Mutex::new(Store::open(&dir).expect("open store")));
        {
            let s = store.lock().unwrap();
            s.insert_message("scope1", "c1", "阿猫", "这把烂分真难受", false).unwrap();
            s.insert_memory("scope1", "c1", "用户偏好猫", "偏好", 80.0, "knowledge").unwrap();
            // 空 scope 的全局记忆应挂到「全局」节点，而不是变成孤岛
            s.insert_memory("", "c1", "全局手记", "", 60.0, "knowledge").unwrap();
            s.upsert_slang("scope1", "烂分", "打得很难受的局").unwrap();
            s.bump_affinity("scope1", "u1", "阿猫", 2.0, 50.0).unwrap();
            s.insert_style_sample("scope1", "c1", "在吗", "嗯，在。").unwrap();
        }

        let port = free_port();
        let assets = std::env::temp_dir().join(format!("heevi-webui-assets-{port}"));
        let _ = std::fs::create_dir_all(&assets);
        std::fs::write(assets.join("fallback1.png"), b"fake-png-bytes").unwrap();
        let handle = spawn(port, Arc::clone(&store), assets.clone());
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // 等服务器就绪（最多 3 秒）
        let mut ready = false;
        for _ in 0..30 {
            if client.get(format!("{base}/api/overview")).send().await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(ready, "WebUI 未在 3 秒内就绪");

        let overview: Value = client.get(format!("{base}/api/overview")).send().await.unwrap().json().await.unwrap();
        assert_eq!(overview["memories"], 2, "总览记忆数不对: {overview}");
        assert_eq!(overview["slang"], 1, "总览黑话数不对: {overview}");
        assert_eq!(overview["affinity"], 1, "总览好感档案数不对: {overview}");

        let styles: Value = client.get(format!("{base}/api/styles")).send().await.unwrap().json().await.unwrap();
        assert_eq!(styles["styles"][0]["bot_text"], "嗯，在。", "风格样本接口异常: {styles}");

        let slang: Value = client.get(format!("{base}/api/slang")).send().await.unwrap().json().await.unwrap();
        assert_eq!(slang["slang"][0]["term"], "烂分", "黑话接口异常: {slang}");

        let aff: Value = client.get(format!("{base}/api/affinity")).send().await.unwrap().json().await.unwrap();
        assert_eq!(aff["affinity"][0]["nick"], "阿猫", "好感度接口异常: {aff}");
        assert!(
            !aff["affinity"][0]["label"].as_str().unwrap_or("").is_empty(),
            "好感度文案缺失: {aff}"
        );

        let g: Value = client.get(format!("{base}/api/graph")).send().await.unwrap().json().await.unwrap();
        let ids: Vec<String> = g["nodes"]
            .as_array()
            .expect("nodes 数组")
            .iter()
            .map(|n| n["id"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(ids.iter().any(|i| i.starts_with("scope:")), "图谱缺 scope 节点: {ids:?}");
        assert!(ids.iter().any(|i| i.starts_with("slang:")), "图谱缺黑话节点: {ids:?}");
        assert!(ids.iter().any(|i| i.starts_with("mem:")), "图谱缺记忆节点: {ids:?}");
        assert!(ids.iter().any(|i| i.starts_with("user:")), "图谱缺用户节点: {ids:?}");
        assert!(!g["links"].as_array().unwrap().is_empty(), "图谱无边");
        assert!(ids.iter().any(|i| i == "scope:全局"), "空 scope 记忆未挂到全局节点: {ids:?}");
        assert!(
            g["links"].as_array().unwrap().iter().any(|l| l["target"] == "scope:全局"),
            "全局边缺失: {g}"
        );

        let html = client.get(format!("{base}/")).send().await.unwrap().text().await.unwrap();
        assert!(html.contains("Heevi Mind"), "首页未渲染");
        assert!(html.contains("图谱"), "首页缺少图谱页签");
        assert!(html.contains("|_|"), "首页缺少 ASCII 横幅");

        let sys: Value = client
            .get(format!("{base}/api/system"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sys["pid"], std::process::id(), "系统接口 pid 不对: {sys}");
        assert!(sys["uptime_secs"].is_u64(), "系统接口缺运行时长: {sys}");
        assert!(sys["mem_mb"].is_number(), "系统接口缺内存占用: {sys}");
        assert_eq!(sys["assets"].as_array().unwrap().len(), 3, "背景图状态缺失: {sys}");

        let bg = client.get(format!("{base}/assets/bg/1")).send().await.unwrap();
        assert_eq!(bg.status(), axum::http::StatusCode::OK, "背景图路由未命中");
        assert_eq!(bg.headers()["content-type"], "image/png", "背景图 Content-Type 不对");
        let bad = client.get(format!("{base}/assets/bg/9")).send().await.unwrap();
        assert_eq!(bad.status(), axum::http::StatusCode::NOT_FOUND, "越界序号应 404");
        let _ = std::fs::remove_dir_all(&assets);

        handle.abort();
    }
}

/// 表情包库目录（mind_data/stickers，与 bot 的 QQ_BOT_STICKER_DIR 默认一致）
fn stickers_dir() -> PathBuf {
    crate::mind::sysstat::assets_dir()
        .parent()
        .map(|p| p.join("stickers"))
        .unwrap_or_else(|| PathBuf::from("mind_data/stickers"))
}

/// 表情包库列表（标签 / 情绪 / 使用次数 / 最近使用时间）
async fn list_stickers() -> impl IntoResponse {
    let dir = stickers_dir();
    let raw = std::fs::read_to_string(dir.join("index.json")).unwrap_or_else(|_| "{}".into());
    let mut items: Vec<Value> = Vec::new();
    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
        if let Some(map) = v.get("items").and_then(|m| m.as_object()) {
            for (id, st) in map {
                let file = st.get("file").and_then(|f| f.as_str()).unwrap_or("");
                if file.is_empty() || !dir.join(file).exists() {
                    continue;
                }
                let mut o = st.clone();
                o["id"] = json!(id);
                items.push(o);
            }
        }
    }
    items.sort_by_key(|o| {
        o.get("label")
            .and_then(|l| l.as_str())
            .unwrap_or("")
            .to_string()
    });
    Json(json!({ "count": items.len(), "items": items }))
}

/// 表情包图片（文件名只允许字母数字与 . _ -）
async fn asset_sticker(Path(file): Path<String>) -> impl IntoResponse {
    let ok = !file.is_empty()
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if !ok {
        return StatusCode::NOT_FOUND.into_response();
    }
    match std::fs::read(stickers_dir().join(&file)) {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, sticker_mime(&file)),
                (header::CACHE_CONTROL, "public, max-age=300"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 按扩展名给表情包图片定 Content-Type（贴纸库已从 PNG 改为 JPG，两种都要认）
fn sticker_mime(file: &str) -> &'static str {
    let lower = file.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        "image/png"
    }
}

#[derive(Deserialize)]
struct DelSticker {
    id: String,
}

/// 删除一张表情包（图片文件 + 索引项）
async fn del_sticker(Json(body): Json<DelSticker>) -> impl IntoResponse {
    let dir = stickers_dir();
    let idx_path = dir.join("index.json");
    let raw = std::fs::read_to_string(&idx_path).unwrap_or_else(|_| "{}".into());
    let mut v: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    let file = v
        .get("items")
        .and_then(|m| m.get(&body.id))
        .and_then(|s| s.get("file"))
        .and_then(|f| f.as_str())
        .map(|s| s.to_string());
    let Some(file) = file else {
        return Json(json!({ "ok": false, "error": "未找到这张表情包" }));
    };
    let _ = std::fs::remove_file(dir.join(&file));
    if let Some(items) = v.get_mut("items").and_then(|m| m.as_object_mut()) {
        items.remove(&body.id);
    }
    if let Ok(s) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(&idx_path, s);
    }
    Json(json!({ "ok": true }))
}

// ================= 认知 / 画像 =================

use crate::mind::insight;

async fn list_concepts(State(store): State<AppState>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "concepts": insight::concept_views(&store) }))
}

#[derive(Deserialize)]
struct ConceptFix {
    term: String,
    #[serde(default)]
    text: String,
}

/// 后台纠正 = 主人纠正：直接生效
async fn correct_concept(State(store): State<AppState>, Json(b): Json<ConceptFix>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let r = insight::correct_concept(&store, b.term.trim(), &b.text, true, "webui", "主人（后台）");
    Json(json!({ "ok": r == insight::Corrected::Applied, "result": format!("{r:?}") }))
}

async fn relearn_concept(State(store): State<AppState>, Json(b): Json<ConceptFix>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    insight::relearn_concept(&store, b.term.trim());
    Json(json!({ "ok": true }))
}

async fn forget_concept(State(store): State<AppState>, Json(b): Json<ConceptFix>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    insight::forget_concept(&store, b.term.trim());
    Json(json!({ "ok": true }))
}

async fn list_corrections(State(store): State<AppState>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let rows: Vec<Value> = insight::pending_corrections(&store)
        .into_iter()
        .map(|(id, term, text, by, ts)| json!({ "id": id, "term": term, "text": text, "by": by, "ts": ts }))
        .collect();
    Json(json!({ "corrections": rows }))
}

#[derive(Deserialize)]
struct Review {
    id: i64,
    approve: bool,
}

async fn review_correction(State(store): State<AppState>, Json(b): Json<Review>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": insight::review_correction(&store, b.id, b.approve) }))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}

async fn list_personas(State(store): State<AppState>, Query(q): Query<LimitQuery>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "personas": insight::persona_views(&store, q.limit.unwrap_or(60).min(300)) }))
}

#[derive(Deserialize)]
struct FacetEdit {
    scope: String,
    uid: String,
    #[serde(default)]
    facet: Option<String>,
    #[serde(default)]
    value: String,
}

async fn add_facet(State(store): State<AppState>, Json(b): Json<FacetEdit>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let ok = insight::owner_add_facet(&store, &b.scope, &b.uid, b.facet.as_deref().unwrap_or(""), &b.value);
    Json(json!({ "ok": ok }))
}

async fn del_facet(State(store): State<AppState>, Json(b): Json<FacetEdit>) -> impl IntoResponse {
    let store = store.lock().unwrap_or_else(|e| e.into_inner());
    let n = insight::delete_facet(&store, &b.scope, &b.uid, b.facet.as_deref().filter(|f| !f.is_empty()), &b.value);
    Json(json!({ "ok": n > 0, "deleted": n }))
}
