use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use helix_core::Selection;
use helix_core::{Tendril, Transaction};
use helix_view::{
    editor::{AiProvider, AiSearchResult},
    DocumentId,
};
use tui::{
    text::{Span, Spans},
    widgets::Cell,
};

use crate::{
    compositor,
    job::Callback,
    ui::{
        ai_prompt::AiPrompt,
        overlay::{overlaid, overlaid_with_size},
        Picker, PickerColumn,
    },
};

use super::Context;

/// Open the AI replace prompt (space A / :ai-replace).
/// Captures the current selection, then shows a multi-line input popup.
pub fn ai_replace_selection(cx: &mut Context) {
    let config = cx.editor.config();
    if !config.ai.enable {
        cx.editor.set_error("AI features are disabled. Set [editor.ai] enable = true");
        return;
    }

    let (view, doc) = current!(cx.editor);
    let text = doc.text().clone();
    let selection = doc.selection(view.id).clone();
    let primary = selection.primary();

    let mut from = primary.from();
    let mut to = primary.to();

    // Trim trailing blank lines from the selection
    let slice = text.slice(..);
    while to > from {
        let line = slice.char_to_line(to.saturating_sub(1));
        let line_start = slice.line_to_char(line);
        let line_text = slice.line(line);
        if line_text.chars().all(|c| c == '\n' || c == '\r' || c == ' ' || c == '\t') {
            to = line_start;
        } else {
            break;
        }
    }
    // Trim leading blank lines from the selection
    while from < to {
        let line = slice.char_to_line(from);
        let next_line_start = if line + 1 < slice.len_lines() {
            slice.line_to_char(line + 1)
        } else {
            slice.len_chars()
        };
        let line_text = slice.line(line);
        if line_text.chars().all(|c| c == '\n' || c == '\r' || c == ' ' || c == '\t') {
            from = next_line_start;
        } else {
            break;
        }
    }

    let selected_text: String = text.slice(from..to).to_string();

    if selected_text.is_empty() {
        cx.editor.set_error("No selection for AI replace");
        return;
    }

    let file_contents = text.to_string();
    let file_path = doc.path().cloned();
    let doc_id = doc.id();
    let doc_version = doc.version();
    let view_id = view.id;

    let ai_config = config.ai.clone();

    let prompt = AiPrompt::new(
        "Tuck Prompt".to_string(),
        move |ctx: &mut compositor::Context, user_instructions: String| {
            start_ai_replace_request(
                ctx,
                ai_config,
                doc_id,
                view_id,
                from,
                to,
                selected_text,
                file_contents,
                file_path,
                doc_version,
                user_instructions,
            );
        },
    );

    cx.push_layer(Box::new(overlaid_with_size(prompt, 60, 30)));
}

/// Open the AI search prompt (:ai-search).
pub fn ai_search(cx: &mut Context) {
    let config = cx.editor.config();
    if !config.ai.enable {
        cx.editor.set_error("AI features are disabled. Set [editor.ai] enable = true");
        return;
    }

    let (_view, doc) = current!(cx.editor);
    let file_path = doc.path().cloned();
    let ai_config = config.ai.clone();

    let prompt = AiPrompt::new(
        "Tuck Search".to_string(),
        move |ctx: &mut compositor::Context, user_instructions: String| {
            start_ai_search_request(ctx, ai_config, file_path, user_instructions);
        },
    );

    cx.push_layer(Box::new(overlaid_with_size(prompt, 60, 30)));
}

/// Cancel the most recent AI request.
pub fn ai_cancel(cx: &mut Context) {
    if let Some(req) = cx.editor.ai_requests.pop() {
        req.stop_ticker.store(true, Ordering::Relaxed);
        let remaining = cx.editor.ai_requests.len();
        if remaining > 0 {
            cx.editor.set_status(format!("AI request cancelled ({} remaining)", remaining));
        } else {
            cx.editor.set_status("AI request cancelled");
        }
    } else {
        cx.editor.set_status("No active AI request");
    }
}

// ---------------------------------------------------------------------------
// Context file discovery
// ---------------------------------------------------------------------------

