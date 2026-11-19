//! Pipeline de un feature. Los LLM crean; lo determinístico decide.
//!
//! draft -> planned -> [HITL #1] approved -> implementing -> documenting -> gating -> qa
//!       -> pr_open (o `ready` si no hay remoto/gh) -> [HITL #2] merged | changes_requested | rejected
use anyhow::{Context as _, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agent::{self, Call};
use crate::config::{self, ForjaConfig};
use crate::db::{self, Feature, Repo};
use crate::feature::{self, Spec};
use crate::gate;
use crate::git;
use crate::guard::{Policy, Role};

const MAX_DIFF: usize = 60_000;

pub struct Ctx {
    pub repo: Repo,
    pub root: PathBuf,
    pub cfg: ForjaConfig,
}

impl Ctx {
    pub fn load(repo: Repo) -> Result<Self> {
        let root = PathBuf::from(&repo.path);
        let cfg = config::load(&root)?;
        Ok(Self { repo, root, cfg })
    }
    fn feature(&self, id: &str) -> Result<Feature> {
        db::get_feature(self.repo.id, id)?.ok_or_else(|| anyhow!("{id} no está registrado en {}", self.repo.name))
    }
    fn safe_name(&self) -> String { self.repo.name.replace('/', "__") }
    fn worktree(&self, id: &str) -> PathBuf { crate::paths::worktrees().join(self.safe_name()).join(id) }
    fn log(&self, id: &str, node: &str, attempt: i64) -> PathBuf {
        crate::paths::logs().join(self.safe_name()).join(id).join(format!("{node}-{attempt}.jsonl"))
    }
    fn budget_left(&self, id: &str) -> Result<f64> {
        Ok(self.cfg.agents.budget_usd_per_feature - self.feature(id)?.cost_usd)
    }
}

fn say(f: &Feature, node: &str, msg: &str) {
    println!("[{}:{node}] {msg}", f.feat_id);
    let _ = db::event(f.id, node, "info", msg);
}

pub fn notify(title: &str, msg: &str) {
    if cfg!(target_os = "macos") {
        let script = format!("display notification {:?} with title {:?}", msg, title);
        let _ = Command::new("osascript").args(["-e", &script]).status();
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { return s.to_string(); }
    let mut cut = n;
    while !s.is_char_boundary(cut) { cut -= 1; }
    format!("{}\n\n[... diff truncado: {} bytes más ...]", &s[..cut], s.len() - cut)
}

fn read_opt(p: &Path) -> String { std::fs::read_to_string(p).unwrap_or_default() }

// ====================================================================== Etapa 1: intake

pub fn intake(ctx: &Ctx, title: &str, description: Option<&str>, use_ai: bool) -> Result<Spec> {
    let known: Vec<String> = db::list_features(ctx.repo.id)?.into_iter().map(|f| f.feat_id).collect();
    let id = feature::next_id(&ctx.root, &known);
    let mut spec = feature::template(&id, title, description, &ctx.cfg);
    if use_ai && agent::claude_available() && description.is_some() {
        let comps: Vec<_> = ctx.cfg.components.iter().map(|c| format!("{} ({})", c.name, c.adapter)).collect();
        let prompt = format!(
            "Convierte esta descripción de feature en YAML con exactamente estas claves: id, title, description, components, entry, test_user, flows (lista de {{name, acceptance: [..]}}), api (lista de 'METODO /ruta'), priority (baja|media|alta).\n\
             id: {id}\ntitle: {title}\ncomponentes válidos: {}\n\nDescripción:\n{}\n\n\
             Los criterios de aceptación deben ser observables por un usuario o por la API (los usa un agente QA de caja negra). \
             Incluye el caso feliz y al menos un caso de error. Responde SOLO el YAML, sin explicación.",
            comps.join(", "), description.unwrap_or(""));
        let tmp = std::env::temp_dir();
        let res = agent::run(Call {
            role: Role::Planner, node: "intake", cwd: &ctx.root, system: "Eres el asistente de intake de forja. No uses herramientas.".into(),
            prompt, budget_usd: 0.5, model: ctx.cfg.agents.model.as_deref(), feature_db_id: None, feature_id: &id,
            log_file: tmp.join(format!("forja-intake-{id}.jsonl")),
        });
        match res.and_then(|o| serde_yaml::from_str::<Spec>(&feature::strip_fences(&o.text)).map_err(Into::into)) {
            Ok(mut s) => {
                s.id = id.clone();
                s.components.retain(|c| ctx.cfg.component(c).is_some());
                if s.components.is_empty() { s.components = spec.components.clone(); }
                if s.validate(&ctx.cfg).is_ok() { spec = s; } else { eprintln!("intake: el YAML generado no pasó validación; uso plantilla"); }
            }
            Err(e) => eprintln!("intake: no pude estructurar con IA ({e}); uso plantilla"),
        }
    }
    let p = feature::save(&ctx.root, &spec)?;
    let f = db::upsert_feature(ctx.repo.id, &spec.id, &spec.title)?;
    db::transition(&f, "draft", &format!("creado {}", p.display()))?;
    Ok(spec)
}

// ====================================================================== Etapa 1: planificador

const PLANNER_SYSTEM: &str = "Eres el agente planificador de forja. Tienes solo herramientas de lectura (Read, Grep, Glob). \
No escribes archivos. Tu respuesta final ES el plan en Markdown y se guardará tal cual en features/<ID>.plan.md.";

pub fn plan(ctx: &Ctx, id: &str) -> Result<PathBuf> {
    let f = ctx.feature(id)?;
    if !["draft", "planned", "failed", "changes_requested"].contains(&f.state.as_str()) {
        bail!("{id} está en '{}'; solo se planea desde draft/planned", f.state);
    }
    let spec = feature::load(&ctx.root, id)?;
    spec.validate(&ctx.cfg).context("features YAML")?;
    let yaml = serde_yaml::to_string(&spec)?;
    let comps: Vec<String> = ctx.cfg.components.iter().map(|c| format!("- {} en `{}` ({})", c.name, c.path, c.adapter)).collect();
    let feedback = f.feedback.clone().map(|c| format!("\n\nEl humano rechazó el plan anterior con estos comentarios; atiéndelos:\n{c}\n\nPlan anterior:\n{}", read_opt(&feature::plan_path(&ctx.root, id)))).unwrap_or_default();
    let prompt = format!(
"Planea la implementación de este feature en el repo actual.

Feature ({id}):
```yaml
{yaml}```

Componentes del repo:
{}

Lee docs/ARCHITECTURE.md y docs/FEATURE_MAP.md si existen y el código relevante. Escritura permitida a los agentes: {:?}.

Tu respuesta debe ser SOLO el plan, en Markdown, con estas secciones:
# Plan {id}: {}
## Archivos
Por componente, archivos a crear o modificar y qué cambia en cada uno.
## Impacto declarado
Arquitectura, migraciones de DB y endpoints nuevos o cambiados. Termina la sección con este bloque exacto:
```impact
architecture: si|no
db_migrations: si|no
endpoints: [\"METODO /ruta\", ...]
```
## Plan de pruebas
Unitarias, de integración y flujos E2E, ligadas a cada criterio de aceptación.
## Riesgos
Riesgos y features vecinos que podrían romperse.{feedback}",
        comps.join("\n"), ctx.cfg.agents.allowed_paths, spec.title);
    say(&f, "plan", "planificando…");
    let out = agent::run(Call {
        role: Role::Planner, node: "plan", cwd: &ctx.root, system: PLANNER_SYSTEM.into(), prompt,
        budget_usd: ctx.budget_left(id)?, model: ctx.cfg.agents.model.as_deref(), feature_db_id: Some(f.id), feature_id: id,
        log_file: ctx.log(id, "plan", f.attempt),
    })?;
    if out.is_error || out.text.trim().is_empty() { bail!("el planificador no produjo un plan"); }
    let p = feature::plan_path(&ctx.root, id);
    std::fs::write(&p, feature::strip_fences_md(&out.text))?;
    db::set_feature_field(f.id, "feedback", None)?;
    db::transition(&ctx.feature(id)?, "planned", &format!("plan ${:.3}", out.cost_usd))?;
    say(&f, "plan", &format!("plan listo en {} — revísalo y aprueba (HITL #1)", p.display()));
    Ok(p)
}

/// HITL #1: aprobar.
pub fn approve(ctx: &Ctx, id: &str) -> Result<()> {
    let f = ctx.feature(id)?;
    if f.state != "planned" { bail!("{id} está en '{}'; solo se aprueba un feature planned", f.state); }
    if !feature::plan_path(&ctx.root, id).exists() { bail!("no existe el plan"); }
    feature::load(&ctx.root, id)?.validate(&ctx.cfg)?;
    db::transition(&f, "approved", "HITL #1 aprobado")
}

/// HITL #1: rechazar con comentarios (regresa a draft; replan los toma en cuenta).
pub fn reject_plan(ctx: &Ctx, id: &str, comment: &str) -> Result<()> {
    let f = ctx.feature(id)?;
    db::set_feature_field(f.id, "feedback", Some(comment))?;
    db::transition(&f, "draft", &format!("plan rechazado: {comment}"))
}

// ====================================================================== Etapa 2–6: coordinador

const IMPLEMENT_SYSTEM: &str = "Eres el nodo `implement` del coordinador de forja. Implementas el feature según el plan aprobado. \
Reglas: no escribes ni modificas pruebas (lo hace otro agente); no haces git commit/push/checkout (lo hace el orquestador); \
solo escribes en agents.allowed_paths. Si forja bloquea una acción, ajústate al motivo; no intentes evadirlo. \
Termina con un resumen breve de lo que cambiaste.";
const TEST_SYSTEM: &str = "Eres el nodo `test` del coordinador de forja. Escribes pruebas a partir de los criterios de aceptación, no a partir de la implementación. \
Solo puedes escribir archivos de prueba. No modifiques código de producción: si una prueba falla por un bug, déjala fallando y repórtalo. \
No uses git. Termina con un resumen: pruebas escritas y resultado al correrlas.";
const QUALITY_SYSTEM: &str = "Eres el nodo `quality` del coordinador de forja. Solo lectura. Comparas el diff contra el plan y los criterios de aceptación. \
No apruebas por gusto: si algo del plan o un criterio no está cubierto, o el gate falló, el veredicto es fix. \
Tu respuesta final es SOLO un objeto JSON: {\"verdict\": \"pass\" | \"fix\", \"issues\": [\"...\"]}.";
const DOCS_SYSTEM: &str = "Eres el agente de documentación de forja. No viste la conversación del implementador: documentas lo que realmente cambió según el diff. \
Solo escribes en docs/ y CHANGES/. No toques el bloque entre <!-- forja:feature-map:start --> y <!-- forja:feature-map:end --> (lo genera forja).";

pub struct RunOpts { pub push: bool }

fn test_globs(cfg: &ForjaConfig) -> Vec<String> {
    let mut g = vec!["tests/**".to_string()];
    for c in &cfg.components {
        if let Ok(a) = crate::adapters::load(&c.adapter) { g.extend(a.test_globs); }
    }
    g.sort(); g.dedup(); g
}

/// Revierte lo que el rol no podía escribir (p. ej. por Bash) y devuelve las violaciones.
fn enforce(wt: &Path, role: Role) -> Result<Vec<String>> {
    let pol = Policy::from_repo(role, wt)?;
    let mut v = vec![];
    for (st, path) in git::changed_files(wt)? {
        if let Err(why) = pol.may_write(Path::new(&path)) {
            git::revert_file(wt, &st, &path)?;
            v.push(format!("{path}: {why} (revertido)"));
        }
    }
    Ok(v)
}

fn agent_node(ctx: &Ctx, f: &Feature, wt: &Path, role: Role, node: &str, system: &str, prompt: String) -> Result<(agent::Outcome, Vec<String>)> {
    db::set_feature_field(f.id, "node", Some(node))?;
    say(f, node, &format!("intento {}", f.attempt));
    let out = agent::run(Call {
        role, node, cwd: wt, system: system.into(), prompt, budget_usd: ctx.budget_left(&f.feat_id)?,
        model: ctx.cfg.agents.model.as_deref(), feature_db_id: Some(f.id), feature_id: &f.feat_id,
        log_file: ctx.log(&f.feat_id, node, f.attempt),
    })?;
    let violations = enforce(wt, role)?;
    for v in &violations { let _ = db::event(f.id, node, "blocked", v); }
    Ok((out, violations))
}

fn run_gate_in(ctx: &Ctx, f: &Feature, wt: &Path, steps: Option<&[String]>, comps: Option<&[String]>, label: &str) -> Result<gate::GateResult> {
    let cfg = config::load(wt).unwrap_or_else(|_| ctx.cfg.clone());
    let log_dir = crate::paths::runs().join(ctx.safe_name()).join(&f.feat_id).join(format!("{label}-{}", f.attempt));
    let run_id = db::start_run(ctx.repo.id, label, Some(&f.feat_id), &log_dir)?;
    let fid = f.id;
    let mut on_start = |p: &gate::PlannedStep| { let _ = db::event(fid, "gate", "step", &format!("{} / {} …", p.component, p.step)); };
    let mut on_line = |_: &str| {};
    let mut on_done = |r: &gate::StepResult| {
        let _ = db::event(fid, "gate", "step", &format!("{} / {}: {:?} {}", r.component, r.step, r.status, r.reason.clone().unwrap_or_default()));
    };
    let res = gate::run(wt, &cfg, &log_dir, steps, comps, false, &mut gate::Hooks { on_start: &mut on_start, on_line: &mut on_line, on_done: &mut on_done })?;
    db::finish_run(run_id, if res.passed() { "pass" } else { "fail" }, &serde_json::to_value(&res)?)?;
    Ok(res)
}

fn parse_verdict(text: &str) -> (bool, Vec<String>) {
    let t = feature::strip_fences(text);
    let start = t.find('{');
    let end = t.rfind('}');
    if let (Some(s), Some(e)) = (start, end) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t[s..=e]) {
            let pass = v["verdict"].as_str() == Some("pass");
            let issues = v["issues"].as_array().map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect()).unwrap_or_default();
            return (pass, issues);
        }
    }
    (false, vec![format!("quality no devolvió JSON válido: {}", text.chars().take(300).collect::<String>())])
}

