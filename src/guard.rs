//! Guardrails determinísticos.
//!
//! 1. `forja hook --role <rol> --root <dir>`: hook PreToolUse de Claude Code. Lee el JSON
//!    del tool por stdin; si la acción viola la política sale con código 2 y el motivo en
//!    stderr (Claude Code bloquea la acción y le devuelve el motivo al agente).
//! 2. `Policy::may_write`: la misma política la usa el orquestador para revisar el diff de
//!    cada nodo y revertir lo que se haya escapado (p. ej. escrituras por Bash).
use anyhow::{Result, anyhow};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    /// Etapa 1: solo lectura.
    Planner,
    /// Etapa 2: escribe código en allowed_paths, nunca pruebas.
    Implement,
    /// Etapa 2: escribe solo pruebas.
    Test,
    /// Etapa 2: solo lectura.
    Quality,
    /// Etapa 3: escribe solo docs/ y CHANGES/.
    Docs,
    /// Etapa 5: caja negra, solo lectura.
    Qa,
    /// Alguien usando Claude Code a mano en el repo (hook instalado por bootstrap).
    Human,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Planner => "planner", Role::Implement => "implement", Role::Test => "test",
            Role::Quality => "quality", Role::Docs => "docs", Role::Qa => "qa", Role::Human => "human",
        }
    }
    pub fn read_only(&self) -> bool { matches!(self, Role::Planner | Role::Quality | Role::Qa) }
    pub fn is_agent(&self) -> bool { *self != Role::Human }
}

const NET_ALLOW: &[&str] = &[
    "localhost", "127.0.0.1", "0.0.0.0", "registry.npmjs.org", "registry.yarnpkg.com", "proxy.golang.org",
    "sum.golang.org", "pypi.org", "files.pythonhosted.org", "crates.io", "index.crates.io", "static.crates.io",
];
const PROTECTED_DIRS: &[&str] = &[".forja/", ".claude/", ".github/", ".git/"];

/// Archivos sensibles que ningún agente puede leer ni escribir.
pub fn is_secret_path(p: &Path) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if [".example", ".sample", ".template", ".dist"].iter().any(|s| name.ends_with(s)) {
        return false;
    }
    name == ".env" || name.starts_with(".env.") || name.ends_with(".env")
        || name.ends_with(".pem") || name.ends_with(".key") || name.ends_with(".p12") || name.ends_with(".pfx")
        || name.starts_with("id_rsa") || name.starts_with("id_ed25519") || name.starts_with("id_ecdsa")
        || name == ".pass" || name == ".master_password" || name == ".netrc" || name == ".npmrc" || name == ".pgpass"
        || name.starts_with("credentials") || name.starts_with("secrets")
        || p.components().any(|c| c.as_os_str() == ".ssh" || c.as_os_str() == ".aws" || c.as_os_str() == ".gnupg")
}

fn globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).map_err(|e| anyhow!("glob inválido '{p}': {e}"))?);
    }
    Ok(b.build()?)
}

