//! Gate determinístico: cada paso es un comando y su código de salida decide. Sin LLM.
use anyhow::Result;
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::adapters;
use crate::config::ForjaConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status { Pass, Fail, Skip }

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub component: String,
    pub step: String,
    pub status: Status,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    pub seconds: f64,
    pub log_file: Option<String>,
    pub reason: Option<String>,
    #[serde(skip)]
    pub tail: String,
}

#[derive(Debug, Default, Serialize)]
pub struct GateResult { pub steps: Vec<StepResult> }

impl GateResult {
    pub fn passed(&self) -> bool { !self.steps.iter().any(|s| s.status == Status::Fail) }
    /// Resumen de fallas para devolverlo como feedback a un agente.
    pub fn failure_feedback(&self) -> String {
        self.steps.iter().filter(|s| s.status == Status::Fail).map(|s| {
            format!("### {} / {} (exit {:?}{})\n$ {}\n```\n{}\n```", s.component, s.step, s.exit_code,
                    s.reason.as_ref().map(|r| format!(", {r}")).unwrap_or_default(), s.command.clone().unwrap_or_default(), s.tail.trim())
        }).collect::<Vec<_>>().join("\n\n")
    }
}

pub struct PlannedStep { pub component: String, pub path: String, pub step: String, pub command: Option<String>, pub skip_reason: Option<String> }

pub fn plan(cfg: &ForjaConfig, steps: Option<&[String]>, components: Option<&[String]>) -> Result<Vec<PlannedStep>> {
    let mut out = vec![];
    for c in &cfg.components {
        if components.is_some_and(|cs| !cs.contains(&c.name)) { continue; }
        let a = adapters::load(&c.adapter)?;
        for step in steps.unwrap_or(&cfg.gate.steps) {
            let cmd = a.command(step, &c.commands);
            let skip_reason = cmd.is_none().then(|| if c.commands.contains_key(step) { "deshabilitado en config.yaml".to_string() } else { format!("el adaptador {} no define '{step}'", a.name) });
            out.push(PlannedStep { component: c.name.clone(), path: c.path.clone(), step: step.clone(), command: cmd, skip_reason });
        }
    }
    Ok(out)
}

pub struct Hooks<'a> {
    pub on_start: &'a mut dyn FnMut(&PlannedStep),
    pub on_line: &'a mut dyn FnMut(&str),
    pub on_done: &'a mut dyn FnMut(&StepResult),
}

pub fn run(root: &Path, cfg: &ForjaConfig, log_dir: &Path, steps: Option<&[String]>, components: Option<&[String]>,
           fail_fast: bool, hooks: &mut Hooks) -> Result<GateResult> {
    std::fs::create_dir_all(log_dir)?;
    let mut res = GateResult::default();
    for p in plan(cfg, steps, components)? {
        let r = match &p.command {
            None => StepResult { component: p.component.clone(), step: p.step.clone(), status: Status::Skip, command: None,
                                 exit_code: None, seconds: 0.0, log_file: None, reason: p.skip_reason.clone(), tail: String::new() },
            Some(cmd) => {
                (hooks.on_start)(&p);
                let log = log_dir.join(format!("{}.{}.log", p.component, p.step));
                let mut r = run_step(cmd, &root.join(&p.path), &log, cfg.gate.timeout_seconds, hooks.on_line)?;
                r.component = p.component.clone();
                r.step = p.step.clone();
                r
            }
        };
        (hooks.on_done)(&r);
        let failed = r.status == Status::Fail;
        res.steps.push(r);
        if fail_fast && failed { break; }
    }
    Ok(res)
}

fn run_step(cmd: &str, cwd: &Path, log: &Path, timeout: u64, on_line: &mut dyn FnMut(&str)) -> Result<StepResult> {
    let t0 = Instant::now();
    let mut lf = std::fs::File::create(log)?;
    writeln!(lf, "$ {cmd}\n# cwd: {}\n", cwd.display())?;
    let mut child = Command::new("bash").arg("-c").arg(format!("{cmd} 2>&1")).current_dir(cwd)
        .stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0).spawn()?;
    let pid = child.id() as i32;
    let timed_out = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    {
        let (t, d) = (timed_out.clone(), done.clone());
        std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(timeout);
            while Instant::now() < end {
                if d.load(Ordering::Relaxed) { return; }
                std::thread::sleep(Duration::from_millis(200));
            }
            t.store(true, Ordering::Relaxed);
            let _ = Command::new("kill").args(["-9", &format!("-{pid}")]).status();
        });
    }
    let mut tail: std::collections::VecDeque<String> = Default::default();
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap_or_default();
        writeln!(lf, "{line}")?;
        on_line(&line);
        tail.push_back(line);
        if tail.len() > 40 { tail.pop_front(); }
    }
    let status = child.wait()?;
    done.store(true, Ordering::Relaxed);
    let code = status.code();
    writeln!(lf, "\n# exit: {code:?}")?;
    let to = timed_out.load(Ordering::Relaxed);
    let reason = if to { Some(format!("timeout tras {timeout}s")) } else if code == Some(127) { Some("herramienta no instalada (exit 127)".into()) } else { None };
    Ok(StepResult {
        component: String::new(), step: String::new(),
        status: if code == Some(0) && !to { Status::Pass } else { Status::Fail },
        command: Some(cmd.to_string()), exit_code: code, seconds: (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0,
        log_file: Some(log.display().to_string()), reason, tail: tail.into_iter().collect::<Vec<_>>().join("\n"),
    })
}

use std::os::unix::process::CommandExt as _;