/// Impacto declarado en el plan (bloque ```impact```).
#[derive(Default, Debug)]
pub struct Impact { pub architecture: Option<bool>, pub db_migrations: Option<bool>, pub endpoints: Vec<String> }

pub fn parse_impact(plan: &str) -> Impact {
    let re = regex::Regex::new(r"(?s)```impact\s*\n(.*?)```").unwrap();
    let mut imp = Impact::default();
    if let Some(c) = re.captures(plan) {
        if let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&c[1]) {
            let yn = |k: &str| v.get(k).map(|x| match x { serde_yaml::Value::Bool(b) => *b, other => other.as_str().is_some_and(|s| s.starts_with('s') || s == "yes" || s == "true") });
            imp.architecture = yn("architecture");
            imp.db_migrations = yn("db_migrations");
            imp.endpoints = v.get("endpoints").and_then(|e| e.as_sequence()).map(|s| s.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        }
    }
    imp
}

/// Discrepancias determinísticas entre el impacto declarado y el diff real, y doc-drift.
pub fn doc_drift(ctx: &Ctx, wt: &Path, spec: &Spec, base: &str) -> Result<(Vec<String>, Vec<String>)> {
    let files = git::output(wt, &["diff", "--name-only", &format!("{base}...HEAD")])?;
    let files: Vec<&str> = files.lines().collect();
    let imp = parse_impact(&read_opt(&feature::plan_path(wt, &spec.id)));
    let migs: Vec<String> = ctx.cfg.components.iter().filter_map(|c| c.database.as_ref()?.migrations.clone()).collect();
    let touches_migrations = files.iter().any(|f| migs.iter().any(|m| f.starts_with(m.as_str())));
    let mut discrepancies = vec![];
    if imp.db_migrations == Some(false) && touches_migrations { discrepancies.push("el plan declaraba sin migraciones de DB, pero el diff trae migraciones".into()); }
    if imp.db_migrations == Some(true) && !touches_migrations { discrepancies.push("el plan declaraba migraciones de DB, pero el diff no trae ninguna".into()); }
    let mut drift = vec![];
    let changes = format!("CHANGES/{}.md", spec.id);
    if !wt.join(&changes).exists() { drift.push(format!("falta {changes}")); }
    if touches_migrations && !files.contains(&"docs/DATABASE.md") { drift.push("hay migraciones nuevas pero docs/DATABASE.md no cambió".into()); }
    if (!spec.api.is_empty() || !imp.endpoints.is_empty()) && !files.contains(&"docs/API.md") { drift.push("el feature cambia endpoints pero docs/API.md no cambió".into()); }
    if !read_opt(&wt.join("docs/FEATURE_MAP.md")).contains(&spec.id) { drift.push("el feature no tiene entrada en docs/FEATURE_MAP.md".into()); }
    Ok((discrepancies, drift))
}

