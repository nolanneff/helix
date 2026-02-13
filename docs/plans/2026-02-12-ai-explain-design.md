# AI Explain Feature Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add an AI explain mode that shows inline streaming progress then opens the explanation in a vertical split markdown scratchpad.

**Architecture:** Mirrors the existing `ai_replace_selection` flow — same prompt popup, same streaming/progress virtual lines, same provider system. The only difference is the completion handler: instead of applying a text replacement, it opens a new `[scratch]` document in a vertical split with markdown syntax highlighting. A new `build_explain_prompt` function provides the explain-specific system prompt.

**Tech Stack:** Rust, Helix editor internals (helix-term, helix-view, helix-core)

---

### Task 1: Add `ai_explain_selection` public entry point

**Files:**
- Modify: `helix-term/src/commands/ai.rs` (add after line ~107, the end of `ai_replace_selection`)

**Step 1: Write the `ai_explain_selection` function**

Add this function after `ai_replace_selection` (after line ~107). It mirrors `ai_replace_selection` but captures the selection without trimming (we want context), and calls `start_ai_explain_request` in its prompt callback:

```rust
/// Open the AI explain prompt (space E / :ai-explain).
/// Captures the current selection, then shows a multi-line input popup.
pub fn ai_explain_selection(cx: &mut Context) {
    let config = cx.editor.config();
    if !config.ai.enable {
        cx.editor.set_error("AI features are disabled. Set enable = true under [editor.ai] in ai-config.toml");
        return;
    }

    let (view, doc) = current!(cx.editor);
    let text = doc.text().clone();
    let selection = doc.selection(view.id).clone();
    let primary = selection.primary();

    let from = primary.from();
    let to = primary.to();

    let selected_text: String = text.slice(from..to).to_string();

    if selected_text.is_empty() {
        cx.editor.set_error("No selection for AI explain");
        return;
    }

    let file_contents = text.to_string();
    let file_path = doc.path().cloned();
    let doc_id = doc.id();
    let view_id = view.id;

    let ai_config = config.ai.clone();

    let prompt = AiPrompt::new(
        "AI Explain".to_string(),
        move |ctx: &mut compositor::Context, user_instructions: String| {
            start_ai_explain_request(
                ctx,
                ai_config,
                doc_id,
                view_id,
                from,
                to,
                selected_text,
                file_contents,
                file_path,
                user_instructions,
            );
        },
    );

    cx.push_layer(Box::new(overlaid_with_size(prompt, 60, 30)));
}
```

**Step 2: Verify it compiles (it won't yet — `start_ai_explain_request` doesn't exist)**

This is expected. We'll add it in Task 3.

---

### Task 2: Add `build_explain_prompt` function

**Files:**
- Modify: `helix-term/src/commands/ai.rs` (add after `build_search_prompt`, around line ~337)

**Step 1: Write the prompt builder**

Add this after `build_search_prompt`:

```rust
fn build_explain_prompt(
    user_instructions: &str,
    selected_text: &str,
    file_contents: &str,
    file_path: Option<&Path>,
    temp_path: &Path,
    context_files_content: &str,
) -> String {
    let file_path_str = file_path
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());

    let user_direction = if user_instructions.trim().is_empty() {
        "Explain this code.".to_string()
    } else {
        user_instructions.to_string()
    };

    format!(
        r#"<DIRECTIONS>
{user_direction}
</DIRECTIONS>
<Context>
You are an expert code explainer. The user has selected code in their editor and wants you to explain it.
Respond in Markdown format.
<SELECTED_CODE>
{selected_text}
</SELECTED_CODE>
<FILE_CONTAINING_SELECTION>
File: {file_path}
{file_contents}
</FILE_CONTAINING_SELECTION>
</Context>
{context_files}<Guidelines>
- Be adaptive: brief for simple code, detailed for complex code
- Keep your explanation under ~100 lines
- Use Markdown formatting: headers, bullet points, code blocks where helpful
- Cover: what the code does, how it works, and any notable patterns or edge cases
- If the user asked a specific question, focus on answering that
- Do NOT include the original code in your explanation unless quoting small snippets
- Write the explanation directly to TEMP_FILE
</Guidelines>
<MustObey>
NEVER alter any file other than TEMP_FILE.
ONLY provide the explanation by writing it to TEMP_FILE.
Never attempt to read TEMP_FILE. It is purely for output.
Once you have written TEMP_FILE, you are done. End the session.
</MustObey>
<TEMP_FILE>{temp_path}</TEMP_FILE>"#,
        user_direction = user_direction,
        selected_text = selected_text,
        file_contents = file_contents,
        file_path = file_path_str,
        context_files = context_files_content,
        temp_path = temp_path.display(),
    )
}
```

---

### Task 3: Make `Editor::new_file_from_document` public

**Files:**
- Modify: `helix-view/src/editor.rs:1975`

**Step 1: Change visibility from `fn` to `pub fn`**

