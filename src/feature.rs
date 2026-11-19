//! features/FEAT-NNN.yaml: entrada del planificador, del coordinador y (después) del agente QA.
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Estados: draft, planned, approved, implementing, documenting, gating, qa, ready, pr_open,
/// merged, changes_requested, failed, rejected (se guardan en SQLite con cada transición).

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub name: String,
    #[serde(default)]
    pub acceptance: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_user: Option<String>,
    #[serde(default)]
    pub flows: Vec<Flow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api: Vec<String>,
    #[serde(default = "media")]
    pub priority: String,
}
fn media() -> String { "media".into() }

impl Spec {
    pub fn validate(&self, cfg: &crate::config::ForjaConfig) -> Result<()> {
        if !regex::Regex::new(r"^FEAT-\d{3,}$").unwrap().is_match(&self.id) { bail!("id inválido '{}': usa FEAT-NNN", self.id); }
        if self.title.trim().is_empty() { bail!("title vacío"); }
        for c in &self.components {
            if cfg.component(c).is_none() { bail!("components: '{c}' no existe en .forja/config.yaml"); }
        }
        if self.flows.iter().any(|f| f.acceptance.is_empty()) { bail!("cada flow necesita al menos un criterio de aceptación"); }
        Ok(())
    }
    pub fn slug(&self) -> String { slug(&self.title) }
    pub fn branch(&self) -> String { format!("feat/{}-{}", self.id, self.slug()) }
}

pub fn slug(s: &str) -> String {
    let map = |c: char| match c { 'á'|'à'|'ä' => 'a', 'é'|'è'|'ë' => 'e', 'í'|'ì'|'ï' => 'i', 'ó'|'ò'|'ö' => 'o', 'ú'|'ù'|'ü' => 'u', 'ñ' => 'n', c => c };
    let s: String = s.to_lowercase().chars().map(map).map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    s.split('-').filter(|p| !p.is_empty()).take(5).collect::<Vec<_>>().join("-")
}

pub fn dir(root: &Path) -> PathBuf { root.join("features") }
pub fn yaml_path(root: &Path, id: &str) -> PathBuf { dir(root).join(format!("{id}.yaml")) }
pub fn plan_path(root: &Path, id: &str) -> PathBuf { dir(root).join(format!("{id}.plan.md")) }

pub fn load(root: &Path, id: &str) -> Result<Spec> {
    let p = yaml_path(root, id);
    let text = std::fs::read_to_string(&p).map_err(|e| anyhow!("{}: {e}", p.display()))?;
    serde_yaml::from_str(&text).map_err(|e| anyhow!("{}: {e}", p.display()))
}

pub fn save(root: &Path, spec: &Spec) -> Result<PathBuf> {
    std::fs::create_dir_all(dir(root))?;
    let p = yaml_path(root, &spec.id);
    std::fs::write(&p, serde_yaml::to_string(spec)?)?;
    Ok(p)
}

pub fn list(root: &Path) -> Vec<Spec> {
    let mut v: Vec<Spec> = std::fs::read_dir(dir(root)).map(|rd| rd.filter_map(|e| e.ok())
        .filter(|e| { let n = e.file_name().to_string_lossy().to_string(); n.starts_with("FEAT-") && n.ends_with(".yaml") })
        .filter_map(|e| serde_yaml::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()).collect()).unwrap_or_default();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

pub fn next_id(root: &Path, known: &[String]) -> String {
    let re = regex::Regex::new(r"FEAT-(\d+)").unwrap();
    let mut max = 0u32;
    let files: Vec<String> = std::fs::read_dir(dir(root)).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
    for s in files.iter().chain(known) {
        if let Some(c) = re.captures(s) { max = max.max(c[1].parse().unwrap_or(0)); }
    }
    format!("FEAT-{:03}", max + 1)
}

/// Plantilla determinística (cuando no se usa IA para el intake).
pub fn template(id: &str, title: &str, description: Option<&str>, cfg: &crate::config::ForjaConfig) -> Spec {
    Spec {
        id: id.into(), title: title.into(), description: description.map(String::from),
        components: cfg.components.iter().map(|c| c.name.clone()).collect(),
        entry: None, test_user: None,
        flows: vec![Flow { name: title.to_lowercase(), acceptance: vec!["TODO: describe el resultado observable".into()] }],
        api: vec![], priority: "media".into(),
    }
}

/// Extrae un bloque YAML/JSON de la respuesta de un LLM (quita ```yaml ... ```).
pub fn strip_fences(s: &str) -> String {
    let t = s.trim();
    let re = regex::Regex::new(r"(?s)```[a-zA-Z]*\n(.*?)```").unwrap();
    re.captures(t).map(|c| c[1].to_string()).unwrap_or_else(|| t.to_string())
}

/// El plan es Markdown; si el modelo lo envolvió en ```markdown ...```, lo quitamos.
pub fn strip_fences_md(s: &str) -> String {
    let t = s.trim();
    if t.starts_with("```") && t.ends_with("```") {
        let inner = t.trim_start_matches(|c| c != '\n').trim_end_matches('`');
        return inner.trim().to_string() + "\n";
    }
    t.to_string() + "\n"
}
