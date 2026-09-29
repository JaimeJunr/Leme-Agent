//! Provider-neutral conversation items and their conversion to the
//! OpenAI-compatible wire format OpenRouter expects.

use crate::llm::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Image {
    pub mime: String,
    /// base64 payload (no `data:` prefix)
    pub data: String,
    #[serde(default)]
    pub label: String,
}

impl Image {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Item {
    User {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
        /// Injected by the harness (verify feedback, compaction summary,
        /// hook output…) rather than typed by the human.
        #[serde(default, skip_serializing_if = "is_false")]
        synthetic: bool,
        /// Monotonic turn id (checkpoints and rewind are keyed by it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<usize>,
    },
    Assistant {
        #[serde(default)]
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning_details: Vec<Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        model: String,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
        /// Output was elided by context pruning.
        #[serde(default, skip_serializing_if = "is_false")]
        pruned: bool,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Item {
    #[cfg(test)]
    pub fn user(s: impl Into<String>) -> Item {
        Item::User {
            content: s.into(),
            images: vec![],
            synthetic: false,
            turn: None,
        }
    }
    pub fn synthetic(s: impl Into<String>) -> Item {
        Item::User {
            content: s.into(),
            images: vec![],
            synthetic: true,
            turn: None,
        }
    }

    pub fn estimate_tokens(&self) -> usize {
        use crate::util::estimate_tokens as est;
        match self {
            Item::User {
                content, images, ..
            } => est(content) + images.len() * 1200,
            Item::Assistant {
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                est(text)
                    + est(reasoning) / 2
                    + tool_calls
                        .iter()
                        .map(|t| est(&t.arguments) + 10)
                        .sum::<usize>()
            }
            Item::Tool {
                content, images, ..
            } => est(content) + images.len() * 1200 + 5,
        }
    }

    pub fn turn(&self) -> Option<usize> {
        match self {
            Item::User { turn, .. } => *turn,
            _ => None,
        }
    }

    pub fn is_real_user(&self) -> bool {
        matches!(
            self,
            Item::User {
                synthetic: false,
                ..
            }
        )
    }
}

pub struct WireOptions {
    pub cache_control: bool,
    pub vision: bool,
    /// Send prior reasoning_details back (needed for Anthropic/Gemini/OpenAI
    /// reasoning continuity across tool calls).
    pub include_reasoning: bool,
}

fn cc() -> Value {
    json!({"type": "ephemeral"})
}

/// Convert system prompt + items to wire messages, placing up to 4 cache
/// breakpoints (system + the last three conversation messages) when the
/// provider needs explicit caching.
pub fn to_wire(system: &str, items: &[Item], opts: &WireOptions) -> Vec<Value> {
    let mut msgs: Vec<Value> = Vec::with_capacity(items.len() + 1);
    if opts.cache_control {
        msgs.push(json!({
            "role": "system",
            "content": [{"type": "text", "text": system, "cache_control": cc()}],
        }));
    } else {
        msgs.push(json!({"role": "system", "content": system}));
    }

    // Tool results must directly follow the assistant message that issued
    // the calls; images from tool results are therefore gathered and sent in
    // one user message after the whole tool block.
    let mut pending_images: Vec<(String, Image)> = vec![];
    let flush_images = |msgs: &mut Vec<Value>, pending: &mut Vec<(String, Image)>| {
        if pending.is_empty() {
            return;
        }
        let mut parts = vec![];
        for (label, img) in pending.drain(..) {
            parts.push(json!({"type": "text", "text": format!("[image from {label}]")}));
            parts.push(json!({"type": "image_url", "image_url": {"url": img.data_url()}}));
        }
        msgs.push(json!({"role": "user", "content": parts}));
    };

    for item in items {
        match item {
            Item::User {
                content, images, ..
            } => {
                flush_images(&mut msgs, &mut pending_images);
                if images.is_empty() || !opts.vision {
                    let mut text = content.clone();
                    if !images.is_empty() {
                        text.push_str(&format!(
                            "\n\n[{} image(s) omitted: the current model does not accept images]",
                            images.len()
                        ));
                    }
                    msgs.push(json!({"role": "user", "content": text}));
                } else {
                    let mut parts = vec![json!({"type": "text", "text": content})];
                    for img in images {
                        parts.push(
                            json!({"type": "image_url", "image_url": {"url": img.data_url()}}),
                        );
                    }
                    msgs.push(json!({"role": "user", "content": parts}));
                }
            }
            Item::Assistant {
                text,
                reasoning_details,
                tool_calls,
                ..
            } => {
                flush_images(&mut msgs, &mut pending_images);
                let mut m = serde_json::Map::new();
                m.insert("role".into(), json!("assistant"));
                if text.is_empty() {
                    m.insert(
                        "content".into(),
                        if tool_calls.is_empty() {
                            json!("")
                        } else {
                            Value::Null
                        },
                    );
                } else {
                    m.insert("content".into(), json!(text));
                }
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            // Providers reject invalid JSON in history; normalise.
                            let args = match serde_json::from_str::<Value>(&tc.arguments) {
                                Ok(_) => tc.arguments.clone(),
                                Err(_) => crate::util::parse_json_lenient(&tc.arguments)
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|_| "{}".into()),
                            };
                            json!({"id": tc.id, "type": "function", "function": {"name": tc.name, "arguments": args}})
                        })
                        .collect();
                    m.insert("tool_calls".into(), Value::Array(calls));
                }
                if opts.include_reasoning && !reasoning_details.is_empty() {
                    m.insert(
                        "reasoning_details".into(),
                        Value::Array(reasoning_details.clone()),
                    );
                }
                msgs.push(Value::Object(m));
            }
            Item::Tool {
                call_id,
                name,
                content,
                images,
                ..
            } => {
                let text = if content.is_empty() {
                    "(no output)".to_string()
                } else {
                    content.clone()
                };
                msgs.push(json!({"role": "tool", "tool_call_id": call_id, "content": text}));
                if opts.vision {
                    for img in images {
                        let label = if img.label.is_empty() {
                            name.clone()
                        } else {
                            img.label.clone()
                        };
                        pending_images.push((label, img.clone()));
                    }
                }
            }
        }
    }
    flush_images(&mut msgs, &mut pending_images);

    if opts.cache_control {
        // Mark the last 3 non-system messages so the next request reads the
        // prefix from cache even after new messages are appended.
        let mut marked = 0;
        for m in msgs.iter_mut().skip(1).rev() {
            if marked >= 3 {
                break;
            }
            if add_cache_breakpoint(m) {
                marked += 1;
            }
        }
    }
    msgs
}

