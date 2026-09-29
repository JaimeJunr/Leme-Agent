//! Context management: cheap pruning of stale tool output first, then an LLM
//! summary that preserves user instructions verbatim ("governance" never
//! decays), the task list and the files in play.

use crate::conversation::Item;
use crate::tools::todo::{TodoItem, render as render_todos};

pub const PRUNED_NOTE: &str =
    "[output pruned to save context — re-run the tool if you need it again]";

/// Replace old tool outputs (outside the protected recent window) with a
/// short note. Only acts if it frees at least `min_savings` tokens, so the
/// prompt cache isn't invalidated for a trivial gain. Returns tokens freed.
pub fn prune_tool_outputs(items: &mut [Item], protect_recent: usize, min_savings: usize) -> usize {
    let mut acc = 0usize;
    let mut candidates = vec![];
    for (i, it) in items.iter().enumerate().rev() {
        let t = it.estimate_tokens();
        acc += t;
        if acc <= protect_recent {
            continue;
        }
        if let Item::Tool {
            name,
            content,
            pruned: false,
            ..
        } = it
        {
            if matches!(
                name.as_str(),
                "skill" | "todo" | "propose_plan" | "ask_user"
            ) {
                continue;
            }
            if content.len() > 800 {
                candidates.push((i, t));
            }
        }
    }
    let savings: usize = candidates.iter().map(|(_, t)| *t).sum();
    if savings < min_savings {
        return 0;
    }
    for (i, _) in candidates {
        if let Item::Tool {
            content,
            pruned,
            images,
            ..
        } = &mut items[i]
        {
            let head: String = content.lines().take(3).collect::<Vec<_>>().join("\n");
            *content = format!("{}\n{PRUNED_NOTE}", crate::util::ellipsize(&head, 300));
            images.clear();
            *pruned = true;
        }
    }
    savings
}

pub fn summary_instructions(custom: Option<&str>) -> String {
    let mut s = String::from(
        "CONTEXT CHECKPOINT. The conversation is about to be compacted: your summary will REPLACE the history above, and you will continue the work from it alone. Do not call any tools now — reply only with the summary, in this structure:

## 1. User requests
Every request the user made, in detail and in order, and what is still open.
## 2. User instructions and constraints
Every instruction, preference, correction and rule the user stated — quote them verbatim. These must survive.
## 3. Key technical context
Architecture, conventions, relevant APIs, build/test/lint commands and environment facts you discovered.
## 4. Files
Each file read or modified: why it matters and exactly what changed (include short code snippets for important changes).
## 5. Errors and fixes
Problems encountered, their root causes and how they were resolved; approaches that failed and must not be retried.
## 6. Progress
What is done and how it was verified.
## 7. Next steps
The remaining work in order. State exactly what you were doing immediately before this checkpoint and the very next action.

Be dense and precise: concrete names, paths, commands, values. No preamble.",
    );
    if let Some(c) = custom {
        if !c.trim().is_empty() {
            s.push_str(&format!(
                "\n\nAdditional focus requested by the user: {}",
                c.trim()
            ));
        }
    }
    s
}

/// Flatten the conversation into a plain-text transcript (fallback path when
/// the conversation itself no longer fits).
pub fn transcript(items: &[Item], budget_chars: usize) -> String {
    let mut parts: Vec<String> = items
        .iter()
        .map(|it| match it {
            Item::User {
                content, synthetic, ..
            } => {
                if *synthetic {
                    format!("[harness]: {}", crate::util::ellipsize(content, 4000))
                } else {
                    format!("[user]: {content}")
                }
            }
            Item::Assistant {
                text, tool_calls, ..
            } => {
                let mut s = String::new();
                if !text.is_empty() {
                    s.push_str(&format!("[assistant]: {text}\n"));
                }
                for tc in tool_calls {
                    s.push_str(&format!(
                        "[tool call {}]: {}\n",
                        tc.name,
                        crate::util::ellipsize(&tc.arguments, 600)
                    ));
                }
                s
            }
            Item::Tool {
                name,
                content,
                is_error,
                ..
            } => {
                format!(
                    "[{} result{}]: {}",
                    name,
                    if *is_error { " (error)" } else { "" },
                    crate::util::ellipsize(content, 1500)
                )
            }
        })
        .collect();
    // Drop from the middle (oldest non-first) until it fits.
    let total = |p: &Vec<String>| p.iter().map(|s| s.len() + 1).sum::<usize>();
    let mut dropped = 0;
    while total(&parts) > budget_chars && parts.len() > 4 {
        // keep the first item (original request) and the tail
        if matches!(
            items.get(1 + dropped),
            Some(Item::User {
                synthetic: false,
                ..
            })
        ) {
            // never drop real user messages: shrink them into the head instead
            let s = parts.remove(1);
            parts[0].push_str(&format!("\n{}", crate::util::ellipsize(&s, 2000)));
        } else {
            parts.remove(1);
        }
        dropped += 1;
    }
    if dropped > 0 {
        parts.insert(1, format!("[… {dropped} earlier entries omitted …]"));
    }
    parts.join("\n")
}

