//! State shared between the agent loop, its tools and subagents.

use crate::config::Config;
use crate::extensions::Extensions;
use crate::llm::{Catalog, LlmClient, Usage};
use crate::mcp::McpManager;
use crate::permissions::Mode;
use crate::session::{Record, Session};
use crate::tools::bash::Jobs;
use crate::tools::todo::TodoItem;
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq)]
struct FileStamp {
    mtime: Option<SystemTime>,
    len: u64,
}

fn stamp(p: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(p).ok()?;
    Some(FileStamp {
        mtime: m.modified().ok(),
        len: m.len(),
    })
}

/// Tracks which files the model has seen, to refuse blind overwrites and to
/// detect files changed on disk behind the agent's back.
#[derive(Default)]
pub struct FileTracker {
    seen: HashMap<PathBuf, FileStamp>,
}

impl FileTracker {
    pub fn mark_read(&mut self, p: &Path) {
        if let Some(s) = stamp(p) {
            self.seen.insert(p.to_path_buf(), s);
        }
    }

    /// Record our own write so it doesn't count as an external change.
    pub fn mark_written(&mut self, p: &Path) {
        self.mark_read(p);
    }

    pub fn was_read(&self, p: &Path) -> bool {
        self.seen.contains_key(p)
    }

    /// Err(message) if the file changed on disk since the model last saw it.
    pub fn check_fresh(&self, p: &Path) -> Result<(), String> {
        let Some(old) = self.seen.get(p) else {
            return Ok(());
        };
        match stamp(p) {
            Some(now) if now != *old => Err(format!(
                "{} was modified on disk since you last read it (by the user or another process). Read it again before editing.",
                p.display()
            )),
            _ => Ok(()),
        }
    }

    pub fn forget(&mut self, p: &Path) {
        self.seen.remove(p);
    }
}

/// Per-turn snapshots of files before the agent modified them.
#[derive(Default)]
pub struct Checkpoints {
    /// (turn, path, blob hash or None if the file did not exist)
    pub entries: Vec<(usize, PathBuf, Option<String>)>,
}

impl Checkpoints {
    pub fn has(&self, turn: usize, p: &Path) -> bool {
        self.entries.iter().any(|(t, q, _)| *t == turn && q == p)
    }

    /// Files changed since the start of `turn` (inclusive), deduplicated.
    pub fn files_since(&self, turn: usize) -> Vec<PathBuf> {
        let mut seen = BTreeSet::new();
        for (t, p, _) in &self.entries {
            if *t >= turn {
                seen.insert(p.clone());
            }
        }
        seen.into_iter().collect()
    }

    /// The state to restore for each file to return to the start of `turn`.
    pub fn restore_plan(&self, turn: usize) -> Vec<(PathBuf, Option<String>)> {
        let mut plan: Vec<(PathBuf, Option<String>)> = vec![];
        for (t, p, blob) in &self.entries {
            if *t >= turn && !plan.iter().any(|(q, _)| q == p) {
                plan.push((p.clone(), blob.clone()));
            }
        }
        plan
    }

    pub fn drop_from(&mut self, turn: usize) {
        self.entries.retain(|(t, _, _)| *t < turn);
    }
}

pub struct Shared {
    pub cfg: RwLock<Arc<Config>>,
    pub root: PathBuf,
    pub cwd: Mutex<PathBuf>,
    pub client: LlmClient,
    pub catalog: RwLock<Arc<Catalog>>,
    pub files: Mutex<FileTracker>,
    pub todos: Mutex<Vec<TodoItem>>,
    pub jobs: Jobs,
    pub checkpoints: Mutex<Checkpoints>,
    /// Current user turn number (index used for checkpoints).
    pub turn: AtomicUsize,
    /// Files modified during the current user turn.
    pub modified: Mutex<BTreeSet<PathBuf>>,
    /// Nested AGENTS.md files already surfaced to the model.
    pub loaded_instructions: Mutex<HashSet<PathBuf>>,
    pub session: RwLock<Arc<Session>>,
    pub ext: Extensions,
    pub mcp: Option<Arc<McpManager>>,
    pub spill_dir: PathBuf,
    pub mode: RwLock<Mode>,
    /// "Allow for this session" rules granted interactively.
    pub session_rules: Mutex<Vec<String>>,
    /// A human is present to answer approvals / questions.
    pub interactive: bool,
    /// Instruction files loaded into the system prompt.
    pub instructions: Vec<(PathBuf, String)>,
    pub git: Option<String>,
    pub sandbox_available: bool,
    /// Session-wide usage including subagents and helper calls.
    pub total_usage: Mutex<Usage>,
    pub title: Mutex<Option<String>>,
}

impl Shared {
    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().clone()
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().clone()
    }

    pub fn session(&self) -> Arc<Session> {
        self.session.read().clone()
    }

    /// Snapshot a file before modification (once per turn per file).
    pub fn checkpoint(&self, path: &Path) {
        let turn = self.turn.load(Ordering::SeqCst);
        {
            let ck = self.checkpoints.lock();
            if ck.has(turn, path) {
                return;
            }
        }
        let session = self.session();
        let blob = match std::fs::read(path) {
            Ok(data) => session.put_blob(&data).ok(),
            Err(_) => None,
        };
        // If the file exists but we failed to store it, don't record a
        // misleading "did not exist" entry.
        if blob.is_none() && path.exists() {
            return;
        }
        session.append(&Record::Checkpoint {
            turn,
            path: path.display().to_string(),
            blob: blob.clone(),
        });
        self.checkpoints
            .lock()
            .entries
            .push((turn, path.to_path_buf(), blob));
    }

    pub fn note_modified(&self, path: &Path) {
        self.modified.lock().insert(path.to_path_buf());
        self.files.lock().mark_written(path);
    }

    /// Restore all files to their state at the start of `turn`.
    /// Returns the list of restored paths.
    pub fn restore_to(&self, turn: usize) -> anyhow::Result<Vec<PathBuf>> {
        let plan = self.checkpoints.lock().restore_plan(turn);
        let session = self.session();
        let mut restored = vec![];
        for (path, blob) in plan {
            match blob {
                Some(hash) => {
                    let data = session.get_blob(&hash)?;
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&path, data)?;
                }
                None => {
                    if path.exists() {
                        std::fs::remove_file(&path)?;
                    }
                }
            }
            self.files.lock().forget(&path);
            restored.push(path);
        }
        self.checkpoints.lock().drop_from(turn);
        Ok(restored)
    }

    /// Write a large output to a spill file the model can page through.
    pub fn spill(&self, prefix: &str, content: &str) -> Option<PathBuf> {
        std::fs::create_dir_all(&self.spill_dir).ok()?;
        let p = self
            .spill_dir
            .join(format!("{prefix}-{}.txt", crate::util::short_id()));
        std::fs::write(&p, content).ok()?;
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_plan_uses_earliest_snapshot() {
        let mut c = Checkpoints::default();
        let a = PathBuf::from("/a");
        c.entries.push((1, a.clone(), Some("h1".into())));
        c.entries.push((2, a.clone(), Some("h2".into())));
        c.entries.push((2, PathBuf::from("/b"), None));
        let plan = c.restore_plan(1);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0], (a.clone(), Some("h1".into())));
        let plan2 = c.restore_plan(2);
        assert_eq!(plan2[0], (a, Some("h2".into())));
        assert_eq!(c.files_since(2).len(), 2);
    }
}
