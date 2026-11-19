//! Adaptadores por stack: un contrato fijo de comandos.
//! Resolución: builtin (embebido) -> ~/.forja/adapters/<name>.yaml -> components[].commands.
use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::collections::BTreeMap;

pub const CONTRACT: &[&str] = &["format_check", "lint", "test", "api_spec", "e2e"];

pub const BUILTIN: &[(&str, &str)] = &[
    ("go", include_str!("../adapters/go.yaml")),
    ("python", include_str!("../adapters/python.yaml")),
    ("typescript", include_str!("../adapters/typescript.yaml")),
    ("rust", include_str!("../adapters/rust.yaml")),
];

#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)] // manifest/docker los usarán las plantillas de repos nuevos (fase 7)
pub struct Adapter {
    pub name: String,
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub commands: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub test_globs: Vec<String>,
    #[serde(default)]
    pub docker: BTreeMap<String, String>,
    #[serde(skip)]
    pub source: String,
}

impl Adapter {
    /// Sobrescritura del repo > adaptador. `Some(None)` en overrides = paso deshabilitado.
    pub fn command(&self, step: &str, overrides: &BTreeMap<String, Option<String>>) -> Option<String> {
        match overrides.get(step) {
            Some(v) => v.clone(),
            None => self.commands.get(step).cloned().flatten(),
        }
    }
}

fn merge(base: serde_yaml::Value, over: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value::Mapping;
    match (base, over) {
        (Mapping(mut b), Mapping(o)) => {
            for (k, v) in o {
                let nv = match b.remove(&k) {
                    Some(bv) => merge(bv, v),
                    None => v,
                };
                b.insert(k, nv);
            }
            Mapping(b)
        }
        (_, o) => o,
    }
}

pub fn load(name: &str) -> Result<Adapter> {
    let mut value = serde_yaml::Value::Null;
    let mut source = vec![];
    if let Some((_, text)) = BUILTIN.iter().find(|(n, _)| *n == name) {
        value = serde_yaml::from_str(text)?;
        source.push("builtin".to_string());
    }
    let user = crate::paths::adapters().join(format!("{name}.yaml"));
    if user.exists() {
        let over: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&user)?)?;
        value = if value.is_null() { over } else { merge(value, over) };
        source.push(user.display().to_string());
    }
    if value.is_null() {
        let names: Vec<_> = BUILTIN.iter().map(|(n, _)| *n).collect();
        return Err(anyhow!("no existe el adaptador '{name}' (builtin: {})", names.join(", ")));
    }
    let mut a: Adapter = serde_yaml::from_value(value)?;
    a.source = source.join(" + ");
    Ok(a)
}

/// Copia los adaptadores embebidos a ~/.forja/adapters para editarlos.
pub fn install(overwrite: bool) -> Result<Vec<std::path::PathBuf>> {
    let dir = crate::paths::adapters();
    std::fs::create_dir_all(&dir)?;
    let mut out = vec![];
    for (name, text) in BUILTIN {
        let p = dir.join(format!("{name}.yaml"));
        if overwrite || !p.exists() {
            std::fs::write(&p, text)?;
            out.push(p);
        }
    }
    Ok(out)
}