fn add_cache_breakpoint(m: &mut Value) -> bool {
    let Some(obj) = m.as_object_mut() else {
        return false;
    };
    match obj.get_mut("content") {
        Some(Value::String(s)) if !s.is_empty() => {
            let text = std::mem::take(s);
            obj.insert(
                "content".into(),
                json!([{"type": "text", "text": text, "cache_control": cc()}]),
            );
            true
        }
        Some(Value::Array(parts)) => {
            if let Some(last) = parts
                .iter_mut()
                .rev()
                .find(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
            {
                last.as_object_mut()
                    .unwrap()
                    .insert("cache_control".into(), cc());
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Ensure every assistant tool call has a matching tool result (e.g. after an
/// interruption or a crash) — providers reject dangling calls.
pub fn repair_dangling_calls(items: &mut Vec<Item>) {
    let mut i = 0;
    while i < items.len() {
        if let Item::Assistant { tool_calls, .. } = &items[i] {
            let ids: Vec<(String, String)> = tool_calls
                .iter()
                .map(|t| (t.id.clone(), t.name.clone()))
                .collect();
            let mut j = i + 1;
            let mut have = vec![];
            while j < items.len() {
                if let Item::Tool { call_id, .. } = &items[j] {
                    have.push(call_id.clone());
                    j += 1;
                } else {
                    break;
                }
            }
            let mut insert_at = j;
            for (id, name) in ids {
                if !have.contains(&id) {
                    items.insert(
                        insert_at,
                        Item::Tool {
                            call_id: id,
                            name,
                            content: "Interrupted: the tool did not run to completion.".into(),
                            is_error: true,
                            images: vec![],
                            pruned: false,
                        },
                    );
                    insert_at += 1;
                }
            }
            i = insert_at;
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: "{\"path\":\"x\"}".into(),
        }
    }

    #[test]
    fn wire_with_cache_and_images() {
        let items = vec![
            Item::user("hi"),
            Item::Assistant {
                text: String::new(),
                reasoning: String::new(),
                reasoning_details: vec![json!({"type":"reasoning.text","text":"t"})],
                tool_calls: vec![tc("1")],
                model: String::new(),
            },
            Item::Tool {
                call_id: "1".into(),
                name: "read".into(),
                content: "img".into(),
                is_error: false,
                images: vec![Image {
                    mime: "image/png".into(),
                    data: "AAA".into(),
                    label: "a.png".into(),
                }],
                pruned: false,
            },
        ];
        let w = to_wire(
            "sys",
            &items,
            &WireOptions {
                cache_control: true,
                vision: true,
                include_reasoning: true,
            },
        );
        assert_eq!(w.len(), 5); // system, user, assistant, tool, image-user
        assert!(w[0]["content"][0]["cache_control"].is_object());
        assert!(w[2]["content"].is_null());
        assert_eq!(w[2]["reasoning_details"][0]["text"], "t");
        assert_eq!(w[3]["role"], "tool");
        assert_eq!(w[4]["role"], "user");
        assert!(
            w[4]["content"][1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png")
        );
        // breakpoints on last messages
        assert!(w[4]["content"][0]["cache_control"].is_object());
        assert!(w[3]["content"][0]["cache_control"].is_object());
    }

    #[test]
    fn repairs_dangling() {
        let mut items = vec![
            Item::user("hi"),
            Item::Assistant {
                text: String::new(),
                reasoning: String::new(),
                reasoning_details: vec![],
                tool_calls: vec![tc("1"), tc("2")],
                model: String::new(),
            },
            Item::Tool {
                call_id: "1".into(),
                name: "read".into(),
                content: "ok".into(),
                is_error: false,
                images: vec![],
                pruned: false,
            },
            Item::user("next"),
        ];
        repair_dangling_calls(&mut items);
        assert_eq!(items.len(), 5);
        assert!(matches!(&items[3], Item::Tool { call_id, is_error: true, .. } if call_id == "2"));
    }
}
