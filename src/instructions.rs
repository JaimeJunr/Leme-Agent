//! Project instruction files (AGENTS.md / HARNESS.md / CLAUDE.md), global and
//! per-directory, including lazily discovered nested files.

use crate::agent::shared::Shared;
use std::path::{Path, PathBuf};

pub const NAMES: [&str; 3] = ["AGENTS.md", "HARNESS.md", "CLAUDE.md"];
const LOCAL_NAMES: [&str; 2] = ["AGENTS.local.md", "CLAUDE.local.md"];
const MAX_FILE_BYTES: usize = 64 * 1024;

fn read_with_imports(path: &Path, depth: usize) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = crate::util::truncate_bytes(&s, MAX_FILE_BYTES).to_string();
    if depth >= 3 {
        return Some(s);
    }
    // `@path/to/file.md` on its own line imports that file (CLAUDE.md style).
    let base = path.parent().unwrap_or(Path::new("."));
    let mut out = String::new();
    for line in s.lines() {
        let t = line.trim();
        if let Some(rel) = t.strip_prefix('@')
            && !rel.is_empty()
            && !rel.contains(' ')
            && (rel.ends_with(".md") || rel.contains('/'))
        {
            let p = crate::util::resolve_path(base, rel);
            if let Some(inc) = read_with_imports(&p, depth + 1) {
                out.push_str(&inc);
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

fn pick(dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    if let Some(p) = NAMES.iter().map(|n| dir.join(n)).find(|p| p.is_file()) {
        v.push(p);
    }
    for n in LOCAL_NAMES {
        let p = dir.join(n);
        if p.is_file() {
            v.push(p);
        }
    }
    v
}

/// Global + root→cwd instruction files, in order of increasing specificity.
pub fn load(root: &Path, cwd: &Path) -> Vec<(PathBuf, String)> {
    let mut out = vec![];
    let global = [
        crate::config::config_dir().join("AGENTS.md"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".claude")
            .join("CLAUDE.md"),
    ];
    if let Some(g) = global.iter().find(|p| p.is_file())
        && let Some(s) = read_with_imports(g, 0)
    {
        out.push((g.clone(), s));
    }
    let mut dirs_chain = vec![];
    let mut d = Some(cwd);
    while let Some(dir) = d {
        dirs_chain.push(dir.to_path_buf());
        if dir == root {
            break;
        }
        d = dir.parent();
        if let Some(p) = d
            && !p.starts_with(root)
        {
            break;
        }
    }
    dirs_chain.reverse();
    for dir in dirs_chain {
        for p in pick(&dir) {
            if let Some(s) = read_with_imports(&p, 0) {
                out.push((p, s));
            }
        }
    }
    out
}

/// When the agent touches a file in a subdirectory that has its own
/// instruction file (not yet loaded), return it so it can be appended to the
/// tool result.
pub fn nested_for(shared: &Shared, file: &Path) -> Option<String> {
    let root = &shared.root;
    if !file.starts_with(root) {
        return None;
    }
    let mut found = vec![];
    let mut d = file.parent();
    while let Some(dir) = d {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        for p in pick(dir) {
            found.push(p);
        }
        d = dir.parent();
    }
    if found.is_empty() {
        return None;
    }
    let mut loaded = shared.loaded_instructions.lock();
    let mut out = String::new();
    for p in found.into_iter().rev() {
        if loaded.contains(&p) {
            continue;
        }
        loaded.insert(p.clone());
        if let Some(s) = read_with_imports(&p, 0) {
            out.push_str(&format!(
                "\n\n<instructions file=\"{}\">\nThese project instructions apply to files under {}:\n{}\n</instructions>",
                crate::util::display_path(root, &p),
                crate::util::display_path(root, p.parent().unwrap_or(root)),
                s.trim()
            ));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_chain_and_imports() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("AGENTS.md"), "root rules\n@docs/style.md\n").unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/style.md"), "use tabs").unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/CLAUDE.md"), "a rules").unwrap();
        let v = load(root, &root.join("a/b"));
        let texts: Vec<&str> = v.iter().map(|(_, s)| s.as_str()).collect();
        let joined = texts.join("|");
        assert!(joined.contains("root rules"));
        assert!(joined.contains("use tabs"));
        assert!(joined.contains("a rules"));
        assert!(joined.find("root rules").unwrap() < joined.find("a rules").unwrap());
    }
}