At line 1975, change:
```rust
fn new_file_from_document(&mut self, action: Action, doc: Document) -> DocumentId {
```
to:
```rust
pub fn new_file_from_document(&mut self, action: Action, doc: Document) -> DocumentId {
```

---

### Task 4: Add `start_ai_explain_request` function and completion handler

**Files:**
- Modify: `helix-term/src/commands/ai.rs` (add after `start_ai_search_request`, around line ~1369)

**Step 1: Write the request spawner**

This closely follows `start_ai_search_request` (lines 1179-1369) but with explain-specific prompt and completion handler. The completion handler opens a vertical split scratch buffer instead of parsing search results:

```rust
fn start_ai_explain_request(
    ctx: &mut compositor::Context,
    ai_config: helix_view::editor::AiConfig,
    doc_id: DocumentId,
    view_id: helix_view::ViewId,
    from: usize,
    to: usize,
    selected_text: String,
    file_contents: String,
    file_path: Option<PathBuf>,
    user_instructions: String,
) {
    let request_id = ctx.editor.next_ai_request_id();

    let context_files_content = discover_context_files(
        file_path.as_deref(),
        &ai_config.context_files,
    );

    let temp_path = std::env::temp_dir().join(format!("helix-ai-explain-{}", request_id));

    let prompt_text = build_explain_prompt(
        &user_instructions,
        &selected_text,
        &file_contents,
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
        label: "AI Explaining".to_string(),
        doc_id,
        original_from: from,
        original_to: to,
        original_text: selected_text.clone(),
        doc_version: 0,
        cancel_tx,
        streaming_text: Arc::clone(&streaming_text),
        tool_display: Arc::clone(&tool_display),
        started_at: tokio::time::Instant::now(),
        stop_ticker: Arc::clone(&stop_ticker),
    });

    let query_preview: String = if user_instructions.is_empty() {
        "Explain selection".to_string()
    } else {
        user_instructions.chars().take(80).collect()
    };
    ctx.editor.set_status(format!("Explain: \"{}\"", query_preview));

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

        let (stdout_collected, stdout_task) =
            spawn_stdout_processor(stdout_handle, Arc::clone(&streaming_text_clone), Arc::clone(&tool_display_clone), provider);

        let cancel_result: anyhow::Result<()> = tokio::select! {
            _ = async {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
            } => { Ok(()) }
            _ = cancel_rx => {
                let _ = child.kill().await;
                let _ = std::fs::remove_file(&temp_path_clone);
                stop_ticker_clone.store(true, Ordering::Relaxed);
                anyhow::bail!("AI explain cancelled")
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
            anyhow::bail!("AI explain failed: {}", stderr_str);
        }

        let stdout_str = stdout_collected.lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        let result_text = if temp_path_clone.exists() {
            let content = std::fs::read_to_string(&temp_path_clone)
                .unwrap_or_else(|_| stdout_str.clone());
            let _ = std::fs::remove_file(&temp_path_clone);
            content
        } else {
            stdout_str
        };

        stop_ticker_clone.store(true, Ordering::Relaxed);

        if result_text.trim().is_empty() {
            let call: Callback = Callback::EditorCompositor(Box::new(
                move |editor: &mut helix_view::Editor, _compositor| {
                    editor.take_ai_request(request_id);
                    editor.set_error("AI returned empty explanation");
                },
            ));
            return Ok(call);
        }

        let call: Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut helix_view::Editor, _compositor| {
                editor.take_ai_request(request_id);
                show_explain_result(editor, &result_text);
            },
        ));
        Ok(call)
    };

    ctx.jobs.callback(future);
}
```

**Step 2: Write the `show_explain_result` helper**

Add this right after `start_ai_explain_request`:

```rust
/// Open the AI explanation in a vertical split scratch buffer with markdown highlighting.
fn show_explain_result(
    editor: &mut helix_view::Editor,
    explanation: &str,
) {
    let rope = helix_core::Rope::from(explanation);
    let doc = helix_view::Document::from(
        rope,
        None,
        editor.config.clone(),
        editor.syn_loader.clone(),
    );
    let doc_id = editor.new_file_from_document(
        helix_view::editor::Action::VerticalSplit,
        doc,
    );

    // Set markdown syntax highlighting
    let loader = editor.syn_loader.load();
    let doc = doc_mut!(editor, &doc_id);
    if let Err(e) = doc.set_language_by_language_id("markdown", &loader) {
        log::warn!("Failed to set markdown language for AI explain buffer: {}", e);
    }

    editor.set_status("AI explanation ready");
}
```

---

### Task 5: Add `start_ai_explain_request_from_compositor` public wrapper

**Files:**
- Modify: `helix-term/src/commands/ai.rs` (add near the other `_from_compositor` wrappers, around line ~873)

**Step 1: Add the public wrapper**

