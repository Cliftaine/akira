mod adapters;
mod agent;
mod bootstrap;
mod config;
mod db;
mod discovery;
mod feature;
mod gate;
mod git;
mod guard;
mod paths;
mod pipeline;
mod tui;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "forja", version, about = "Pipeline agéntico de features sobre Claude Code. Sin subcomando abre la TUI.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Crea ~/.forja y copia los adaptadores para que puedas editarlos.
    Setup { #[arg(long)] overwrite: bool },
    /// Descubrimiento read-only: stack, herramientas y huecos. No escribe nada.
    Discover { path: PathBuf, #[arg(long)] json: bool },
    /// Conecta un repo: descubre, propone .forja/config.yaml y lo registra.
    Connect {
        path: PathBuf,
        /// Escribe sin preguntar.
        #[arg(long, short)] yes: bool,
        /// Sobrescribe un config.yaml existente.
        #[arg(long)] force: bool,
        /// Solo imprime el borrador.
        #[arg(long)] print: bool,
    },
    /// Lista los repos registrados.
    Repos,
    /// Quita un repo del registro (no borra archivos).
    Forget { repo: String },
    /// Valida .forja/config.yaml.
    Validate { #[arg(long)] repo: Option<String> },
    /// Corre el gate determinístico (format_check, lint, test por componente).
    Gate {
        #[arg(long)] repo: Option<String>,
        /// Directorio donde correr (p. ej. un worktree). Por defecto la raíz del repo.
        #[arg(long)] dir: Option<PathBuf>,
        #[arg(long = "step")] steps: Vec<String>,
        #[arg(long = "component")] components: Vec<String>,
        #[arg(long)] fail_fast: bool,
        #[arg(long, short)] verbose: bool,
        #[arg(long)] dry_run: bool,
    },
    /// Revisa herramientas instaladas (git, gh, claude, docker y las de cada adaptador).
    Doctor { #[arg(long)] repo: Option<String> },
    /// Genera el PR de bootstrap (make ci, forja-ci, docs, hooks) en forja/bootstrap.
    Bootstrap { #[arg(long)] repo: Option<String>, #[arg(long)] no_push: bool, #[arg(long)] force: bool },
    /// Hook PreToolUse de guardrails (lo invoca Claude Code). Sale con 2 para bloquear.
    Hook { #[arg(long, value_enum)] role: guard::Role, #[arg(long)] root: Option<PathBuf> },
    /// Ciclo de vida de features.
    #[command(subcommand)]
    Feature(FeatCmd),
    /// Sincroniza los PR abiertos con GitHub (merge, cierre, cambios pedidos).
    PrSync { #[arg(long)] repo: Option<String> },
    /// Imprime la tabla del FEATURE_MAP generada desde features/*.yaml.
    Map { #[arg(long)] repo: Option<String> },
}

#[derive(Subcommand)]
enum FeatCmd {
    /// Intake: crea features/FEAT-NNN.yaml (con IA si das --description).
    New { #[arg(long)] repo: Option<String>, #[arg(long)] title: String, #[arg(long)] description: Option<String>, #[arg(long)] no_ai: bool },
    /// Lista features y su estado.
    List { #[arg(long)] repo: Option<String> },
    /// Corre el agente planificador (solo lectura).
    Plan { id: String, #[arg(long)] repo: Option<String> },
    /// HITL #1: aprueba el plan.
    Approve { id: String, #[arg(long)] repo: Option<String> },
    /// HITL #1: rechaza el plan con comentarios.
    Reject { id: String, #[arg(long)] comment: String, #[arg(long)] replan: bool, #[arg(long)] repo: Option<String> },
    /// Corre el coordinador: implement → test → quality → docs → gate → PR.
    Run { id: String, #[arg(long)] no_push: bool, #[arg(long)] repo: Option<String> },
    /// HITL #2 local: pide cambios (regresa a la etapa 2 en la misma rama).
    Changes { id: String, #[arg(long)] comment: String, #[arg(long)] repo: Option<String> },
    /// HITL #2 local: marca como merged o rejected y limpia el worktree.
    Close { id: String, #[arg(long)] merged: bool, #[arg(long, default_value = "")] reason: String, #[arg(long)] repo: Option<String> },
    /// Historial auditable y últimos eventos.
    Status { id: String, #[arg(long)] repo: Option<String> },
}

/// Resuelve un repo registrado por nombre/ruta, o el repo git del cwd.
pub fn resolve_repo(r: Option<&str>) -> Result<db::Repo> {
    let key = match r {
        Some(k) => k.to_string(),
        None => {
            let cwd = std::env::current_dir()?;
            git::output(&cwd, &["rev-parse", "--show-toplevel"]).unwrap_or_else(|_| cwd.display().to_string())
        }
    };
    db::find_repo(&key)?.ok_or_else(|| anyhow!("'{key}' no está registrado. Usa `forja connect <ruta>` o --repo <nombre>"))
}

/// Discover + borrador. Devuelve (reporte, yaml).
pub fn draft(path: &Path) -> Result<(discovery::Report, String)> {
    let rep = discovery::discover(path)?;
    let cfg = discovery::draft_config(&rep);
    let yaml = cfg.to_yaml(&discovery::config_header(&rep))?;
    Ok((rep, yaml))
}

/// Escribe el config validado y registra el repo.
pub fn write_config(rep: &discovery::Report, yaml: &str) -> Result<db::Repo> {
    let root = PathBuf::from(&rep.root);
    let cfg = config::parse(yaml, Some(&root))?;
    let p = config::path(&root);
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, yaml)?;
    db::upsert_repo(&cfg.repo, &root, rep.remote.as_deref(), &rep.stack())
}

fn print_report(rep: &discovery::Report) {
    println!("repo:     {}  ({})", rep.repo, rep.root);
    println!("remoto:   {}", rep.remote.as_deref().unwrap_or("—"));
    println!("rama:     {}", rep.default_branch);
    println!("stack:    {}", rep.stack().join(", "));
    for c in &rep.components {
        println!("  • {:<12} {:<10} {:<8} {:<9} fw: {}{}", c.name, c.path, c.adapter, c.project_type, c.frameworks.join(","),
                 c.db_engine.as_ref().map(|d| format!("  db: {d}")).unwrap_or_default());
    }
    println!("docker:   {}  compose: {}  CI: {}", if rep.dockerfiles.is_empty() { "—".into() } else { rep.dockerfiles.join(",") },
             if rep.compose_files.is_empty() { "—".into() } else { rep.compose_files.join(",") },
             if rep.ci_workflows.is_empty() { "—".into() } else { rep.ci_workflows.join(",") });
    println!("migraciones: {}   seeds: {}", if rep.migrations.is_empty() { "—".into() } else { rep.migrations.join(",") }, if rep.seeds.is_empty() { "—".into() } else { rep.seeds.join(",") });
    if !rep.gaps.is_empty() {
        println!("\nhuecos ({}):", rep.gaps.len());
        for g in &rep.gaps { println!("  - {g}"); }
    }
}

fn ask(q: &str) -> String {
    use std::io::Write;
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_lowercase()
}

pub fn edit_text(initial: &str, suffix: &str) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!("forja-edit-{}{suffix}", std::process::id()));
    std::fs::write(&tmp, initial)?;
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let parts = shell_words::split(&editor)?;
    let st = std::process::Command::new(&parts[0]).args(&parts[1..]).arg(&tmp).status()?;
    if !st.success() { bail!("el editor salió con error"); }
    let out = std::fs::read_to_string(&tmp)?;
    let _ = std::fs::remove_file(tmp);
    Ok(out)
}

fn main() {
    let cli = Cli::parse();
    // El hook debe salir con 0/2 exactos y nada más en stdout.
    if let Some(Cmd::Hook { role, root }) = &cli.cmd {
        std::process::exit(guard::run_hook(*role, root.clone()));
    }
    let code = match run(cli) {
        Ok(c) => c,
        Err(e) => { eprintln!("error: {e:#}"); 1 }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    let Some(cmd) = cli.cmd else { tui::run()?; return Ok(0) };
    match cmd {
        Cmd::Setup { overwrite } => {
            let home = paths::ensure()?;
            for p in adapters::install(overwrite)? { println!("adaptador: {}", p.display()); }
            db::open()?;
            println!("listo: {}", home.display());
        }
        Cmd::Discover { path, json } => {
            let rep = discovery::discover(&path)?;
            if json { println!("{}", serde_json::to_string_pretty(&rep)?); } else { print_report(&rep); }
        }
        Cmd::Connect { path, yes, force, print } => {
            let (rep, mut yaml) = draft(&path)?;
            if print { print!("{yaml}"); return Ok(0); }
            print_report(&rep);
            if rep.has_forja_config && !force {
                let root = PathBuf::from(&rep.root);
                let cfg = config::load(&root)?;
                let r = db::upsert_repo(&cfg.repo, &root, rep.remote.as_deref(), &rep.stack())?;
                println!("\nya existe {}; lo registré como '{}' sin cambios (usa --force para regenerarlo).", config::CONFIG_REL, r.name);
                return Ok(0);
            }
            println!("\n--- borrador de {} ---\n{yaml}", config::CONFIG_REL);
            if !yes {
                loop {
                    match ask("[w] escribir  [e] editar  [c] cancelar: ").as_str() {
                        "w" => break,
                        "e" => {
                            yaml = edit_text(&yaml, ".yaml")?;
                            if let Err(e) = config::parse(&yaml, Some(Path::new(&rep.root))) { println!("{e}"); }
                        }
                        _ => { println!("cancelado"); return Ok(1); }
                    }
                }
            }
            let r = write_config(&rep, &yaml)?;
            println!("escrito {} y registrado '{}'. Siguiente: `forja gate --repo {}` y `forja bootstrap --repo {}`", config::path(Path::new(&rep.root)).display(), r.name, r.name, r.name);
        }
        Cmd::Repos => {
            let repos = db::list_repos()?;
            if repos.is_empty() { println!("sin repos. `forja connect <ruta>`"); }
            for r in repos { println!("{:<28} {:<22} {:<50} última corrida: {}", r.name, r.stack.join(","), r.path, r.last_run_at.unwrap_or_else(|| "—".into())); }
        }
        Cmd::Forget { repo } => { let r = resolve_repo(Some(&repo))?; db::remove_repo(r.id)?; println!("olvidado {}", r.name); }
        Cmd::Validate { repo } => {
            let r = resolve_repo(repo.as_deref())?;
            config::load(Path::new(&r.path))?;
            println!("✓ {} válido", config::path(Path::new(&r.path)).display());
        }
        Cmd::Gate { repo, dir, steps, components, fail_fast, verbose, dry_run } => {
            let r = resolve_repo(repo.as_deref())?;
            let root = dir.unwrap_or_else(|| PathBuf::from(&r.path));
            let cfg = config::load(&root).or_else(|_| config::load(Path::new(&r.path)))?;
            let steps = (!steps.is_empty()).then_some(steps);
            let comps = (!components.is_empty()).then_some(components);
            if dry_run {
                for p in gate::plan(&cfg, steps.as_deref(), comps.as_deref())? {
                    println!("{:<28} {}", format!("{}/{}", p.component, p.step), p.command.unwrap_or_else(|| format!("(omitido: {})", p.skip_reason.unwrap_or_default())));
                }
                return Ok(0);
            }
            let log_dir = paths::runs().join(r.name.replace('/', "__")).join(chrono::Local::now().format("gate-%Y%m%d-%H%M%S").to_string());
            let run_id = db::start_run(r.id, "gate", None, &log_dir)?;
            let mut on_start = |p: &gate::PlannedStep| { if verbose { println!("── {}/{}: {}", p.component, p.step, p.command.as_deref().unwrap_or("")); } };
            let mut on_line = |l: &str| { if verbose { println!("   {l}"); } };
            let mut on_done = |s: &gate::StepResult| {
                let icon = match s.status { gate::Status::Pass => "✓", gate::Status::Fail => "✗", gate::Status::Skip => "○" };
                println!("{icon} {:<28} {:>7.2}s {}", format!("{}/{}", s.component, s.step), s.seconds, s.reason.clone().unwrap_or_default());
                if s.status == gate::Status::Fail && !verbose {
                    for l in s.tail.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev() { println!("    │ {l}"); }
                }
            };
            let res = gate::run(&root, &cfg, &log_dir, steps.as_deref(), comps.as_deref(), fail_fast, &mut gate::Hooks { on_start: &mut on_start, on_line: &mut on_line, on_done: &mut on_done })?;
            db::finish_run(run_id, if res.passed() { "pass" } else { "fail" }, &serde_json::to_value(&res)?)?;
            let skipped = res.steps.iter().filter(|s| s.status == gate::Status::Skip).count();
            println!("\ngate: {}{}  ·  logs: {}", if res.passed() { "PASS" } else { "FAIL" }, if skipped > 0 { format!(" ({skipped} pasos omitidos)") } else { String::new() }, log_dir.display());
            return Ok(if res.passed() { 0 } else { 1 });
        }
        Cmd::Doctor { repo } => {
            let mut tools: Vec<(String, String)> = ["git", "gh", "claude", "docker"].iter().map(|t| (t.to_string(), "forja".into())).collect();
            if let Ok(r) = resolve_repo(repo.as_deref()) {
                if let Ok(cfg) = config::load(Path::new(&r.path)) {
                    for c in &cfg.components {
                        for t in adapters::load(&c.adapter)?.tools { tools.push((t, format!("{} ({})", c.name, c.adapter))); }
                    }
                }
            }
            let mut missing = 0;
            for (t, who) in tools {
                let ok = which::which(&t).is_ok();
                if !ok { missing += 1; }
                println!("{} {:<16} {}", if ok { "✓" } else { "✗" }, t, who);
            }
            return Ok(if missing > 0 { 1 } else { 0 });
        }
        Cmd::Bootstrap { repo, no_push, force } => {
            let r = resolve_repo(repo.as_deref())?;
            let o = bootstrap::run(&r, !no_push, force)?;
            println!("\nworktree: {}", o.worktree);
            println!("agregado:");
            for c in &o.created { println!("  + {c}"); }
            for n in &o.notes { println!("  ⚠ {n}"); }
            if let Some(u) = o.pr_url { println!("PR: {u}"); }
            return Ok(if o.gate_passed || force { 0 } else { 1 });
        }
        Cmd::Hook { .. } => unreachable!(),
        Cmd::PrSync { repo } => {
            let ctx = pipeline::Ctx::load(resolve_repo(repo.as_deref())?)?;
            for l in pipeline::sync_prs(&ctx)? { println!("{l}"); }
        }
        Cmd::Map { repo } => {
            let r = resolve_repo(repo.as_deref())?;
            print!("{}", pipeline::feature_map_table(Path::new(&r.path), Some(r.id)));
        }
        Cmd::Feature(fc) => return feature_cmd(fc),
    }
    Ok(0)
}

fn feature_cmd(fc: FeatCmd) -> Result<i32> {
    let ctx = |r: &Option<String>| -> Result<pipeline::Ctx> { pipeline::Ctx::load(resolve_repo(r.as_deref())?) };
    match fc {
        FeatCmd::New { repo, title, description, no_ai } => {
            let c = ctx(&repo)?;
            let spec = pipeline::intake(&c, &title, description.as_deref(), !no_ai)?;
            println!("{}\n→ {} (edítalo si hace falta) y luego `forja feature plan {}`", serde_yaml::to_string(&spec)?, feature::yaml_path(&c.root, &spec.id).display(), spec.id);
        }
        FeatCmd::List { repo } => {
            let c = ctx(&repo)?;
            for f in db::list_features(c.repo.id)? {
                println!("{:<9} {:<18} {:<40} ${:<6.3} {}", f.feat_id, f.state, f.title, f.cost_usd, f.pr_url.unwrap_or_default());
            }
        }
        FeatCmd::Plan { id, repo } => {
            let c = ctx(&repo)?;
            let f = db::get_feature(c.repo.id, &id)?.ok_or_else(|| anyhow!("no existe {id}"))?;
            db::set_feature_pid(f.id, Some(std::process::id() as i64))?;
            db::set_feature_field(f.id, "node", Some("plan"))?;
            let res = pipeline::plan(&c, &id);
            db::set_feature_pid(f.id, None)?;
            db::set_feature_field(f.id, "node", None)?;
            res?;
        }
        FeatCmd::Approve { id, repo } => { let c = ctx(&repo)?; pipeline::approve(&c, &id)?; println!("{id} aprobado. `forja feature run {id}`"); }
        FeatCmd::Reject { id, comment, replan, repo } => {
            let c = ctx(&repo)?;
            pipeline::reject_plan(&c, &id, &comment)?;
            if replan { return feature_cmd(FeatCmd::Plan { id, repo }); } else { println!("{id} regresó a draft con tus comentarios. `forja feature plan {id}`"); }
        }
        FeatCmd::Run { id, no_push, repo } => {
            let c = ctx(&repo)?;
            let f = db::get_feature(c.repo.id, &id)?.ok_or_else(|| anyhow!("no existe {id}"))?;
            db::set_feature_pid(f.id, Some(std::process::id() as i64))?;
            let res = pipeline::run(&c, &id, &pipeline::RunOpts { push: !no_push });
            db::set_feature_pid(f.id, None)?;
            db::set_feature_field(f.id, "node", None)?;
            if let Err(e) = &res {
                // errores inesperados (no los fallos del gate, que ya marcan failed) también dejan rastro
                let cur = db::get_feature(c.repo.id, &id)?.ok_or_else(|| anyhow!("no existe {id}"))?;
                if cur.state != "failed" {
                    db::event(cur.id, "coordinator", "error", &format!("{e:#}"))?;
                    db::transition(&cur, "failed", &format!("error: {e:#}"))?;
                    pipeline::notify("forja", &format!("{id} falló: {e}"));
                }
            }
            res?;
        }
        FeatCmd::Changes { id, comment, repo } => { let c = ctx(&repo)?; pipeline::request_changes(&c, &id, &comment)?; println!("{id} → changes_requested. `forja feature run {id}`"); }
        FeatCmd::Close { id, merged, reason, repo } => { let c = ctx(&repo)?; pipeline::close(&c, &id, merged, &reason)?; println!("{id} → {}", if merged { "merged" } else { "rejected" }); }
        FeatCmd::Status { id, repo } => {
            let c = ctx(&repo)?;
            let f = db::get_feature(c.repo.id, &id)?.ok_or_else(|| anyhow!("no existe {id}"))?;
            println!("{} · {} · estado {} · intento {} · ${:.3} · rama {} · {}", f.feat_id, f.title, f.state, f.attempt, f.cost_usd, f.branch.unwrap_or_default(), f.pr_url.unwrap_or_default());
            println!("\ntransiciones:");
            for (from, to, note, ts) in db::transitions(f.id)? { println!("  {ts}  {from:>16} → {to:<18} {note}"); }
            println!("\neventos recientes:");
            for e in db::events(f.id, 25)? { println!("  {} [{}:{}] {}", e.ts, e.node, e.kind, e.text.replace('\n', " ")); }
        }
    }
    Ok(0)
}
