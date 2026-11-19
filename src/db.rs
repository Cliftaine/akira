//! Registro central en ~/.forja/registry.db: repos, features, transiciones, eventos y corridas.
use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS repos (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, path TEXT NOT NULL UNIQUE, remote TEXT,
  stack TEXT NOT NULL DEFAULT '[]', created_at TEXT NOT NULL, last_run_at TEXT
);
CREATE TABLE IF NOT EXISTS features (
  id INTEGER PRIMARY KEY, repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  feat_id TEXT NOT NULL, title TEXT NOT NULL, state TEXT NOT NULL, branch TEXT, worktree TEXT,
  pr_url TEXT, cost_usd REAL NOT NULL DEFAULT 0, attempt INTEGER NOT NULL DEFAULT 0,
  node TEXT, pid INTEGER, feedback TEXT, updated_at TEXT NOT NULL,
  UNIQUE(repo_id, feat_id)
);
CREATE TABLE IF NOT EXISTS transitions (
  id INTEGER PRIMARY KEY, feature_id INTEGER NOT NULL REFERENCES features(id) ON DELETE CASCADE,
  from_state TEXT, to_state TEXT NOT NULL, attempt INTEGER, cost_usd REAL, note TEXT, ts TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY, feature_id INTEGER NOT NULL REFERENCES features(id) ON DELETE CASCADE,
  node TEXT, kind TEXT NOT NULL, text TEXT NOT NULL, ts TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY, repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  kind TEXT NOT NULL, feature TEXT, status TEXT NOT NULL, started_at TEXT NOT NULL,
  finished_at TEXT, log_dir TEXT, summary TEXT
);
"#;

pub fn now() -> String { chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true) }

pub fn open() -> Result<Connection> {
    crate::paths::ensure()?;
    let c = Connection::open(crate::paths::registry())?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    c.execute_batch(SCHEMA)?;
    Ok(c)
}

#[derive(Debug, Clone)]
pub struct Repo { pub id: i64, pub name: String, pub path: String, pub remote: Option<String>, pub stack: Vec<String>, pub last_run_at: Option<String> }

fn repo_row(r: &rusqlite::Row) -> rusqlite::Result<Repo> {
    let stack: String = r.get("stack")?;
    Ok(Repo { id: r.get("id")?, name: r.get("name")?, path: r.get("path")?, remote: r.get("remote")?,
              stack: serde_json::from_str(&stack).unwrap_or_default(), last_run_at: r.get("last_run_at")? })
}

pub fn upsert_repo(name: &str, path: &Path, remote: Option<&str>, stack: &[String]) -> Result<Repo> {
    let c = open()?;
    let p = path.display().to_string();
    c.execute("INSERT INTO repos(name,path,remote,stack,created_at) VALUES (?1,?2,?3,?4,?5)
               ON CONFLICT(path) DO UPDATE SET name=excluded.name, remote=excluded.remote, stack=excluded.stack",
              params![name, p, remote, serde_json::to_string(stack)?, now()])?;
    Ok(c.query_row("SELECT * FROM repos WHERE path=?1", [p], repo_row)?)
}