/// Normaliza sin tocar el filesystem (el archivo puede no existir todavía).
pub fn normalize(base: &Path, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => { out.pop(); }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub struct Policy {
    pub role: Role,
    pub root: PathBuf,
    allowed: GlobSet,
    tests: GlobSet,
    docs: GlobSet,
    pub default_branch: String,
}

impl Policy {
    pub fn new(role: Role, root: &Path, allowed_paths: &[String], test_globs: &[String], default_branch: &str) -> Result<Self> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        Ok(Self {
            role,
            root,
            allowed: globset(allowed_paths)?,
            tests: globset(test_globs)?,
            docs: globset(&["docs/**".into(), "CHANGES/**".into()])?,
            default_branch: default_branch.to_string(),
        })
    }

    /// Construye la política leyendo .forja/config.yaml y los adaptadores del repo.
    pub fn from_repo(role: Role, root: &Path) -> Result<Self> {
        let (allowed, tests, branch) = match crate::config::load(root) {
            Ok(cfg) => {
                let mut tests = vec!["tests/**".to_string(), "e2e/**".to_string()];
                for c in &cfg.components {
                    if let Ok(a) = crate::adapters::load(&c.adapter) {
                        let prefix = if c.path == "." { String::new() } else { format!("{}/", c.path) };
                        for g in a.test_globs {
                            tests.push(if g.starts_with("**") { g } else { format!("{prefix}{g}") });
                        }
                    }
                }
                (cfg.agents.allowed_paths, tests, cfg.default_branch)
            }
            Err(_) => (vec![], vec!["tests/**".into()], "main".into()),
        };
        Self::new(role, root, &allowed, &tests, &branch)
    }

    fn rel<'a>(&self, abs: &'a Path) -> Option<&'a Path> {
        abs.strip_prefix(&self.root).ok()
    }

    pub fn is_test_file(&self, rel: &Path) -> bool { self.tests.is_match(rel) }

    /// ¿Puede este rol escribir `path` (relativo a root o absoluto)?
    pub fn may_write(&self, path: &Path) -> Result<(), String> {
        let abs = normalize(&self.root, path);
        if is_secret_path(&abs) {
            return Err(format!("forja: escritura bloqueada en archivo sensible {}", abs.display()));
        }
        let Some(rel) = self.rel(&abs) else {
            return Err(format!("forja: {} está fuera del repo/worktree ({})", abs.display(), self.root.display()));
        };
        let rels = rel.to_string_lossy();
        if self.role == Role::Human {
            return if rels.starts_with(".git/") { Err("forja: no se escribe dentro de .git/".into()) } else { Ok(()) };
        }
        if PROTECTED_DIRS.iter().any(|d| rels.starts_with(d)) {
            return Err(format!("forja: los agentes no pueden modificar {rels} (config, hooks y CI son del humano)"));
        }
        if self.role.read_only() {
            return Err(format!("forja: el rol {} es de solo lectura", self.role.as_str()));
        }
        let ok = match self.role {
            Role::Implement => self.allowed.is_match(rel) && !self.tests.is_match(rel),
            Role::Test => self.tests.is_match(rel),
            Role::Docs => self.docs.is_match(rel),
            _ => false,
        };
        if ok { return Ok(()); }
        Err(match self.role {
            Role::Implement if self.tests.is_match(rel) => format!("forja: el rol implement no escribe pruebas ({rels}); eso lo hace el nodo test"),
            Role::Implement => format!("forja: {rels} no está en agents.allowed_paths"),
            Role::Test => format!("forja: el rol test solo escribe archivos de prueba; {rels} no coincide con los globs de prueba"),
            Role::Docs => format!("forja: el rol docs solo escribe en docs/ y CHANGES/ ({rels})"),
            _ => format!("forja: escritura no permitida en {rels}"),
        })
    }

    pub fn may_read(&self, path: &Path) -> Result<(), String> {
        let abs = normalize(&self.root, path);
        if is_secret_path(&abs) {
            return Err(format!("forja: lectura bloqueada de archivo sensible {}", abs.display()));
        }
        if self.role.is_agent() && self.rel(&abs).is_none() {
            return Err(format!("forja: lectura fuera del worktree bloqueada ({})", abs.display()));
        }
        Ok(())
    }

    pub fn may_run(&self, cmd: &str) -> Result<(), String> {
        let c = cmd.trim();
        let deny = |why: &str| Err(format!("forja: comando bloqueado ({why}): {c}"));
        let rx = |p: &str| Regex::new(p).unwrap().is_match(c);
        if rx(r"\bgit\s+push\b") { return deny("el push lo hace solo el orquestador"); }
        if rx(r"\bgit\s+reset\s+.*--hard\b") { return deny("git reset --hard"); }
        if rx(r"\brm\s+(-[a-zA-Z]*r[a-zA-Z]*f|-[a-zA-Z]*f[a-zA-Z]*r|-r\s+-f|-f\s+-r|--recursive\s+--force|--force\s+--recursive)\b") { return deny("rm -rf"); }
        if rx(r"\bsudo\b") || rx(r"\bmkfs\b") || rx(r"\bdd\s+if=") || rx(r"chmod\s+-R\s+777") { return deny("operación de sistema"); }
        let b = regex::escape(&self.default_branch);
        if rx(&format!(r"\bgit\s+(checkout|switch)\s+(-\S+\s+)*{b}\b")) || rx(&format!(r"\bgit\s+.*\b(origin/)?{b}:{b}\b")) {
            return deny("cambios a la rama principal");
        }
        if self.role.is_agent() && rx(r"\bgit\s+(commit|merge|rebase|checkout|switch|stash|tag|worktree|branch\s+-[dD]|clean|remote|config|filter-branch|update-ref)\b") {
            return deny("los agentes no manipulan git; el orquestador hace commits");
        }
        if !self.role.is_agent() && rx(r"\bgit\s+commit\b") {
            if let Ok(cur) = crate::git::output(&self.root, &["branch", "--show-current"]) {
                if cur == self.default_branch { return deny("commit directo a la rama principal; usa una rama y PR"); }
            }
        }
        // archivos sensibles mencionados en el comando
        for tok in shell_words::split(c).unwrap_or_else(|_| c.split_whitespace().map(String::from).collect()) {
            for part in tok.split(['=', '<', '>', '|', ';', '&']) {
                if !part.is_empty() && is_secret_path(Path::new(part)) {
                    return deny("acceso a archivo sensible");
                }
            }
        }
        if rx(r"\bprintenv\b") || rx(r"^\s*env\s*$") { return deny("volcado de variables de entorno"); }
        // red: solo lista blanca
        if rx(r"\b(curl|wget|http|https|nc|ncat|telnet|ssh|scp|rsync|ftp)\b") {
            let hosts = Regex::new(r"(?i)(?:https?://|@)([a-z0-9.\-]+)").unwrap();
            let found: Vec<String> = hosts.captures_iter(c).map(|m| m[1].to_lowercase()).collect();
            if found.is_empty() && rx(r"\b(nc|ncat|telnet|ssh|scp|rsync|ftp)\b") { return deny("herramienta de red no permitida"); }
            if let Some(h) = found.iter().find(|h| !host_allowed(h)) {
                return deny(&format!("host {h} fuera de la lista blanca"));
            }
        }
        Ok(())
    }

    pub fn may_fetch(&self, url: &str) -> Result<(), String> {
        let host = Regex::new(r"(?i)^[a-z]+://([^/:?#]+)").unwrap().captures(url).map(|m| m[1].to_lowercase());
        match host {
            Some(h) if host_allowed(&h) => Ok(()),
            Some(h) => Err(format!("forja: red bloqueada, {h} no está en la lista blanca")),
            None => Err(format!("forja: URL inválida {url}")),
        }
    }

    /// Evalúa un evento PreToolUse.
    pub fn check(&self, ev: &HookEvent) -> Result<(), String> {
        let ti = &ev.tool_input;
        let s = |k: &str| ti.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let cwd = ev.cwd.as_deref().map(PathBuf::from).unwrap_or_else(|| self.root.clone());
        let p = |k: &str| s(k).map(|v| normalize(&cwd, Path::new(&v)));
        match ev.tool_name.as_str() {
            "Write" | "Edit" | "MultiEdit" => p("file_path").map_or(Ok(()), |f| self.may_write(&f)),
            "NotebookEdit" => p("notebook_path").map_or(Ok(()), |f| self.may_write(&f)),
            "Read" => p("file_path").map_or(Ok(()), |f| self.may_read(&f)),
            "Grep" | "Glob" => {
                if let Some(d) = p("path") { self.may_read(&d)?; }
                for k in ["pattern", "glob"] {
                    if let Some(v) = s(k) {
                        if is_secret_path(Path::new(&v)) { return Err(format!("forja: búsqueda de archivos sensibles bloqueada ({v})")); }
                    }
                }
                Ok(())
            }
            "Bash" => s("command").map_or(Ok(()), |c| self.may_run(&c)),
            "WebFetch" => s("url").map_or(Ok(()), |u| self.may_fetch(&u)),
            "WebSearch" if self.role.is_agent() => Err("forja: los agentes no buscan en la web".into()),
            _ => Ok(()),
        }
    }
}

