//! Append-only JSONL sessions: resume, fork, rewind and file checkpoints.

use crate::conversation::Item;
use crate::llm::Usage;
use crate::tools::todo::TodoItem;
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Meta {
        id: String,
        cwd: String,
        created: String,
        model: String,
        #[serde(default)]
        parent: Option<String>,
    },
    Item {
        item: Item,
    },
    /// History replaced by a compacted version.
    Compaction {
        items: Vec<Item>,
        #[serde(default)]
        summary: String,
    },
    /// History truncated to `len` items (rewind).
    Truncate {
        len: usize,
    },
    Usage {
        usage: Usage,
        model: String,
    },
    Title {
        title: String,
    },
    Todos {
        todos: Vec<TodoItem>,
    },
    Model {
        model: String,
    },
    /// Original content of `path` before turn `turn` modified it
    /// (`blob` = None means the file did not exist).
    Checkpoint {
        turn: usize,
        path: String,
        blob: Option<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Replayed {
    pub items: Vec<Item>,
    pub usage: Usage,
    pub title: Option<String>,
    pub todos: Vec<TodoItem>,
    pub model: Option<String>,
    pub checkpoints: Vec<(usize, PathBuf, Option<String>)>,
    pub cwd: Option<String>,
    pub created: Option<String>,
}

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    file: Mutex<Option<File>>,
}

pub fn project_slug(root: &Path) -> String {
    let s = root.display().to_string();
    let mut slug: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug = slug.trim_matches('-').to_string();
    if slug.len() > 60 {
        slug = slug[slug.len() - 60..].to_string();
    }
    format!("{}-{}", slug, &crate::util::sha256_hex(s.as_bytes())[..8])
}

pub fn sessions_dir(root: &Path) -> PathBuf {
    crate::config::data_dir()
        .join("sessions")
        .join(project_slug(root))
}

impl Session {
    pub fn create(root: &Path, model: &str, parent: Option<String>) -> Result<Session> {
        let dir = sessions_dir(root);
        std::fs::create_dir_all(&dir)?;
        let id = format!(
            "{}-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            &crate::util::short_id()[..6]
        );
        let path = dir.join(format!("{id}.jsonl"));
        let s = Session {
            id: id.clone(),
            path,
            file: Mutex::new(None),
        };
        s.append(&Record::Meta {
            id,
            cwd: root.display().to_string(),
            created: crate::util::now_rfc3339(),
            model: model.to_string(),
            parent,
        });
        Ok(s)
    }

    pub fn open(path: &Path) -> Result<Session> {
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("bad session path")?
            .to_string();
        Ok(Session {
            id,
            path: path.to_path_buf(),
            file: Mutex::new(None),
        })
    }

    /// Append a record; failures are reported once but never fatal.
    pub fn append(&self, rec: &Record) {
        let mut guard = self.file.lock();
        if guard.is_none() {
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                Ok(f) => *guard = Some(f),
                Err(_) => return,
            }
        }
        if let (Some(f), Ok(line)) = (guard.as_mut(), serde_json::to_string(rec)) {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.path.with_extension("blobs")
    }

    /// Store content-addressed file content; returns its hash.
    pub fn put_blob(&self, data: &[u8]) -> Result<String> {
        let hash = crate::util::sha256_hex(data);
        let dir = self.blob_dir();
        std::fs::create_dir_all(&dir)?;
        let p = dir.join(&hash);
        if !p.exists() {
            std::fs::write(&p, data)?;
        }
        Ok(hash)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.blob_dir().join(hash))?)
    }

    pub fn replay(&self) -> Result<Replayed> {
        replay_file(&self.path)
    }

    /// Copy this session's history into a new session (fork).
    pub fn fork(&self, root: &Path, model: &str, items: &[Item]) -> Result<Session> {
        let s = Session::create(root, model, Some(self.id.clone()))?;
        s.append(&Record::Compaction {
            items: items.to_vec(),
            summary: String::new(),
        });
        // Share blobs so undo keeps working across the fork.
        let src = self.blob_dir();
        if src.is_dir() {
            let dst = s.blob_dir();
            let _ = std::fs::create_dir_all(&dst);
            if let Ok(rd) = std::fs::read_dir(&src) {
                for e in rd.flatten() {
                    let _ = std::fs::copy(e.path(), dst.join(e.file_name()));
                }
            }
        }
        Ok(s)
    }
}

