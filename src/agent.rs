//! Ejecuta un nodo agéntico con `claude -p --output-format stream-json`.
//! Cada rol recibe su system prompt, sus herramientas permitidas, su directorio de trabajo
//! y el hook de guardrails (`forja hook --role ...`) vía --settings.
use anyhow::{Result, anyhow, bail};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::db;
use crate::guard::{self, Role};

pub struct Call<'a> {
    pub role: Role,
    pub node: &'a str,
    pub cwd: &'a Path,
    pub system: String,
    pub prompt: String,
    pub budget_usd: f64,
    pub model: Option<&'a str>,
    pub feature_db_id: Option<i64>,
    pub feature_id: &'a str,
    pub log_file: PathBuf,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub text: String,
    pub cost_usd: f64,
    pub is_error: bool,
    pub turns: u64,
    pub blocked: u32,
}

pub fn tools_for(role: Role) -> &'static str {
    match role {
        Role::Planner | Role::Quality => "Read,Grep,Glob",
        Role::Implement | Role::Test => "Read,Grep,Glob,Edit,Write,Bash",
        Role::Docs => "Read,Grep,Glob,Edit,Write",
        Role::Qa => "Read,Bash,WebFetch",
        Role::Human => "default",
    }
}

pub fn claude_available() -> bool { which::which("claude").is_ok() }

fn forja_bin() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "forja".into())
}

fn summarize_tool(name: &str, input: &serde_json::Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let detail = match name {
        "Read" | "Write" | "Edit" | "MultiEdit" => s("file_path").to_string(),
        "Bash" => s("command").chars().take(160).collect(),
        "Grep" => format!("{} {}", s("pattern"), s("path")),
        "Glob" => s("pattern").to_string(),
        "WebFetch" => s("url").to_string(),
        _ => input.to_string().chars().take(120).collect(),
    };
    format!("{name} {detail}")
}

pub fn run(call: Call) -> Result<Outcome> {
    if !claude_available() {
        bail!("no encontré el CLI `claude` en PATH");
    }
    if call.budget_usd <= 0.0 {
        bail!("presupuesto del feature agotado");
    }
    let settings = guard::hook_settings(&forja_bin(), call.role, Some(call.cwd));
    let mut cmd = Command::new("claude");
    cmd.arg("-p").arg(&call.prompt)
        .args(["--output-format", "stream-json", "--verbose"])
        .args(["--tools", tools_for(call.role)])
        .args(["--allowedTools", tools_for(call.role)])
        .args(["--permission-mode", "dontAsk"])
        .args(["--append-system-prompt", &call.system])
        .args(["--settings", &settings.to_string()])
        .args(["--max-budget-usd", &format!("{:.2}", call.budget_usd)])
        .arg("--strict-mcp-config")
        .current_dir(call.cwd)
        .env("FORJA_ROLE", call.role.as_str())
        .env("FORJA_ROOT", call.cwd)
        .env("FORJA_FEATURE", call.feature_id)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(m) = call.model { cmd.args(["--model", m]); }

    if let Some(p) = call.log_file.parent() { std::fs::create_dir_all(p)?; }
    let mut log = std::fs::File::create(&call.log_file)?;
    let ev = |kind: &str, text: &str| {
        if let Some(id) = call.feature_db_id { let _ = db::event(id, call.node, kind, text); }
    };
    ev("start", &format!("{} · herramientas {} · presupuesto ${:.2}", call.role.as_str(), tools_for(call.role), call.budget_usd));

    let mut child = cmd.spawn().map_err(|e| anyhow!("no pude lanzar claude: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    let err_handle = std::thread::spawn(move || { let mut s = String::new(); let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut s); s });
    let mut out = Outcome::default();
    let mut got_result = false;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line?;
        writeln!(log, "{line}")?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        match v["type"].as_str().unwrap_or("") {
            "system" if v["subtype"] == "init" => ev("init", &format!("modelo {}", v["model"].as_str().unwrap_or("?"))),
            "assistant" => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str() {
                        Some("text") => { let t = c["text"].as_str().unwrap_or("").trim(); if !t.is_empty() { ev("text", &t.chars().take(400).collect::<String>()); } }
                        Some("tool_use") => ev("tool", &summarize_tool(c["name"].as_str().unwrap_or("?"), &c["input"])),
                        _ => {}
                    }
                }
            }
            "user" => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] == "tool_result" && c["is_error"] == true {
                        let t = match &c["content"] { serde_json::Value::String(s) => s.clone(), other => other.to_string() };
                        if t.contains("forja:") { out.blocked += 1; ev("blocked", &t.chars().take(300).collect::<String>()); }
                        else { ev("tool_error", &t.chars().take(300).collect::<String>()); }
                    }
                }
            }
            "result" => {
                got_result = true;
                out.text = v["result"].as_str().unwrap_or("").to_string();
                out.cost_usd = v["total_cost_usd"].as_f64().unwrap_or(0.0);
                out.is_error = v["is_error"].as_bool().unwrap_or(false) || v["subtype"].as_str().is_some_and(|s| s != "success");
                out.turns = v["num_turns"].as_u64().unwrap_or(0);
                ev("result", &format!("{} · {} turnos · ${:.4}{}", v["subtype"].as_str().unwrap_or("?"), out.turns, out.cost_usd,
                                      if out.blocked > 0 { format!(" · {} acciones bloqueadas", out.blocked) } else { String::new() }));
            }
            _ => {}
        }
    }
    let status = child.wait()?;
    let stderr = err_handle.join().unwrap_or_default();
    if let Some(id) = call.feature_db_id { db::add_cost(id, out.cost_usd)?; }
    if !got_result {
        ev("error", &format!("claude terminó sin resultado ({status}): {}", stderr.trim().chars().take(400).collect::<String>()));
        bail!("claude terminó sin resultado ({status}): {}", stderr.trim());
    }
    Ok(out)
}
