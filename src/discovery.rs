//! Descubrimiento read-only de un repo: solo manifiestos y nombres de archivo.
//! Nunca abre .env, llaves ni credenciales y nunca escribe.
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::config::{Agents, Component, Database, Environment, ForjaConfig, OpenApi};
use crate::{git, guard};

const PRUNE: &[&str] = &["node_modules", "vendor", "dist", "build", "target", "venv", "__pycache__", "coverage"];
const MANIFESTS: &[(&str, &str)] = &[("go.mod", "go"), ("pyproject.toml", "python"), ("package.json", "typescript"), ("Cargo.toml", "rust")];
const SOURCE_DIRS: &[&str] = &["cmd", "internal", "pkg", "src", "app", "lib", "api"];
const MAX_DEPTH: usize = 3;

#[derive(Debug, Clone, Serialize, Default)]
pub struct ComponentFinding {
    pub name: String,
    pub path: String,
    pub adapter: String,
    pub project_type: String,
    pub frameworks: Vec<String>,
    pub tools: BTreeMap<String, bool>,
    pub commands: BTreeMap<String, Option<String>>,
    pub has_tests: bool,
    pub db_engine: Option<String>,
    pub openapi: Option<(String, String)>,
    pub e2e: Option<String>,
    pub source_dirs: Vec<String>,
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Report {
    pub root: String,
    pub repo: String,
    pub remote: Option<String>,
    pub default_branch: String,
    pub components: Vec<ComponentFinding>,
    pub dockerfiles: Vec<String>,
    pub dockerfile_has_test_stage: bool,
    pub compose_files: Vec<String>,
    pub ci_workflows: Vec<String>,
    pub makefile_ci_target: bool,
    pub migrations: Vec<String>,
    pub seeds: Vec<String>,
    pub openapi_files: Vec<String>,
    pub docs: BTreeMap<String, bool>,
    pub has_forja_config: bool,
    pub gaps: Vec<String>,
}

impl Report {
    pub fn stack(&self) -> Vec<String> {
        let mut s: Vec<_> = self.components.iter().map(|c| c.adapter.clone()).collect();
        s.sort();
        s.dedup();
        s
    }
}

fn read(p: &Path) -> String {
    if guard::is_secret_path(p) {
        return String::new(); // regla dura: no leemos archivos sensibles
    }
    std::fs::read_to_string(p).unwrap_or_default()
}

fn walk(root: &Path, depth: usize) -> impl Iterator<Item = walkdir::DirEntry> {
    WalkDir::new(root).max_depth(depth).sort_by_file_name().into_iter().filter_entry(|e| {
        if e.depth() == 0 { return true; }
        let n = e.file_name().to_string_lossy();
        !(e.file_type().is_dir() && (n.starts_with('.') || PRUNE.contains(&n.as_ref())))
    }).filter_map(|e| e.ok())
}

fn rel(root: &Path, p: &Path) -> String {
    let r = p.strip_prefix(root).unwrap_or(p).to_string_lossy().to_string();
    if r.is_empty() { ".".into() } else { r }
}

fn any_file(base: &Path, re: &Regex) -> bool {
    walk(base, 8).any(|e| e.file_type().is_file() && re.is_match(&e.file_name().to_string_lossy()))
}

pub fn repo_slug(remote: Option<&str>, root: &Path) -> String {
    if let Some(r) = remote {
        let re = Regex::new(r"[:/]([^/:]+/[^/]+?)(?:\.git)?/?$").unwrap();
        if let Some(c) = re.captures(r) {
            return c[1].to_lowercase();
        }
    }
    root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into())
}

// ------------------------------------------------------------------ detectores por stack

