//! TUI (ratatui): Repos · Features · Plan · En vivo · Feature map · PRs.
//! Las acciones largas (plan, run, gate, bootstrap) se lanzan como procesos `forja ...` en
//! segundo plano; la TUI solo lee SQLite y los archivos del repo, así nunca se bloquea.
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{self, ForjaConfig};
use crate::db::{self, Feature, Repo};
use crate::discovery::Report;
use crate::{feature, pipeline};

const TABS: &[&str] = &["1 Repos", "2 Features", "3 Plan", "4 En vivo", "5 Feature map", "6 PRs"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab { Repos, Features, Plan, Live, Map, Prs }
impl Tab {
    fn idx(self) -> usize { self as usize }
    fn from(i: usize) -> Self { [Tab::Repos, Tab::Features, Tab::Plan, Tab::Live, Tab::Map, Tab::Prs][i.min(5)] }
}

#[derive(Clone)]
enum InputAction { ConnectPath, FeatureTitle, FeatureDesc(String), RejectPlan(String), RequestChanges(String) }

enum Modal {
    None,
    Input { title: String, value: String, action: InputAction },
    Connect { report: Box<Report>, yaml: String, error: Option<String>, scroll: u16 },
    Help,
}

/// Acciones que requieren soltar la terminal (editor externo).
enum Suspend { EditFile(PathBuf), EditConnect }

struct Job { name: String, child: Child, log: PathBuf }

struct App {
    tab: Tab,
    repos: Vec<Repo>,
    repo_state: ListState,
    active: Option<Repo>,
    cfg: Option<ForjaConfig>,
    features: Vec<Feature>,
    feat_state: TableState,
    reports: HashMap<i64, Result<Report, String>>,
    modal: Modal,
    status: String,
    scroll: u16,
    jobs: Vec<Job>,
    last_refresh: Instant,
    quit: bool,
}

pub fn run() -> Result<()> {
    crate::paths::ensure()?;
    let mut app = App::new()?;
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
}

impl App {
    fn new() -> Result<Self> {
        let mut app = App {
            tab: Tab::Repos, repos: vec![], repo_state: ListState::default(), active: None, cfg: None,
            features: vec![], feat_state: TableState::default(), reports: HashMap::new(), modal: Modal::None,
            status: "? ayuda · q salir".into(), scroll: 0, jobs: vec![], last_refresh: Instant::now(), quit: false,
        };
        app.refresh()?;
        // si el cwd es un repo registrado, lo activamos
        if let Ok(r) = crate::resolve_repo(None) {
            if let Some(i) = app.repos.iter().position(|x| x.id == r.id) {
                app.repo_state.select(Some(i));
                app.activate(r);
                app.tab = Tab::Features;
            }
        }
        if app.repo_state.selected().is_none() && !app.repos.is_empty() { app.repo_state.select(Some(0)); }
        Ok(app)
    }

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        if let Some(s) = self.on_key(k) {
                            ratatui::restore();
                            let r = self.suspend(s);
                            *term = ratatui::init();
                            if let Err(e) = r { self.status = format!("✗ {e:#}"); }
                        }
                    }
                }
            }
            if self.last_refresh.elapsed() > Duration::from_millis(1000) {
                let _ = self.refresh();
                self.poll_jobs();
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ datos

    fn refresh(&mut self) -> Result<()> {
        self.last_refresh = Instant::now();
        self.repos = db::list_repos()?;
        if let Some(a) = &self.active {
            let id = a.id;
            self.features = db::list_features(id)?;
            if self.feat_state.selected().is_none() && !self.features.is_empty() { self.feat_state.select(Some(0)); }
            if let Some(i) = self.feat_state.selected() { if i >= self.features.len() { self.feat_state.select(self.features.len().checked_sub(1)); } }
        }
        Ok(())
    }

    fn activate(&mut self, r: Repo) {
        self.cfg = config::load(Path::new(&r.path)).ok();
        if self.cfg.is_none() { self.status = format!("⚠ {} no tiene .forja/config.yaml válido", r.name); }
        self.active = Some(r);
        self.features.clear();
        self.feat_state.select(None);
        let _ = self.refresh();
    }

    fn selected_repo(&self) -> Option<&Repo> { self.repo_state.selected().and_then(|i| self.repos.get(i)) }
    fn selected_feature(&self) -> Option<&Feature> { self.feat_state.selected().and_then(|i| self.features.get(i)) }
    fn root(&self) -> Option<PathBuf> { self.active.as_ref().map(|r| PathBuf::from(&r.path)) }

    fn report(&mut self, r: &Repo) -> &Result<Report, String> {
        self.reports.entry(r.id).or_insert_with(|| crate::discovery::discover(Path::new(&r.path)).map_err(|e| e.to_string()))
    }

    // ------------------------------------------------------------------ trabajos en segundo plano

    fn spawn(&mut self, name: &str, args: &[&str]) {
        let exe = std::env::current_exe().unwrap_or_else(|_| "forja".into());
        let dir = crate::paths::logs().join("jobs");
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join(format!("{}-{}.log", chrono::Local::now().format("%Y%m%d-%H%M%S"), name.replace([' ', '/'], "_")));
        let Ok(out) = std::fs::File::create(&log) else { self.status = "✗ no pude crear el log".into(); return };
        let err = out.try_clone().unwrap();
        use std::os::unix::process::CommandExt;
        match Command::new(exe).args(args).stdin(Stdio::null()).stdout(out).stderr(err).process_group(0).spawn() {
            Ok(child) => { self.status = format!("▶ {name} en segundo plano"); self.jobs.push(Job { name: name.into(), child, log }); }
            Err(e) => self.status = format!("✗ {name}: {e}"),
        }
    }

    fn poll_jobs(&mut self) {
        let mut done = vec![];
        for (i, j) in self.jobs.iter_mut().enumerate() {
            if let Ok(Some(st)) = j.child.try_wait() { done.push((i, st.success())); }
        }
        for (i, ok) in done.into_iter().rev() {
            let j = self.jobs.remove(i);
            let last = std::fs::read_to_string(&j.log).unwrap_or_default().lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
            self.status = if ok { format!("✓ {} terminó · {}", j.name, last) } else { format!("✗ {} falló · {} · log: {}", j.name, last, j.log.display()) };
            self.reports.clear();
            if let Some(r) = self.active.clone() { self.cfg = config::load(Path::new(&r.path)).ok(); }
        }
    }

    fn kill_feature(&mut self) {
        let Some(f) = self.selected_feature().cloned() else { return };
        if let Some(pid) = f.pid {
            let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
            let _ = db::set_feature_pid(f.id, None);
            self.status = format!("■ detenido {} (pid {pid}); `r` lo reanuda", f.feat_id);
        } else {
            self.status = format!("{} no tiene un proceso corriendo", f.feat_id);
        }
    }

    // ------------------------------------------------------------------ teclado

    fn on_key(&mut self, k: KeyEvent) -> Option<Suspend> {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') { self.quit = true; return None; }
        match &mut self.modal {
            Modal::Input { value, .. } => {
                match k.code {
                    KeyCode::Esc => self.modal = Modal::None,
                    KeyCode::Enter => self.submit_input(),
                    KeyCode::Backspace => { value.pop(); }
                    KeyCode::Char(c) => value.push(c),
                    _ => {}
                }
                return None;
            }
            Modal::Connect { scroll, .. } => {
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.modal = Modal::None,
                    KeyCode::Char('j') | KeyCode::Down => *scroll = scroll.saturating_add(1),
                    KeyCode::Char('k') | KeyCode::Up => *scroll = scroll.saturating_sub(1),
                    KeyCode::Char('e') => return Some(Suspend::EditConnect),
                    KeyCode::Char('w') => self.write_connect(),
                    _ => {}
                }
                return None;
            }
            Modal::Help => { self.modal = Modal::None; return None; }
            Modal::None => {}
        }
        match k.code {
            KeyCode::Char('q') => { self.quit = true; return None; }
            KeyCode::Char('?') => { self.modal = Modal::Help; return None; }
            KeyCode::Char(c @ '1'..='6') => { self.tab = Tab::from(c as usize - '1' as usize); self.scroll = 0; return None; }
            KeyCode::Tab => { self.tab = Tab::from((self.tab.idx() + 1) % 6); self.scroll = 0; return None; }
            KeyCode::BackTab => { self.tab = Tab::from((self.tab.idx() + 5) % 6); self.scroll = 0; return None; }
            _ => {}
        }
        match self.tab {
            Tab::Repos => self.key_repos(k),
            Tab::Features | Tab::Prs => self.key_features(k),
            Tab::Plan => self.key_plan(k),
            Tab::Live | Tab::Map => { self.key_nav_features(k); self.key_scroll(k); None }
        }
    }

    fn key_scroll(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('j') | KeyCode::PageDown => self.scroll = self.scroll.saturating_add(if k.code == KeyCode::PageDown { 20 } else { 1 }),
            KeyCode::Char('k') | KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(if k.code == KeyCode::PageUp { 20 } else { 1 }),
            _ => {}
        }
    }

    fn key_nav_features(&mut self, k: KeyEvent) {
        let n = self.features.len();
        if n == 0 { return; }
        let i = self.feat_state.selected().unwrap_or(0);
        match k.code {
            KeyCode::Down => self.feat_state.select(Some((i + 1).min(n - 1))),
            KeyCode::Up => self.feat_state.select(Some(i.saturating_sub(1))),
            _ => return,
        }
        self.scroll = 0;
    }

    fn key_repos(&mut self, k: KeyEvent) -> Option<Suspend> {
        let n = self.repos.len();
        let i = self.repo_state.selected().unwrap_or(0);
        match k.code {
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.repo_state.select(Some((i + 1).min(n - 1))),
            KeyCode::Up | KeyCode::Char('k') if n > 0 => self.repo_state.select(Some(i.saturating_sub(1))),
            KeyCode::Enter => if let Some(r) = self.selected_repo().cloned() { self.status = format!("repo activo: {}", r.name); self.activate(r); self.tab = Tab::Features; },
            KeyCode::Char('c') => {
                let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                self.modal = Modal::Input { title: "Conectar repo existente: ruta local".into(), value: cwd, action: InputAction::ConnectPath };
            }
            KeyCode::Char('d') => if let Some(r) = self.selected_repo().cloned() { self.reports.remove(&r.id); self.status = format!("descubrimiento de {} actualizado", r.name); },
            KeyCode::Char('g') => if let Some(r) = self.selected_repo().cloned() { self.spawn(&format!("gate {}", r.name), &["gate", "--repo", &r.name]); },
            KeyCode::Char('b') => if let Some(r) = self.selected_repo().cloned() { self.spawn(&format!("bootstrap {}", r.name), &["bootstrap", "--repo", &r.name]); },
            KeyCode::Char('e') => if let Some(r) = self.selected_repo() { return Some(Suspend::EditFile(config::path(Path::new(&r.path)))); },
            KeyCode::Char('x') => if let Some(r) = self.selected_repo().cloned() {
                let _ = db::remove_repo(r.id);
                if self.active.as_ref().is_some_and(|a| a.id == r.id) { self.active = None; self.features.clear(); }
                self.status = format!("olvidado {} (no se borró nada del disco)", r.name);
                let _ = self.refresh();
                self.repo_state.select(if self.repos.is_empty() { None } else { Some(0) });
            },
            _ => {}
        }
        None
    }

    fn need_repo(&mut self) -> Option<Repo> {
        if self.active.is_none() { self.status = "elige un repo en la pestaña 1 (Enter)".into(); }
        self.active.clone()
    }

    fn key_features(&mut self, k: KeyEvent) -> Option<Suspend> {
        self.key_nav_features(k);
        if matches!(k.code, KeyCode::Char('j')) { self.key_nav_features(KeyEvent::from(KeyCode::Down)); }
        if matches!(k.code, KeyCode::Char('k')) && self.tab != Tab::Features { self.key_nav_features(KeyEvent::from(KeyCode::Up)); }
        let Some(repo) = self.need_repo() else { return None };
        let f = self.selected_feature().cloned();
        let rn = repo.name.clone();
        match (k.code, &f) {
            (KeyCode::Char('n'), _) => self.modal = Modal::Input { title: "Nuevo feature: título".into(), value: String::new(), action: InputAction::FeatureTitle },
            (KeyCode::Enter, Some(_)) => { self.tab = if self.tab == Tab::Prs { Tab::Live } else { Tab::Plan }; self.scroll = 0; }
            (KeyCode::Char('l'), Some(_)) => { self.tab = Tab::Live; self.scroll = 0; }
            (KeyCode::Char('p'), Some(f)) => { let id = f.feat_id.clone(); self.spawn(&format!("plan {id}"), &["feature", "plan", &id, "--repo", &rn]); }
            (KeyCode::Char('a'), Some(f)) => self.approve(&f.feat_id.clone()),
            (KeyCode::Char('r'), Some(f)) => {
                if f.pid.is_some() { self.status = format!("{} ya está corriendo", f.feat_id); return None; }
                let id = f.feat_id.clone();
                self.spawn(&format!("run {id}"), &["feature", "run", &id, "--repo", &rn]);
                self.tab = Tab::Live;
            }
            (KeyCode::Char('K'), Some(_)) => self.kill_feature(),
            (KeyCode::Char('c'), Some(f)) => self.modal = Modal::Input { title: format!("{}: cambios pedidos (vuelve a la etapa 2)", f.feat_id), value: String::new(), action: InputAction::RequestChanges(f.feat_id.clone()) },
            (KeyCode::Char('m'), Some(f)) => self.close(&f.feat_id.clone(), true),
            (KeyCode::Char('X'), Some(f)) => self.close(&f.feat_id.clone(), false),
            (KeyCode::Char('s'), _) => self.spawn("pr-sync", &["pr-sync", "--repo", &rn]),
            (KeyCode::Char('o'), Some(f)) => {
                if let Some(u) = &f.pr_url { let _ = Command::new("open").arg(u).status(); }
                else if let Some(w) = &f.worktree { self.status = format!("sin PR; worktree: {w}"); }
            }
            _ => {}
        }
        None
    }

    fn key_plan(&mut self, k: KeyEvent) -> Option<Suspend> {
        self.key_nav_features(k);
        self.key_scroll(k);
        let (Some(root), Some(f)) = (self.root(), self.selected_feature().cloned()) else { return None };
        let rn = self.active.as_ref().unwrap().name.clone();
        match k.code {
            KeyCode::Char('a') => self.approve(&f.feat_id),
            KeyCode::Char('x') => self.modal = Modal::Input { title: format!("Rechazar plan de {}: comentarios (dispara nueva planeación)", f.feat_id), value: String::new(), action: InputAction::RejectPlan(f.feat_id.clone()) },
            KeyCode::Char('p') => self.spawn(&format!("plan {}", f.feat_id), &["feature", "plan", &f.feat_id, "--repo", &rn]),
            KeyCode::Char('e') => return Some(Suspend::EditFile(feature::yaml_path(&root, &f.feat_id))),
            KeyCode::Char('E') => return Some(Suspend::EditFile(feature::plan_path(&root, &f.feat_id))),
            _ => {}
        }
        None
    }

    fn with_ctx<T>(&mut self, f: impl FnOnce(&pipeline::Ctx) -> Result<T>) -> Option<T> {
        let repo = self.active.clone()?;
        match pipeline::Ctx::load(repo).and_then(|c| f(&c)) {
            Ok(v) => Some(v),
            Err(e) => { self.status = format!("✗ {e:#}"); None }
        }
    }

    fn approve(&mut self, id: &str) {
        if self.with_ctx(|c| pipeline::approve(c, id)).is_some() {
            self.status = format!("✓ {id} aprobado (HITL #1). `r` para correr el coordinador");
            let _ = self.refresh();
        }
    }

    fn close(&mut self, id: &str, merged: bool) {
        let reason = if merged { "merge confirmado desde la TUI" } else { "rechazado desde la TUI" };
        if self.with_ctx(|c| pipeline::close(c, id, merged, reason)).is_some() {
            self.status = format!("{id} → {}", if merged { "merged" } else { "rejected" });
            let _ = self.refresh();
        }
    }

    fn submit_input(&mut self) {
        let Modal::Input { value, action, .. } = std::mem::replace(&mut self.modal, Modal::None) else { return };
        let value = value.trim().to_string();
        if value.is_empty() {
            if let InputAction::FeatureDesc(title) = action { self.new_feature(&title, None); return; }
            self.status = "cancelado (vacío)".into();
            return;
        }
        match action {
            InputAction::ConnectPath => {
                let path = PathBuf::from(shellexpand(&value));
                match crate::draft(&path) {
                    Ok((report, yaml)) => {
                        let error = config::parse(&yaml, Some(Path::new(&report.root))).err().map(|e| e.to_string());
                        self.modal = Modal::Connect { report: Box::new(report), yaml, error, scroll: 0 };
                    }
                    Err(e) => self.status = format!("✗ {e:#}"),
                }
            }
            InputAction::FeatureTitle => {
                self.modal = Modal::Input { title: format!("«{value}»: describe el feature en lenguaje natural (Enter vacío = plantilla)"), value: String::new(), action: InputAction::FeatureDesc(value) };
            }
            InputAction::FeatureDesc(title) => self.new_feature(&title, Some(&value)),
            InputAction::RejectPlan(id) => {
                if self.with_ctx(|c| pipeline::reject_plan(c, &id, &value)).is_some() {
                    let rn = self.active.as_ref().unwrap().name.clone();
                    self.spawn(&format!("replan {id}"), &["feature", "plan", &id, "--repo", &rn]);
                }
            }
            InputAction::RequestChanges(id) => {
                if self.with_ctx(|c| pipeline::request_changes(c, &id, &value)).is_some() {
                    self.status = format!("{id} → changes_requested. `r` para que el coordinador lo atienda");
                }
            }
        }
        let _ = self.refresh();
    }

    fn new_feature(&mut self, title: &str, desc: Option<&str>) {
        let Some(repo) = self.need_repo() else { return };
        let mut args = vec!["feature".to_string(), "new".into(), "--repo".into(), repo.name.clone(), "--title".into(), title.into()];
        if let Some(d) = desc { args.push("--description".into()); args.push(d.into()); }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.spawn(&format!("intake «{title}»"), &refs);
    }

    fn write_connect(&mut self) {
        let Modal::Connect { report, yaml, .. } = std::mem::replace(&mut self.modal, Modal::None) else { return };
        match crate::write_config(&report, &yaml) {
            Ok(r) => {
                self.status = format!("✓ {} conectado. `b` para el PR de bootstrap", r.name);
                let _ = self.refresh();
                if let Some(i) = self.repos.iter().position(|x| x.id == r.id) { self.repo_state.select(Some(i)); }
                self.reports.remove(&r.id);
                self.activate(r);
            }
            Err(e) => {
                let error = Some(format!("{e:#}"));
                self.modal = Modal::Connect { report, yaml, error, scroll: 0 };
            }
        }
    }

    fn suspend(&mut self, s: Suspend) -> Result<()> {
        match s {
            Suspend::EditFile(p) => {
                let cur = std::fs::read_to_string(&p).unwrap_or_default();
                let suffix = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                let new = crate::edit_text(&cur, &suffix)?;
                if new != cur {
                    if p.ends_with(config::CONFIG_REL) {
                        config::parse(&new, p.parent().and_then(Path::parent))?;
                    }
                    std::fs::write(&p, new)?;
                    self.status = format!("✓ guardado {}", p.display());
                    if let Some(r) = self.active.clone() { self.cfg = config::load(Path::new(&r.path)).ok(); }
                } else {
                    self.status = "sin cambios".into();
                }
            }
            Suspend::EditConnect => {
                if let Modal::Connect { report, yaml, error, .. } = &mut self.modal {
                    *yaml = crate::edit_text(yaml, ".yaml")?;
                    *error = config::parse(yaml, Some(Path::new(&report.root))).err().map(|e| e.to_string());
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ dibujo

    fn draw(&mut self, f: &mut Frame) {
        let [head, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(2)]).areas(f.area());
        let [tabs_a, repo_a] = Layout::horizontal([Constraint::Min(0), Constraint::Length(40)]).areas(head);
        f.render_widget(Tabs::new(TABS.to_vec()).select(self.tab.idx()).highlight_style(Style::new().fg(Color::Black).bg(Color::Cyan).bold()).divider("│"), tabs_a);
        let jobs = if self.jobs.is_empty() { String::new() } else { format!("⟳{} ", self.jobs.len()) };
        let active = self.active.as_ref().map(|r| r.name.clone()).unwrap_or_else(|| "sin repo activo".into());
        f.render_widget(Paragraph::new(Line::from(vec![Span::raw(jobs).yellow(), Span::raw("forja · ").dark_gray(), Span::raw(active).bold()])).right_aligned(), repo_a);
        match self.tab {
            Tab::Repos => self.draw_repos(f, body),
            Tab::Features => self.draw_features(f, body, false),
            Tab::Plan => self.draw_plan(f, body),
            Tab::Live => self.draw_live(f, body),
            Tab::Map => self.draw_map(f, body),
            Tab::Prs => self.draw_features(f, body, true),
        }
        let hints = match self.tab {
            Tab::Repos => "↑↓ mover · Enter activar · c conectar · d redescubrir · e editar config · g gate · b bootstrap · x olvidar",
            Tab::Features => "n nuevo · p planear · Enter plan · a aprobar · r correr · l en vivo · K detener · c cambios · m merged · X rechazar",
            Tab::Plan => "↑↓ feature · j/k scroll · a aprobar (HITL #1) · x rechazar con comentarios · p replanear · e editar YAML · E editar plan",
            Tab::Live => "↑↓ feature · j/k scroll · se actualiza cada segundo",
            Tab::Map => "j/k scroll · tabla generada desde features/*.yaml",
            Tab::Prs => "↑↓ mover · s sincronizar con GitHub · o abrir PR · c pedir cambios · m merged · X rechazar · Enter en vivo",
        };
        f.render_widget(Paragraph::new(vec![Line::from(self.status.clone()).yellow(), Line::from(format!("{hints} · Tab/1-6 pestañas · ? ayuda · q salir")).dark_gray()]), foot);
        self.draw_modal(f);
    }

    fn draw_repos(&mut self, f: &mut Frame, area: Rect) {
        let [left, right] = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).areas(area);
        let items: Vec<ListItem> = self.repos.iter().map(|r| {
            let mark = if self.active.as_ref().is_some_and(|a| a.id == r.id) { "● " } else { "  " };
            ListItem::new(vec![
                Line::from(vec![Span::raw(mark).green(), Span::raw(r.name.clone()).bold()]),
                Line::from(format!("    {} · {}", r.stack.join(", "), r.last_run_at.clone().map(|t| format!("última corrida {t}")).unwrap_or_else(|| "sin corridas".into()))).dark_gray(),
            ])
        }).collect();
        let empty = items.is_empty();
        let list = List::new(items).block(Block::bordered().title(" Repos registrados ")).highlight_style(Style::new().bg(Color::DarkGray)).highlight_symbol("▶");
        f.render_stateful_widget(list, left, &mut self.repo_state);
        if empty {
            f.render_widget(Paragraph::new("\n  Sin repos todavía.\n\n  c  conectar un repo existente (ruta local)\n\n  Inicializar repos nuevos y buscar en GitHub\n  llegan en la fase 7 del roadmap.").dark_gray(), left.inner(ratatui::layout::Margin::new(1, 1)));
        }
        let Some(r) = self.selected_repo().cloned() else { return };
        let text = match self.report(&r) {
            Err(e) => Text::from(format!("✗ {e}")).red(),
            Ok(rep) => report_text(rep),
        };
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().title(format!(" {} · {} ", r.name, r.path))), right);
    }

    fn draw_features(&mut self, f: &mut Frame, area: Rect, prs: bool) {
        let rows: Vec<(usize, &Feature)> = self.features.iter().enumerate()
            .filter(|(_, x)| !prs || ["ready", "pr_open", "changes_requested", "merged", "rejected"].contains(&x.state.as_str())).collect();
        let budget = self.cfg.as_ref().map(|c| c.agents.budget_usd_per_feature).unwrap_or(0.0);
        let header = if prs { vec!["ID", "Título", "Estado", "Rama", "PR / worktree"] } else { vec!["ID", "Título", "Estado", "Nodo", "Vuelta", "Costo", "PR"] };
        let table_rows: Vec<Row> = rows.iter().map(|(_, x)| {
            let running = if x.pid.is_some() { "⟳ " } else { "" };
            if prs {
                Row::new(vec![Cell::from(x.feat_id.clone()), Cell::from(x.title.clone()), Cell::from(x.state.clone()).style(state_style(&x.state)),
                              Cell::from(x.branch.clone().unwrap_or_default()), Cell::from(x.pr_url.clone().or(x.worktree.clone()).unwrap_or_default())])
            } else {
                Row::new(vec![Cell::from(format!("{running}{}", x.feat_id)), Cell::from(x.title.clone()), Cell::from(x.state.clone()).style(state_style(&x.state)),
                              Cell::from(x.node.clone().unwrap_or_default()), Cell::from(x.attempt.to_string()),
                              Cell::from(format!("${:.2}/{budget:.0}", x.cost_usd)), Cell::from(x.pr_url.clone().unwrap_or_default())])
            }
        }).collect();
        let widths: Vec<Constraint> = if prs {
            vec![Constraint::Length(10), Constraint::Percentage(30), Constraint::Length(18), Constraint::Percentage(25), Constraint::Min(10)]
        } else {
            vec![Constraint::Length(12), Constraint::Min(20), Constraint::Length(18), Constraint::Length(11), Constraint::Length(7), Constraint::Length(11), Constraint::Length(30)]
        };
        let title = if prs { " PRs y ramas listas (HITL #2) " } else { " Features " };
        let t = Table::new(table_rows, widths).header(Row::new(header).bold().underlined())
            .block(Block::bordered().title(title)).row_highlight_style(Style::new().bg(Color::DarkGray)).highlight_symbol("▶");
        // mapear la selección global a la fila visible
        let mut st = TableState::default();
        if let Some(sel) = self.feat_state.selected() { st.select(rows.iter().position(|(i, _)| *i == sel)); }
        f.render_stateful_widget(t, area, &mut st);
        if prs && st.selected().is_none() {
            if let Some((i, _)) = rows.first() { self.feat_state.select(Some(*i)); }
        }
        if self.features.is_empty() && self.active.is_some() {
            f.render_widget(Paragraph::new("  Sin features. Presiona n para crear uno.").dark_gray(), area.inner(ratatui::layout::Margin::new(1, 2)));
        }
    }

    fn draw_plan(&mut self, f: &mut Frame, area: Rect) {
        let (Some(root), Some(x)) = (self.root(), self.selected_feature().cloned()) else {
            f.render_widget(Paragraph::new("Selecciona un feature en la pestaña 2").block(Block::bordered()), area);
            return;
        };
        let [l, r] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);
        let yaml = std::fs::read_to_string(feature::yaml_path(&root, &x.feat_id)).unwrap_or_else(|e| format!("✗ {e}"));
        let plan = std::fs::read_to_string(feature::plan_path(&root, &x.feat_id))
            .unwrap_or_else(|_| if x.pid.is_some() { "⟳ planificando…".into() } else { "Sin plan todavía. Presiona p para correr el planificador.".into() });
        let mut ytxt = Text::from(yaml);
        if let Some(fb) = &x.feedback {
            ytxt.lines.push(Line::from(""));
            ytxt.lines.push(Line::from("── comentarios pendientes ──").yellow());
            for l in fb.lines() { ytxt.lines.push(Line::from(l.to_string()).yellow()); }
        }
        f.render_widget(Paragraph::new(ytxt).wrap(Wrap { trim: false }).block(Block::bordered().title(format!(" features/{}.yaml ", x.feat_id))), l);
        f.render_widget(Paragraph::new(markdown(&plan)).wrap(Wrap { trim: false }).scroll((self.scroll, 0))
            .block(Block::bordered().title(Line::from(vec![Span::raw(format!(" {}.plan.md · ", x.feat_id)), Span::styled(x.state.clone(), state_style(&x.state)), Span::raw(" ")]))), r);
    }

    fn draw_live(&mut self, f: &mut Frame, area: Rect) {
        let Some(x) = self.selected_feature().cloned() else {
            f.render_widget(Paragraph::new("Selecciona un feature en la pestaña 2").block(Block::bordered()), area);
            return;
        };
        let budget = self.cfg.as_ref().map(|c| c.agents.budget_usd_per_feature).unwrap_or(0.0);
        let [top, rest] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let [l, r] = Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(rest);
        let alive = x.pid.map(|p| Command::new("kill").args(["-0", &p.to_string()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())).unwrap_or(false);
        let head = Line::from(vec![
            Span::raw(format!(" {} ", x.feat_id)).bold(), Span::raw(format!("{}  ", x.title)),
            Span::styled(format!(" {} ", x.state), state_style(&x.state).add_modifier(Modifier::REVERSED)),
            Span::raw(format!("  nodo: {}  vuelta: {}  costo: ${:.3} / ${budget:.2}  ", x.node.clone().unwrap_or_else(|| "—".into()), x.attempt, x.cost_usd)),
            if alive { Span::raw("⟳ corriendo").green() } else if x.pid.is_some() { Span::raw("proceso perdido").red() } else { Span::raw("inactivo").dark_gray() },
        ]);
        f.render_widget(Paragraph::new(head).block(Block::bordered()), top);
        let tr: Vec<ListItem> = db::transitions(x.id).unwrap_or_default().into_iter().map(|(_, to, note, ts)| {
            ListItem::new(vec![Line::from(vec![Span::raw(ts.get(11..19).unwrap_or("").to_string()).dark_gray(), Span::raw(" "), Span::styled(to.clone(), state_style(&to))]),
                               Line::from(format!("  {note}")).dark_gray()])
        }).collect();
        f.render_widget(List::new(tr).block(Block::bordered().title(" Transiciones ")), l);
        let h = r.height.saturating_sub(2) as usize;
        let evs = db::events(x.id, 400).unwrap_or_default();
        let lines: Vec<Line> = evs.iter().map(|e| {
            let style = match e.kind.as_str() {
                "tool" => Style::new().fg(Color::Cyan), "blocked" | "error" => Style::new().fg(Color::Red), "tool_error" => Style::new().fg(Color::LightRed),
                "result" | "verdict" => Style::new().fg(Color::Green), "text" => Style::new(), "drift" => Style::new().fg(Color::Yellow), _ => Style::new().fg(Color::Gray),
            };
            Line::from(vec![Span::raw(e.ts.get(11..19).unwrap_or("").to_string()).dark_gray(), Span::raw(format!(" {:<10}", e.node)).magenta(),
                            Span::styled(format!("{:<8} ", e.kind), style), Span::styled(e.text.replace('\n', " "), style)])
        }).collect();
        // sigue el final salvo que el usuario haga scroll hacia arriba
        let max = lines.len().saturating_sub(h) as u16;
        let off = max.saturating_sub(self.scroll);
        f.render_widget(Paragraph::new(lines).scroll((off, 0)).block(Block::bordered().title(" Stream: nodos, herramientas, guardrails, costo ")), r);
    }

    fn draw_map(&mut self, f: &mut Frame, area: Rect) {
        let Some(root) = self.root() else { f.render_widget(Paragraph::new("Sin repo activo").block(Block::bordered()), area); return };
        let states: HashMap<String, String> = self.features.iter().map(|x| (x.feat_id.clone(), x.state.clone())).collect();
        let mut rows = vec![];
        for s in feature::list(&root) {
            let st = states.get(&s.id).cloned().unwrap_or_else(|| "—".into());
            let qa = if root.join(format!("qa-reports/{}.md", s.id)).exists() { "ver reporte" } else { "⏳ pendiente" };
            for fl in &s.flows {
                rows.push(Row::new(vec![Cell::from(s.id.clone()), Cell::from(fl.name.clone()), Cell::from(fl.acceptance.join(" · ")),
                                        Cell::from(st.clone()).style(state_style(&st)), Cell::from(qa)]).height(1));
            }
        }
        let t = Table::new(rows, [Constraint::Length(10), Constraint::Percentage(25), Constraint::Min(30), Constraint::Length(18), Constraint::Length(12)])
            .header(Row::new(vec!["Feature", "Flujo", "Criterios de aceptación", "Estado", "QA"]).bold().underlined())
            .block(Block::bordered().title(" Feature map (QA de caja negra: fase 5 del roadmap) "));
        f.render_widget(t, area);
    }

    fn draw_modal(&mut self, f: &mut Frame) {
        let area = f.area();
        match &self.modal {
            Modal::None => {}
            Modal::Input { title, value, .. } => {
                let w = (area.width as f32 * 0.7) as u16;
                let r = Rect { x: (area.width - w) / 2, y: area.height / 2 - 2, width: w, height: 3 };
                f.render_widget(Clear, r);
                // muestra el final del texto para que el cursor siempre sea visible
                let room = w.saturating_sub(3) as usize;
                let chars: Vec<char> = value.chars().collect();
                let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
                f.render_widget(Paragraph::new(format!("{shown}▏")).block(Block::bordered().title(format!(" {title} ")).title_bottom(" Enter aceptar · Esc cancelar ").border_style(Style::new().cyan())), r);
            }
            Modal::Connect { report, yaml, error, scroll } => {
                let r = area.inner(ratatui::layout::Margin::new(4, 2));
                f.render_widget(Clear, r);
                let [l, rr] = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(r);
                f.render_widget(Paragraph::new(report_text(report)).wrap(Wrap { trim: false }).block(Block::bordered().title(" Descubrimiento (read-only) ")), l);
                let mut t = Text::from(yaml.as_str());
                if let Some(e) = error { t.lines.insert(0, Line::from(format!("✗ {e}")).red()); t.lines.insert(1, Line::from("")); }
                f.render_widget(Paragraph::new(t).scroll((*scroll, 0)).block(Block::bordered().title(" Borrador de .forja/config.yaml ")
                    .title_bottom(" w escribir y registrar · e editar · j/k scroll · Esc cancelar ").border_style(Style::new().cyan())), rr);
            }
            Modal::Help => {
                let r = area.inner(ratatui::layout::Margin::new(area.width / 6, 3));
                f.render_widget(Clear, r);
                let help = "Flujo
  1 Repos      c conecta un repo: descubre, propone config.yaml (w/e), b abre el PR de bootstrap.
  2 Features   n crea un feature (título + descripción → YAML con IA), p lo planea.
  3 Plan       HITL #1: a aprueba, x rechaza con comentarios (replanea), e/E edita YAML/plan.
  2 Features   r corre el coordinador: implement → test → quality → docs → gate → PR.
  4 En vivo    stream de cada nodo: herramientas, acciones bloqueadas por guardrails y costo.
  6 PRs        HITL #2: s sincroniza con GitHub, c pide cambios, m merged, X rechaza.

Guardrails
  Cada agente corre con `forja hook --role <rol>` como hook PreToolUse y, al terminar cada
  nodo, forja revierte cualquier archivo que el rol no podía tocar.

Trabajos en segundo plano
  Se lanzan como procesos `forja …`; su log queda en ~/.forja/logs/jobs/. K detiene uno.

Pulsa cualquier tecla para cerrar.";
                f.render_widget(Paragraph::new(help).block(Block::bordered().title(" Ayuda ").border_style(Style::new().cyan())), r);
            }
        }
    }
}

