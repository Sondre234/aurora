//! Extension point for a local-LLM assistant. Documented only: nothing implements or
//! calls this yet, and the launcher never touches the network.
//!
//! The idea is that a provider can contribute extra rows under the app results for a
//! free-form query ("convert 5 miles to km", "what does chmod 755 do"). A future
//! implementation must keep the launcher's contract:
//!
//! - **Local only.** Talk to a model running on this machine (a unix socket or a spawned
//!   child), never to a remote service.
//! - **Never on the UI thread.** [`LlmHook::suggest`] is called from a worker thread; the
//!   launcher shows app results immediately and appends suggestions when they arrive.
//! - **Cancel by supersession.** Each keystroke starts a new query; answers for an older
//!   `query` are dropped by the caller, so an implementation may stop early when
//!   [`LlmHook::cancelled`] returns true.
//! - **Actions go through IPC.** A suggestion's [`Action::Spawn`] is launched like an app,
//!   through the compositor's `Spawn` request.

/// What choosing a suggestion does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Run a program (`argv[0]` from `PATH`, no shell), like launching an app.
    Spawn(Vec<String>),
    /// Copy text to the clipboard (the launcher would run `wl-copy`).
    Copy(String),
}

/// One extra result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub title: String,
    pub subtitle: Option<String>,
    pub action: Action,
}

/// A source of suggestions beyond the app index.
pub trait LlmHook: Send {
    /// Short provider name for logs.
    fn name(&self) -> &str;

    /// Suggestions for `query`, best first. Blocking is fine (worker thread), but return
    /// early once [`LlmHook::cancelled`] is true.
    fn suggest(&mut self, query: &str) -> Vec<Suggestion>;

    /// True when the launcher no longer wants the answer for the query being processed.
    fn cancelled(&self) -> bool {
        false
    }
}