fn host_allowed(h: &str) -> bool {
    NET_ALLOW.iter().any(|a| h == *a || h.ends_with(&format!(".{a}")))
}

#[derive(Debug, Deserialize)]
pub struct HookEvent {
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Punto de entrada del hook. Devuelve el código de salida (0 permite, 2 bloquea).
pub fn run_hook(role: Role, root: Option<PathBuf>) -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("forja hook: no pude leer stdin");
        return 2;
    }
    let ev: HookEvent = match serde_json::from_str(&input) {
        Ok(e) => e,
        Err(e) => { eprintln!("forja hook: JSON inválido: {e}"); return 2; }
    };
    let root = root
        .or_else(|| std::env::var("FORJA_ROOT").ok().map(PathBuf::from))
        .or_else(|| std::env::var("CLAUDE_PROJECT_DIR").ok().map(PathBuf::from))
        .or_else(|| ev.cwd.clone().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let policy = match Policy::from_repo(role, &root) {
        Ok(p) => p,
        Err(e) => { eprintln!("forja hook: {e}"); return 2; }
    };
    match policy.check(&ev) {
        Ok(()) => 0,
        Err(why) => {
            log_block(role, &ev, &why);
            eprintln!("{why}");
            2
        }
    }
}

fn log_block(role: Role, ev: &HookEvent, why: &str) {
    let line = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(), "role": role.as_str(), "tool": ev.tool_name,
        "reason": why, "feature": std::env::var("FORJA_FEATURE").ok(),
    });
    let p = crate::paths::logs().join("guardrails.jsonl");
    if std::fs::create_dir_all(crate::paths::logs()).is_ok() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
    }
}