fn shellexpand(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") { return dirs::home_dir().map(|h| h.join(rest).display().to_string()).unwrap_or_else(|| s.into()); }
    s.to_string()
}

fn state_style(s: &str) -> Style {
    Style::new().fg(match s {
        "draft" => Color::Gray, "planned" => Color::Yellow, "approved" => Color::Cyan,
        "implementing" | "documenting" | "gating" | "qa" => Color::Blue, "ready" | "pr_open" => Color::Magenta,
        "merged" => Color::Green, "failed" => Color::Red, "changes_requested" => Color::LightRed, "rejected" => Color::DarkGray, _ => Color::White,
    })
}

fn report_text(rep: &Report) -> Text<'static> {
    let mut t = Text::default();
    let kv = |k: &str, v: String| Line::from(vec![Span::raw(format!("{k:<12}")).dark_gray(), Span::raw(v)]);
    t.lines.push(kv("repo", rep.repo.clone()));
    t.lines.push(kv("remoto", rep.remote.clone().unwrap_or_else(|| "—".into())));
    t.lines.push(kv("rama", rep.default_branch.clone()));
    t.lines.push(kv("config", if rep.has_forja_config { "✓ .forja/config.yaml".into() } else { "✗ sin .forja/config.yaml".into() }));
    t.lines.push(Line::from(""));
    for c in &rep.components {
        t.lines.push(Line::from(vec![Span::raw(format!("● {} ", c.name)).bold(), Span::raw(format!("{} · {} · {}", c.path, c.adapter, c.project_type)).cyan()]));
        t.lines.push(Line::from(format!("  {}{}", c.frameworks.join(", "), c.db_engine.as_ref().map(|d| format!(" · db {d}")).unwrap_or_default())).dark_gray());
    }
    t.lines.push(Line::from(""));
    let list = |v: &Vec<String>| if v.is_empty() { "—".to_string() } else { v.join(", ") };
    t.lines.push(kv("docker", list(&rep.dockerfiles)));
    t.lines.push(kv("compose", list(&rep.compose_files)));
    t.lines.push(kv("CI", list(&rep.ci_workflows)));
    t.lines.push(kv("migraciones", list(&rep.migrations)));
    if !rep.gaps.is_empty() {
        t.lines.push(Line::from(""));
        t.lines.push(Line::from(format!("Huecos ({}) — los cubre el PR de bootstrap:", rep.gaps.len())).yellow());
        for g in &rep.gaps { t.lines.push(Line::from(format!("  · {g}"))); }
    }
    t
}

/// Resaltado mínimo de Markdown para el plan.
fn markdown(s: &str) -> Text<'static> {
    let mut code = false;
    Text::from(s.lines().map(|l| {
        if l.starts_with("```") { code = !code; return Line::from(l.to_string()).dark_gray(); }
        if code { return Line::from(l.to_string()).green(); }
        if l.starts_with("# ") { Line::from(l.to_string()).bold().cyan() }
        else if l.starts_with("## ") { Line::from(l.to_string()).bold().yellow() }
        else if l.starts_with("### ") { Line::from(l.to_string()).bold() }
        else { Line::from(l.to_string()) }
    }).collect::<Vec<_>>())
}
