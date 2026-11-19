//! PR de bootstrap: agrega lo que le falta al repo para operar con forja, en la rama
//! forja/bootstrap, y lo pasa por el mismo gate que cualquier feature.
use anyhow::{Result, bail};
use std::path::Path;
use std::process::Command;

use crate::config::{self, ForjaConfig};
use crate::db::Repo;
use crate::guard::{self, Role};
use crate::{adapters, gate, git};

pub const BRANCH: &str = "forja/bootstrap";

pub struct Outcome { pub created: Vec<String>, pub gate_passed: bool, pub pr_url: Option<String>, pub worktree: String, pub notes: Vec<String> }

pub fn ci_script(cfg: &ForjaConfig) -> Result<String> {
    let mut s = String::from("#!/usr/bin/env bash\n# Generado por forja: el gate determinístico (make ci). Corre igual local, en worktrees y en GitHub Actions.\nset -uo pipefail\ncd \"$(dirname \"$0\")/..\"\nfail=0\nrun() {\n  local name=\"$1\" dir=\"$2\" cmd=\"$3\"\n  echo \"::group::$name\"\n  ( cd \"$dir\" && bash -c \"$cmd\" ) || { echo \"FALLÓ: $name\"; fail=1; }\n  echo \"::endgroup::\"\n}\n");
    for p in gate::plan(cfg, None, None)? {
        match p.command {
            Some(c) => s.push_str(&format!("run {} {} {}\n", shell_words::quote(&format!("{}/{}", p.component, p.step)), shell_words::quote(&p.path), shell_words::quote(&c))),
            None => s.push_str(&format!("echo \"omitido: {}/{} ({})\"\n", p.component, p.step, p.skip_reason.unwrap_or_default())),
        }
    }
    s.push_str("exit $fail\n");
    Ok(s)
}

fn workflow(cfg: &ForjaConfig) -> String {
    let mut setup = String::new();
    let stacks: std::collections::BTreeSet<&str> = cfg.components.iter().map(|c| c.adapter.as_str()).collect();
    for c in &cfg.components {
        match c.adapter.as_str() {
            "go" => setup.push_str(&format!("      - uses: actions/setup-go@v5\n        with: {{ go-version-file: '{}/go.mod' }}\n      - run: go install github.com/golangci/golangci-lint/v2/cmd/golangci-lint@latest\n", c.path)),
            "typescript" => setup.push_str(&format!("      - uses: actions/setup-node@v4\n        with: {{ node-version: lts/* }}\n      - run: npm ci\n        working-directory: {}\n", c.path)),
            _ => {}
        }
    }
    if stacks.contains("python") { setup.push_str("      - uses: actions/setup-python@v5\n        with: { python-version: '3.12' }\n      - run: pip install ruff mypy pytest pytest-cov\n"); }
    if stacks.contains("rust") { setup.push_str("      - uses: dtolnay/rust-toolchain@stable\n        with: { components: 'rustfmt, clippy' }\n"); }
    format!("# Generado por forja. Status check requerido por la protección de rama.\nname: forja-ci\non:\n  pull_request:\n  push:\n    branches: [{}]\njobs:\n  forja-ci:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n{setup}      - run: make ci\n", cfg.default_branch)
}

const DOCS: &[(&str, &str)] = &[
    ("docs/ARCHITECTURE.md", "# Arquitectura\n\n> Mantenido por el agente de documentación de forja. Una sección por componente.\n"),
    ("docs/DATABASE.md", "# Base de datos\n\n> Esquema generado con tbls (hechos) y narrativa del agente (qué cambió y por qué).\n"),
    ("docs/API.md", "# API\n\n> OpenAPI generado por el adaptador (hechos) y descripción, ejemplos y errores por endpoint (agente).\n"),
    ("docs/adr/0001-adoptar-forja.md", "# ADR 0001: adoptar forja\n\nEstado: propuesto\n\nLos features se implementan como PR generados por agentes y validados por un gate determinístico (`make ci`). Solo un humano aprueba el merge.\n"),
    ("features/.gitkeep", ""),
    ("CHANGES/.gitkeep", ""),
];

fn write_if_missing(wt: &Path, rel: &str, content: &str, created: &mut Vec<String>) -> Result<()> {
    let p = wt.join(rel);
    if p.exists() { return Ok(()); }
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, content)?;
    created.push(rel.to_string());
    Ok(())
}

fn merge_claude_settings(wt: &Path, created: &mut Vec<String>) -> Result<()> {
    let p = wt.join(".claude/settings.json");
    let mut v: serde_json::Value = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| serde_json::json!({}));
    if v.to_string().contains("forja hook") { return Ok(()); }
    let hook = guard::hook_settings("forja", Role::Human, None)["hooks"]["PreToolUse"][0].clone();
    let obj = v.as_object_mut().ok_or_else(|| anyhow::anyhow!(".claude/settings.json no es un objeto"))?;
    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let pre = hooks.as_object_mut().unwrap().entry("PreToolUse").or_insert_with(|| serde_json::json!([]));
    pre.as_array_mut().ok_or_else(|| anyhow::anyhow!("hooks.PreToolUse no es lista"))?.push(hook);
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, serde_json::to_string_pretty(&v)? + "\n")?;
    created.push(".claude/settings.json (hook forja)".into());
    Ok(())
}