fn detect_go(c: &mut ComponentFinding, base: &Path, root: &Path) {
    let text = read(&base.join("go.mod"));
    let deps = [("gin", "github.com/gin-gonic/gin"), ("echo", "github.com/labstack/echo"), ("chi", "github.com/go-chi/chi"),
                ("fiber", "github.com/gofiber/fiber"), ("grpc", "google.golang.org/grpc"), ("cobra", "github.com/spf13/cobra")];
    c.frameworks = deps.iter().filter(|(_, m)| text.contains(m)).map(|(k, _)| k.to_string()).collect();
    if text.contains("jackc/pgx") || text.contains("lib/pq") {
        c.db_engine = Some("postgres".into());
    } else if text.contains("go-sql-driver/mysql") {
        c.db_engine = Some("mysql".into());
    }
    let web = ["gin", "echo", "chi", "fiber", "grpc"];
    c.project_type = if c.frameworks.iter().any(|f| web.contains(&f.as_str())) { "api" }
        else if base.join("cmd").is_dir() || c.frameworks.contains(&"cobra".into()) { "cli" } else { "library" }.into();
    let golangci = [base, root].iter().any(|d| [".golangci.yml", ".golangci.yaml", ".golangci.toml", ".golangci.json"].iter().any(|f| d.join(f).exists()));
    let swag = text.contains("swaggo/swag");
    c.tools.insert("golangci-lint config".into(), golangci);
    c.tools.insert("swag".into(), swag);
    c.has_tests = any_file(base, &Regex::new(r"_test\.go$").unwrap());
    if swag {
        c.openapi = Some(("swag".into(), "docs/openapi.yaml".into()));
    }
    if !golangci {
        c.gaps.push("no hay .golangci.yml (golangci-lint usará su config por defecto)".into());
    }
    if c.project_type == "api" && !swag {
        c.commands.insert("api_spec".into(), None);
        c.gaps.push("API sin generador de OpenAPI (swag no está en go.mod)".into());
    }
}

fn js_runner(base: &Path, root: &Path) -> &'static str {
    for d in [base, root] {
        if d.join("pnpm-lock.yaml").exists() { return "pnpm exec"; }
        if d.join("yarn.lock").exists() { return "yarn"; }
        if d.join("bun.lockb").exists() || d.join("bun.lock").exists() { return "bunx"; }
    }
    "npx --no-install"
}

fn detect_ts(c: &mut ComponentFinding, base: &Path, root: &Path) {
    let pkg: serde_json::Value = serde_json::from_str(&read(&base.join("package.json"))).unwrap_or_default();
    let mut deps = std::collections::BTreeSet::new();
    for k in ["dependencies", "devDependencies"] {
        if let Some(o) = pkg.get(k).and_then(|v| v.as_object()) {
            deps.extend(o.keys().cloned());
        }
    }
    let has = |names: &[&str]| names.iter().any(|n| deps.contains(*n));
    let fw = [("react", "react"), ("vue", "vue"), ("svelte", "svelte"), ("next", "next"), ("vite", "vite"),
              ("express", "express"), ("@nestjs/core", "nest"), ("fastify", "fastify"), ("hono", "hono")];
    c.frameworks = fw.iter().filter(|(k, _)| deps.contains(*k)).map(|(_, v)| v.to_string()).collect();
    c.project_type = if has(&["react", "vue", "svelte", "next", "@angular/core"]) { "frontend" }
        else if has(&["express", "@nestjs/core", "fastify", "hono"]) { "api" }
        else if pkg.get("bin").is_some() { "cli" } else { "library" }.into();
    let glob_exists = |prefix: &str| std::fs::read_dir(base).map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with(prefix))).unwrap_or(false);
    let ts = base.join("tsconfig.json").exists() || has(&["typescript"]);
    let eslint = has(&["eslint"]) || glob_exists("eslint.config.") || glob_exists(".eslintrc");
    let prettier = has(&["prettier"]);
    let (vitest, jest) = (has(&["vitest"]), has(&["jest"]));
    let playwright = has(&["@playwright/test", "playwright"]);
    for (k, v) in [("typescript", ts), ("eslint", eslint), ("prettier", prettier), ("vitest", vitest), ("jest", jest), ("playwright", playwright)] {
        c.tools.insert(k.into(), v);
    }
    c.frameworks.push(if ts { "typescript" } else { "javascript" }.into());
    let run = js_runner(base, root);

    c.commands.insert("format_check".into(), prettier.then(|| format!("{run} prettier --check .")));
    if !prettier { c.gaps.push("no hay prettier: format_check deshabilitado".into()); }
    let mut lint = vec![];
    if eslint { lint.push(format!("{run} eslint .")); } else { c.gaps.push("no hay eslint configurado".into()); }
    if ts { lint.push(format!("{run} tsc --noEmit")); }
    c.commands.insert("lint".into(), (!lint.is_empty()).then(|| lint.join(" && ")));
    let test = if vitest { Some(format!("{run} vitest run --coverage")) } else if jest { Some(format!("{run} jest --coverage")) } else { None };
    if test.is_none() { c.gaps.push("no hay framework de pruebas (vitest/jest): test deshabilitado".into()); }
    c.commands.insert("test".into(), test);
    if playwright {
        c.e2e = Some("playwright".into());
        c.commands.insert("e2e".into(), Some(format!("{run} playwright test")));
    } else {
        c.commands.insert("e2e".into(), None);
        if c.project_type == "frontend" { c.gaps.push("frontend sin pruebas E2E (playwright)".into()); }
    }
    c.has_tests = vitest || jest;
    // Quita sobrescrituras que coinciden con el adaptador por defecto.
    if let Ok(a) = crate::adapters::load("typescript") {
        c.commands.retain(|k, v| a.commands.get(k) != Some(v));
    }
}