pub fn replay_file(path: &Path) -> Result<Replayed> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut r = Replayed::default();
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Record>(&line) else {
            continue;
        };
        match rec {
            Record::Meta {
                cwd,
                created,
                model,
                ..
            } => {
                r.cwd = Some(cwd);
                r.created = Some(created);
                r.model = Some(model);
            }
            Record::Item { item } => r.items.push(item),
            Record::Compaction { items, .. } => r.items = items,
            Record::Truncate { len } => {
                r.items.truncate(len);
            }
            Record::Usage { usage, .. } => r.usage.add(&usage),
            Record::Title { title } => r.title = Some(title),
            Record::Todos { todos } => r.todos = todos,
            Record::Model { model } => r.model = Some(model),
            Record::Checkpoint { turn, path, blob } => {
                r.checkpoints.push((turn, PathBuf::from(path), blob))
            }
        }
    }
    Ok(r)
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub modified: std::time::SystemTime,
    pub messages: usize,
    pub cost: f64,
}

/// List sessions for a project, newest first.
pub fn list(root: &Path) -> Vec<SessionSummary> {
    let dir = sessions_dir(root);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out = vec![];
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let Ok(rep) = replay_file(&p) else { continue };
        let first_user = rep.items.iter().find_map(|i| match i {
            Item::User {
                content,
                synthetic: false,
                ..
            } => Some(crate::util::first_line(content, 70)),
            _ => None,
        });
        let messages = rep.items.iter().filter(|i| i.is_real_user()).count();
        if messages == 0 {
            continue;
        }
        out.push(SessionSummary {
            id: p.file_stem().unwrap().to_string_lossy().to_string(),
            title: rep.title.or(first_user).unwrap_or_else(|| "(empty)".into()),
            path: p,
            modified,
            messages,
            cost: rep.usage.cost,
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

pub fn find(root: &Path, id_prefix: &str) -> Result<PathBuf> {
    let all = list(root);
    let matches: Vec<_> = all
        .iter()
        .filter(|s| s.id.starts_with(id_prefix) || s.id.ends_with(id_prefix))
        .collect();
    match matches.len() {
        0 => bail!("no session matching `{id_prefix}`"),
        1 => Ok(matches[0].path.clone()),
        _ => Ok(matches[0].path.clone()),
    }
}

/// Render a session as Markdown (for /export).
pub fn to_markdown(items: &[Item], title: &str) -> String {
    let mut out = format!("# {title}\n\n");
    for it in items {
        match it {
            Item::User {
                content, synthetic, ..
            } => {
                if *synthetic {
                    out.push_str(&format!(
                        "> _harness:_ {}\n\n",
                        content.replace('\n', "\n> ")
                    ));
                } else {
                    out.push_str(&format!("## User\n\n{content}\n\n"));
                }
            }
            Item::Assistant {
                text, tool_calls, ..
            } => {
                if !text.is_empty() {
                    out.push_str(&format!("## Assistant\n\n{text}\n\n"));
                }
                for tc in tool_calls {
                    out.push_str(&format!(
                        "**tool** `{}` `{}`\n\n",
                        tc.name,
                        crate::util::ellipsize(&tc.arguments, 300)
                    ));
                }
            }
            Item::Tool {
                name,
                content,
                is_error,
                ..
            } => {
                let fence = if content.contains("```") {
                    "````"
                } else {
                    "```"
                };
                out.push_str(&format!(
                    "<details><summary>{} result{}</summary>\n\n{fence}\n{}\n{fence}\n\n</details>\n\n",
                    name,
                    if *is_error { " (error)" } else { "" },
                    crate::util::ellipsize(content, 4000)
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_replay() {
        crate::testutil::env();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let s = Session::create(&root, "m", None).unwrap();
        s.append(&Record::Item {
            item: Item::user("one"),
        });
        s.append(&Record::Item {
            item: Item::user("two"),
        });
        s.append(&Record::Truncate { len: 1 });
        s.append(&Record::Usage {
            usage: Usage {
                cost: 0.5,
                ..Default::default()
            },
            model: "m".into(),
        });
        s.append(&Record::Title { title: "T".into() });
        let r = s.replay().unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.title.as_deref(), Some("T"));
        assert!((r.usage.cost - 0.5).abs() < 1e-9);
        let h = s.put_blob(b"hello").unwrap();
        assert_eq!(s.get_blob(&h).unwrap(), b"hello");
        let l = list(&root);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].title, "T");
    }
}