/// The first message of the compacted history.
pub fn continuation_message(
    summary: &str,
    todos: &[TodoItem],
    modified: &[String],
    last_user: Option<&str>,
) -> String {
    let mut s = String::from(
        "This session continues from an earlier conversation that was compacted to save context. Summary of everything so far:\n\n",
    );
    s.push_str(summary.trim());
    if !modified.is_empty() {
        s.push_str("\n\nFiles modified in this session (re-read before editing):\n");
        for m in modified.iter().take(50) {
            s.push_str(&format!("- {m}\n"));
        }
    }
    if !todos.is_empty() {
        s.push_str("\n\nCurrent task list:\n");
        s.push_str(&render_todos(todos));
    }
    if let Some(u) = last_user {
        s.push_str(&format!(
            "\n\nThe user's most recent message, verbatim:\n<message>\n{}\n</message>",
            crate::util::ellipsize(u, 6000)
        ));
    }
    s.push_str("\n\nContinue the work from where it stopped. Do not ask the user to repeat anything; re-read files as needed.");
    s
}

/// Pick a tail of recent items to keep verbatim: starts at a user or
/// assistant message (never an orphan tool result) and fits in `budget`.
pub fn tail_start(items: &[Item], budget: usize) -> usize {
    let mut acc = 0;
    let mut best = items.len();
    for i in (0..items.len()).rev() {
        acc += items[i].estimate_tokens();
        if acc > budget {
            break;
        }
        if matches!(items[i], Item::User { .. } | Item::Assistant { .. }) {
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(n: usize) -> Item {
        Item::Tool {
            call_id: "x".into(),
            name: "read".into(),
            content: "x".repeat(n),
            is_error: false,
            images: vec![],
            pruned: false,
        }
    }

    #[test]
    fn prune_respects_threshold_and_recency() {
        let mut items = vec![Item::user("hi"), tool(40_000), tool(40_000), tool(4_000)];
        let saved = prune_tool_outputs(&mut items, 12_000, 5_000);
        assert!(saved > 0);
        assert!(matches!(&items[1], Item::Tool { pruned: true, .. }));
        assert!(matches!(&items[3], Item::Tool { pruned: false, .. }));
        let mut small = vec![Item::user("hi"), tool(2_000), tool(2_000)];
        assert_eq!(prune_tool_outputs(&mut small, 100, 50_000), 0);
    }

    #[test]
    fn tail_never_starts_with_tool() {
        let items = vec![
            Item::user("a"),
            Item::Assistant {
                text: "b".into(),
                reasoning: String::new(),
                reasoning_details: vec![],
                tool_calls: vec![],
                model: String::new(),
            },
            tool(100),
            tool(100),
        ];
        let s = tail_start(&items, 1_000_000);
        assert_eq!(s, 0);
        let s2 = tail_start(&items, 60);
        assert!(
            s2 == items.len() || matches!(items[s2], Item::User { .. } | Item::Assistant { .. })
        );
    }

    #[test]
    fn transcript_keeps_user_messages() {
        let mut items = vec![Item::user("first request")];
        for _ in 0..50 {
            items.push(tool(3000));
        }
        items.push(Item::user("second request"));
        let t = transcript(&items, 20_000);
        assert!(t.contains("first request"));
        assert!(t.contains("second request"));
        assert!(t.len() < 30_000);
    }
}
