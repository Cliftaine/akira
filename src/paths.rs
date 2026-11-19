//! Rutas globales. FORJA_HOME permite aislar todo (pruebas, sandbox).
use std::path::PathBuf;

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("FORJA_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir().expect("sin $HOME").join(".forja")
}
pub fn registry() -> PathBuf { home().join("registry.db") }
pub fn adapters() -> PathBuf { home().join("adapters") }
pub fn runs() -> PathBuf { home().join("runs") }
pub fn logs() -> PathBuf { home().join("logs") }
pub fn worktrees() -> PathBuf { home().join("worktrees") }

pub fn ensure() -> std::io::Result<PathBuf> {
    for d in [home(), adapters(), runs(), logs(), worktrees(), home().join("templates")] {
        std::fs::create_dir_all(d)?;
    }
    Ok(home())
}