```rust
pub fn start_ai_explain_request_from_compositor(
    ctx: &mut compositor::Context,
    ai_config: helix_view::editor::AiConfig,
    doc_id: DocumentId,
    view_id: helix_view::ViewId,
    from: usize,
    to: usize,
    selected_text: String,
    file_contents: String,
    file_path: Option<PathBuf>,
    user_instructions: String,
) {
    start_ai_explain_request(
        ctx, ai_config, doc_id, view_id, from, to, selected_text, file_contents,
        file_path, user_instructions,
    );
}
```

---

### Task 6: Register the MappableCommand

**Files:**
- Modify: `helix-term/src/commands.rs`

**Step 1: Add to the MappableCommand list (around line 607)**

After the `ai_replace_selection` entry:
```rust
ai_replace_selection, "Replace selection using AI",
```
Add:
```rust
ai_explain_selection, "Explain selection using AI",
```

**Step 2: Add the dispatch function (after line ~6331)**

After the `ai_replace_selection` dispatch fn:
```rust
fn ai_explain_selection(cx: &mut Context) {
    ai::ai_explain_selection(cx);
}
```

---

### Task 7: Add `:ai-explain` typed command

**Files:**
- Modify: `helix-term/src/commands/typed.rs`

**Step 1: Add the handler function (after the `ai_search_cmd` function, around line ~2915)**

```rust
fn ai_explain(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let config = cx.editor.config();
    if !config.ai.enable {
        anyhow::bail!("AI features are disabled. Set enable = true under [editor.ai] in ai-config.toml");
    }

    let (view, doc) = current!(cx.editor);
    let text = doc.text().clone();
    let selection = doc.selection(view.id).clone();
    let primary = selection.primary();
    let from = primary.from();
    let to = primary.to();
    let selected_text: String = primary.fragment(text.slice(..)).into_owned();

    if selected_text.is_empty() {
        anyhow::bail!("No selection for AI explain");
    }

    let file_contents = text.to_string();
    let file_path = doc.path().cloned();
    let doc_id = doc.id();
    let view_id = view.id;
    let ai_config = config.ai.clone();

    let prompt = crate::ui::ai_prompt::AiPrompt::new(
        "AI Explain".to_string(),
        move |ctx: &mut compositor::Context, user_instructions: String| {
            super::ai::start_ai_explain_request_from_compositor(
                ctx,
                ai_config,
                doc_id,
                view_id,
                from,
                to,
                selected_text,
                file_contents,
                file_path,
                user_instructions,
            );
        },
    );

    let callback = async move {
        let call: crate::job::Callback =
            crate::job::Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                compositor.push(Box::new(crate::ui::overlay::overlaid(prompt)));
            }));
        Ok(call)
    };
    cx.jobs.callback(callback);

    Ok(())
}
```

**Step 2: Add the TypableCommand registration (after `:ai-search` entry, around line ~4033)**

```rust
TypableCommand {
    name: "ai-explain",
    aliases: &["aie"],
    doc: "Explain selected code using AI in a split pane.",
    fun: ai_explain,
    completer: CommandCompleter::none(),
    signature: Signature::DEFAULT,
},
```

---

### Task 8: Add `Space + E` keybinding

**Files:**
- Modify: `helix-term/src/keymap/default.rs` (around line 238)

**Step 1: Add the binding in the space menu**

After the existing `"A" => ai_replace_selection,` line (line 238), add:

```rust
"E" => ai_explain_selection,
```

Note: `"E"` is currently bound to `file_explorer_in_current_buffer_directory` (line 229). We need to check if lowercase `"e"` is already the file explorer. Looking at lines 228-229:
```
"e" => file_explorer,
"E" => file_explorer_in_current_buffer_directory,
```

So `"E"` is taken. We should use a different key. Options:
- `"X"` — eXplain (currently unbound in space menu)
- `"I"` — Info/explain (check if unbound)

Let me verify. Looking at the space menu keys used: f, F, e, E, b, j, s, S, d, D, g, a, A, '. Free keys include: c, h, i, k, l, m, n, o, p, q, r, t, u, v, w, x, y, z and uppercase variants.

Use `"X"` for eXplain. It's intuitive and unbound:

```rust
"X" => ai_explain_selection,
```

---

### Task 9: Build and test

**Step 1: Build the project**

Run: `cargo build` from the project root.

Fix any compile errors (missing imports, type mismatches).

**Step 2: Manual test**

1. Open a Rust file with `hxtuck`
2. Select a function with `v` + movement
3. Press `Space X`
4. Press Enter (general explanation) or type a question
5. Verify: inline progress appears (spinner, thinking, tools)
6. Verify: when complete, progress clears and vertical split opens with markdown-highlighted explanation
7. Verify: `:ai-explain` typed command works the same way
8. Verify: `:ai-cancel` cancels an in-progress explain request

**Step 3: Commit**

```bash
git add helix-term/src/commands/ai.rs helix-term/src/commands.rs helix-term/src/commands/typed.rs helix-term/src/keymap/default.rs helix-view/src/editor.rs
git commit -m "feat(ai): add AI explain mode with vertical split scratchpad"
```
