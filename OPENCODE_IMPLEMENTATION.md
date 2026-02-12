# OpenCode Provider Implementation Guide

## Goal
Update the `AiProvider::OpenCode` variant to work with the current [opencode-ai/opencode](https://github.com/opencode-ai/opencode) CLI tool, which has a different JSON streaming format than Claude Code.

## Background

The codebase already has an `AiProvider::OpenCode` enum variant but the implementation is a skeleton that won't work:
- The CLI args target the old archived `sst/opencode` project
- The NDJSON stream parser (`process_stream_line`) only understands Claude's `stream-json` format
- Tool names differ (lowercase in OpenCode vs PascalCase in Claude)

## Key Resources

- **OpenCode CLI docs**: https://opencode.ai/docs/cli/
- **OpenCode tools list**: https://opencode.ai/docs/tools/
- **OpenCode internals deep dive**: https://cefboud.com/posts/coding-agents-internals-opencode-deepdive/
- **OpenCode SDK (event types)**: https://opencode.ai/docs/sdk/
- **OpenCode GitHub**: https://github.com/opencode-ai/opencode

## Architecture Overview

### Files to modify

1. **`helix-view/src/editor.rs`** — `AiProvider` enum and `AiConfig` (no changes needed unless adding OpenCode-specific config fields)
2. **`helix-term/src/commands/ai.rs`** — ALL the work happens here:
   - `build_cli_command()` — CLI arg construction
   - `process_stream_line()` — NDJSON event parsing
   - `format_tool_display()` — Tool name display formatting
   - `spawn_stdout_processor()` — May need provider parameter

### Current flow (Claude)

```
User submits prompt
  → build_cli_command(AiProvider::Claude, ...) produces:
      claude --model sonnet --output-format stream-json --verbose
             --include-partial-messages --allowedTools Write,Read,Bash,...
             -p "prompt text"
  → Process spawned, stdout piped
  → spawn_stdout_processor reads lines, calls process_stream_line()
  → process_stream_line parses Claude's NDJSON format:
      {"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"..."}}}
  → Updates streaming_buf (thinking/text), tool_buf (tool calls), result_buf (final output)
  → Callback reads temp file or result_buf for the replacement/search result
```

## What Needs to Change

### 1. `build_cli_command()` (line ~591 in ai.rs)

Current OpenCode args are wrong. Update to:

```rust
AiProvider::OpenCode => (
    "opencode".to_string(),
    vec![
        "run".to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--model".to_string(),
        model.to_string(),  // format: "provider/model" e.g. "anthropic/claude-sonnet-4-5"
        prompt.to_string(),
    ],
),
```

**Key differences from Claude:**
- Command is `opencode run` (not just `opencode`)
- `--format json` (not `--output-format stream-json`)
- `--model` takes `provider/model` format
- No `--allowedTools` flag (OpenCode doesn't support restricting tools via CLI)
- No `--verbose` or `--include-partial-messages` equivalents
- Prompt is a positional arg (not `-p`)

### 2. `process_stream_line()` (line ~358 in ai.rs)

This is the biggest change. The function currently ONLY handles Claude's format. You need to add OpenCode event parsing.

**OpenCode event types** (from the deep dive article):
- `text-start` — text block begins
- `text-delta` — incremental text content
- `text-end` — text block ends
- `tool-call` — tool invocation with name and arguments
- `tool-result` — tool execution result
- `tool-error` — tool execution error
- `start-step` — agent step begins
- `finish-step` — agent step ends

**IMPORTANT**: The exact JSON structure of OpenCode's `--format json` output is NOT fully documented. You will need to:
1. Install OpenCode: `npm install -g opencode` or check their install docs
2. Run `opencode run --format json "hello"` and capture the raw output
3. Examine the JSON lines to understand the exact schema

**Recommended approach** — make `process_stream_line` provider-aware:

```rust
fn process_stream_line(
    line: &str,
    state: &mut StreamParseState,
    streaming_buf: &Arc<Mutex<String>>,
    tool_buf: &Arc<Mutex<String>>,
    result_buf: &Arc<Mutex<String>>,
    provider: &AiProvider,  // NEW PARAMETER
) -> bool {
    match provider {
        AiProvider::Claude => process_claude_stream_line(line, state, streaming_buf, tool_buf, result_buf),
        AiProvider::OpenCode => process_opencode_stream_line(line, state, streaming_buf, tool_buf, result_buf),
        AiProvider::Custom => process_claude_stream_line(line, state, streaming_buf, tool_buf, result_buf), // default to claude format
    }
}
```

Then extract the current Claude parsing into `process_claude_stream_line()` and write a new `process_opencode_stream_line()`.

**Likely OpenCode event mapping** (VERIFY by capturing actual output):

| OpenCode Event | Action | Claude Equivalent |
|---|---|---|
| `text-delta` | Append to `streaming_buf` | `content_block_delta` + `text_delta` |
| `text-start` | Clear `streaming_buf` | `content_block_start` + `text` type |
| `tool-call` | Set `tool_buf` with tool name/args | `content_block_start` + `tool_use` |
| `tool-result` | Clear `tool_buf` | `content_block_stop` |
| `finish-step` | May contain final result | `result` |

### 3. `format_tool_display()` (line ~526 in ai.rs)

OpenCode uses lowercase tool names. Add mappings:

```rust
let primary_key = match tool_name {
    // Claude tools (PascalCase)
    "Bash" | "bash" => "command",
    "Read" | "read" => "file_path",
    "Write" | "write" => "file_path",
    "Edit" | "edit" => "file_path",
    "Glob" | "glob" => "pattern",
    "Grep" | "grep" => "pattern",
    "WebSearch" | "websearch" => "query",
    "WebFetch" | "webfetch" => "url",
    _ => "",
};
```

Also update the Write/write detection for "Finalizing…":
```rust
if (tool_name == "Write" || tool_name == "write") {
    ...
}
```

### 4. `spawn_stdout_processor()` (line ~495 in ai.rs)

Needs to accept and pass through the provider:

```rust
fn spawn_stdout_processor(
    stdout_handle: Option<tokio::process::ChildStdout>,
    streaming_text: Arc<Mutex<String>>,
    tool_display: Arc<Mutex<String>>,
    provider: AiProvider,  // NEW - must be Clone
) -> (Arc<Mutex<String>>, tokio::task::JoinHandle<()>) {
    // ... pass provider to process_stream_line calls
}
```

**Note**: `AiProvider` already derives `Clone`, so this works.

### 5. Update all callers of `spawn_stdout_processor`

There are exactly 2 call sites in `ai.rs`:
- In `start_ai_replace_request()` (~line 800)
- In `start_ai_search_request()` (~line 1060)

Both need to pass `ai_config.provider.clone()` through the async block to `spawn_stdout_processor`.

The `provider` needs to be cloned into the async move block alongside the other clones:
```rust
let provider = ai_config.provider.clone();
// ... later in the async block:
let (stdout_collected, stdout_task) =
    spawn_stdout_processor(stdout_handle, Arc::clone(&streaming_text_clone), Arc::clone(&tool_display_clone), provider);
```

## Testing Plan

1. **Claude still works**: Run `:ai-replace` and `:ai-search` with `provider = "claude"` — verify streaming, tool calls, virtual lines, and results all work unchanged.

2. **OpenCode streaming**:
   - Set config to `provider = "open-code"` with a valid model
   - Run `:ai-replace` on a simple selection
   - Verify the statusline shows thinking text and tool calls
   - Verify the replacement is applied from the temp file

3. **OpenCode search**:
   - Run `:ai-search`
   - Verify results appear and `:ai-results` picker works

4. **Capture raw output first**: Before writing the parser, run this to see the actual format:
   ```bash
   opencode run --format json "Write 'hello' to /tmp/test.txt" 2>/dev/null
   ```
   Save this output — it's the ground truth for writing `process_opencode_stream_line()`.

## Config Example

```toml
[editor.ai]
enable = true
provider = "open-code"
model = "anthropic/claude-sonnet-4-5"
```

## Edge Cases to Handle

1. **OpenCode might not write to our temp file** — It has its own `write` tool but the file path is determined by the AI. Our prompt instructs it to write to TEMP_FILE, but verify this works.

2. **No `--allowedTools` equivalent** — OpenCode doesn't restrict tools via CLI. The AI might use tools we don't expect. The `⚠ Write: path` warning we added handles unexpected writes.

3. **OpenCode's `--format json` might not be line-delimited** — Verify it outputs one JSON object per line (NDJSON). If it uses a different framing (e.g., SSE `data: {...}\n\n`), the line reader in `spawn_stdout_processor` needs adjustment.

4. **Error handling** — OpenCode may report errors differently. Check stderr output format.

5. **Tool argument format** — OpenCode's tool call JSON structure for arguments may differ from Claude's `partial_json` incremental delivery. Claude streams tool args token-by-token; OpenCode might send complete args in one event.
