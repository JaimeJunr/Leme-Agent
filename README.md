# Leme

**A fast, native coding agent for your terminal — powered by any model on OpenRouter.**

**Leme** (Portuguese for *helm*) is a single ~13 MB Rust binary (no Node, no Python) that starts in ~3 ms and runs in ~10 MB of RAM.
It reads, searches and edits your code, runs commands, verifies its work, and keeps going until the task is done —
with Claude, GPT, Gemini, DeepSeek, Grok, Qwen, Kimi, GLM or any of the 400+ models on [OpenRouter](https://openrouter.ai/models),
all with one API key.

```
◆ leme v0.1.0  anthropic/claude-sonnet-5.5
  ~/code/api · AGENTS.md · 12 MCP tools

› add rate limiting to the /login endpoint

✻ Thought for 3.1s
● Todo 4 items · 0/4 done
● Grep "fn login" · 3 matches in 2 files
● Read src/routes/auth.rs · 212 lines
● Edit src/routes/auth.rs · +18 -2
    @@ -40,6 +40,22 @@
    +    let key = format!("login:{}", addr.ip());
    ...
● Bash cargo test auth · ok · 8.2s
    test result: ok. 14 passed; 0 failed
────────────────────────────────────────────────────────────────
› ▌
  ⏵⏵ accept edits · claude-sonnet-5.5 · ctx 9% · $0.041 · cache 91%
```

## Why Leme

Most coding agents are either locked to one vendor, or model-agnostic but generic. `leme` is built *for* OpenRouter
and squeezes the most out of it, while matching the workflow features of the best agents:

**Native and fast**
- Rust, single static-ish binary, instant startup, tiny memory footprint — run dozens of sessions side by side.
- Inline terminal UI: finished output goes to your terminal's **native scrollback** (scroll, search, select and copy just work);
  only a small live region is redrawn, using synchronized updates — no full-screen takeover, no flicker.
- In-process ripgrep engine for `grep`/`glob` (no subprocess per search), parallel execution of independent read-only tools.

**Built for OpenRouter**
- **Every model, one key.** Live catalog with context sizes, prices and capabilities; fuzzy `/model` picker; switch mid-session.
- **Multi-model by design.** A *main* model does the work, a cheap *small* model handles titles, web-page digests and the
  `explore` subagent, and an *oracle* model (a different, stronger reasoner) is one `consult` tool call away for second opinions.
- **Prompt caching done right.** Automatic `cache_control` breakpoints for Anthropic/Gemini/Qwen, a byte-stable system prompt,
  and `session_id` sticky routing so the provider cache stays warm across the whole session.
- **Reasoning that survives tool calls.** `reasoning_details` (incl. signatures/encrypted blocks) are round-tripped exactly,
  so Claude/GPT/Gemini keep their chain of thought across tool use. Per-model effort mapping (`/effort`).
- **Cost control.** Live cost and cache-hit % in the status line, `/cost` with key credits, `max_cost` budget that stops a run.
- **Web search without another API key** (OpenRouter web plugin), plus `web_fetch` with HTML→Markdown and small-model digests.
- **Resilient.** Exponential backoff with jitter, `Retry-After`, mid-stream failure recovery, OpenRouter `models` fallbacks,
  lenient repair of malformed tool-call JSON from weaker models, empty-response and cut-off (`finish_reason=length`) recovery.

**Gets things right**
- **Tolerant, safe edits.** Exact `old_string` matching with fallbacks for whitespace drift, wrong indentation (re-indents your
  replacement to the file's style), CRLF files and over-escaped strings — but never silently picks one of several matches.
  Misses return the closest region so the model self-corrects in one step. `apply_patch` format automatically for OpenAI models.
- **Guards:** refuses to overwrite files it hasn't read, detects files changed on disk behind its back, surfaces nested
  `AGENTS.md` files when it touches a directory.
- **Verify loop.** Configure `verify = ["cargo check"]` and every turn that changed files is checked automatically; failures are
  fed back until they pass (bounded).
- **Loop detection** stops runaway identical tool calls; **steering** lets you type while it works — messages land at its next step.
- **Context management that doesn't forget.** Stale tool output is pruned first (only when it pays for the cache miss); then an
  in-context summary (cache-hitting) that keeps every user instruction verbatim, the task list and the files in play.

**Safe autonomy**
- Permission modes (Shift+Tab): `default` → `accept-edits` → `auto` → `plan`, plus `yolo`.
- `auto` runs shell commands in an **OS sandbox** (Linux Landlock / macOS Seatbelt): write access only to the workspace, temp dirs
  and tool caches — and destructive commands (`git push`, `rm -rf ~`, `curl | sh`, `sudo`…) still ask.
- A shell parser classifies read-only commands (`ls`, `git status/diff/log`, `rg`, `cargo metadata`…) so they never prompt.
- Per-project "always allow" rules (`bash(cargo test*)`, `edit(src/**)`, `mcp__github__*`), deny rules, ask rules.
- **Checkpoints & rewind:** every file the agent touches is snapshotted per turn (content-addressed, no git needed).
  `Esc Esc` or `/rewind` restores files *and* conversation to any earlier message; `/undo` reverts the last turn.

**Extensible and compatible**
- `AGENTS.md` / `CLAUDE.md` / `LEME.md` (global, project, nested; `@imports`), skills (`SKILL.md`), custom slash commands,
  subagent definitions, hooks, MCP servers (stdio + streamable HTTP) — reading the same `.claude/` layout and `.mcp.json` you
  may already have.
- Headless mode for scripts/CI with `text`, `json` and `stream-json` output, exit codes, stdin piping.

## Install

From source (Rust ≥ 1.85):

```sh
cargo install --git https://github.com/JaimeJunr/Leme-Agent
# or
git clone https://github.com/JaimeJunr/Leme-Agent && cd Leme-Agent && cargo build --release
./target/release/leme --version
```

The release workflow (`.github/workflows/release.yml`) builds Linux and macOS binaries for every `v*` tag.

## Quick start

```sh
export OPENROUTER_API_KEY=sk-or-...        # https://openrouter.ai/keys  (or: leme login)
cd your-project
leme                                     # interactive
leme "why does the login test fail?"     # start with a prompt
leme -c                                  # continue the last session
```

On first run without a key, `leme` asks for it and stores it in `~/.config/leme/config.toml` (mode 600).
Run `/init` in a new project to have the agent write an `AGENTS.md` describing how to build, test and work in it.

### In the prompt

| Input | Does |
|---|---|
| `@path/to/file` | attach a file (or image, or directory listing) — tab-completes |
| `!cmd` | run a shell command yourself; output goes into the conversation |
| `# note` | save a note to `AGENTS.md` (persistent project memory) |
| `/command` | slash commands (see below) |
| enter / shift+enter, alt+enter, ctrl+j, trailing `\` | send / newline |
| esc | interrupt the agent · **esc esc** rewind |
| shift+tab | cycle permission mode |
| ctrl+o | verbose (full reasoning, full diffs and outputs) |
| ↑ ↓ | history · ctrl+c clear / interrupt / exit |

Typing while the agent works queues the message; it's delivered at the agent's next step. Pasting a long block collapses it
into a placeholder; pasting an image path attaches the image.

### Slash commands

`/model` (fuzzy catalog picker) · `/effort` · `/mode` · `/plan` · `/compact [focus]` · `/rewind` · `/undo` · `/clear` ·
`/resume` · `/fork` · `/diff` · `/review` · `/init` · `/cost` · `/context` · `/todo` · `/export [file]` · `/copy` ·
`/tools` · `/mcp` · `/skills` · `/agents` · `/status` · `/config` · `/help` — plus your own commands.

## Tools

| Tool | |
|---|---|
| `read` | files with line numbers, paging, images; suggests paths on typos |
| `edit`, `multi_edit`, `write` | tolerant string replacement (atomic multi-edit), diffs, checkpoints |
| `apply_patch` | envelope patch format (auto-selected for OpenAI models) |
| `grep`, `glob`, `ls` | in-process ripgrep engine, `.gitignore`-aware, sorted by recency |
| `bash`, `jobs` | persistent cwd, live output, timeouts, process-group kill, head+tail truncation with full log on disk, background jobs |
| `web_search`, `web_fetch` | OpenRouter web plugin; HTML→Markdown with optional small-model digest |
| `todo` | task list shown live in the UI and preserved across compaction |
| `task` | subagents with fresh context (built-in `explore` on the cheap model, `general`, or your own); run in parallel |
| `consult` | ask the oracle model for a second opinion, with files attached |
| `skill` | load a skill's instructions on demand |
| `ask_user`, `propose_plan` | clarifying questions and plan approval |
| `mcp__server__tool` | anything your MCP servers expose |

## Configuration

Layers, lowest to highest priority: built-in defaults → `~/.config/leme/config.toml` → `<project>/.leme/config.toml`
→ `<project>/.leme/local.toml` (personal; "always allow" answers are saved here) → environment → CLI flags.
`leme config init` writes a commented template.

```toml
model        = "anthropic/claude-sonnet-5.5"
small_model  = "google/gemini-3.5-flash-lite"   # titles, web digests, explore subagent
oracle_model = "openai/gpt-5.6-sol"            # the `consult` tool
effort       = "high"                           # reasoning effort (mapped per model)
mode         = "default"                        # default | accept-edits | auto | yolo | plan
verify       = ["cargo check --quiet", "cargo test --quiet"]
max_cost     = 5.0                              # stop a session at $5
context_limit = 400000                          # cap the usable window (0 = model's full window)
fallback_models = ["openai/gpt-5.6-terra"]      # OpenRouter-side failover

[provider]                                      # OpenRouter provider routing, passed through
sort = "throughput"
data_collection = "deny"

[permissions]
allow = ["bash(cargo test*)", "bash(npm run *)", "web_fetch(docs.rs)"]
deny  = ["read(*.env*)", "bash(git push*)"]

[[hooks]]                                       # session_start | user_prompt | pre_tool | post_tool | stop
event   = "post_tool"
matcher = "edit|multi_edit|write|apply_patch"
command = "cargo fmt"

[mcp.github]                                    # stdio; use `url` + `headers` for HTTP servers
command = "github-mcp-server"
args    = ["stdio"]
env     = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
```

Hooks receive a JSON payload on stdin. Exit code 2 blocks (a `pre_tool` hook denies the call, a `stop` hook sends the agent
back to work with your message); stdout of `session_start` / `user_prompt` hooks is added as context.

### Instructions, skills, commands, subagents

| What | Where (first match wins) |
|---|---|
| Instructions | `~/.config/leme/AGENTS.md`, then `AGENTS.md` / `LEME.md` / `CLAUDE.md` (+ `*.local.md`) from the project root down to the cwd; nested ones load when the agent touches that directory |
| Skills | `.leme/skills/<name>/SKILL.md`, `.claude/skills/…`, `.agents/skills/…`, `~/.config/leme/skills/…`, `~/.claude/skills/…` |
| Commands | `.leme/commands/*.md` (`$ARGUMENTS`, `$1…$9`, `` !`shell` `` expansion, `model:` frontmatter), `.claude/commands/…` |
| Subagents | `.leme/agents/*.md` with `name`, `description`, `tools`, `model` (`small`/`main`/`oracle`/any id) frontmatter, `.claude/agents/…` |
| MCP | `[mcp.*]` in config, or `.mcp.json` in the project root |

## Headless / CI

```sh
leme -p "fix the lint errors" --mode auto --verify "npm run lint" --max-cost 1
git diff | leme -p "review this diff" --output-format json | jq -r .result
leme -p "…" --output-format stream-json      # one JSON event per line (text, tool_start, tool_end + diffs, usage, …)
```

Exit codes: `0` done, `1` error, `2` budget/step limit, `130` interrupted. Without a TTY, permission prompts become
denials the model is told about — grant what a job needs with `--mode`, `--allow` rules or config.

## Sessions

Sessions are append-only JSONL files under `~/.local/share/leme/sessions/<project>/` with file checkpoints stored
content-addressed next to them. `leme -c` continues the latest, `leme -r` opens a picker, `leme sessions` lists them,
`/fork` branches a conversation, `/export` writes Markdown.

## Architecture

```
src/
  main.rs            CLI (clap), login, models, sessions, config
  app.rs             bootstrap: config layers, catalog, MCP, session, agent
  llm/               OpenRouter client: SSE streaming, retries, reasoning_details merge, catalog + pricing
  conversation.rs    provider-neutral items → wire format, cache breakpoints, dangling-call repair
  agent/             loop, events, shared state (file tracker, checkpoints), prompt, compaction
  tools/             read/edit/write/patch/grep/glob/ls/bash/jobs/web/todo/task/consult/skill/ask/plan
  permissions.rs     modes, rules, shell command classifier, destructive-command detection
  sandbox.rs         Landlock (Linux, via a re-exec helper) and Seatbelt (macOS)
  mcp.rs             MCP client (stdio + streamable HTTP)
  hooks.rs, instructions.rs, extensions.rs, session.rs
  ui/                inline renderer, streaming Markdown, composer, slash commands
  headless.rs        -p / json / stream-json
```

The agent core emits events over a channel and never touches the terminal, so the same core drives the TUI, headless mode
and (next) other front-ends such as the Agent Client Protocol.

## Development

```sh
cargo test            # unit tests + end-to-end tests against a scripted mock OpenRouter server
cargo clippy
cargo run -- --help
```
