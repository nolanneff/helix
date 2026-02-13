# AI Features — hxtuck

This document covers the AI-assisted editing features in the hxtuck fork of Helix.
It describes the architecture, configuration, commands, and known build
requirements.

> **Note:** Some sections (runtime bake-in, terminal drain fix) document
> temporary workarounds that may be removed in the future.

---

## Table of Contents

- [Build Notes](#build-notes)
- [Configuration](#configuration)
- [Commands & Keybindings](#commands--keybindings)
- [Architecture Overview](#architecture-overview)
- [Request Lifecycle](#request-lifecycle)
- [Providers](#providers)
- [Inline Progress Display](#inline-progress-display)
- [Statusline Integration](#statusline-integration)
- [Temporary Changes](#temporary-changes)

---

## Build Notes

### Binary Name

The fork builds as `hxtuck` to avoid conflicting with a system-installed `hx`.
This is set in `helix-term/Cargo.toml`:

```toml
[[bin]]
name = "hxtuck"
```

### Runtime Directory Bake-In

When building with `cargo install --path helix-term`, the build script
(`helix-loader/build.rs`) automatically bakes in `HELIX_DEFAULT_RUNTIME`
pointing to the workspace `runtime/` directory. This removes the need to
manually set the environment variable or symlink the runtime after install.

```rust
// helix-loader/build.rs
if std::env::var("HELIX_DEFAULT_RUNTIME").is_err() {
    let workspace_runtime = Path::new(&manifest_dir)
        .parent().unwrap().join("runtime");
    if workspace_runtime.exists() {
        println!("cargo:rustc-env=HELIX_DEFAULT_RUNTIME={}", ...);
    }
}
```

The runtime directory resolution priority is:

1. `CARGO_MANIFEST_DIR/runtime` (development / `cargo run`)
2. User config dir (`~/.config/helix/runtime`)
3. `HELIX_RUNTIME` environment variable
4. `HELIX_DEFAULT_RUNTIME` (build-time, set by the build script above)
5. Subdirectory of the hxtuck executable

> **May be removed later.** This is a convenience for the fork; upstream Helix
> expects users to manage runtime paths themselves.

### Terminal Drain on Exit

The TUI backend (`helix-tui/src/backend/termina.rs`) drains stale
escape-sequence responses from the input buffer before switching back to cooked
mode. Without this, late-arriving terminal capability responses can leak as
visible garbage after exit.

This drain runs in both `restore()` and the `Drop` impl:

```rust
while self.terminal.poll(Event::is_escape, Some(Duration::ZERO))? {
    let _ = self.terminal.read(Event::is_escape)?;
}
```

> **May be removed later.** This works around a timing issue specific to certain
> terminal emulators. If upstream addresses it, this patch can be dropped.

---

## Configuration

AI settings live in a separate file: `~/.config/helix/ai-config.toml`

This file is loaded after the main `config.toml` and overlays the `[editor.ai]`
section. The separation keeps AI-specific settings out of the main config.

### Loading

```rust
// helix-term/src/config.rs
if let Ok(ai_toml) = fs::read_to_string(helix_loader::ai_config_file()) {
    if let Ok(ai_file) = toml::from_str::<AiConfigFile>(&ai_toml) {
        config.editor.ai = ai_file.editor.ai;
    }
}
```

### Full Config Reference

```toml
[editor.ai]

# Enable AI features (required)
enable = true

# Provider: "claude", "opencode", or "custom"
provider = "claude"

# Model name passed to the provider CLI
model = "sonnet"

# Model for AI search (empty = use main model)
search-model = ""

# Maximum concurrent AI requests (0 = unlimited)
max-concurrent = 5

# Return to normal mode after launching an AI request
return-to-normal = true

# Context file names to auto-discover (walked up from file dir to workspace root)
context-files = ["AGENT.md"]

# Selection line threshold for flipping progress below the selection.
# When a selection exceeds this many lines AND starts in the top half of the
# viewport, progress virtual lines appear below instead of above.
# Set to 0 to disable (always show above).
progress-flip-threshold = 40

# Custom command template (only used when provider = "custom").
# Use {model} and {prompt} as placeholders.
custom-command = []
```

### Defaults

| Field | Default | Description |
|-------|---------|-------------|
| `enable` | `false` | Must be explicitly enabled |
| `provider` | `"claude"` | Claude CLI |
| `model` | `"sonnet"` | Model name |
| `search-model` | `""` | Falls back to `model` |
| `max-concurrent` | `5` | Concurrent request limit |
| `return-to-normal` | `true` | Switch to normal mode on launch |
| `context-files` | `["AGENT.md"]` | Auto-discovered context |
| `progress-flip-threshold` | `40` | Lines before flip (0 = disabled) |
| `custom-command` | `[]` | Template for custom provider |

### Minimal Example

```toml
[editor.ai]
enable = true
provider = "claude"
model = "opus"
```

---

## Commands & Keybindings

### Keybindings (Space Menu)

| Key | Command | Description |
|-----|---------|-------------|
| `Space A` | `ai_replace_selection` | AI replace — opens prompt UI for instructions |
| `Space X` | `ai_explain_selection` | AI explain — launches immediately, no prompt |

### Typed Commands

All typed commands support inline arguments. If arguments are provided, the
request launches directly. If no arguments are given, a multi-line prompt UI
opens.

| Command | Aliases | Description |
|---------|---------|-------------|
| `:ai-replace <instructions>` | `:ai` | Replace selection with AI-generated code |
| `:ai-explain <question>` | `:aie` | Explain selected code (opens split pane) |
| `:ai-search <query>` | — | Search project for code matching a description |
| `:ai-cancel [index]` | — | Cancel a request (1 = most recent, default) |
| `:ai-cancel-all` | — | Cancel all active requests |
| `:ai-results` | — | Reopen the search results picker |

### Examples

```
:ai add error handling for the network call
:aie why does this use recursion instead of iteration?
:ai-search find all places where authentication is bypassed
:ai-cancel
```

---

## Architecture Overview

The AI system spans four crates:

```
helix-view          helix-term
┌──────────────┐    ┌───────────────────────────────┐
│ AiConfig     │    │ commands/ai.rs                │
│ AiProvider   │    │   - Entry points              │
│ AiRequestState│   │   - Prompt building           │
│              │    │   - CLI spawning               │
│ annotations/ │    │   - Stream processing          │
│  ai_progress │    │   - Result application         │
│  (space      │    │                               │
│   reservation)│   │ ui/ai_prompt.rs               │
└──────────────┘    │   - Multi-line input popup     │
                    │                               │
                    │ ui/text_decorations/           │
                    │  ai_progress.rs               │
                    │   - Spinner + thinking render  │
                    │                               │
                    │ ui/statusline.rs              │
                    │   - Statusline spinner         │
                    │                               │
                    │ commands/typed.rs             │
                    │   - :ai-replace, :aie, etc.   │
                    │                               │
                    │ keymap/default.rs             │
                    │   - Space A, Space X           │
                    └───────────────────────────────┘
```

### Key Files

| File | Purpose |
|------|---------|
| `helix-view/src/editor.rs` | `AiConfig`, `AiProvider`, config defaults |
| `helix-view/src/ai.rs` | `AiRequestState` — tracks a live request |
| `helix-view/src/annotations/ai_progress.rs` | Virtual line space reservation |
| `helix-term/src/commands/ai.rs` | All AI logic (prompts, CLI, streaming, results) |
| `helix-term/src/ui/ai_prompt.rs` | Multi-line prompt popup component |
| `helix-term/src/ui/text_decorations/ai_progress.rs` | Inline spinner/thinking/tool rendering |
| `helix-term/src/ui/statusline.rs` | `render_ai_spinner()` |
| `helix-term/src/ui/editor.rs` | Wires annotations + decorations into render |
| `helix-term/src/config.rs` | Loads `ai-config.toml` |

---

## Request Lifecycle

```
1. User triggers command (keybinding or typed)
     ↓
2. Capture selection, file contents, context files
     ↓
3. Show prompt UI (if no inline args) or launch directly
     ↓
4. return_to_normal → switch to Normal mode
     ↓
5. Build provider-specific CLI command
     ↓
6. Push AiRequestState onto editor.ai_requests
     ↓
7. Spawn async task:
   a. Spawn CLI subprocess (setsid on Unix for process group isolation)
   b. Pipe prompt via stdin (Claude) or pass as arg (OpenCode/Custom)
   c. Read stdout line-by-line as NDJSON
   d. Parse stream events → update streaming_text & tool_display buffers
   e. Spawn 80ms ticker task → requests editor redraws for animation
     ↓
8. During processing:
   - Virtual lines show spinner + thinking text + tool calls
   - Statusline shows spinner with latest thinking/tool snippet
     ↓
9. On completion:
   - Replace: Read temp file, verify content hasn't changed (staleness),
              apply Transaction, append to undo history
   - Explain: Create new Document, set markdown highlighting,
              open in vertical split, mark as unmodified
   - Search:  Parse result lines, store in editor.ai_search_results,
              jump to first match
     ↓
10. Remove AiRequestState, stop ticker
```

### Staleness Protection (Replace)

Before applying a replacement, the system verifies the document content at the
selection range hasn't changed since the request started. This prevents stale
replacements when LSP formatting, other AI requests, or manual edits modify the
buffer during processing.

### Concurrent Requests

Multiple AI requests can run simultaneously (up to `max-concurrent`). Each
request has independent:
- Streaming text buffer (`Arc<Mutex<String>>`)
- Tool display buffer (`Arc<Mutex<String>>`)
- Cancellation channel (`oneshot::Sender`)
- Ticker task (for spinner animation)

The statusline shows the most recent request's status. Virtual line annotations
stack independently per request.

---

## Providers

### Claude (default)

```
claude --model <model> --output-format stream-json --verbose \
       --include-partial-messages \
       --allowedTools Write,Edit,Read,Bash,WebSearch,WebFetch -p
```

- Prompt piped via **stdin** to avoid OS `ARG_MAX` limits on large selections
- Stream format: NDJSON with `stream_event` wrapper containing content blocks
- Content block types: `thinking` (reasoning), `text` (output), `tool_use` (actions)
- Delta types: `thinking_delta`, `text_delta`, `input_json_delta`

### OpenCode

```
opencode run --format json --model <model> <prompt>
```

- Prompt passed as CLI argument
- Stream format: JSON objects with `type` field
- Types: `text`, `text_delta`, `tool_use`, `tool_call`, `tool_result`,
  `step_start`, `step_finish`

### Custom

Uses the `custom-command` config array with `{model}` and `{prompt}`
placeholder substitution:

```toml
custom-command = ["my-ai", "--model", "{model}", "--prompt", "{prompt}"]
```

---

## Inline Progress Display

Progress is shown as virtual lines anchored to the document, using Helix's
`LineAnnotation` / `Decoration` system.

### Two-Layer Architecture

1. **Annotation** (`helix-view/src/annotations/ai_progress.rs`):
   Reserves vertical space. Tells the document formatter how many virtual lines
   to insert after specific document lines. Does no rendering.

2. **Decoration** (`helix-term/src/ui/text_decorations/ai_progress.rs`):
   Renders content into the reserved virtual lines — spinners, thinking text,
   and tool call display.

### Layout

Spinners always appear on **both sides** of the selection (above and below).
Thinking text and tool call lines are placed on one side based on
`show_below`:

```
show_below = false (default):          show_below = true:

  ◠ Analyzing                            [selection line 1]
    thinking text line 1                  [selection line 2]
    thinking text line 2                  ...
    → Read: src/main.rs                   [selection line N]
  [selection line 1]                      ◠ Analyzing
  [selection line 2]                        thinking text line 1
  ...                                       thinking text line 2
  [selection line N]                        → Read: src/main.rs
  ◠ Analyzing                             ◠ Analyzing
```

The flip triggers when:
- `progress-flip-threshold > 0`
- Selection line count >= threshold
- Selection start is in the top half of the viewport

### Spinner Animation

Two independent spinners:
- **Inline** (virtual lines): `◜ ◠ ◝ ◞ ◡ ◟` — 80ms interval
- **Statusline**: `⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏` — 120ms interval

Labels vary by request type:
- Replace: inline "Implementing", statusline "AI"
- Search: inline "Searching", statusline "AI Searching"
- Explain: inline "Analyzing", statusline "AI Analyzer"

---

## Statusline Integration

The AI spinner renders in the statusline when any request is active:

```
⠋ AI Analyzer: thinking snippet here → Read: src/file.rs
```

Format: `{frame} {label}: {thinking} {tool}`

When multiple requests are active: `AI[3]: ...` (shows count, displays most
recent request's status).

---

## Temporary Changes

These modifications are workarounds that may be reverted:

### 1. Runtime Bake-In (`helix-loader/build.rs`)

Automatically sets `HELIX_DEFAULT_RUNTIME` at build time so `cargo install`
works without manual runtime setup. This is a convenience for the fork and may
be removed if a better distribution method is adopted.

### 2. Terminal Drain (`helix-tui/src/backend/termina.rs`)

Drains stale escape-sequence responses from the terminal input buffer on exit.
Prevents visible garbage characters after quitting. Applied in both the
`restore()` method and the `Drop` implementation. May be removed if upstream
Helix or the termina backend addresses the timing issue.
