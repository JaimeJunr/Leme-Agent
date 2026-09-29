//! End-to-end tests of the agent loop against a scripted mock OpenRouter.

use crate::agent::UserInput;
use crate::agent::events::StopReason;
use crate::conversation::Item;
use crate::testutil::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn tool_msgs(req: &Value) -> Vec<Value> {
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .cloned()
        .collect()
}

async fn run(agent: &mut crate::agent::Agent, prompt: &str) -> StopReason {
    agent
        .run_turn(UserInput::from(prompt), CancellationToken::new())
        .await
}

#[tokio::test]
async fn write_file_then_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(vec![
        Resp::Sse(vec![
            json!({"choices": [{"delta": {"reasoning": "plan", "reasoning_details": [{"type": "reasoning.text", "text": "plan", "index": 0, "signature": "s1"}]}}]}),
            call("c1", "write", json!({"path": "hello.txt", "content": "hi there\n"})),
            finish("tool_calls", 1200),
        ]),
        say("Created hello.txt."),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "accept-edits", |_| {}).await;
    let r = run(&mut b.agent, "create hello.txt").await;
    assert_eq!(r, StopReason::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hi there\n"
    );
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    // Tool definitions and usage accounting are requested.
    assert!(
        reqs[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "edit")
    );
    assert_eq!(reqs[0]["usage"]["include"], true);
    // Second request carries the tool result and preserved reasoning.
    let tm = tool_msgs(&reqs[1]);
    assert_eq!(tm.len(), 1);
    assert_eq!(tm[0]["tool_call_id"], "c1");
    let asst = reqs[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap()
        .clone();
    assert_eq!(asst["reasoning_details"][0]["signature"], "s1");
    assert_eq!(asst["tool_calls"][0]["function"]["name"], "write");
    // Usage accumulated.
    assert!(b.agent.usage.cost > 0.0);
    // Checkpoint recorded for undo.
    assert_eq!(b.agent.shared.checkpoints.lock().entries.len(), 1);
    let (text, restored) = b.agent.rewind(0, true).unwrap();
    assert_eq!(text.as_deref(), Some("create hello.txt"));
    assert_eq!(restored.len(), 1);
    assert!(
        !dir.path().join("hello.txt").exists(),
        "undo removes the created file"
    );
}

#[tokio::test]
async fn permission_required_when_non_interactive() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(vec![
        tool("c1", "write", json!({"path": "x.txt", "content": "x"})),
        say("ok"),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "default", |_| {}).await;
    assert_eq!(run(&mut b.agent, "write x").await, StopReason::Done);
    assert!(!dir.path().join("x.txt").exists());
    let tm = tool_msgs(&mock.requests()[1]);
    assert!(
        tm[0]["content"]
            .as_str()
            .unwrap()
            .contains("Permission required")
    );
}

#[tokio::test]
async fn parallel_reads_and_persistent_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    let mock = Mock::start(vec![
        Resp::Sse(vec![
            calls(&[
                ("r1", "read", json!({"path": "a.txt"})),
                ("g1", "grep", json!({"pattern": "beta"})),
            ]),
            finish("tool_calls", 1000),
        ]),
        tool("b1", "bash", json!({"command": "cd sub && pwd"})),
        tool("b2", "bash", json!({"command": "pwd"})),
        say("done"),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "yolo", |_| {}).await;
    assert_eq!(run(&mut b.agent, "look").await, StopReason::Done);
    let reqs = mock.requests();
    let tm = tool_msgs(&reqs[1]);
    assert_eq!(tm.len(), 2);
    assert!(tm[0]["content"].as_str().unwrap().contains("     1\talpha"));
    assert!(tm[1]["content"].as_str().unwrap().contains("a.txt"));
    let last = tool_msgs(&reqs[3]);
    assert!(
        last.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .trim_end()
            .ends_with("/sub"),
        "{:?}",
        last
    );
}

#[tokio::test]
async fn fuzzy_edit_applies() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("m.py"), "def f():\n    return 1\n").unwrap();
    let mock = Mock::start(vec![
        tool("e1", "edit", json!({"path": "m.py", "old_string": "def f():\n  return 1", "new_string": "def f():\n  return 2"})),
        say("edited"),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "accept-edits", |_| {}).await;
    assert_eq!(run(&mut b.agent, "change").await, StopReason::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("m.py")).unwrap(),
        "def f():\n    return 2\n"
    );
}

