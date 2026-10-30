//! VLR.gg 与 lolesports.com（LoL 官方）数据源。
//! VLR 使用公开 HTML（遵守 robots.txt 与低频率轮询）；LOL 使用 lolesports 网关 API（getSchedule）。
//! 系列赛=league.name，赛段=blockName，状态=state，去重= match.id。
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Provider { Vlr, Lol }

impl Provider {
    pub fn name(self) -> &'static str { match self { Self::Vlr => "VLR", Self::Lol => "LOL" } }
    pub fn prefix(self) -> &'static str { match self { Self::Vlr => "vlr", Self::Lol => "lol" } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Match {
    pub provider: Provider,
    pub id: String,
    pub timestamp: String,
    pub team_a: String,
    pub team_b: String,
    #[serde(default)]
    pub code_a: String,
    #[serde(default)]
    pub code_b: String,
    pub score_a: Option<u8>,
    pub score_b: Option<u8>,
    pub event: String,
    pub stage: String,
    pub url: String,
    pub finished: bool,
}

impl Match {
    pub fn scope_stage(&self) -> String { if !self.stage.is_empty() { self.stage.clone() } else { self.event.clone() } }

    pub fn key(&self) -> String { format!("{}:{}", self.provider.prefix(), self.id) }
    pub fn line(&self) -> String {
        let score = match (self.score_a, self.score_b) { (Some(a), Some(b)) => format!("{a} : {b}"), _ => "待开赛".to_string() };
        let date = if self.timestamp.len() >= 10 { &self.timestamp[..10] } else { "-" };
        format!("{} {} {} {} · 📅 {}", if self.finished { "🏁 赛报" } else { "⏳ 赛程" }, self.team_a, score, self.team_b, date)
    }