/// Tabla determinística del FEATURE_MAP desde features/*.yaml.
pub fn feature_map_table(root: &Path, repo_id: Option<i64>) -> String {
    let states: std::collections::HashMap<String, String> = repo_id.and_then(|id| db::list_features(id).ok())
        .map(|v| v.into_iter().map(|f| (f.feat_id, f.state)).collect()).unwrap_or_default();
    let mut s = String::from("| Feature | Título | Componentes | Flujo | Criterios | Estado | QA |\n|---|---|---|---|---|---|---|\n");
    for spec in feature::list(root) {
        let st = states.get(&spec.id).cloned().unwrap_or_else(|| "—".into());
        let qa = qa_status(root, &spec.id);
        for fl in &spec.flows {
            s.push_str(&format!("| {} | {} | {} | {} | {} | {} | {} |\n", spec.id, spec.title, spec.components.join(", "), fl.name, fl.acceptance.len(), st, qa));
        }
    }
    s
}

fn qa_status(root: &Path, id: &str) -> &'static str {
    let r = read_opt(&root.join(format!("qa-reports/{id}.md")));
    if r.is_empty() { "⏳ pendiente" } else if r.contains('❌') { "❌" } else if r.contains("⚠") { "⚠️" } else { "✅" }
}

pub fn write_feature_map(root: &Path, repo_id: Option<i64>) -> Result<()> {
    let p = root.join("docs/FEATURE_MAP.md");
    std::fs::create_dir_all(root.join("docs"))?;
    let (start, end) = ("<!-- forja:feature-map:start -->", "<!-- forja:feature-map:end -->");
    let block = format!("{start}\n{}{end}", feature_map_table(root, repo_id));
    let cur = read_opt(&p);
    let new = match (cur.find(start), cur.find(end)) {
        (Some(s), Some(e)) if e > s => format!("{}{block}{}", &cur[..s], &cur[e + end.len()..]),
        _ if cur.is_empty() => format!("# Feature map\n\nTabla generada por forja desde `features/*.yaml`. Las notas de riesgo por flujo van debajo.\n\n{block}\n\n## Notas de riesgo\n"),
        _ => format!("{cur}\n\n{block}\n"),
    };
    std::fs::write(p, new)?;
    Ok(())
}