#[tokio::test]
async fn retries_transient_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(vec![Resp::Status(503, "overloaded".into()), say("hello")]).await;
    let mut b = boot(dir.path(), &mock, "default", |_| {}).await;
    assert_eq!(run(&mut b.agent, "hi").await, StopReason::Done);
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn non_retryable_error_stops() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(vec![Resp::Status(401, "invalid key".into())]).await;
    let mut b = boot(dir.path(), &mock, "default", |_| {}).await;
    match run(&mut b.agent, "hi").await {
        StopReason::Error(e) => assert!(e.contains("invalid key"), "{e}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn verify_failures_are_fed_back() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(vec![
        tool("w1", "write", json!({"path": "a.txt", "content": "a"})),
        say("done"),
        tool("w2", "write", json!({"path": "ok.txt", "content": "ok"})),
        say("fixed"),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "accept-edits", |c| {
        c.verify = vec!["test -f ok.txt".into()]
    })
    .await;
    assert_eq!(run(&mut b.agent, "go").await, StopReason::Done);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4);
    let third = reqs[2]["messages"].to_string();
    assert!(third.contains("Automatic verification failed"));
}

#[tokio::test]
async fn loop_detection_stops_runaway() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), "same").unwrap();
    let mut script = vec![];
    for i in 0..8 {
        script.push(tool(&format!("c{i}"), "read", json!({"path": "x"})));
    }
    let mock = Mock::start(script).await;
    let mut b = boot(dir.path(), &mock, "default", |_| {}).await;
    match run(&mut b.agent, "loop").await {
        StopReason::Error(e) => assert!(e.contains("loop")),
        other => panic!("{other:?}"),
    }
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 5);
    assert!(
        reqs[3]["messages"]
            .to_string()
            .contains("identical results")
    );
}

#[tokio::test]
async fn auto_compaction_replaces_history() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.txt"), "x".repeat(2000)).unwrap();
    let mock = Mock::start(vec![
        Resp::Sse(vec![call("r1", "read", json!({"path": "big.txt"})), finish("tool_calls", 19_500)]),
        say("## 1. User requests\nsummarized everything about the big file and the plan to continue working on it in detail"),
        say("all good"),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "default", |c| c.context_limit = 24_000).await;
    assert_eq!(run(&mut b.agent, "read big").await, StopReason::Done);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    assert!(
        reqs[1]["messages"]
            .to_string()
            .contains("CONTEXT CHECKPOINT")
    );
    match &b.agent.items[0] {
        Item::User {
            content,
            synthetic: true,
            ..
        } => assert!(content.contains("summarized everything")),
        other => panic!("{other:?}"),
    }
    let replay = b.agent.shared.session().replay().unwrap();
    assert_eq!(replay.items.len(), b.agent.items.len());
}

#[tokio::test]
async fn subagent_report_returns_to_parent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lib.rs"), "pub fn secret() {}\n").unwrap();
    let mock = Mock::start(vec![
        tool("t1", "task", json!({"description": "find secret", "prompt": "where is secret defined?", "agent": "explore"})),
        tool("s1", "grep", json!({"pattern": "fn secret"})),
        say("secret is defined in lib.rs:1"),
        say("It is in lib.rs."),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "default", |_| {}).await;
    assert_eq!(run(&mut b.agent, "where is secret").await, StopReason::Done);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4);
    // Subagent has its own context and no task tool.
    assert!(
        !reqs[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "task")
    );
    assert!(
        reqs[1]["messages"][0]["content"]
            .to_string()
            .contains("Your role: explore")
    );
    let tm = tool_msgs(&reqs[3]);
    assert!(
        tm[0]["content"]
            .as_str()
            .unwrap()
            .contains("secret is defined in lib.rs:1")
    );
}

