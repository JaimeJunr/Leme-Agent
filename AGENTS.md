# harness — agent instructions

Native Rust coding-agent CLI for OpenRouter. Single binary crate (`src/main.rs`), edition 2024.

## Commands
- Build: `cargo build` (release: `cargo build --release`)
- Test everything: `cargo test` — unit tests live next to the code; end-to-end agent tests are in `src/e2e_tests.rs` and run against the scripted mock server in `src/testutil.rs` (no network, no API key).
- Single test: `cargo test <name>` (e.g. `cargo test e2e_tests::verify_failures_are_fed_back`)
- Lint/format: `cargo clippy` (must be warning-free) and `cargo fmt`

## Architecture
- `agent/mod.rs` is the loop: build request → stream → run tools (parallel for `ToolKind::parallel_safe`) → repeat. It emits `AgentEvent`s through `EventSink`; it never touches the terminal.
- Front-ends consume events: `ui/` (inline TUI) and `headless.rs`. Keep new features front-end agnostic: add an event, handle it in both.
- `agent/shared.rs::Shared` is state shared with tools and subagents (config, client, catalog, file tracker, checkpoints, todos, jobs, session, mode).
- Tools implement `tools::Tool` (`name/description/schema/kind/summarize/preview/run`) and are registered in `tools::builtin`. Tool descriptions are prompts: keep them precise.
- Conversation state is provider-neutral (`conversation::Item`); `conversation::to_wire` produces OpenRouter messages, including cache breakpoints and `reasoning_details` round-tripping.
- Sessions are append-only JSONL (`session::Record`); anything that changes history must append a record (`Item`, `Compaction`, `Truncate`, …) so replay stays exact.
- The system prompt must stay byte-stable within a session (prompt caching): put dynamic data in messages, and call `Agent::invalidate_system_prompt` only on model/mode changes.

## Conventions
- Errors to the model are `ToolOutput::err` with an actionable message (what went wrong and how to fix it); never panic in tools.
- Never block the async runtime: filesystem walks and searches go through `spawn_blocking`.
- Don't hold `parking_lot` guards across `.await` (the agent future must be `Send`).
- UI text goes through `ui::style::Line`; never print directly to stdout while the TUI is active.
- New behavior needs a test; agent-loop behavior gets an e2e test with a scripted `Mock`.
