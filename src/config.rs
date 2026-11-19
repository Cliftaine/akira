//! Modelo de .forja/config.yaml. El orquestador solo lee este archivo.
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::adapters::{self, CONTRACT};

pub const CONFIG_REL: &str = ".forja/config.yaml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenApi {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrations: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_doc: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub name: String,
    #[serde(default = "dot")]
    pub path: String,
    pub adapter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openapi: Option<OpenApi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<Database>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2e: Option<String>,
    /// Sobrescribe el contrato del adaptador. `null` deshabilita el paso.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, Option<String>>,
}
fn dot() -> String { ".".into() }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    #[serde(default)]
    pub compose: Option<String>,
    #[serde(default)]
    pub seed: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaxRetries {
    #[serde(default = "three")]
    pub code: u32,
    #[serde(default = "two")]
    pub gate: u32,
    #[serde(default = "two")]
    pub qa: u32,
}
impl Default for MaxRetries { fn default() -> Self { Self { code: 3, gate: 2, qa: 2 } } }
fn three() -> u32 { 3 }
fn two() -> u32 { 2 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agents {
    #[serde(default)]
    pub max_retries: MaxRetries,
    #[serde(default = "five")]
    pub budget_usd_per_feature: f64,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Modelo para los agentes (opcional; si falta usa el default de claude).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}
impl Default for Agents {
    fn default() -> Self { Self { max_retries: MaxRetries::default(), budget_usd_per_feature: 5.0, allowed_paths: vec![], model: None } }
}
fn five() -> f64 { 5.0 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qa {
    #[serde(default = "fifteen")]
    pub max_minutes: u32,
    #[serde(default = "two_hundred")]
    pub max_actions: u32,
    #[serde(default = "yes")]
    pub exploratory: bool,
}
impl Default for Qa { fn default() -> Self { Self { max_minutes: 15, max_actions: 200, exploratory: true } } }
fn fifteen() -> u32 { 15 }
fn two_hundred() -> u32 { 200 }
fn yes() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    #[serde(default = "default_steps")]
    pub steps: Vec<String>,
    #[serde(default = "nine_hundred")]
    pub timeout_seconds: u64,
}
impl Default for Gate { fn default() -> Self { Self { steps: default_steps(), timeout_seconds: 900 } } }
fn default_steps() -> Vec<String> { vec!["format_check".into(), "lint".into(), "test".into()] }
fn nine_hundred() -> u64 { 900 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForjaConfig {
    pub repo: String,
    #[serde(default = "main_branch")]
    pub default_branch: String,
    pub components: Vec<Component>,
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub agents: Agents,
    #[serde(default)]
    pub qa: Qa,
    #[serde(default)]
    pub gate: Gate,
}
fn main_branch() -> String { "main".into() }

impl ForjaConfig {
    pub fn to_yaml(&self, header: &str) -> Result<String> {
        Ok(format!("{header}{}", serde_yaml::to_string(self)?))
    }
    pub fn component(&self, name: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.name == name)
    }
}

pub fn path(root: &Path) -> PathBuf { root.join(CONFIG_REL) }

pub fn load(root: &Path) -> Result<ForjaConfig> {
    let p = path(root);
    if !p.exists() {
        return Err(anyhow!("no existe {}. Corre `forja connect {}` primero.", p.display(), root.display()));
    }
    parse(&std::fs::read_to_string(&p)?, Some(root))
}

pub fn parse(text: &str, root: Option<&Path>) -> Result<ForjaConfig> {
    let cfg: ForjaConfig = serde_yaml::from_str(text).map_err(|e| anyhow!("config.yaml inválido: {e}"))?;
    let problems = problems(&cfg, root);
    if !problems.is_empty() {
        return Err(anyhow!("config.yaml inválido:\n  - {}", problems.join("\n  - ")));
    }
    Ok(cfg)
}

pub fn problems(cfg: &ForjaConfig, root: Option<&Path>) -> Vec<String> {
    let mut p = vec![];
    if cfg.components.is_empty() {
        p.push("components: no hay componentes".into());
    }
    let mut seen = std::collections::HashSet::new();
    for c in &cfg.components {
        if !seen.insert(&c.name) {
            p.push(format!("components: nombre repetido '{}'", c.name));
        }
        if let Err(e) = adapters::load(&c.adapter) {
            p.push(format!("components.{}: {e}", c.name));
        }
        for step in c.commands.keys() {
            if !CONTRACT.contains(&step.as_str()) {
                p.push(format!("components.{}.commands: '{step}' no está en el contrato {CONTRACT:?}", c.name));
            }
        }
        if let Some(r) = root {
            if !r.join(&c.path).is_dir() {
                p.push(format!("components.{}.path: no existe '{}'", c.name, c.path));
            }
        }
    }
    for s in &cfg.gate.steps {
        if !CONTRACT.contains(&s.as_str()) {
            p.push(format!("gate.steps: '{s}' no está en el contrato {CONTRACT:?}"));
        }
    }
    for g in &cfg.agents.allowed_paths {
        if globset::Glob::new(g).is_err() {
            p.push(format!("agents.allowed_paths: glob inválido '{g}'"));
        }
    }
    p
}
