//! The machino package system.
//!
//! A package is a directory with a `machino.pkg` manifest — a deliberately
//! trivial line-based format (AI agents and humans both parse it at a
//! glance, no TOML/JSON parser needed):
//!
//! ```text
//! # machino.pkg
//! name myapp
//! version 0.1.0
//! dep mathx ../mathx
//! dep strkit https://github.com/user/strkit-mno 0.2.0
//! ```
//!
//! `machino pkg sync` installs every dependency into `machino_modules/`
//! next to the manifest — local paths are copied, git URLs are cloned
//! (`--depth 1`, optionally at a tag/branch given as the third word) —
//! and records what was installed in `machino.lock`. Dependencies are
//! resolved transitively and flattened; two packages that want the same
//! name from different sources is an error.
//!
//! Programs import from packages with the `pkg:` prefix:
//!
//! ```text
//! import "pkg:mathx/mathx.mno"
//! ```
//!
//! which resolves to `<project root>/machino_modules/mathx/mathx.mno`,
//! where the project root is the nearest ancestor directory of the entry
//! file that contains `machino.pkg`.

use std::path::{Path, PathBuf};

pub struct Manifest {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub version: String,
    pub deps: Vec<Dep>,
}

#[derive(Clone)]
pub struct Dep {
    pub name: String,
    pub source: String,
    pub reference: Option<String>,
}

pub fn parse_manifest(text: &str, path: &Path) -> Result<Manifest, String> {
    let mut name = String::new();
    let mut version = "0.0.0".to_string();
    let mut deps = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["name", n] => name = n.to_string(),
            ["version", v] => version = v.to_string(),
            ["dep", n, src] => deps.push(Dep {
                name: n.to_string(),
                source: src.to_string(),
                reference: None,
            }),
            ["dep", n, src, r] => deps.push(Dep {
                name: n.to_string(),
                source: src.to_string(),
                reference: Some(r.to_string()),
            }),
            _ => {
                return Err(format!(
                    "error: {}:{}: cannot parse manifest line '{}'\nexpected: name <n> | version <v> | dep <name> <source> [ref]",
                    path.display(),
                    i + 1,
                    line
                ))
            }
        }
    }
    if name.is_empty() {
        return Err(format!(
            "error: {}: manifest is missing a 'name' line",
            path.display()
        ));
    }
    Ok(Manifest {
        name,
        version,
        deps,
    })
}

/// Nearest ancestor of `start` (inclusive) containing machino.pkg.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    if let Ok(abs) = dir.canonicalize() {
        dir = abs;
    }
    loop {
        if dir.join("machino.pkg").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolves a `pkg:name/path.mno` import against the project root of the
/// entry file.
pub fn resolve_pkg_import(import: &str, entry: &Path) -> Result<PathBuf, String> {
    let rest = import.strip_prefix("pkg:").expect("caller checked");
    let root = find_project_root(entry).ok_or_else(|| {
        format!(
            "error: cannot resolve import \"{}\": no machino.pkg found in any ancestor of {} (run 'machino pkg init <name>' first)",
            import,
            entry.display()
        )
    })?;
    let path = root.join("machino_modules").join(rest);
    if !path.exists() {
        return Err(format!(
            "error: cannot resolve import \"{}\": {} does not exist (run 'machino pkg sync')",
            import,
            path.display()
        ));
    }
    Ok(path)
}

fn is_git_source(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.ends_with(".git")
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("error: cannot create {}: {}", to.display(), e))?;
    let entries = std::fs::read_dir(from)
        .map_err(|e| format!("error: cannot read {}: {}", from.display(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("error: {}", e))?;
        let name = entry.file_name();
        if name == ".git" || name == "machino_modules" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .map_err(|e| format!("error: cannot copy {}: {}", src.display(), e))?;
        }
    }
    Ok(())
}

fn git_head(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Installs one dependency into machino_modules/<name>. Returns a lockfile
/// entry: (name, source, resolved).
fn install_dep(root: &Path, dep: &Dep) -> Result<(String, String, String), String> {
    let modules = root.join("machino_modules");
    std::fs::create_dir_all(&modules)
        .map_err(|e| format!("error: cannot create {}: {}", modules.display(), e))?;
    let target = modules.join(&dep.name);
    if target.exists() {
        std::fs::remove_dir_all(&target)
            .map_err(|e| format!("error: cannot clean {}: {}", target.display(), e))?;
    }
    if is_git_source(&dep.source) {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["clone", "--depth", "1", "--quiet"]);
        if let Some(r) = &dep.reference {
            cmd.args(["--branch", r]);
        }
        cmd.arg(&dep.source).arg(&target);
        let status = cmd
            .status()
            .map_err(|e| format!("error: cannot run git: {}", e))?;
        if !status.success() {
            return Err(format!(
                "error: git clone of '{}' ({}) failed",
                dep.name, dep.source
            ));
        }
        let commit = git_head(&target).unwrap_or_else(|| "unknown".to_string());
        Ok((dep.name.clone(), dep.source.clone(), commit))
    } else {
        let src = if Path::new(&dep.source).is_absolute() {
            PathBuf::from(&dep.source)
        } else {
            root.join(&dep.source)
        };
        if !src.is_dir() {
            return Err(format!(
                "error: dependency '{}' points at '{}', which is not a directory",
                dep.name, dep.source
            ));
        }
        copy_dir(&src, &target)?;
        Ok((dep.name.clone(), dep.source.clone(), "path".to_string()))
    }
}