    pub fn text(&self) -> String {
        let score = match (self.score_a, self.score_b) {
            (Some(a), Some(b)) => format!("{} : {}", a, b),
            _ => "待开赛".to_string(),
        };
        let stage = if self.stage.is_empty() || self.stage == self.event { String::new() } else { format!(" · {}", self.stage) };
        format!("{} {}\n🆚 {} {} {}\n📅 {}\n🎪 {}{}\n🔗 {}",
            match self.provider { Provider::Vlr => "🎯 瓦洛兰特", Provider::Lol => "⚔️ 英雄联盟" },
            if self.finished { "赛报" } else { "赛程" }, self.team_a, score, self.team_b,
            self.timestamp, self.event, stage, self.url)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription { pub provider: Provider, pub channel: String, pub team: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEntry { pub provider: Provider, pub channel: String, pub team: String, pub match_id: String, pub stage: String }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct State { subscriptions: Vec<Subscription>, pushed: Vec<String>, watching: Vec<WatchEntry> }

pub struct Store { path: PathBuf, state: State }
impl Store {
    pub fn load(path: PathBuf) -> Self {
        let state = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self { path, state }
    }
    fn save(&self) { let _ = self.path.parent().map(std::fs::create_dir_all); if let Ok(s)=serde_json::to_string_pretty(&self.state) { let _=std::fs::write(&self.path,s); } }
    pub fn add(&mut self, provider: Provider, channel: String, team: String) -> bool {
        if self.state.subscriptions.iter().any(|s| s.provider==provider && s.channel==channel && s.team.eq_ignore_ascii_case(&team)) { return false; }
        self.state.subscriptions.push(Subscription { provider, channel, team }); self.save(); true
    }
    pub fn remove(&mut self, provider: Provider, channel: &str, team: &str) -> bool {
        let n=self.state.subscriptions.len(); self.state.subscriptions.retain(|s| !(s.provider==provider && s.channel==channel && s.team.eq_ignore_ascii_case(team))); let yes=n!=self.state.subscriptions.len(); if yes {self.save();} yes
    }
    pub fn list(&self, provider: Provider, channel: &str) -> Vec<String> { self.state.subscriptions.iter().filter(|s|s.provider==provider&&s.channel==channel).map(|s|s.team.clone()).collect() }
    #[allow(dead_code)]
    pub fn targets(&self, provider: Provider, team: &str) -> Vec<String> { self.state.subscriptions.iter().filter(|s|s.provider==provider&&s.team.eq_ignore_ascii_case(team)).map(|s|s.channel.clone()).collect() }
    #[allow(dead_code)]
    pub fn teams(&self, provider: Provider) -> Vec<String> { let mut v:Vec<_>=self.state.subscriptions.iter().filter(|s|s.provider==provider).map(|s|s.team.clone()).collect(); v.sort();v.dedup();v }
    pub fn pushed(&self,key:&str)->bool { self.state.pushed.iter().any(|k|k==key) }
    pub fn mark(&mut self,key:String) { if !self.pushed(&key) {self.state.pushed.push(key); if self.state.pushed.len()>3000 {let d=self.state.pushed.len()-2000;self.state.pushed.drain(..d);}self.save();} }

    pub fn sub_pairs(&self, provider: Provider) -> Vec<(String,String)> {
        let mut v:Vec<_> = self.state.subscriptions.iter()
            .filter(|s| s.provider==provider)
            .map(|s| (s.channel.clone(), s.team.clone())).collect();
        v.sort(); v.dedup(); v
    }
    pub fn watched(&self, provider: Provider, channel: &str, team: &str, match_id: &str) -> bool {
        self.state.watching.iter().any(|w| w.provider==provider && w.channel==channel && w.team.eq_ignore_ascii_case(team) && w.match_id==match_id)
    }
    pub fn add_watch(&mut self, provider: Provider, channel: &str, team: &str, match_id: &str, stage: &str) {
        if self.watched(provider, channel, team, match_id) { return; }
        self.state.watching.push(WatchEntry{provider, channel:channel.to_string(), team:team.to_string(), match_id:match_id.to_string(), stage:stage.to_string()});
        if self.state.watching.len()>2000 { let d=self.state.watching.len()-1000; self.state.watching.drain(..d); }
        self.save();
    }
    pub fn remove_watch(&mut self, provider: Provider, channel: &str, team: &str, match_id: &str) {
        let n=self.state.watching.len();
        self.state.watching.retain(|w| !(w.provider==provider && w.channel==channel && w.team.eq_ignore_ascii_case(team) && w.match_id==match_id));
        if n!=self.state.watching.len() { self.save(); }
    }
}

fn client() -> reqwest::Client { reqwest::Client::builder().user_agent("HeeviBot/1.0 (esports; respectful low-rate client)").gzip(true).timeout(Duration::from_secs(25)).build().unwrap() }

pub async fn vlr_matches() -> Result<Vec<Match>> {
    let mut all = Vec::new();
    for path in ["https://www.vlr.gg/matches", "https://www.vlr.gg/matches/results"] {
        let h = client().get(path).send().await?.error_for_status()?.text().await?;
        all.extend(parse_vlr(&h, path.ends_with("/results"))?);
    }
    Ok(all)
}
fn strip(s:&str)->String { let r=regex::Regex::new(r"<[^>]+>").unwrap(); regex::Regex::new(r"\s+").unwrap().replace_all(&r.replace_all(s,"").replace("&nbsp;"," ").replace("&ndash;","-")," ").trim().to_string() }
fn attr(block:&str, name:&str)->String { let re=regex::Regex::new(&format!(r#"{}="([^"]*)""#,regex::escape(name))).unwrap(); re.captures(block).and_then(|c|c.get(1)).map(|m|m.as_str().to_string()).unwrap_or_default() }
fn parse_vlr(h:&str, is_results:bool)->Result<Vec<Match>> {
    let row=regex::Regex::new(r#"(?s)<a[^>]*class="[^"]*match-item[^"]*"[^>]*>(.*?)</a>"#).unwrap();
    let mut out=Vec::new();
    for c in row.captures_iter(h) { let b=&c[0]; let href=attr(b,"href"); let id=href.split('/').find(|x|!x.is_empty()&&x.chars().all(|c|c.is_ascii_digit())).unwrap_or(""); if id.is_empty(){continue;}
        let teams:Vec<String>=regex::Regex::new(r#"(?s)class="[^"]*match-item-vs-team-name[^"]*"[^>]*>(.*?)</div>"#).unwrap().captures_iter(b).map(|x|strip(&x[1])).filter(|x|!x.is_empty()).collect(); if teams.len()<2{continue;}
        let scores:Vec<Option<u8>>=regex::Regex::new(r#"(?s)class="[^"]*match-item-vs-team-score[^"]*"[^>]*>(.*?)</[^>]+>"#).unwrap().captures_iter(b).map(|x|strip(&x[1]).parse().ok()).collect();
        let time=regex::Regex::new(r#"(?s)class="[^"]*match-item-time[^"]*"[^>]*>(.*?)</[^>]+>"#).unwrap().captures(b).map(|x|strip(&x[1])).unwrap_or_default();
        let event=regex::Regex::new(r#"(?s)class="[^"]*match-item-event[^"]*"[^>]*>(.*?)</[^>]+>"#).unwrap().captures(b).map(|x|strip(&x[1])).unwrap_or_default();
        let finished=is_results;
        out.push(Match{provider:Provider::Vlr,id:id.into(),timestamp:time,team_a:teams[0].clone(),team_b:teams[1].clone(),code_a:String::new(),code_b:String::new(),score_a:scores.get(0).cloned().flatten(),score_b:scores.get(1).cloned().flatten(),event:event.clone(),stage:event.clone(),url:format!("https://www.vlr.gg{}",href),finished});
    } Ok(out)
}


pub async fn lpl_matches() -> Result<Vec<Match>> {
    let key = std::env::var("LOL_API_KEY").context("LOL_API_KEY 未设置（lolesports 网关）")?;
    let url = "https://esports-api.lolesports.com/persisted/gw/getSchedule?hl=en-US&cacheBust=0";
    let v: Value = client().get(url)
        .header("x-api-key", key)
        .header("Origin", "https://lolesports.com")
        .header("Referer", "https://lolesports.com/")
        .send().await?.error_for_status()?.json().await.context("lolesports schedule")?;
    let arr = v.get("data").and_then(|d| d.get("schedule")).and_then(|s| s.get("events")).and_then(|e| e.as_array()).cloned()
        .ok_or_else(|| anyhow!("lolesports 响应无 events"))?;
    Ok(arr.into_iter().filter_map(|e| {
        let m = e.get("match")?;
        let id = m.get("id")?.as_str().unwrap_or("").to_string();
        if id.is_empty() { return None; }
        let te = m.get("teams")?.as_array()?;
        if te.len() < 2 { return None; }
        let tn = |i: usize| te.get(i).and_then(|t| t.get("name")).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let tc = |i: usize| te.get(i).and_then(|t| t.get("code")).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let sc = |i: usize| te.get(i).and_then(|t| t.get("result")).and_then(|r| r.get("gameWins")).and_then(|x| x.as_u64()).map(|n| n as u8);
        let state = e.get("state").and_then(|x| x.as_str()).unwrap_or("");
        let finished = state == "completed";
        let league = e.get("league").and_then(|x| x.get("name")).and_then(|x| x.as_str()).unwrap_or("");
        let block = e.get("blockName").and_then(|x| x.as_str()).unwrap_or("");
        let start = e.get("startTime").and_then(|x| x.as_str()).unwrap_or("");
        Some(Match {
            provider: Provider::Lol, id: id.clone(),
            timestamp: start.to_string(),
            team_a: tn(0).clone(), team_b: tn(1).clone(),
            code_a: tc(0), code_b: tc(1),
            score_a: sc(0), score_b: sc(1),
            event: league.to_string(), stage: block.to_string(),
            url: format!("https://lolesports.com/en-US/match/{}", id),
            finished,
        })
    }).collect())
}

/// 常见战队缩写 → 完整队名（用于缩写也能命中）
fn alias_names(q:&str)->Vec<String> {
    let up=q.trim().to_ascii_lowercase();
    let names: &[&str] = match up.as_str() {
        "navi" => &["natus vincere"],
        "g2" => &["g2 esports","g2"],
        "fnc" => &["fnatic"],
        "vit" => &["team vitality","vitality"],
        "sen" => &["sentinels"],
        "prx" => &["paper rex"],
        "rrq" => &["rex regum qeon","rrq"],
        "loud" => &["loud"],
        "edg" => &["edward gaming","edg"],
        "gen"|"geng" => &["gen.g","gen g"],
        "drx" => &["drx"],
        "t1" => &["t1"],
        "blg" => &["bilibili gaming","blg"],
        "tes" => &["top esports","tes"],
        "jdg" => &["jd gaming","jdg"],
        "wbg" => &["weibo gaming","wbg"],
        "ig" => &["invictus gaming","ig"],
        "lng" => &["lng esports","lng"],
        _ => &[],
    };
    names.iter().map(|n| n.to_string()).collect::<Vec<_>>()
}

/// 查询词扩展：原词 + 去空格 + 别名
fn expand_terms(q:&str)->Vec<String> {
    let mut t=vec![q.trim().to_ascii_lowercase()];
    let sq: String = q.trim().chars().filter(|c|!c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    if !sq.is_empty() && sq!=t[0] { t.push(sq); }
    for n in alias_names(q) { t.push(n); }
    t
}

pub async fn find_matches(provider:Provider, q:&str)->Result<Vec<Match>> {
    let mut v=match provider{Provider::Vlr=>vlr_matches().await?,Provider::Lol=>lpl_matches().await?};
    let terms=expand_terms(q);
    v.retain(|m|{ let a=m.team_a.to_ascii_lowercase(); let b=m.team_b.to_ascii_lowercase(); let ca=m.code_a.to_ascii_lowercase(); let cb=m.code_b.to_ascii_lowercase(); let e=m.event.to_ascii_lowercase(); terms.iter().any(|t|a.contains(t)||b.contains(t)||ca.contains(t)||cb.contains(t)||e.contains(t)) });
    Ok(v)
}

/// 从一个候选列表里挑出“最接近查询词的规范队名”（用于订阅时存全名）
pub fn current_stage(matches:&[Match])->Option<String> {
    for m in matches { if !m.finished { return Some(m.scope_stage()); } }
    for m in matches { if m.finished { return Some(m.scope_stage()); } }
    None
}

/// 按（系列赛, 赛段）分组，组间用大标题＋横线分隔
pub fn format_list(matches:&[Match]) -> String {
    let mut groups: Vec<(String,String,Vec<Match>)> = Vec::new();
    for m in matches {
        let key = (m.event.clone(), m.scope_stage());
        if let Some(g) = groups.iter_mut().find(|(e,s,_)| *e==key.0 && *s==key.1) { g.2.push(m.clone()); }
        else { groups.push((key.0, key.1, vec![m.clone()])); }
    }
    let mut out = String::new();
    for (i,(ev, st, ms)) in groups.iter().enumerate() {
        if i>0 { out.push_str("\n────────────────────\n"); }
        let title = if ev.is_empty() { st.clone() } else if st.is_empty() { ev.clone() } else { format!("{ev}  ·  {st}") };
        out.push_str(&format!("━ {} ━\n", title));
        for m in ms { out.push_str(&format!("  {}\n", m.line())); }
    }
    out.trim().to_string()
}

pub fn canonical_team(q:&str, hits:&[Match]) -> Option<String> {
    let terms=expand_terms(q);
    for m in hits {
        let a=m.team_a.to_ascii_lowercase(); let b=m.team_b.to_ascii_lowercase(); let ca=m.code_a.to_ascii_lowercase(); let cb=m.code_b.to_ascii_lowercase();
        if terms.iter().any(|t| a.contains(t)||ca.contains(t)) { return Some(m.team_a.clone()); }
        if terms.iter().any(|t| b.contains(t)||cb.contains(t)) { return Some(m.team_b.clone()); }
    }
    hits.first().map(|m| m.team_a.clone())
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] #[ignore]
    fn lol_live_smoke() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let v = rt.block_on(lpl_matches()).unwrap();
        assert!(!v.is_empty());
        let blg = v.iter().filter(|m| m.code_a.eq_ignore_ascii_case("BLG") || m.code_b.eq_ignore_ascii_case("BLG")).count();
        println!("lol events: {} ; BLG hits: {} ; sample: {:?}", v.len(), blg, v.first().map(|m|(&m.team_a,&m.event,&m.stage,m.finished)));
    }
    #[test] fn store_roundtrip(){let p=std::env::temp_dir().join("heevi-extra-sports-test.json");let mut s=Store::load(p.clone());assert!(s.add(Provider::Vlr,"g".into(),"RRQ".into()));assert!(!s.add(Provider::Vlr,"g".into(),"rrq".into()));assert_eq!(s.list(Provider::Vlr,"g"),vec!["RRQ"]);let _=std::fs::remove_file(p);} #[test] fn provider_prefix(){assert_eq!(Provider::Lol.prefix(),"lol");}}