fn detect_python(c: &mut ComponentFinding, base: &Path, _root: &Path) {
    let mut text = read(&base.join("pyproject.toml"));
    let data: toml::Table = text.parse().unwrap_or_default();
    if let Ok(rd) = std::fs::read_dir(base) {
        for e in rd.filter_map(|e| e.ok()) {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("requirements") && n.ends_with(".txt") { text.push('\n'); text.push_str(&read(&e.path())); }
        }
    }
    let tool = data.get("tool").and_then(|t| t.as_table()).cloned().unwrap_or_default();
    let has = |n: &str| tool.contains_key(n) || Regex::new(&format!(r#"(?m)^[\s"']*{}\b"#, regex::escape(n))).unwrap().is_match(&text);
    let lower = text.to_lowercase();
    c.frameworks = ["fastapi", "django", "flask", "typer", "click"].iter().filter(|n| Regex::new(&format!(r"\b{n}\b")).unwrap().is_match(&lower)).map(|s| s.to_string()).collect();
    let fwh = |n: &str| c.frameworks.iter().any(|f| f == n);
    c.project_type = if fwh("fastapi") || fwh("django") || fwh("flask") { "api" }
        else if fwh("typer") || fwh("click") || data.get("project").and_then(|p| p.get("scripts")).is_some() { "cli" } else { "library" }.into();
    if lower.contains("psycopg") || lower.contains("asyncpg") { c.db_engine = Some("postgres".into()); }
    for k in ["ruff", "black", "mypy", "pytest", "flake8"] { c.tools.insert(k.into(), has(k)); }
    let t = |k: &str| c.tools.get(k).copied().unwrap_or(false);
    let run = if base.join("uv.lock").exists() { "uv run " } else if base.join("poetry.lock").exists() { "poetry run " } else { "" };
    let (fmt, mut lint) = if t("ruff") { (Some(format!("{run}ruff format --check .")), vec![format!("{run}ruff check .")]) }
        else if t("black") { (Some(format!("{run}black --check .")), if t("flake8") { vec![format!("{run}flake8")] } else { vec![] }) }
        else { c.gaps.push("no hay ruff ni black configurado".into()); (None, vec![]) };
    if t("mypy") { lint.push(format!("{run}mypy .")); }
    c.commands.insert("format_check".into(), fmt);
    c.commands.insert("lint".into(), (!lint.is_empty()).then(|| lint.join(" && ")));
    let pytest = t("pytest");
    c.commands.insert("test".into(), pytest.then(|| format!("{run}pytest --cov")));
    if !pytest { c.gaps.push("no hay pytest: test deshabilitado".into()); }
    c.has_tests = any_file(base, &Regex::new(r"^test_.*\.py$|_test\.py$").unwrap());
    if fwh("fastapi") { c.openapi = Some(("fastapi".into(), "docs/openapi.json".into())); }
    if let Ok(a) = crate::adapters::load("python") {
        c.commands.retain(|k, v| a.commands.get(k) != Some(v));
    }
}

fn detect_rust(c: &mut ComponentFinding, base: &Path, _root: &Path) {
    let text = read(&base.join("Cargo.toml"));
    c.frameworks = ["axum", "actix-web", "rocket", "clap", "tokio", "utoipa", "ratatui"].iter()
        .filter(|n| Regex::new(&format!(r"(?m)^{}\s*=", regex::escape(n))).unwrap().is_match(&text)).map(|s| s.to_string()).collect();
    let fwh = |n: &str| c.frameworks.iter().any(|f| f == n);
    c.project_type = if fwh("axum") || fwh("actix-web") || fwh("rocket") { "api" }
        else if fwh("clap") || base.join("src/main.rs").exists() { "cli" } else { "library" }.into();
    if Regex::new(r"(?m)^(sqlx|diesel|tokio-postgres)\s*=").unwrap().is_match(&text) && text.contains("postgres") {
        c.db_engine = Some("postgres".into());
    }
    c.has_tests = true; // cargo test siempre existe; los tests pueden ser inline
    if fwh("utoipa") { c.openapi = Some(("utoipa".into(), "docs/openapi.json".into())); }
}

// ------------------------------------------------------------------ principal

pub fn discover(root: &Path) -> Result<Report> {
    let root = root.canonicalize().map_err(|e| anyhow!("{}: {e}", root.display()))?;
    if !root.is_dir() { bail!("{} no es un directorio", root.display()); }
    let remote = git::output(&root, &["remote", "get-url", "origin"]).ok();
    let mut rep = Report {
        root: root.display().to_string(),
        repo: repo_slug(remote.as_deref(), &root),
        default_branch: git::default_branch(&root),
        remote,
        ..Default::default()
    };
    let compose_re = Regex::new(r"^(docker-)?compose(\.[\w-]+)?\.ya?ml$").unwrap();
    let openapi_re = Regex::new(r"(?i)^(openapi|swagger)\.(ya?ml|json)$").unwrap();
    let test_stage = Regex::new(r"(?im)^\s*FROM\s+.+\s+AS\s+test\b").unwrap();
    let mut claimed: BTreeMap<&str, Vec<PathBuf>> = BTreeMap::new();

    for e in walk(&root, MAX_DEPTH + 1) {
        if !e.file_type().is_file() { continue; }
        let f = e.file_name().to_string_lossy().to_string();
        let dir = e.path().parent().unwrap().to_path_buf();
        let rdir = dir.strip_prefix(&root).unwrap().to_path_buf();
        if e.depth() <= MAX_DEPTH {
            if let Some((_, adapter)) = MANIFESTS.iter().find(|(m, _)| *m == f) {
                let list = claimed.entry(adapter).or_default();
                if !list.iter().any(|p| rdir.starts_with(p)) {
                    list.push(rdir.clone());
                    let name = if rdir.as_os_str().is_empty() { rep.repo.rsplit('/').next().unwrap().to_string() } else { rdir.file_name().unwrap().to_string_lossy().to_string() };
                    let mut c = ComponentFinding { name, path: rel(&root, &dir), adapter: adapter.to_string(), ..Default::default() };
                    match *adapter {
                        "go" => detect_go(&mut c, &dir, &root),
                        "typescript" => detect_ts(&mut c, &dir, &root),
                        "python" => detect_python(&mut c, &dir, &root),
                        _ => detect_rust(&mut c, &dir, &root),
                    }
                    c.source_dirs = SOURCE_DIRS.iter().filter(|d| dir.join(d).is_dir()).map(|s| s.to_string()).collect();
                    rep.components.push(c);
                }
            }
            if f == "Dockerfile" || f.starts_with("Dockerfile.") || f.ends_with(".Dockerfile") {
                rep.dockerfiles.push(rel(&root, e.path()));
                if test_stage.is_match(&read(e.path())) { rep.dockerfile_has_test_stage = true; }
            }
            if compose_re.is_match(&f) { rep.compose_files.push(rel(&root, e.path())); }
            if openapi_re.is_match(&f) { rep.openapi_files.push(rel(&root, e.path())); }
        }
        let dname = rdir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if ["migrations", "migration", "migrate"].contains(&dname.as_str()) && (f.ends_with(".sql") || f.ends_with(".go") || f.ends_with(".py")) {
            let r = rel(&root, &dir);
            if !rep.migrations.contains(&r) { rep.migrations.push(r); }
        }
        if ["seed", "seeds"].contains(&dname.as_str()) || Regex::new(r"^seed.*\.sql$").unwrap().is_match(&f) {
            let r = if f.ends_with(".sql") && !["seed", "seeds"].contains(&dname.as_str()) { rel(&root, e.path()) } else { rel(&root, &dir) };
            if !rep.seeds.contains(&r) { rep.seeds.push(r); }
        }
    }
    if let Some(mk) = ["Makefile", "makefile"].iter().map(|m| root.join(m)).find(|p| p.exists()) {
        rep.makefile_ci_target = Regex::new(r"(?m)^ci\s*:").unwrap().is_match(&read(&mk));
    }
    let wf = root.join(".github/workflows");
    if let Ok(rd) = std::fs::read_dir(&wf) {
        let mut w: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".yml") || n.ends_with(".yaml")).map(|n| format!(".github/workflows/{n}")).collect();
        w.sort();
        rep.ci_workflows = w;
    }
    for d in ["README.md", "CLAUDE.md", "docs/ARCHITECTURE.md", "docs/DATABASE.md", "docs/API.md", "docs/FEATURE_MAP.md", ".claude/settings.json"] {
        rep.docs.insert(d.into(), root.join(d).is_file());
    }
    rep.docs.insert("docs/adr/".into(), root.join("docs/adr").is_dir());
    rep.docs.insert("features/".into(), root.join("features").is_dir());
    rep.has_forja_config = root.join(crate::config::CONFIG_REL).exists();
    let pg = Regex::new(r#"image:\s*['"]?postgres"#).unwrap();
    if rep.compose_files.iter().any(|cf| pg.is_match(&read(&root.join(cf)))) {
        for c in rep.components.iter_mut().filter(|c| c.project_type == "api" && c.db_engine.is_none()) {
            c.db_engine = Some("postgres".into());
        }
    }
    rep.gaps = gaps(&rep);
    Ok(rep)
}

fn gaps(r: &Report) -> Vec<String> {
    let mut g = vec![];
    if r.components.is_empty() { g.push("no se detectó ningún componente (go.mod, package.json, pyproject.toml, Cargo.toml)".into()); }
    if r.dockerfiles.is_empty() { g.push("no hay Dockerfile".into()); }
    else if !r.dockerfile_has_test_stage { g.push("el Dockerfile no tiene stage `AS test`".into()); }
    if !r.compose_files.iter().any(|c| c.contains(".ci.")) { g.push("no hay docker-compose.ci.yml para el ambiente efímero".into()); }
    if !r.makefile_ci_target { g.push("no hay target `make ci`".into()); }
    if r.ci_workflows.is_empty() { g.push("no hay workflows de CI en .github/workflows".into()); }
    else if !r.ci_workflows.iter().any(|w| w.contains("forja")) { g.push("no hay workflow forja-ci".into()); }
    for (d, present) in &r.docs {
        if !present && d != "README.md" { g.push(format!("falta {d}")); }
    }
    if r.components.iter().any(|c| c.db_engine.is_some()) && r.migrations.is_empty() {
        g.push("hay base de datos pero no se encontraron migraciones".into());
    }
    for c in &r.components {
        if !c.has_tests { g.push(format!("[{}] no se encontraron pruebas", c.name)); }
        g.extend(c.gaps.iter().map(|x| format!("[{}] {x}", c.name)));
    }
    g
}

pub fn draft_config(r: &Report) -> ForjaConfig {
    let mut allowed = vec![];
    let components = r.components.iter().map(|c| {
        let prefix = if c.path == "." { String::new() } else { format!("{}/", c.path) };
        if c.source_dirs.is_empty() { allowed.push(format!("{prefix}**")); }
        allowed.extend(c.source_dirs.iter().map(|d| format!("{prefix}{d}/**")));
        Component {
            name: c.name.clone(), path: c.path.clone(), adapter: c.adapter.clone(),
            openapi: c.openapi.as_ref().map(|(g, o)| OpenApi { generator: Some(g.clone()), out: Some(o.clone()) }),
            database: c.db_engine.as_ref().map(|e| Database { engine: e.clone(), migrations: r.migrations.first().cloned(), schema_doc: Some("tbls".into()) }),
            e2e: c.e2e.clone(),
            commands: c.commands.clone(),
        }
    }).collect();
    allowed.push("tests/**".into());
    allowed.extend(r.migrations.iter().map(|m| format!("{m}/**")));
    allowed.dedup();
    ForjaConfig {
        repo: r.repo.clone(),
        default_branch: r.default_branch.clone(),
        components,
        environment: Environment {
            compose: r.compose_files.iter().find(|c| c.contains(".ci.")).cloned(),
            seed: r.seeds.iter().find(|s| s.ends_with(".sql")).or(r.seeds.first()).cloned(),
            base_url: None,
        },
        agents: Agents { allowed_paths: allowed, ..Default::default() },
        qa: Default::default(),
        gate: Default::default(),
    }
}

pub fn config_header(r: &Report) -> String {
    let mut s = String::from("# .forja/config.yaml — generado por `forja connect`. Revísalo y edítalo a mano.\n# components[].commands sobrescribe el contrato del adaptador; null deshabilita el paso.\n#\n");
    if !r.gaps.is_empty() {
        s.push_str("# Huecos detectados (los cubre `forja bootstrap`):\n");
        for g in &r.gaps { s.push_str(&format!("#   - {g}\n")); }
    }
    s.push('\n');
    s
}