/// Installs all dependencies (transitively, flattened) and writes
/// machino.lock. Conflicting sources for the same name are an error.
pub fn sync(root: &Path) -> Result<Vec<(String, String, String)>, String> {
    let manifest_path = root.join("machino.pkg");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("error: cannot read {}: {}", manifest_path.display(), e))?;
    let manifest = parse_manifest(&text, &manifest_path)?;

    let mut installed: Vec<(String, String, String)> = Vec::new();
    let mut sources: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut queue: Vec<Dep> = manifest.deps.clone();
    while let Some(dep) = queue.pop() {
        match sources.get(&dep.name) {
            Some(existing) if existing == &dep.source => continue, // already installed
            Some(existing) => {
                return Err(format!(
                    "error: dependency conflict: '{}' is required from both '{}' and '{}'",
                    dep.name, existing, dep.source
                ))
            }
            None => {}
        }
        sources.insert(dep.name.clone(), dep.source.clone());
        let entry = install_dep(root, &dep)?;
        println!("installed {} ({} @ {})", entry.0, entry.1, entry.2);
        // relative path deps declared by this package resolve against the
        // package's original location (for path deps) or its clone (for git)
        let dep_origin = if is_git_source(&dep.source) {
            root.join("machino_modules").join(&dep.name)
        } else if Path::new(&dep.source).is_absolute() {
            PathBuf::from(&dep.source)
        } else {
            root.join(&dep.source)
        };
        installed.push(entry);
        // transitive dependencies of the installed package
        let sub_manifest = root
            .join("machino_modules")
            .join(&dep.name)
            .join("machino.pkg");
        if sub_manifest.exists() {
            let sub_text = std::fs::read_to_string(&sub_manifest)
                .map_err(|e| format!("error: cannot read {}: {}", sub_manifest.display(), e))?;
            let sub = parse_manifest(&sub_text, &sub_manifest)?;
            for mut d in sub.deps {
                if !is_git_source(&d.source) && !Path::new(&d.source).is_absolute() {
                    d.source = dep_origin.join(&d.source).display().to_string();
                }
                queue.push(d);
            }
        }
    }

    let mut lock = String::from("# machino.lock — written by 'machino pkg sync'\n");
    let mut sorted = installed.clone();
    sorted.sort();
    for (name, source, resolved) in &sorted {
        lock.push_str(&format!("{} {} {}\n", name, source, resolved));
    }
    let lock_path = root.join("machino.lock");
    std::fs::write(&lock_path, lock)
        .map_err(|e| format!("error: cannot write {}: {}", lock_path.display(), e))?;
    Ok(installed)
}

pub fn init(dir: &Path, name: &str) -> Result<(), String> {
    let manifest = dir.join("machino.pkg");
    if manifest.exists() {
        return Err(format!("error: {} already exists", manifest.display()));
    }
    std::fs::write(
        &manifest,
        format!(
            "# machino.pkg — package manifest\nname {}\nversion 0.1.0\n\n# dependencies: dep <name> <path-or-git-url> [tag]\n",
            name
        ),
    )
    .map_err(|e| format!("error: cannot write {}: {}", manifest.display(), e))?;
    println!("created {}", manifest.display());
    Ok(())
}

pub fn add(root: &Path, name: &str, source: &str, reference: Option<&str>) -> Result<(), String> {
    let manifest_path = root.join("machino.pkg");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("error: cannot read {}: {} (run 'machino pkg init' first)", manifest_path.display(), e))?;
    let manifest = parse_manifest(&text, &manifest_path)?;
    if manifest.deps.iter().any(|d| d.name == name) {
        return Err(format!(
            "error: dependency '{}' is already in {}",
            name,
            manifest_path.display()
        ));
    }
    let mut line = format!("dep {} {}", name, source);
    if let Some(r) = reference {
        line.push(' ');
        line.push_str(r);
    }
    let mut new_text = text;
    if !new_text.ends_with('\n') {
        new_text.push('\n');
    }
    new_text.push_str(&line);
    new_text.push('\n');
    std::fs::write(&manifest_path, new_text)
        .map_err(|e| format!("error: cannot write {}: {}", manifest_path.display(), e))?;
    println!("added {} -> {}", name, source);
    Ok(())
}
