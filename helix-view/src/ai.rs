use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::DocumentId;

/// Monotonic identifier for AI requests.
pub type RequestId = u64;

/// Tracks an in-flight AI request
pub struct AiRequestState {
    /// Unique identifier for this request
    pub id: RequestId,
    /// Label shown in the statusline (e.g. "AI Searching", "AI")
    pub label: String,
    /// The document this request targets
    pub doc_id: DocumentId,
    /// Start of the original selection (char offset)
    pub original_from: usize,
    /// End of the original selection (char offset)
    pub original_to: usize,
    /// The original selected text content
    pub original_text: String,
    /// The document version at request time (for staleness detection)
    pub doc_version: i32,
    /// Channel to signal cancellation to the async task
    pub cancel_tx: tokio::sync::oneshot::Sender<()>,
    /// Streaming thinking/text from AI CLI stdout (shared with async task)
    pub streaming_text: Arc<Mutex<String>>,
    /// Current tool call display (e.g. "→ Read: src/main.rs"), separate from thinking
    pub tool_display: Arc<Mutex<String>>,
    /// When the request started (for spinner frame calculation)
    pub started_at: tokio::time::Instant,
    /// Signal to stop the spinner animation ticker
    pub stop_ticker: Arc<AtomicBool>,
}

impl Drop for AiRequestState {
    fn drop(&mut self) {
        self.stop_ticker.store(true, Ordering::Relaxed);
    }
}