#[tokio::test]
async fn openai_models_get_apply_patch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    let patch = "*** Begin Patch\n*** Update File: a.txt\n one\n-two\n+TWO\n*** End Patch";
    let mock = Mock::start(vec![
        tool("p1", "apply_patch", json!({"patch": patch})),
        say("patched"),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "accept-edits", |c| {
        c.model = "openai/gpt-5.6-sol".into()
    })
    .await;
    assert_eq!(run(&mut b.agent, "patch it").await, StopReason::Done);
    let reqs = mock.requests();
    let names: Vec<String> = reqs[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"apply_patch".to_string()));
    assert!(!names.contains(&"edit".to_string()));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\nTWO\n"
    );
}

#[tokio::test]
async fn anthropic_style_models_get_cache_breakpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(vec![say("hi")]).await;
    let mut b = boot(dir.path(), &mock, "default", |c| {
        c.model = "anthropic/claude-test".into()
    })
    .await;
    assert_eq!(run(&mut b.agent, "hello").await, StopReason::Done);
    let req = &mock.requests()[0];
    assert!(req["messages"][0]["content"][0]["cache_control"].is_object());
    assert!(req["session_id"].as_str().is_some());
}

#[tokio::test]
async fn plan_mode_blocks_edits() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(vec![
        tool("b1", "bash", json!({"command": "touch nope"})),
        say("ok"),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "plan", |_| {}).await;
    assert_eq!(run(&mut b.agent, "plan it").await, StopReason::Done);
    assert!(!dir.path().join("nope").exists());
    let reqs = mock.requests();
    let names: Vec<String> = reqs[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();
    assert!(!names.contains(&"edit".to_string()));
    assert!(
        reqs[0]["messages"][0]["content"]
            .to_string()
            .contains("PLAN MODE")
    );
    assert!(
        tool_msgs(&reqs[1])[0]["content"]
            .as_str()
            .unwrap()
            .contains("plan mode")
    );
}

#[tokio::test]
async fn headless_json_output() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(vec![say("42")]).await;
    let b = boot(dir.path(), &mock, "default", |_| {}).await;
    let code = crate::headless::run(
        b.agent,
        b.rx,
        "answer".into(),
        crate::headless::Format::Json,
        false,
    )
    .await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn steer_messages_are_injected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "x").unwrap();
    let mock = Mock::start(vec![tool("r1", "read", json!({"path": "f"})), say("ok")]).await;
    let mut b = boot(dir.path(), &mock, "default", |_| {}).await;
    b.agent.steer.lock().push("also check tests".into());
    assert_eq!(run(&mut b.agent, "go").await, StopReason::Done);
    assert!(
        mock.requests()[0]["messages"]
            .to_string()
            .contains("also check tests")
    );
}

#[tokio::test]
async fn rewind_after_compaction_only_reverts_that_turn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "v0").unwrap();
    let mock = Mock::start(vec![
        // turn 0: edit a.txt v0 -> v1
        tool("e1", "edit", json!({"path": "a.txt", "old_string": "v0", "new_string": "v1"})),
        say("ok"),
        // turn 1: edit a.txt v1 -> v2
        tool("e2", "edit", json!({"path": "a.txt", "old_string": "v1", "new_string": "v2"})),
        say("ok"),
        // manual compaction summary
        say("## 1. User requests\nTwo edits of a.txt were requested and done; nothing else is pending right now."),
        // turn 2 (after compaction): v2 -> v3
        tool("e3", "edit", json!({"path": "a.txt", "old_string": "v2", "new_string": "v3"})),
        say("ok"),
    ])
    .await;
    let mut b = boot(dir.path(), &mock, "accept-edits", |_| {}).await;
    run(&mut b.agent, "first").await;
    run(&mut b.agent, "second").await;
    b.agent
        .compact(None, &CancellationToken::new())
        .await
        .unwrap();
    run(&mut b.agent, "third").await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "v3"
    );
    // The third message has turn id 2 even though history was compacted.
    let t = b.agent.items.iter().rev().find_map(|i| i.turn()).unwrap();
    assert_eq!(t, 2);
    b.agent.rewind(t, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "v2",
        "only the last turn is reverted"
    );
    // Replay reproduces turn ids for resumed sessions.
    let rep = b.agent.shared.session().replay().unwrap();
    assert_eq!(crate::session::next_turn_id(&rep), 3);
}
