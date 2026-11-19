//! Envoltura mínima de git. El push lo hace solo este módulo, y nunca a la rama principal.
use anyhow::{Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

pub fn output(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        return Err(anyhow!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn run(dir: &Path, args: &[&str]) -> Result<()> { output(dir, args).map(|_| ()) }

/// Como `output` pero sin recortar: el formato porcelain depende de los espacios iniciales.
fn output_raw(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        return Err(anyhow!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Parsea `git status --porcelain=v1`: (estado XY, ruta).
pub fn parse_porcelain(out: &str) -> Vec<(String, String)> {
    out.lines().filter(|l| l.len() > 3).map(|l| {
        let path = l[3..].rsplit(" -> ").next().unwrap().trim_matches('"').to_string();
        (l[..2].to_string(), path)
    }).collect()
}

pub fn default_branch(root: &Path) -> String {
    if let Ok(h) = output(root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some((_, b)) = h.split_once('/') { return b.to_string(); }
    }
    for b in ["main", "master"] {
        if output(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")]).is_ok() { return b.into(); }
    }
    output(root, &["branch", "--show-current"]).unwrap_or_else(|_| "main".into())
}

pub fn has_remote(root: &Path) -> bool { output(root, &["remote", "get-url", "origin"]).is_ok() }

/// Crea (o reutiliza) un worktree con su rama desde la rama base.
pub fn ensure_worktree(repo: &Path, wt: &Path, branch: &str, base: &str) -> Result<()> {
    if wt.join(".git").exists() { return Ok(()); }
    if let Some(p) = wt.parent() { std::fs::create_dir_all(p)?; }
    let base_ref = if has_remote(repo) && run(repo, &["fetch", "origin", base]).is_ok() { format!("origin/{base}") } else { base.to_string() };
    let wts = wt.display().to_string();
    if output(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
        run(repo, &["worktree", "add", &wts, branch])
    } else {
        run(repo, &["worktree", "add", "-b", branch, &wts, &base_ref])
    }
}

pub fn remove_worktree(repo: &Path, wt: &Path) -> Result<()> {
    run(repo, &["worktree", "remove", "--force", &wt.display().to_string()])
}

/// Archivos cambiados (modificados, nuevos, borrados) respecto a HEAD, incluyendo no rastreados.
pub fn changed_files(wt: &Path) -> Result<Vec<(String, String)>> {
    Ok(parse_porcelain(&output_raw(wt, &["status", "--porcelain=v1", "--untracked-files=all"])?))
}

/// Revierte un archivo a HEAD (o lo borra si es nuevo).
pub fn revert_file(wt: &Path, status: &str, path: &str) -> Result<()> {
    if status.contains('?') || status.contains('A') {
        let _ = run(wt, &["rm", "--cached", "-q", "--", path]);
        let p = wt.join(path);
        if p.is_file() { std::fs::remove_file(p)?; }
        Ok(())
    } else {
        run(wt, &["checkout", "HEAD", "--", path])
    }
}

/// Commit de todo lo pendiente. Devuelve false si no había cambios.
pub fn commit_all(wt: &Path, msg: &str) -> Result<bool> {
    run(wt, &["add", "-A"])?;
    if output(wt, &["diff", "--cached", "--quiet"]).is_ok() { return Ok(false); }
    run(wt, &["commit", "-q", "--no-verify", "-m", msg])?;
    Ok(true)
}

pub fn diff_vs(wt: &Path, base: &str) -> Result<String> { output(wt, &["diff", &format!("{base}...HEAD")]) }

pub fn diff_stat(wt: &Path, base: &str) -> Result<String> { output(wt, &["diff", "--stat", &format!("{base}...HEAD")]) }

pub fn push_branch(wt: &Path, branch: &str, default_branch: &str) -> Result<()> {
    if branch == default_branch || branch == "main" || branch == "master" {
        bail!("forja nunca hace push a la rama principal");
    }
    run(wt, &["push", "-u", "origin", &format!("{branch}:{branch}")])
}

#[cfg(test)]
mod tests {
    #[test]
    fn porcelain_keeps_first_char() {
        let v = super::parse_porcelain(" M internal/calc/calc.go\n?? docs/x.md\nR  a.go -> b.go\n");
        assert_eq!(v[0], (" M".into(), "internal/calc/calc.go".into()));
        assert_eq!(v[1], ("??".into(), "docs/x.md".into()));
        assert_eq!(v[2].1, "b.go");
    }
}