pub fn run(ctx: &Ctx, id: &str, opts: &RunOpts) -> Result<()> {
    let mut f = ctx.feature(id)?;
    if !["approved", "changes_requested", "implementing", "documenting", "gating", "qa", "failed"].contains(&f.state.as_str()) {
        bail!("{id} está en '{}'; necesita estar approved (HITL #1) o changes_requested", f.state);
    }
    if f.state == "failed" && f.branch.is_none() { bail!("{id} falló antes de tener rama; vuelve a planear"); }
    let spec = feature::load(&ctx.root, id)?;
    let base_branch = ctx.cfg.default_branch.clone();
    let wt = ctx.worktree(id);
    let branch = f.branch.clone().unwrap_or_else(|| spec.branch());

    // --- aislamiento: rama + worktree propios
    git::ensure_worktree(&ctx.root, &wt, &branch, &base_branch)?;
    db::set_feature_field(f.id, "branch", Some(&branch))?;
    db::set_feature_field(f.id, "worktree", Some(&wt.display().to_string()))?;
    for rel in [format!("features/{id}.yaml"), format!("features/{id}.plan.md"), config::CONFIG_REL.to_string()] {
        let (src, dst) = (ctx.root.join(&rel), wt.join(&rel));
        if src.exists() && (!dst.exists() || rel.starts_with("features/")) {
            std::fs::create_dir_all(dst.parent().unwrap())?;
            std::fs::copy(&src, &dst)?;
        }
    }
    git::commit_all(&wt, &format!("chore({id}): feature y plan aprobado"))?;
    let base_ref = if git::has_remote(&ctx.root) { format!("origin/{base_branch}") } else { base_branch.clone() };
    let base_ref = if git::output(&wt, &["rev-parse", "--verify", "--quiet", &base_ref]).is_ok() { base_ref } else { base_branch.clone() };

    let plan_md = read_opt(&feature::plan_path(&wt, id));
    let yaml = serde_yaml::to_string(&spec)?;
    let comps: Vec<String> = if spec.components.is_empty() { ctx.cfg.components.iter().map(|c| c.name.clone()).collect() } else { spec.components.clone() };
    let test_cmds: Vec<String> = comps.iter().filter_map(|n| {
        let c = ctx.cfg.component(n)?;
        let a = crate::adapters::load(&c.adapter).ok()?;
        a.command("test", &c.commands).map(|t| format!("- {n}: `cd {} && {t}`", c.path))
    }).collect();
    let mut feedback = f.feedback.clone().map(|fb| format!("Comentarios del revisor humano en el PR:\n{fb}")).unwrap_or_default();
    let mc = ctx.cfg.agents.max_retries.code as i64;
    let mg = ctx.cfg.agents.max_retries.gate as i64;

    let fail = |f: &Feature, why: &str| -> Result<()> {
        db::transition(&ctx.feature(&f.feat_id)?, "failed", why)?;
        let _ = db::event(f.id, "coordinator", "error", why);
        notify("forja", &format!("{} falló: {why}", f.feat_id));
        bail!("{}: {why}", f.feat_id)
    };

    for gate_round in 0..=mg {
        // ---------------------------------------------------- Etapa 2: implement -> test -> quality
        db::transition(&ctx.feature(id)?, "implementing", if feedback.is_empty() { "inicio" } else { "con feedback" })?;
        let mut code_ok = false;
        for attempt in 1..=mc {
            db::set_attempt(f.id, attempt + gate_round * mc)?;
            f = ctx.feature(id)?;
            let fb = if feedback.is_empty() { String::new() } else { format!("\n\n## Feedback a atender (vuelta anterior)\n{feedback}") };
            let (_, v1) = agent_node(ctx, &f, &wt, Role::Implement, "implement", IMPLEMENT_SYSTEM, format!(
                "Implementa {id} en este worktree.\n\n## Feature\n```yaml\n{yaml}```\n\n## Plan aprobado\n{plan_md}\n\nallowed_paths: {:?}{fb}", ctx.cfg.agents.allowed_paths))?;
            git::commit_all(&wt, &format!("feat({id}): {} (vuelta {attempt})", spec.title))?;

            let (_, v2) = agent_node(ctx, &f, &wt, Role::Test, "test", TEST_SYSTEM, format!(
                "Escribe y corre las pruebas de {id}.\n\n## Criterios de aceptación\n```yaml\n{yaml}```\n\n## Plan de pruebas (del plan aprobado)\n{plan_md}\n\nGlobs de archivos de prueba permitidos: {:?}\nComandos de prueba:\n{}",
                test_globs(&ctx.cfg), test_cmds.join("\n")))?;
            git::commit_all(&wt, &format!("test({id}): pruebas de aceptación (vuelta {attempt})"))?;

            // determinístico: format_check + lint + test de los componentes del feature
            let g = run_gate_in(ctx, &f, &wt, None, Some(&comps), "gate-code")?;
            let diff = truncate(&git::diff_vs(&wt, &base_ref).unwrap_or_default(), MAX_DIFF);
            let gate_txt = g.steps.iter().map(|s| format!("- {} / {}: {:?}{}", s.component, s.step, s.status, s.reason.as_ref().map(|r| format!(" ({r})")).unwrap_or_default())).collect::<Vec<_>>().join("\n");
            let (q, _) = agent_node(ctx, &f, &wt, Role::Quality, "quality", QUALITY_SYSTEM, format!(
                "Evalúa {id}.\n\n## Feature\n```yaml\n{yaml}```\n\n## Plan\n{plan_md}\n\n## Resultado del gate determinístico\n{gate_txt}\n\n{}\n\n## Diff contra {base_ref}\n```diff\n{diff}\n```",
                if g.passed() { String::new() } else { format!("## Fallas\n{}", g.failure_feedback()) }))?;
            let (pass, mut issues) = parse_verdict(&q.text);
            issues.extend(v1.into_iter().chain(v2).map(|v| format!("guardrail: {v}")));
            let _ = db::event(f.id, "quality", "verdict", &format!("{} · {} problemas", if pass && g.passed() { "pass" } else { "fix" }, issues.len()));
            if pass && g.passed() { code_ok = true; break; }
            feedback = format!("{}\n\n{}", issues.iter().map(|i| format!("- {i}")).collect::<Vec<_>>().join("\n"),
                               if g.passed() { String::new() } else { g.failure_feedback() });
            if ctx.budget_left(id)? <= 0.0 { return fail(&f, "presupuesto agotado"); }
        }
        if !code_ok { return fail(&f, &format!("quality/gate no pasó tras {mc} vueltas")); }
        db::set_feature_field(f.id, "feedback", None)?;

        // ---------------------------------------------------- Etapa 3: documentación
        db::transition(&ctx.feature(id)?, "documenting", "")?;
        f = ctx.feature(id)?;
        write_feature_map(&wt, Some(ctx.repo.id))?;
        let diff = truncate(&git::diff_vs(&wt, &base_ref).unwrap_or_default(), MAX_DIFF);
        let stat = git::diff_stat(&wt, &base_ref).unwrap_or_default();
        let (discrepancies, _) = doc_drift(ctx, &wt, &spec, &base_ref)?;
        agent_node(ctx, &f, &wt, Role::Docs, "docs", DOCS_SYSTEM, format!(
"Documenta {id}.

1. Escribe SIEMPRE CHANGES/{id}.md con: `## Resumen para el revisor`, `## Archivos` (usa el stat), `## Discrepancias plan vs diff`.
2. Actualiza docs/ARCHITECTURE.md (y agrega un ADR en docs/adr/) solo si el diff toca límites de módulos, dependencias o infraestructura.
3. Actualiza docs/DATABASE.md si hay migraciones nuevas: qué cambió y por qué.
4. Actualiza docs/API.md si cambian endpoints: descripción, ejemplos y errores por endpoint.
5. En docs/FEATURE_MAP.md agrega notas de riesgo por flujo debajo de la tabla generada (no edites la tabla).

Discrepancias detectadas por forja (inclúyelas): {discrepancies:?}

## Feature
```yaml
{yaml}```
## Plan
{plan_md}
## Stat
{stat}
## Diff
```diff
{diff}
```"))?;
        write_feature_map(&wt, Some(ctx.repo.id))?;
        git::commit_all(&wt, &format!("docs({id}): documentación del feature"))?;

        // ---------------------------------------------------- Etapa 4: gate completo + doc-drift
        db::transition(&ctx.feature(id)?, "gating", "")?;
        f = ctx.feature(id)?;
        let g = run_gate_in(ctx, &f, &wt, None, None, "gate")?;
        let (_, drift) = doc_drift(ctx, &wt, &spec, &base_ref)?;
        for d in &drift { let _ = db::event(f.id, "gate", "drift", d); }
        if g.passed() && drift.is_empty() {
            // ------------------------------------------------ Etapa 5: QA (pendiente)
            db::transition(&ctx.feature(id)?, "qa", "etapa 5 aún no implementada: se omite")?;
            say(&f, "qa", "QA de caja negra aún no implementado (roadmap fase 5); se omite");
            return open_pr(ctx, id, &wt, &branch, &base_ref, &discrepancies, opts);
        }
        feedback = format!("El gate falló:\n{}\n{}", g.failure_feedback(), drift.iter().map(|d| format!("- doc-drift: {d}")).collect::<Vec<_>>().join("\n"));
        say(&f, "gate", &format!("falló (ronda {}/{})", gate_round + 1, mg + 1));
    }
    fail(&ctx.feature(id)?, &format!("el gate no pasó tras {} rondas", mg + 1))
}

// ====================================================================== Etapa 6: PR

fn open_pr(ctx: &Ctx, id: &str, wt: &Path, branch: &str, base_ref: &str, discrepancies: &[String], opts: &RunOpts) -> Result<()> {
    let f = ctx.feature(id)?;
    let spec = feature::load(wt, id)?;
    let remote = git::has_remote(&ctx.root);
    if remote {
        let before = git::output(wt, &["rev-parse", "HEAD"])?;
        let _ = git::run(wt, &["fetch", "origin", &ctx.cfg.default_branch]);
        if git::run(wt, &["rebase", base_ref]).is_err() {
            let _ = git::run(wt, &["rebase", "--abort"]);
            db::transition(&f, "failed", "conflictos al hacer rebase; resolución automática pendiente")?;
            notify("forja", &format!("{id}: conflictos de rebase con {base_ref}"));
            bail!("{id}: conflictos al hacer rebase sobre {base_ref}; resuélvelos en {}", wt.display());
        }
        if git::output(wt, &["rev-parse", "HEAD"])? != before {
            let g = run_gate_in(ctx, &f, wt, None, None, "gate-rebase")?;
            if !g.passed() {
                db::transition(&f, "failed", "el gate falló tras el rebase")?;
                bail!("{id}: el gate falló después del rebase:\n{}", g.failure_feedback());
            }
        }
    }
    let body = pr_body(ctx, wt, &spec, &f, discrepancies, base_ref);
    let body_file = crate::paths::logs().join(ctx.safe_name()).join(id).join("pr-body.md");
    std::fs::create_dir_all(body_file.parent().unwrap())?;
    std::fs::write(&body_file, &body)?;
    let has_gh = which::which("gh").is_ok();
    if !(remote && opts.push && has_gh) {
        let why = if !remote { "sin remoto" } else if !opts.push { "--no-push" } else { "gh no instalado" };
        db::transition(&f, "ready", &format!("rama {branch} lista localmente ({why})"))?;
        say(&f, "pr", &format!("rama {branch} lista en {} ({why}). Cuerpo del PR: {}", wt.display(), body_file.display()));
        notify("forja", &format!("{id} listo para revisión ({why})"));
        return Ok(());
    }
    git::push_branch(wt, branch, &ctx.cfg.default_branch)?;
    let out = Command::new("gh").current_dir(wt).args(["pr", "create", "--base", &ctx.cfg.default_branch, "--head", branch,
        "--title", &format!("feat({id}): {}", spec.title), "--body-file"]).arg(&body_file).output()?;
    if !out.status.success() { bail!("gh pr create: {}", String::from_utf8_lossy(&out.stderr)); }
    let url = String::from_utf8_lossy(&out.stdout).trim().lines().last().unwrap_or("").to_string();
    db::set_feature_field(f.id, "pr_url", Some(&url))?;
    db::transition(&ctx.feature(id)?, "pr_open", &url)?;
    say(&f, "pr", &format!("PR abierto: {url} (HITL #2)"));
    notify("forja", &format!("{id}: PR listo para revisión"));
    Ok(())
}

fn pr_body(ctx: &Ctx, wt: &Path, spec: &Spec, f: &Feature, discrepancies: &[String], base_ref: &str) -> String {
    let changes = read_opt(&wt.join(format!("CHANGES/{}.md", spec.id)));
    let pol = Policy::from_repo(Role::Test, wt).ok();
    let files = git::output(wt, &["diff", "--name-only", &format!("{base_ref}...HEAD")]).unwrap_or_default();
    let tests: Vec<&str> = files.lines().filter(|l| pol.as_ref().is_some_and(|p| p.is_test_file(Path::new(l)))).collect();
    let mut s = format!("## {} — {}\n\n{}\n\n## Criterios de aceptación\n", spec.id, spec.title, if changes.is_empty() { "_Sin CHANGES_".into() } else { changes });
    for fl in &spec.flows {
        s.push_str(&format!("**{}**\n", fl.name));
        for a in &fl.acceptance { s.push_str(&format!("- [ ] {a}\n")); }
    }
    s.push_str("\n## Discrepancias plan vs diff\n");
    if discrepancies.is_empty() { s.push_str("Ninguna detectada por forja.\n"); } else { for d in discrepancies { s.push_str(&format!("- ⚠️ {d}\n")); } }
    s.push_str("\n## QA\nEtapa 5 (QA de caja negra) aún no implementada en esta versión de forja.\n\n## Pruebas nuevas\n");
    if tests.is_empty() { s.push_str("Ninguna.\n"); } else { for t in tests { s.push_str(&format!("- `{t}`\n")); } }
    s.push_str(&format!("\n## Costo\n${:.3} USD (presupuesto ${:.2}) · {} vueltas\n\n---\nGenerado por forja. El merge requiere tu aprobación (HITL #2).\n",
                        f.cost_usd, ctx.cfg.agents.budget_usd_per_feature, f.attempt));
    s
}

/// HITL #2 vía GitHub: sincroniza estado y comentarios de los PR abiertos.
pub fn sync_prs(ctx: &Ctx) -> Result<Vec<String>> {
    if which::which("gh").is_err() { bail!("gh no está instalado"); }
    let mut out = vec![];
    for f in db::list_features(ctx.repo.id)?.into_iter().filter(|f| f.state == "pr_open") {
        let Some(url) = &f.pr_url else { continue };
        let o = Command::new("gh").args(["pr", "view", url, "--json", "state,reviewDecision,reviews,comments"]).output()?;
        if !o.status.success() { out.push(format!("{}: {}", f.feat_id, String::from_utf8_lossy(&o.stderr).trim())); continue; }
        let v: serde_json::Value = serde_json::from_slice(&o.stdout)?;
        match v["state"].as_str() {
            Some("MERGED") => { close(ctx, &f.feat_id, true, "merge en GitHub")?; out.push(format!("{}: merged", f.feat_id)); }
            Some("CLOSED") => { close(ctx, &f.feat_id, false, "PR cerrado en GitHub")?; out.push(format!("{}: rejected", f.feat_id)); }
            _ if v["reviewDecision"] == "CHANGES_REQUESTED" => {
                let mut fb = vec![];
                for r in v["reviews"].as_array().into_iter().flatten().chain(v["comments"].as_array().into_iter().flatten()) {
                    if let Some(b) = r["body"].as_str().filter(|b| !b.trim().is_empty()) { fb.push(format!("- {}", b.trim())); }
                }
                db::set_feature_field(f.id, "feedback", Some(&fb.join("\n")))?;
                db::transition(&f, "changes_requested", "cambios pedidos en el PR")?;
                out.push(format!("{}: changes_requested ({} comentarios) → `forja feature run {}`", f.feat_id, fb.len(), f.feat_id));
            }
            _ => out.push(format!("{}: abierto, esperando revisión", f.feat_id)),
        }
    }
    Ok(out)
}

/// Cierre del ciclo: merged (limpia worktree) o rejected.
pub fn close(ctx: &Ctx, id: &str, merged: bool, reason: &str) -> Result<()> {
    let f = ctx.feature(id)?;
    db::transition(&f, if merged { "merged" } else { "rejected" }, reason)?;
    if let Some(wt) = &f.worktree {
        if Path::new(wt).exists() { git::remove_worktree(&ctx.root, Path::new(wt))?; }
    }
    Ok(())
}

pub fn request_changes(ctx: &Ctx, id: &str, comments: &str) -> Result<()> {
    let f = ctx.feature(id)?;
    if !["pr_open", "ready"].contains(&f.state.as_str()) { bail!("{id} está en '{}'", f.state); }
    db::set_feature_field(f.id, "feedback", Some(comments))?;
    db::transition(&f, "changes_requested", comments)
}