/// Walk from the document's directory up to the workspace root, collecting
/// the contents of any context files (e.g. AGENT.md, CLAUDE.md).
fn discover_context_files(
    doc_path: Option<&Path>,
    context_file_names: &[String],
) -> String {
    let start_dir = match doc_path.and_then(|p| p.parent()) {
        Some(dir) => dir.to_path_buf(),
        None => helix_stdx::env::current_working_dir(),
    };

    let (workspace_root, _) = helix_loader::find_workspace_in(&start_dir);
    let mut collected = String::new();

    for ancestor in start_dir.ancestors() {
        for name in context_file_names {
            let candidate = ancestor.join(name);
            if candidate.is_file() {
                if let Ok(content) = std::fs::read_to_string(&candidate) {
                    collected.push_str(&format!(
                        "<ContextFile path=\"{}\">\n{}\n</ContextFile>\n",
                        candidate.display(),
                        content.trim()
                    ));
                }
            }
        }
        if ancestor == workspace_root.as_path() {
            break;
        }
    }

    collected
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Convert a char offset into 1-based (line, col) within `text`.
fn char_offset_to_line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, c) in text.char_indices() {
        if i >= offset {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn build_replace_prompt(
    user_instructions: &str,
    selected_text: &str,
    file_contents: &str,
    selection_from: usize,
    selection_to: usize,
    file_path: Option<&Path>,
    temp_path: &Path,
    context_files_content: &str,
) -> String {
    let (from_line, from_col) = char_offset_to_line_col(file_contents, selection_from);
    let (to_line, to_col) = char_offset_to_line_col(file_contents, selection_to);
    let file_path_str = file_path
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let range_str = format!(
        "range(point({},{}),point({},{}))",
        from_line, from_col, to_line, to_col
    );

    format!(
        r#"<DIRECTIONS>
{user_instructions}
</DIRECTIONS>
<Context>
You receive a selection in a code editor that you need to replace with new code.
The selection's contents may contain notes, incorporate the notes every time if there are some.
Consider the context of the selection and what you are supposed to be implementing.
<SELECTION_LOCATION>
{range}
</SELECTION_LOCATION>
<SELECTION_CONTENT>
{selected_text}
</SELECTION_CONTENT>
<FILE_CONTAINING_SELECTION>
{file_contents}
</FILE_CONTAINING_SELECTION>
</Context>
{context_files}<Location><File>{file_path}</File><Function>{range_dup}</Function></Location>
<FunctionText>{selected_text_dup}</FunctionText>
<MustObey>
NEVER alter any file other than TEMP_FILE.
Never provide the requested changes as conversational output. Return only the code.
ONLY provide requested changes by writing the change to TEMP_FILE.
Never attempt to read TEMP_FILE. It is purely for output.
Previous contents, which may not exist, can be written over without worry.
After writing TEMP_FILE once you should be done. Be done and end the session.
</MustObey>
<TEMP_FILE>{temp_path}</TEMP_FILE>"#,
        user_instructions = user_instructions,
        range = range_str,
        selected_text = selected_text,
        file_contents = file_contents,
        context_files = context_files_content,
        file_path = file_path_str,
        range_dup = range_str,
        selected_text_dup = selected_text,
        temp_path = temp_path.display(),
    )
}

fn build_search_prompt(
    user_instructions: &str,
    temp_path: &Path,
    context_files_content: &str,
    current_file_path: Option<&Path>,
) -> String {
    let current_file_hint = current_file_path
        .map(|p| format!("\nThe user is currently viewing: {}\n", p.display()))
        .unwrap_or_default();

    format!(
        r#"<DIRECTIONS>
{user_instructions}
</DIRECTIONS>
<Context>
You are given a prompt and you must search through this project and return code that matches the description provided.{current_file_hint}
<Rule>You must provide output without any commentary, just text locations</Rule>
<Rule>Text locations are in the format of: /path/to/file.ext:lnum:cnum,X,NOTES
lnum = starting line number 1 based
cnum = starting column number 1 based
X = how many lines should be highlighted
NOTES = A SHORT description (max 6 words) of why this highlight is important
</Rule>
<Rule>NOTES must be very concise (under 6 words), no new lines</Rule>
<Rule>You must adhere to the output format</Rule>
<Rule>Double check output format before writing it to the file</Rule>
<Rule>Each location is separated by new lines</Rule>
<Rule>Each path is specified in absolute pathing</Rule>
<Rule>You can provide notes you think are relevant per location</Rule>
<Example>
You have found 3 locations in files foo.js, bar.js, and baz.js.
There are 2 locations in foo.js, 1 in bar.js and baz.js.
<Output>
/path/to/project/src/foo.js:24:8,3,handles user auth
/path/to/project/src/foo.js:71:12,7,token validation logic
/path/to/project/src/bar.js:13:2,1,error handler entry
/path/to/project/src/baz.js:1:1,52,main config setup
</Output>
<Meaning>
This means that the search results found
foo.js at line 24, char 8 and the next 2 lines
foo.js at line 71, char 12 and the next 6 lines
bar.js at line 13, char 2
baz.js at line 1, char 1 and the next 51 lines
</Meaning>
</Example>
</Context>
{context_files}<MustObey>
NEVER alter any file other than TEMP_FILE.
Never provide the requested changes as conversational output. Return only the code.
ONLY provide requested changes by writing the change to TEMP_FILE.
Never attempt to read TEMP_FILE. It is purely for output.
Previous contents, which may not exist, can be written over without worry.
After writing TEMP_FILE once you should be done. Be done and end the session.
</MustObey>
<TEMP_FILE>{temp_path}</TEMP_FILE>"#,
        user_instructions = user_instructions,
        current_file_hint = current_file_hint,
        context_files = context_files_content,
        temp_path = temp_path.display(),
    )
}

// ---------------------------------------------------------------------------
// Shared NDJSON stream processor
// ---------------------------------------------------------------------------

/// Mutable state for tracking the current content block while parsing NDJSON.
struct StreamParseState {
    current_tool_name: Option<String>,
    tool_args_json: String,
}

impl StreamParseState {
    fn new() -> Self {
        Self {
            current_tool_name: None,
            tool_args_json: String::new(),
        }
    }
}

/// Dispatch a single NDJSON line to the appropriate provider-specific parser.
///
/// - `streaming_buf`: thinking/text content (shown in virtual lines & statusline)
/// - `tool_buf`: current tool call display (shown on a dedicated line)
/// - `result_buf`: final output text
///
/// Returns `true` if a redraw is needed.
fn process_stream_line(
    line: &str,
    state: &mut StreamParseState,
    streaming_buf: &Arc<Mutex<String>>,
    tool_buf: &Arc<Mutex<String>>,
    result_buf: &Arc<Mutex<String>>,
    provider: &AiProvider,
) -> bool {
    match provider {
        AiProvider::Claude | AiProvider::Custom => {
            process_claude_stream_line(line, state, streaming_buf, tool_buf, result_buf)
        }
        AiProvider::OpenCode => {
            process_opencode_stream_line(line, state, streaming_buf, tool_buf, result_buf)
        }
    }
}

/// Process a single NDJSON line from Claude's stream-json output.
fn process_claude_stream_line(
    line: &str,
    state: &mut StreamParseState,
    streaming_buf: &Arc<Mutex<String>>,
    tool_buf: &Arc<Mutex<String>>,
    result_buf: &Arc<Mutex<String>>,
) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let json = match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(j) => j,
        Err(_) => return false,
    };

    match json.get("type").and_then(|t| t.as_str()) {
        Some("stream_event") => {
            let event_type = json.pointer("/event/type").and_then(|t| t.as_str());

            match event_type {
                Some("content_block_start") => {
                    let block_type = json
                        .pointer("/event/content_block/type")
                        .and_then(|t| t.as_str());
                    match block_type {
                        Some("tool_use") => {
                            let name = json
                                .pointer("/event/content_block/name")
                                .and_then(|t| t.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            if let Ok(mut buf) = tool_buf.lock() {
                                buf.clear();
                                // Write is always to our temp file → show "Finalizing…"
                                if name == "Write" {
                                    buf.push_str("Finalizing…");
                                } else {
                                    buf.push_str(&format!("→ {}…", name));
                                }
                            }
                            state.current_tool_name = Some(name);
                            state.tool_args_json.clear();
                            return true;
                        }
                        Some("thinking") | Some("text") => {
                            state.current_tool_name = None;
                            state.tool_args_json.clear();
                            // Clear tool display when thinking resumes
                            if let Ok(mut buf) = tool_buf.lock() {
                                buf.clear();
                            }
                            // Clear streaming text for fresh block
                            if let Ok(mut buf) = streaming_buf.lock() {
                                buf.clear();
                            }
                        }
                        _ => {}
                    }
                }
                Some("content_block_delta") => {
                    if let Some(delta) = json.pointer("/event/delta") {
                        let delta_type = delta.get("type").and_then(|t| t.as_str());
                        match delta_type {
                            Some("thinking_delta") => {
                                if let Some(text) = delta.get("thinking").and_then(|t| t.as_str()) {
                                    if let Ok(mut buf) = streaming_buf.lock() {
                                        buf.push_str(text);
                                    }
                                    return true;
                                }
                            }
                            Some("text_delta") => {
                                if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                    if let Ok(mut buf) = streaming_buf.lock() {
                                        buf.push_str(text);
                                    }
                                    return true;
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(partial) = delta.get("partial_json").and_then(|t| t.as_str()) {
                                    state.tool_args_json.push_str(partial);
                                    let tool = state.current_tool_name.as_deref().unwrap_or("tool");
                                    let summary = format_tool_display(tool, &state.tool_args_json);
                                    if let Ok(mut buf) = tool_buf.lock() {
                                        buf.clear();
                                        buf.push_str(&summary);
                                    }
                                    return true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Some("assistant") => {
            // Fallback: parse complete assistant messages
            // (when extended thinking suppresses stream_event deltas)
            if let Some(content) = json.pointer("/message/content") {
                if let Some(blocks) = content.as_array() {
                    for block in blocks {
                        let block_type = block.get("type").and_then(|t| t.as_str());
                        let text = match block_type {
                            Some("thinking") => block.get("thinking").and_then(|t| t.as_str()),
                            Some("text") => block.get("text").and_then(|t| t.as_str()),
                            _ => None,
                        };
                        if let Some(text) = text {
                            if let Ok(mut buf) = streaming_buf.lock() {
                                *buf = text.to_string();
                            }
                            return true;
                        }
                    }
                }
            }
        }
        Some("result") => {
            if let Some(result) = json.get("result").and_then(|r| r.as_str()) {
                if let Ok(mut collected) = result_buf.lock() {
                    *collected = result.to_string();
                }
            }
        }
        _ => {}
    }

    false
}

/// Process a single NDJSON line from OpenCode's `--format json` output.
///
/// OpenCode emits one JSON object per line. Known top-level types:
///   step_start, step_finish, text, tool_use
///
/// Actual field paths (from real output):
///   - Text:     part.text
///   - Tool:     part.tool (name), part.state.input (args), part.state.output (result)
///   - Thinking: part.metadata.<provider>.reasoning_details[].text
fn process_opencode_stream_line(
    line: &str,
    state: &mut StreamParseState,
    streaming_buf: &Arc<Mutex<String>>,
    tool_buf: &Arc<Mutex<String>>,
    result_buf: &Arc<Mutex<String>>,
) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let json = match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(j) => j,
        Err(_) => return false,
    };

    let part = json.get("part");

    match json.get("type").and_then(|t| t.as_str()) {
        Some("text") | Some("text_delta") => {
            if let Some(text) = part.and_then(|p| p.get("text")).and_then(|t| t.as_str()) {
                if let Ok(mut buf) = streaming_buf.lock() {
                    buf.push_str(text);
                }
                if let Ok(mut buf) = result_buf.lock() {
                    buf.push_str(text);
                }
                return true;
            }
        }
        Some("tool_use") | Some("tool_call") => {
            let p = match part {
                Some(p) => p,
                None => return false,
            };

            // Tool name: part.tool
            let tool_name = p
                .get("tool")
                .or_else(|| p.get("name"))
                .or_else(|| p.get("toolName"))
                .and_then(|t| t.as_str())
                .unwrap_or("tool");

            // Tool input: part.state.input (object with tool-specific args)
            let input_str = p
                .pointer("/state/input")
                .or_else(|| p.get("input"))
                .map(|v| v.to_string())
                .unwrap_or_default();

            // Check if this is a write to our temp file
            let is_temp_write = (tool_name == "write" || tool_name == "Write")
                && input_str.contains("helix-ai");

            // Build display strings BEFORE locking any buffers, so we can
            // set streaming_buf and tool_buf back-to-back without a gap
            // (prevents the ticker from rendering a partial state).
            let tool_display_str = if is_temp_write {
                "Finalizing…".to_string()
            } else {
                format_tool_display(tool_name, &input_str)
            };

            // Extract reasoning/thinking from provider metadata if present.
            // Path: part.metadata.<provider>.reasoning_details[].text
            let mut reasoning_text = String::new();
            if let Some(metadata) = p.get("metadata") {
                if let Some(obj) = metadata.as_object() {
                    for (_provider, details) in obj {
                        if let Some(reasons) = details.get("reasoning_details").and_then(|r| r.as_array()) {
                            for reason in reasons {
                                if let Some(text) = reason.get("text").and_then(|t| t.as_str()) {
                                    reasoning_text = text.to_string();
                                }
                            }
                        }
                    }
                }
            }

            // Set both buffers back-to-back: reasoning first, then tool display
            if !reasoning_text.is_empty() {
                if let Ok(mut buf) = streaming_buf.lock() {
                    buf.clear();
                    buf.push_str(&reasoning_text);
                }
            }
            if let Ok(mut buf) = tool_buf.lock() {
                buf.clear();
                buf.push_str(&tool_display_str);
            }

            state.current_tool_name = Some(tool_name.to_string());
            state.tool_args_json.clear();
            return true;
        }
        Some("tool_result") => {
            if let Ok(mut buf) = tool_buf.lock() {
                buf.clear();
            }
            state.current_tool_name = None;
            state.tool_args_json.clear();
            return true;
        }
        Some("tool_error") => {
            let error_msg = part
                .and_then(|p| p.get("error"))
                .or_else(|| json.get("error"))
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error");
            if let Ok(mut buf) = tool_buf.lock() {
                buf.clear();
                buf.push_str(&format!("⚠ error: {}", error_msg));
            }
            return true;
        }
        Some("step_start") => {
            // Clear display buffers for fresh step, keep result_buf accumulating
            if let Ok(mut buf) = streaming_buf.lock() {
                buf.clear();
            }
            if let Ok(mut buf) = tool_buf.lock() {
                buf.clear();
            }
        }
        Some("step_finish") => {
            // No-op
        }
        _ => {
            return false;
        }
    }

    false
}

/// Spawn a tokio task that reads stdout line-by-line and processes NDJSON events.
/// Returns the result buffer (for reading the final output) and the task handle.
fn spawn_stdout_processor(
    stdout_handle: Option<tokio::process::ChildStdout>,
    streaming_text: Arc<Mutex<String>>,
    tool_display: Arc<Mutex<String>>,
    provider: AiProvider,
) -> (Arc<Mutex<String>>, tokio::task::JoinHandle<()>) {
    let result_buf = Arc::new(Mutex::new(String::new()));
    let result_buf_clone = Arc::clone(&result_buf);

    // Debug: if HELIX_AI_DEBUG is set, log raw NDJSON to that path
    let debug_path = std::env::var("HELIX_AI_DEBUG").ok().map(PathBuf::from);

    let handle = tokio::spawn(async move {
        let mut state = StreamParseState::new();
        let mut debug_file = debug_path.as_ref().and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        if let Some(stdout) = stdout_handle {
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Some(ref mut f) = debug_file {
                            use std::io::Write;
                            let _ = writeln!(f, "{}", line.trim_end());
                        }
                        if process_stream_line(&line, &mut state, &streaming_text, &tool_display, &result_buf_clone, &provider) {
                            helix_event::request_redraw();
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    });

    (result_buf, handle)
}

// ---------------------------------------------------------------------------
// Tool call display formatting
// ---------------------------------------------------------------------------

/// Extract a human-readable summary from a (potentially partial) tool call.
/// e.g. `→ Bash: find . -name '*.rs'` or `→ Read: src/main.rs`
fn format_tool_display(tool_name: &str, partial_json: &str) -> String {
    // Map each tool to its primary argument key.
    // Includes both Claude (PascalCase) and OpenCode (lowercase) tool names.
    let primary_key = match tool_name {
        "Bash" | "bash" => "command",
        "Read" | "read" => "file_path",
        "Write" => "file_path",       // Claude uses file_path
        "write" => "path",            // OpenCode uses path
        "Edit" => "file_path",        // Claude uses file_path
        "edit" => "path",             // OpenCode uses path
        "Glob" | "glob" => "pattern",
        "Grep" | "grep" => "pattern",
        "WebSearch" | "websearch" => "query",
        "WebFetch" | "webfetch" => "url",
        "patch" => "path",            // OpenCode-only
        "list" => "path",             // OpenCode-only (like ls)
        "codesearch" => "query",      // OpenCode-only — displayed as "Searching"
        _ => "",
    };

    // Display name override for cleaner statusline
    let display_name = match tool_name {
        "codesearch" => "Searching",
        _ => tool_name,
    };

    if !primary_key.is_empty() {
        if let Some(value) = extract_json_string_value(partial_json, primary_key) {
            if !value.is_empty() {
                if tool_name == "Write" || tool_name == "write" {
                    // Writing to our temp file = finalizing results
                    if value.contains("helix-ai") {
                        return "Finalizing…".to_string();
                    }
                    // AI is writing to a non-temp file — warn the user
                    return format!("⚠ Write: {}", value);
                }
                return format!("→ {}: {}", display_name, value);
            }
        }
    }

    // For Write/write with no args yet, show Finalizing (most likely the temp file)
    if tool_name == "Write" || tool_name == "write" {
        return "Finalizing…".to_string();
    }

    // Fallback: just the display name with ellipsis
    format!("→ {}…", display_name)
}

/// Extract a string value for `key` from partial JSON like `{"command":"find .`.
/// Handles incomplete strings (no closing quote yet).
fn extract_json_string_value(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let key_pos = json.find(&pattern)?;
    let after_key = &json[key_pos + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_colon = after_colon.trim_start();
    let after_quote = after_colon.strip_prefix('"')?;
    if let Some(end) = after_quote.find('"') {
        Some(after_quote[..end].to_string())
    } else {
        Some(after_quote.to_string())
    }
}

// ---------------------------------------------------------------------------
// CLI command construction
// ---------------------------------------------------------------------------

fn build_cli_command(
    provider: &AiProvider,
    model: &str,
    prompt: &str,
    custom_command: &[String],
) -> (String, Vec<String>) {
    match provider {
        AiProvider::Claude => (
            "claude".to_string(),
            vec![
                "--model".to_string(),
                model.to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--include-partial-messages".to_string(),
                "--allowedTools".to_string(),
                "Write,Edit,Read,Bash,WebSearch,WebFetch".to_string(),
                "-p".to_string(),
                prompt.to_string(),
            ],
        ),
        AiProvider::OpenCode => (
            "opencode".to_string(),
            vec![
                "run".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "--model".to_string(),
                model.to_string(),
                prompt.to_string(),
            ],
        ),
        AiProvider::Custom => {
            if custom_command.is_empty() {
                return ("echo".to_string(), vec!["Error: no custom_command configured".to_string()]);
            }
            let program = custom_command[0].clone();
            let args: Vec<String> = custom_command[1..]
                .iter()
                .map(|arg| {
                    arg.replace("{model}", model)
                        .replace("{prompt}", prompt)
                })
                .collect();
            (program, args)
        }
    }
}

// ---------------------------------------------------------------------------
// Async request spawning
// ---------------------------------------------------------------------------

/// Public entry point for typed commands to start an AI replace request.
pub fn start_ai_replace_request_from_compositor(
    ctx: &mut compositor::Context,
    ai_config: helix_view::editor::AiConfig,
    doc_id: DocumentId,
    view_id: helix_view::ViewId,
    from: usize,
    to: usize,
    selected_text: String,
    file_contents: String,
    file_path: Option<PathBuf>,
    doc_version: i32,
    user_instructions: String,
) {
    start_ai_replace_request(
        ctx, ai_config, doc_id, view_id, from, to, selected_text, file_contents,
        file_path, doc_version, user_instructions,
    );
}

/// Public entry point for typed commands to start an AI search request.
pub fn start_ai_search_request_from_compositor(
    ctx: &mut compositor::Context,
    ai_config: helix_view::editor::AiConfig,
    file_path: Option<PathBuf>,
    user_instructions: String,
) {
    start_ai_search_request(ctx, ai_config, file_path, user_instructions);
}

fn start_ai_replace_request(
    ctx: &mut compositor::Context,
    ai_config: helix_view::editor::AiConfig,
    doc_id: DocumentId,
    view_id: helix_view::ViewId,
    from: usize,
    to: usize,
    selected_text: String,
    file_contents: String,
    file_path: Option<PathBuf>,
    doc_version: i32,
    user_instructions: String,
) {
    // Check max concurrent limit early, before allocating resources
    let max = ai_config.max_concurrent;
    if max > 0 && ctx.editor.ai_requests.len() >= max {
        ctx.editor.set_error(format!(
            "Maximum concurrent AI requests ({}) reached. Cancel one first.",
            max
        ));
        return;
    }

    // Assign request_id BEFORE creating temp path so each concurrent request
    // gets a guaranteed-unique file (millis can collide on Windows ~15ms ticks)
    let request_id = ctx.editor.next_ai_request_id();

    let context_files_content = discover_context_files(
        file_path.as_deref(),
        &ai_config.context_files,
    );

    let temp_path = std::env::temp_dir().join(format!("helix-ai-{}", request_id));

    let prompt_text = build_replace_prompt(
        &user_instructions,
        &selected_text,
        &file_contents,
        from,
        to,
        file_path.as_deref(),
        &temp_path,
        &context_files_content,
    );

    let (program, args) = build_cli_command(
        &ai_config.provider,
        &ai_config.model,
        &prompt_text,
        &ai_config.custom_command,
    );

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();

    let streaming_text = Arc::new(Mutex::new(String::new()));
    let tool_display = Arc::new(Mutex::new(String::new()));
    let stop_ticker = Arc::new(AtomicBool::new(false));
    ctx.editor.ai_requests.push(helix_view::ai::AiRequestState {
        id: request_id,
        label: "AI".to_string(),
        doc_id,
        original_from: from,
        original_to: to,
        original_text: selected_text,
        doc_version,
        cancel_tx,
        streaming_text: Arc::clone(&streaming_text),
        tool_display: Arc::clone(&tool_display),
        started_at: tokio::time::Instant::now(),
        stop_ticker: Arc::clone(&stop_ticker),
    });

    ctx.editor.set_status("AI processing...");

    // Spawn a spinner ticker that redraws every 80ms
    let ticker_stop = Arc::clone(&stop_ticker);
    tokio::spawn(async move {
        while !ticker_stop.load(Ordering::Relaxed) {
            tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
            if ticker_stop.load(Ordering::Relaxed) {
                break;
            }
            helix_event::request_redraw();
        }
    });

    let temp_path_clone = temp_path.clone();
    let streaming_text_clone = Arc::clone(&streaming_text);
    let tool_display_clone = Arc::clone(&tool_display);
    let stop_ticker_clone = Arc::clone(&stop_ticker);
    let provider = ai_config.provider.clone();

    let future = async move {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        // Detach the child from our controlling terminal so it cannot send
        // escape-sequence queries (e.g. capability detection) whose responses
        // would leak into our input buffer and appear as garbage on quit.
        #[cfg(unix)]
        {
            unsafe {
                cmd.pre_exec(|| {
                    // Create a new session, which detaches from the controlling tty.
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn AI CLI '{}': {}", program, e))?;

        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Collect stderr for error reporting only
        let stderr_collected = Arc::new(Mutex::new(String::new()));
        let stderr_collected_clone = Arc::clone(&stderr_collected);
        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = stderr_handle {
                let mut reader = tokio::io::BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim_end().to_string();
                            if let Ok(mut collected) = stderr_collected_clone.lock() {
                                if !collected.is_empty() {
                                    collected.push('\n');
                                }
                                collected.push_str(&trimmed);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        // Parse NDJSON stdout via the shared stream processor.
        let (stdout_collected, stdout_task) =
            spawn_stdout_processor(stdout_handle, Arc::clone(&streaming_text_clone), Arc::clone(&tool_display_clone), provider);

        // Handle cancellation while tasks run
        let cancel_result: anyhow::Result<()> = tokio::select! {
            _ = async {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
            } => { Ok(()) }
            _ = cancel_rx => {
                let _ = child.kill().await;
                let _ = std::fs::remove_file(&temp_path_clone);
                stop_ticker_clone.store(true, Ordering::Relaxed);
                anyhow::bail!("AI request cancelled")
            }
        };

        if let Err(e) = cancel_result {
            return Err(e);
        }

        // Wait for the process to finish
        let status = child.wait().await?;

        let stderr_str = stderr_collected.lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        if !status.success() {
            stop_ticker_clone.store(true, Ordering::Relaxed);
            anyhow::bail!("AI CLI failed: {}", stderr_str);
        }

        let stdout_str = stdout_collected.lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Try reading from temp file first, fall back to collected stdout
        let replacement = if temp_path_clone.exists() {
            let content = std::fs::read_to_string(&temp_path_clone)
                .unwrap_or_else(|_| stdout_str.clone());
            let _ = std::fs::remove_file(&temp_path_clone);
            content
        } else {
            stdout_str
        };

        stop_ticker_clone.store(true, Ordering::Relaxed);

        // Guard: never apply an empty replacement (would delete the selection)
        if replacement.trim().is_empty() {
            let call: Callback = Callback::EditorCompositor(Box::new(
                move |editor: &mut helix_view::Editor, _compositor| {
                    editor.take_ai_request(request_id);
                    editor.set_error("AI returned empty result — selection unchanged");
                },
            ));
            return Ok(call);
        }

        let call: Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut helix_view::Editor, _compositor| {
                apply_ai_replacement(editor, request_id, doc_id, view_id, &replacement);
            },
        ));
        Ok(call)
    };

    ctx.jobs.callback(future);
}

fn apply_ai_replacement(
    editor: &mut helix_view::Editor,
    request_id: helix_view::ai::RequestId,
    doc_id: DocumentId,
    view_id: helix_view::ViewId,
    replacement: &str,
) {
    // Remove this specific request and read the (possibly remapped) offsets from it.
    let req = match editor.take_ai_request(request_id) {
        Some(r) => r,
        None => return, // already cancelled
    };
    let from = req.original_from;
    let to = req.original_to;

    // Check doc still exists
    let doc = match editor.documents.get(&doc_id) {
        Some(doc) => doc,
        None => {
            editor.set_error("AI: document no longer exists");
            return;
        }
    };

    // Bounds check before content comparison
    let doc_len = doc.text().len_chars();
    if from >= to || to > doc_len {
        let _ = editor.registers.write('+', vec![replacement.to_string()]);
        editor.set_error(format!(
            "AI: invalid range [{}, {}) in doc of {} chars. Result saved to clipboard.",
            from, to, doc_len
        ));
        return;
    }

    // Content-based staleness check: verify the text at the remapped position still
    // matches the original selection. This is more robust than version comparison
    // because LSP edits (diagnostics refresh, etc.) can bump the version without
    // touching our selection region.
    let current_text: String = doc.text().slice(from..to).into();
    if current_text != req.original_text {
        let _ = editor.registers.write('+', vec![replacement.to_string()]);
        editor.set_error(
            "Selection changed during AI request. Result saved to clipboard (paste with \"+p or Ctrl-v)",
        );
        return;
    }

    let doc = editor.documents.get_mut(&doc_id).unwrap();

    // Apply the transaction
    let transaction = Transaction::change(
        doc.text(),
        std::iter::once((from, to, Some(Tendril::from(replacement)))),
    );
    doc.apply(&transaction, view_id);

    // Append to history (need separate borrows)
    let view = editor.tree.get_mut(view_id);
    let doc = editor.documents.get_mut(&doc_id).unwrap();
    doc.append_changes_to_history(view);

    // Scroll the view to keep the cursor visible after large replacements
    let scrolloff = editor.config().scrolloff;
    let doc = editor.documents.get_mut(&doc_id).unwrap();
    let view = editor.tree.get_mut(view_id);
    view.ensure_cursor_in_view(doc, scrolloff);

    // Remap other pending requests on the same document through the changeset.
    // Use Assoc::After for both from and to so positions at exact boundaries
    // move PAST the inserted text rather than collapsing into it.
    let changes = transaction.changes();
    for req in &mut editor.ai_requests {
        if req.doc_id == doc_id {
            use helix_core::Assoc;
            req.original_from = changes.map_pos(req.original_from, Assoc::After);
            req.original_to = changes.map_pos(req.original_to, Assoc::After);
        }
    }

    editor.set_status("AI replacement applied");
}

fn start_ai_search_request(
    ctx: &mut compositor::Context,
    ai_config: helix_view::editor::AiConfig,
    file_path: Option<PathBuf>,
    user_instructions: String,
) {
    let request_id = ctx.editor.next_ai_request_id();

    let context_files_content = discover_context_files(
        file_path.as_deref(),
        &ai_config.context_files,
    );

    let temp_path = std::env::temp_dir().join(format!("helix-ai-search-{}", request_id));

    let prompt_text = build_search_prompt(
        &user_instructions,
        &temp_path,
        &context_files_content,
        file_path.as_deref(),
    );

    let search_model = if ai_config.search_model.is_empty() {
        &ai_config.model
    } else {
        &ai_config.search_model
    };
    let (program, args) = build_cli_command(
        &ai_config.provider,
        search_model,
        &prompt_text,
        &ai_config.custom_command,
    );

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();

    let current_doc_id = doc!(ctx.editor).id();
    let streaming_text = Arc::new(Mutex::new(String::new()));
    let tool_display = Arc::new(Mutex::new(String::new()));
    let stop_ticker = Arc::new(AtomicBool::new(false));
    ctx.editor.ai_requests.push(helix_view::ai::AiRequestState {
        id: request_id,
        label: "AI Searching".to_string(),
        doc_id: current_doc_id,
        original_from: 0,
        original_to: 0,
        original_text: String::new(),
        doc_version: 0,
        cancel_tx,
        streaming_text: Arc::clone(&streaming_text),
        tool_display: Arc::clone(&tool_display),
        started_at: tokio::time::Instant::now(),
        stop_ticker: Arc::clone(&stop_ticker),
    });

    let query_preview: String = user_instructions.chars().take(80).collect();
    ctx.editor.set_status(format!("Search: \"{}\"", query_preview));

    // Spawn a spinner ticker that redraws every 80ms
    let ticker_stop = Arc::clone(&stop_ticker);
    tokio::spawn(async move {
        while !ticker_stop.load(Ordering::Relaxed) {
            tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
            if ticker_stop.load(Ordering::Relaxed) {
                break;
            }
            helix_event::request_redraw();
        }
    });

    let temp_path_clone = temp_path.clone();
    let streaming_text_clone = Arc::clone(&streaming_text);
    let tool_display_clone = Arc::clone(&tool_display);
    let stop_ticker_clone = Arc::clone(&stop_ticker);
    let provider = ai_config.provider.clone();

    let future = async move {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        // Detach the child from our controlling terminal so it cannot send
        // escape-sequence queries whose responses would leak as garbage on quit.
        #[cfg(unix)]
        {
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn AI CLI '{}': {}", program, e))?;

        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Collect stderr for error reporting
        let stderr_collected = Arc::new(Mutex::new(String::new()));
        let stderr_collected_clone = Arc::clone(&stderr_collected);
        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = stderr_handle {
                let mut reader = tokio::io::BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim_end().to_string();
                            if let Ok(mut collected) = stderr_collected_clone.lock() {
                                if !collected.is_empty() {
                                    collected.push('\n');
                                }
                                collected.push_str(&trimmed);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        // Parse NDJSON stdout via the shared stream processor.
        let (stdout_collected, stdout_task) =
            spawn_stdout_processor(stdout_handle, Arc::clone(&streaming_text_clone), Arc::clone(&tool_display_clone), provider);

        // Handle cancellation while tasks run
        let cancel_result: anyhow::Result<()> = tokio::select! {
            _ = async {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
            } => { Ok(()) }
            _ = cancel_rx => {
                let _ = child.kill().await;
                let _ = std::fs::remove_file(&temp_path_clone);
                stop_ticker_clone.store(true, Ordering::Relaxed);
                anyhow::bail!("AI search cancelled")
            }
        };

        if let Err(e) = cancel_result {
            return Err(e);
        }

        let status = child.wait().await?;

        let stderr_str = stderr_collected.lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        if !status.success() {
            stop_ticker_clone.store(true, Ordering::Relaxed);
            anyhow::bail!("AI search failed: {}", stderr_str);
        }

        let stdout_str = stdout_collected.lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Try reading from temp file first, fall back to collected stdout
        let result_text = if temp_path_clone.exists() {
            let content = std::fs::read_to_string(&temp_path_clone)
                .unwrap_or_else(|_| stdout_str.clone());
            let _ = std::fs::remove_file(&temp_path_clone);
            content
        } else {
            stdout_str
        };

        stop_ticker_clone.store(true, Ordering::Relaxed);

        let call: Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut helix_view::Editor, compositor| {
                editor.take_ai_request(request_id);
                show_search_results(editor, compositor, &result_text);
            },
        ));
        Ok(call)
    };

    ctx.jobs.callback(future);
}

/// Parse search results, store them on the editor, jump to the first, and show a summary.
/// Format: /path/to/file.ext:lnum:cnum,X,NOTES
fn show_search_results(
    editor: &mut helix_view::Editor,
    _compositor: &mut compositor::Compositor,
    result_text: &str,
) {
    use helix_view::editor::AiSearchResult;

    let lines: Vec<&str> = result_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();

    if lines.is_empty() {
        editor.ai_search_results.clear();
        editor.set_status("AI search: no results found");
        return;
    }

    // Parse all results and store them
    let results: Vec<AiSearchResult> = lines
        .iter()
        .filter_map(|line| {
            parse_search_line(line).map(|parsed| AiSearchResult {
                path: PathBuf::from(parsed.path),
                line_num: parsed.line_num.saturating_sub(1), // convert to 0-indexed
                highlight_lines: parsed.highlight_lines,
                notes: parsed.notes.to_string(),
            })
        })
        .collect();

    let result_count = results.len();
    editor.ai_search_results = results;

    // Jump to the first result — open in a split if it's a different file,
    // or just move the cursor if it's already the current document.
    if let Some(first) = editor.ai_search_results.first() {
        let result_path = first.path.clone();
        let line = first.line_num;
        let current_doc_path = doc!(editor).path().cloned();
        let is_same_file = current_doc_path
            .as_ref()
            .and_then(|cp| std::fs::canonicalize(cp).ok())
            .zip(std::fs::canonicalize(&result_path).ok())
            .map_or(false, |(a, b)| a == b);

        if is_same_file {
            // Same file — just jump the cursor
            let (view, doc) = current!(editor);
            let text = doc.text().slice(..);
            let line_idx = line.min(text.len_lines().saturating_sub(1));
            let pos = text.line_to_char(line_idx);
            doc.set_selection(view.id, Selection::point(pos));
            let scrolloff = editor.config().scrolloff;
            let (view, doc) = current!(editor);
            view.ensure_cursor_in_view(doc, scrolloff);
        } else {
            // Different file — open in a vertical split
            match editor.open(&result_path, helix_view::editor::Action::VerticalSplit) {
                Ok(_) => {
                    let (view, doc) = current!(editor);
                    let text = doc.text().slice(..);
                    let line_idx = line.min(text.len_lines().saturating_sub(1));
                    let pos = text.line_to_char(line_idx);
                    doc.set_selection(view.id, Selection::point(pos));
                    helix_view::align_view(doc, view, helix_view::Align::Center);
                }
                Err(e) => {
                    editor.set_error(format!("AI search: failed to open file: {}", e));
                    return;
                }
            }
        }
    }

    let msg = format!(
        "AI search: {} result{}. :ai-results to browse",
        result_count,
        if result_count == 1 { "" } else { "s" },
    );
    editor.set_status(msg);
}

/// Parsed search result with all fields from the AI output format.
struct ParsedSearchLine<'a> {
    path: &'a str,
    line_num: usize,   // 1-based from AI, caller converts to 0-based
    highlight_lines: usize, // the X value (how many lines to highlight)
    notes: &'a str,
}

/// Parse a single search result line: /path:lnum:cnum,X,NOTES
/// Handles Windows drive letters (e.g. F:\path\file.rs:42:1,3,NOTES).
fn parse_search_line(line: &str) -> Option<ParsedSearchLine<'_>> {
    // Skip past Windows drive letter colon (e.g. "F:") so it doesn't
    // get consumed by the `:` split used to find the line number.
    let skip = if line.len() >= 2
        && line.as_bytes()[0].is_ascii_alphabetic()
        && line.as_bytes()[1] == b':'
    {
        2
    } else {
        0
    };

    let mut parts = line[skip..].splitn(3, ':');
    let path_tail = parts.next()?;
    let lnum_str = parts.next()?;
    let rest = parts.next().unwrap_or("");

    let lnum: usize = lnum_str.parse().ok()?;

    // Reconstruct full path including the drive letter prefix
    let path = &line[..skip + path_tail.len()];

    // rest is "cnum,X,NOTES" — split into cnum, X, NOTES
    let mut comma_parts = rest.splitn(3, ',');
    let _cnum = comma_parts.next().unwrap_or("1");
    let x_str = comma_parts.next().unwrap_or("1");
    let notes = comma_parts.next().unwrap_or("").trim();
    let highlight_lines = x_str.trim().parse::<usize>().unwrap_or(1);

    Some(ParsedSearchLine {
        path,
        line_num: lnum,
        highlight_lines,
        notes,
    })
}

// ---------------------------------------------------------------------------
// :ai-results picker
// ---------------------------------------------------------------------------

struct AiResultsConfig {
    directory_style: helix_view::theme::Style,
    number_style: helix_view::theme::Style,
    colon_style: helix_view::theme::Style,
}

/// Build the AI results picker from editor state. Must be called on the main
/// thread (inside an EditorCompositor callback) because Picker is not Send.
fn build_ai_results_picker(editor: &mut helix_view::Editor) -> Picker<AiSearchResult, AiResultsConfig> {
    let config = AiResultsConfig {
        directory_style: editor.theme.get("ui.text.directory"),
        number_style: editor.theme.get("constant.numeric.integer"),
        colon_style: editor.theme.get("punctuation"),
    };

    let columns = [
        PickerColumn::new("location", |item: &AiSearchResult, config: &AiResultsConfig| {
            let path = helix_stdx::path::get_relative_path(&item.path);
            let directories = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
                .unwrap_or_default();
            let filename = item
                .path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();

            Cell::from(Spans::from(vec![
                Span::styled(directories, config.directory_style),
                Span::raw(filename),
                Span::styled(":", config.colon_style),
                Span::styled((item.line_num + 1).to_string(), config.number_style),
            ]))
        }),
        PickerColumn::new("notes", |item: &AiSearchResult, _config: &AiResultsConfig| {
            Cell::from(item.notes.as_str())
        }),
    ];

    let items: Vec<AiSearchResult> = editor.ai_search_results.clone();

    Picker::new(
        columns,
        1, // "notes" column used for filtering
        items,
        config,
        move |cx, result: &AiSearchResult, action| {
            let path = &result.path;
            let line_num = result.line_num;

            let doc = match cx.editor.open(path, action) {
                Ok(id) => doc_mut!(cx.editor, &id),
                Err(e) => {
                    cx.editor
                        .set_error(format!("Failed to open file '{}': {}", path.display(), e));
                    return;
                }
            };

            let view = view_mut!(cx.editor);
            let text = doc.text();
            if line_num >= text.len_lines() {
                cx.editor.set_error("Line no longer exists in file");
                return;
            }
            let start = text.line_to_char(line_num);
            let end = text.line_to_char((line_num + 1).min(text.len_lines()));
            doc.set_selection(view.id, Selection::single(start, end));
            if action.align_view(view, doc.id()) {
                helix_view::align_view(doc, view, helix_view::Align::Center);
            }
        },
    )
    .truncate_start(false)
    .with_preview(|_editor, result: &AiSearchResult| {
        let end_line = result.line_num + result.highlight_lines.saturating_sub(1);
        Some((result.path.as_path().into(), Some((result.line_num, end_line))))
    })
}

/// Open a file picker showing stored AI search results.
/// Accepts `compositor::Context` so it can be called from typed commands directly.
pub fn ai_show_results(cx: &mut compositor::Context) {
    if cx.editor.ai_search_results.is_empty() {
        cx.editor.set_status("No AI search results");
        return;
    }

    // Build the picker via EditorCompositor callback because Picker is not Send
    let callback = async move {
        let call: Callback = Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
                let picker = build_ai_results_picker(editor);
                compositor.push(Box::new(overlaid(picker)));
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);
}