pub fn list_repos() -> Result<Vec<Repo>> {
    let c = open()?;
    let mut st = c.prepare("SELECT * FROM repos ORDER BY COALESCE(last_run_at, created_at) DESC")?;
    let rows = st.query_map([], repo_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Busca por nombre exacto, ruta o sufijo ('yuzuko' encuentra 'cliftaine/yuzuko').
pub fn find_repo(r: &str) -> Result<Option<Repo>> {
    let c = open()?;
    let path = Path::new(r).canonicalize().map(|p| p.display().to_string()).unwrap_or_else(|_| r.to_string());
    if let Some(x) = c.query_row("SELECT * FROM repos WHERE name=?1 OR path=?2", params![r, path], repo_row).optional()? {
        return Ok(Some(x));
    }
    let mut st = c.prepare("SELECT * FROM repos WHERE name LIKE ?1")?;
    let rows = st.query_map([format!("%/{r}")], repo_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(if rows.len() == 1 { rows.into_iter().next() } else { None })
}

pub fn remove_repo(id: i64) -> Result<()> { open()?.execute("DELETE FROM repos WHERE id=?1", [id])?; Ok(()) }

// ------------------------------------------------------------------ features

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Feature {
    pub id: i64, pub repo_id: i64, pub feat_id: String, pub title: String, pub state: String,
    pub branch: Option<String>, pub worktree: Option<String>, pub pr_url: Option<String>,
    pub cost_usd: f64, pub attempt: i64, pub node: Option<String>, pub pid: Option<i64>,
    pub feedback: Option<String>, pub updated_at: String,
}

fn feat_row(r: &rusqlite::Row) -> rusqlite::Result<Feature> {
    Ok(Feature {
        id: r.get("id")?, repo_id: r.get("repo_id")?, feat_id: r.get("feat_id")?, title: r.get("title")?,
        state: r.get("state")?, branch: r.get("branch")?, worktree: r.get("worktree")?, pr_url: r.get("pr_url")?,
        cost_usd: r.get("cost_usd")?, attempt: r.get("attempt")?, node: r.get("node")?, pid: r.get("pid")?,
        feedback: r.get("feedback")?, updated_at: r.get("updated_at")?,
    })
}

pub fn upsert_feature(repo_id: i64, feat_id: &str, title: &str) -> Result<Feature> {
    let c = open()?;
    c.execute("INSERT INTO features(repo_id,feat_id,title,state,updated_at) VALUES (?1,?2,?3,'draft',?4)
               ON CONFLICT(repo_id,feat_id) DO UPDATE SET title=excluded.title",
              params![repo_id, feat_id, title, now()])?;
    get_feature(repo_id, feat_id)?.ok_or_else(|| anyhow!("feature no encontrado"))
}

pub fn get_feature(repo_id: i64, feat_id: &str) -> Result<Option<Feature>> {
    Ok(open()?.query_row("SELECT * FROM features WHERE repo_id=?1 AND feat_id=?2", params![repo_id, feat_id], feat_row).optional()?)
}

pub fn list_features(repo_id: i64) -> Result<Vec<Feature>> {
    let c = open()?;
    let mut st = c.prepare("SELECT * FROM features WHERE repo_id=?1 ORDER BY feat_id")?;
    let rows = st.query_map([repo_id], feat_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Cambia el estado y registra la transición (auditable).
pub fn transition(f: &Feature, to: &str, note: &str) -> Result<()> {
    let c = open()?;
    c.execute("UPDATE features SET state=?1, updated_at=?2 WHERE id=?3", params![to, now(), f.id])?;
    c.execute("INSERT INTO transitions(feature_id,from_state,to_state,attempt,cost_usd,note,ts) VALUES (?1,?2,?3,?4,?5,?6,?7)",
              params![f.id, (f.state != to).then_some(&f.state), to, f.attempt, f.cost_usd, note, now()])?;
    Ok(())
}

pub fn set_feature_field(id: i64, field: &str, value: Option<&str>) -> Result<()> {
    let allowed = ["branch", "worktree", "pr_url", "node", "feedback"];
    if !allowed.contains(&field) { return Err(anyhow!("campo no editable: {field}")); }
    open()?.execute(&format!("UPDATE features SET {field}=?1, updated_at=?2 WHERE id=?3"), params![value, now(), id])?;
    Ok(())
}

pub fn set_feature_pid(id: i64, pid: Option<i64>) -> Result<()> {
    open()?.execute("UPDATE features SET pid=?1 WHERE id=?2", params![pid, id])?; Ok(())
}

pub fn add_cost(id: i64, usd: f64) -> Result<f64> {
    let c = open()?;
    c.execute("UPDATE features SET cost_usd=cost_usd+?1 WHERE id=?2", params![usd, id])?;
    Ok(c.query_row("SELECT cost_usd FROM features WHERE id=?1", [id], |r| r.get(0))?)
}

pub fn set_attempt(id: i64, n: i64) -> Result<()> { open()?.execute("UPDATE features SET attempt=?1 WHERE id=?2", params![n, id])?; Ok(()) }

pub fn event(feature_id: i64, node: &str, kind: &str, text: &str) -> Result<()> {
    open()?.execute("INSERT INTO events(feature_id,node,kind,text,ts) VALUES (?1,?2,?3,?4,?5)", params![feature_id, node, kind, text, now()])?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Event { pub node: String, pub kind: String, pub text: String, pub ts: String }

pub fn events(feature_id: i64, limit: usize) -> Result<Vec<Event>> {
    let c = open()?;
    let mut st = c.prepare("SELECT node,kind,text,ts FROM (SELECT * FROM events WHERE feature_id=?1 ORDER BY id DESC LIMIT ?2) ORDER BY id")?;
    let rows = st.query_map(params![feature_id, limit as i64], |r| Ok(Event { node: r.get(0)?, kind: r.get(1)?, text: r.get(2)?, ts: r.get(3)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn transitions(feature_id: i64) -> Result<Vec<(String, String, String, String)>> {
    let c = open()?;
    let mut st = c.prepare("SELECT COALESCE(from_state,''), to_state, COALESCE(note,''), ts FROM transitions WHERE feature_id=?1 ORDER BY id")?;
    let rows = st.query_map([feature_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ------------------------------------------------------------------ runs

pub fn start_run(repo_id: i64, kind: &str, feature: Option<&str>, log_dir: &Path) -> Result<i64> {
    let c = open()?;
    c.execute("INSERT INTO runs(repo_id,kind,feature,status,started_at,log_dir) VALUES (?1,?2,?3,'running',?4,?5)",
              params![repo_id, kind, feature, now(), log_dir.display().to_string()])?;
    Ok(c.last_insert_rowid())
}

pub fn finish_run(id: i64, status: &str, summary: &serde_json::Value) -> Result<()> {
    let c = open()?;
    let ts = now();
    c.execute("UPDATE runs SET status=?1, finished_at=?2, summary=?3 WHERE id=?4", params![status, ts, summary.to_string(), id])?;
    c.execute("UPDATE repos SET last_run_at=?1 WHERE id=(SELECT repo_id FROM runs WHERE id=?2)", params![ts, id])?;
    Ok(())
}