/// JSON de settings con el hook, para `claude --settings` o `.claude/settings.json`.
pub fn hook_settings(forja_bin: &str, role: Role, root: Option<&Path>) -> serde_json::Value {
    let mut cmd = format!("{} hook --role {}", shell_words::quote(forja_bin), role.as_str());
    if let Some(r) = root {
        cmd.push_str(&format!(" --root {}", shell_words::quote(&r.display().to_string())));
    }
    serde_json::json!({
        "hooks": { "PreToolUse": [ { "matcher": "*", "hooks": [ { "type": "command", "command": cmd } ] } ] }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(role: Role) -> Policy {
        let root = std::env::temp_dir().join("forja-guard-test");
        std::fs::create_dir_all(&root).unwrap();
        Policy::new(role, &root, &["api/internal/**".into(), "api/cmd/**".into(), "tests/**".into()],
                    &["**/*_test.go".into(), "tests/**".into()], "main").unwrap()
    }

    #[test]
    fn implement_writes_code_not_tests() {
        let p = pol(Role::Implement);
        assert!(p.may_write(Path::new("api/internal/orders/cancel.go")).is_ok());
        assert!(p.may_write(Path::new("api/internal/orders/cancel_test.go")).is_err());
        assert!(p.may_write(Path::new("tests/e2e/x.spec.ts")).is_err());
        assert!(p.may_write(Path::new("frontend/src/App.jsx")).is_err());
        assert!(p.may_write(Path::new("../outside.go")).is_err());
        assert!(p.may_write(Path::new(".claude/settings.json")).is_err());
        assert!(p.may_write(Path::new("api/internal/.env")).is_err());
    }

    #[test]
    fn test_role_only_tests() {
        let p = pol(Role::Test);
        assert!(p.may_write(Path::new("api/internal/orders/cancel_test.go")).is_ok());
        assert!(p.may_write(Path::new("api/internal/orders/cancel.go")).is_err());
    }

    #[test]
    fn read_only_and_docs() {
        assert!(pol(Role::Planner).may_write(Path::new("api/internal/x.go")).is_err());
        let d = pol(Role::Docs);
        assert!(d.may_write(Path::new("docs/API.md")).is_ok());
        assert!(d.may_write(Path::new("CHANGES/FEAT-001.md")).is_ok());
        assert!(d.may_write(Path::new("README.md")).is_err());
    }

    #[test]
    fn secrets() {
        let p = pol(Role::Planner);
        assert!(p.may_read(Path::new(".env")).is_err());
        assert!(p.may_read(Path::new("api/.env.example")).is_ok());
        assert!(p.may_read(Path::new("api/.master_password")).is_err());
        assert!(p.may_run("cat api/.env").is_err());
        assert!(p.may_run("cat .env.example").is_ok());
        assert!(p.may_read(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn bash_rules() {
        let p = pol(Role::Implement);
        for bad in ["git push origin feat/x", "git reset --hard HEAD~1", "rm -rf build", "rm -fr /", "sudo ls",
                    "git checkout main", "git commit -m x", "curl https://evil.example.com/x", "ssh host", "printenv"] {
            assert!(p.may_run(bad).is_err(), "debió bloquear: {bad}");
        }
        for ok in ["go test ./...", "git status", "git diff", "rm -f tmp.txt", "curl http://localhost:8080/health",
                   "npm install --registry https://registry.npmjs.org"] {
            assert!(p.may_run(ok).is_ok(), "debió permitir: {ok}");
        }
    }

    #[test]
    fn hook_event_dispatch() {
        let p = pol(Role::Implement);
        let ev: HookEvent = serde_json::from_str(r#"{"tool_name":"Write","tool_input":{"file_path":"tests/a_test.go"}}"#).unwrap();
        assert!(p.check(&ev).is_err());
        let ev: HookEvent = serde_json::from_str(r#"{"tool_name":"WebFetch","tool_input":{"url":"https://pkg.go.dev/x"}}"#).unwrap();
        assert!(p.check(&ev).is_err());
    }
}