pub fn run(repo: &Repo, push: bool, force: bool) -> Result<Outcome> {
    let root = Path::new(&repo.path);
    let cfg = config::load(root)?;
    let wt = crate::paths::worktrees().join(repo.name.replace('/', "__")).join("bootstrap");
    git::ensure_worktree(root, &wt, BRANCH, &cfg.default_branch)?;
    let mut created = vec![];
    let mut notes = vec![];

    // config del repo (si aún no estaba versionado)
    let cfg_dst = wt.join(config::CONFIG_REL);
    if !cfg_dst.exists() {
        std::fs::create_dir_all(cfg_dst.parent().unwrap())?;
        std::fs::copy(config::path(root), &cfg_dst)?;
        created.push(config::CONFIG_REL.into());
    }
    let script = ci_script(&cfg)?;
    let ci = wt.join(".forja/ci.sh");
    if std::fs::read_to_string(&ci).unwrap_or_default() != script {
        std::fs::write(&ci, &script)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ci, std::fs::Permissions::from_mode(0o755))?;
        created.push(".forja/ci.sh".into());
    }
    let mk = wt.join("Makefile");
    let mk_text = std::fs::read_to_string(&mk).unwrap_or_default();
    if !regex::Regex::new(r"(?m)^ci\s*:").unwrap().is_match(&mk_text) {
        let block = "\n# --------------- forja ---------------\n.PHONY: ci\n## Gate determinístico (mismo comando local, en worktrees y en GitHub Actions)\nci:\n\t@bash .forja/ci.sh\n";
        std::fs::write(&mk, format!("{mk_text}{block}"))?;
        created.push(if mk_text.is_empty() { "Makefile".into() } else { "Makefile (target ci)".into() });
    }
    write_if_missing(&wt, ".github/workflows/forja-ci.yml", &workflow(&cfg), &mut created)?;
    for (rel, content) in DOCS { write_if_missing(&wt, rel, content, &mut created)?; }
    if !wt.join("docs/FEATURE_MAP.md").exists() {
        crate::pipeline::write_feature_map(&wt, Some(repo.id))?;
        created.push("docs/FEATURE_MAP.md".into());
    }
    merge_claude_settings(&wt, &mut created)?;
    let claude_md = wt.join("CLAUDE.md");
    let cm = std::fs::read_to_string(&claude_md).unwrap_or_default();
    if !cm.contains("forja") {
        std::fs::write(&claude_md, format!("{cm}{}\n## forja\n\nEste repo opera con forja: cada feature vive en `features/FEAT-NNN.yaml`, su plan en `features/FEAT-NNN.plan.md` y entra a `{}` solo por PR aprobado. El gate es `make ci`. Los hooks de `.claude/settings.json` aplican los guardrails (`forja hook`).\n", if cm.is_empty() { "# CLAUDE.md\n" } else { "\n" }, cfg.default_branch))?;
        created.push("CLAUDE.md (sección forja)".into());
    }
    let has_docker = std::fs::read_dir(&wt).map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with("Dockerfile"))).unwrap_or(false);
    if !has_docker { notes.push("Dockerfile multi-stage y docker-compose.ci.yml aún no se generan (fase 2/5 del roadmap)".into()); }
    for c in &cfg.components {
        if let Ok(a) = adapters::load(&c.adapter) {
            for t in a.tools.iter().filter(|t| which::which(t).is_err()) { notes.push(format!("herramienta `{t}` ({}) no está instalada en esta máquina", c.name)); }
        }
    }

    git::commit_all(&wt, "chore(forja): bootstrap del repo (make ci, forja-ci, docs, hooks)")?;

    // mismo gate que cualquier feature
    let log_dir = crate::paths::runs().join(repo.name.replace('/', "__")).join("bootstrap");
    let mut s = |_: &gate::PlannedStep| {};
    let mut l = |_: &str| {};
    let mut d = |r: &gate::StepResult| println!("  {:<28} {:?} {}", format!("{}/{}", r.component, r.step), r.status, r.reason.clone().unwrap_or_default());
    println!("gate sobre {BRANCH}:");
    let g = gate::run(&wt, &cfg, &log_dir, None, None, false, &mut gate::Hooks { on_start: &mut s, on_line: &mut l, on_done: &mut d })?;
    let mut out = Outcome { created, gate_passed: g.passed(), pr_url: None, worktree: wt.display().to_string(), notes };
    if !g.passed() && !force {
        out.notes.push(format!("el gate falló; no se abre PR. Logs: {}. Ajusta .forja/config.yaml o el código y repite (o usa --force).", log_dir.display()));
        return Ok(out);
    }
    if !(push && git::has_remote(root) && which::which("gh").is_ok()) {
        out.notes.push(format!("rama {BRANCH} lista en el worktree; no se hizo push (sin remoto, sin gh o --no-push)"));
        return Ok(out);
    }
    git::push_branch(&wt, BRANCH, &cfg.default_branch)?;
    let mut body = String::from("## Bootstrap de forja\n\nAgrega:\n");
    for c in &out.created { body.push_str(&format!("- `{c}`\n")); }
    body.push_str(&format!("\nGate: {}\n", if g.passed() { "✅ pasó" } else { "❌ falló (forzado)" }));
    for n in &out.notes { body.push_str(&format!("- ⚠️ {n}\n")); }
    let o = Command::new("gh").current_dir(&wt).args(["pr", "create", "--base", &cfg.default_branch, "--head", BRANCH, "--title", "chore(forja): bootstrap", "--body", &body]).output()?;
    if !o.status.success() { bail!("gh pr create: {}", String::from_utf8_lossy(&o.stderr)); }
    out.pr_url = Some(String::from_utf8_lossy(&o.stdout).trim().to_string());
    Ok(out)
}
