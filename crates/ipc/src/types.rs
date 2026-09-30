//! Wire types. Append-only evolution: see the crate docs.

use serde::{Deserialize, Serialize};

use crate::{PROTO_VERSION, Theme, ThemeSnapshot};

/// Window identity, unique for the life of the compositor and never reused.
pub type WindowId = u64;

/// One message on the wire. `id` pairs a `Request` with its `Response`/`Error`; it is
/// nonzero for requests and 0 for everything else (events, hello, subscribe).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub id: u64,
    pub body: Body,
}

impl Frame {
    pub fn new(id: u64, body: Body) -> Self {
        Self { id, body }
    }

    pub fn hello(client: impl Into<String>) -> Self {
        Self::new(0, Body::Hello(Hello::new(client)))
    }

    pub fn request(id: u64, request: Request) -> Self {
        Self::new(id, Body::Request(request))
    }

    pub fn event(event: Event) -> Self {
        Self::new(0, Body::Event(event))
    }

    pub fn error(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(
            id,
            Body::Error(Error {
                code,
                message: message.into(),
            }),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Body {
    Hello(Hello),
    Request(Request),
    Response(Response),
    Error(Error),
    Event(Event),
    Subscribe(Vec<Topic>),
    Unsubscribe(Vec<Topic>),
}

/// First frame in each direction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub proto_version: u32,
    /// Free-form identifier for logs (`"shell"`, `"launcher"`, `"auroractl"`, `"aurora-comp"`).
    pub client: String,
}

impl Hello {
    pub fn new(client: impl Into<String>) -> Self {
        Self {
            proto_version: PROTO_VERSION,
            client: client.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// The first frame was not a `Hello`.
    NotHello,
    /// The peer speaks another protocol version.
    VersionMismatch { ours: u32, theirs: u32 },
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotHello => write!(f, "first frame was not a hello"),
            Self::VersionMismatch { ours, theirs } => {
                write!(
                    f,
                    "protocol version mismatch (ours {ours}, theirs {theirs})"
                )
            }
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Accepts a peer's hello only when its version equals [`PROTO_VERSION`].
pub fn check_hello(hello: &Hello) -> Result<(), HandshakeError> {
    if hello.proto_version == PROTO_VERSION {
        Ok(())
    } else {
        Err(HandshakeError::VersionMismatch {
            ours: PROTO_VERSION,
            theirs: hello.proto_version,
        })
    }
}

/// Validates the first frame of a connection: it must be a compatible `Hello`.
pub fn check_first_frame(frame: &Frame) -> Result<&Hello, HandshakeError> {
    match &frame.body {
        Body::Hello(h) => check_hello(h).map(|()| h),
        _ => Err(HandshakeError::NotHello),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Topic {
    Workspaces,
    Windows,
    Focus,
    Outputs,
    Theme,
    Config,
}

impl Topic {
    pub const ALL: [Topic; 6] = [
        Topic::Workspaces,
        Topic::Windows,
        Topic::Focus,
        Topic::Outputs,
        Topic::Theme,
        Topic::Config,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Malformed or semantically invalid request.
    BadRequest,
    /// The named window, output or workspace does not exist.
    NotFound,
    /// The compositor refuses (for example unlocking over IPC).
    Denied,
    /// Not supported by this compositor build or in the current state.
    Unsupported,
    Internal,
    VersionMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

// ---- state ----

/// Logical geometry and scale of one output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    /// Logical (scaled) size in px.
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// A workspace on one output. Workspace numbers are per output and start at 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub output: String,
    pub index: u32,
    /// The workspace currently shown on its output.
    pub active: bool,
    /// Number of windows on it (0 means empty).
    pub windows: u32,
    /// Any window on it is urgent.
    pub urgent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub app_id: String,
    pub title: String,
    pub workspace: u32,
    pub output: String,
    pub floating: bool,
    pub fullscreen: bool,
    pub urgent: bool,
}

/// The whole observable state. Deltas (`Event`) apply to it with [`Snapshot::apply`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub outputs: Vec<OutputInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub windows: Vec<WindowInfo>,
    pub focused_window: Option<WindowId>,
    pub active_output: Option<String>,
}

impl Snapshot {
    /// Applies one delta. `Event::Snapshot` replaces everything; events for other topics
    /// (`Theme`, `Config`) are ignored. Upserts keep list order; removals of unknown
    /// items are no-ops, so a client that joined mid-stream stays consistent.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Snapshot(s) => *self = s.clone(),
            Event::OutputChanged(o) => upsert(&mut self.outputs, o.clone(), |x| x.name == o.name),
            Event::OutputRemoved { name } => self.outputs.retain(|o| &o.name != name),
            Event::WorkspaceChanged(w) => upsert(&mut self.workspaces, w.clone(), |x| {
                x.output == w.output && x.index == w.index
            }),
            Event::WorkspaceRemoved { output, index } => self
                .workspaces
                .retain(|w| !(&w.output == output && w.index == *index)),
            Event::WindowChanged(w) => upsert(&mut self.windows, w.clone(), |x| x.id == w.id),
            Event::WindowClosed { id } => {
                self.windows.retain(|w| w.id != *id);
                if self.focused_window == Some(*id) {
                    self.focused_window = None;
                }
            }
            Event::FocusChanged { window, output } => {
                self.focused_window = *window;
                self.active_output = output.clone();
            }
            Event::Theme(_) | Event::ConfigReloaded { .. } => {}
        }
    }
}

fn upsert<T>(list: &mut Vec<T>, item: T, same: impl Fn(&T) -> bool) {
    match list.iter_mut().find(|x| same(x)) {
        Some(slot) => *slot = item,
        None => list.push(item),
    }
}

// ---- events ----

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// Full state, sent right after `Subscribe`.
    Snapshot(Snapshot),
    /// Output added or changed (geometry, scale).
    OutputChanged(OutputInfo),
    OutputRemoved {
        name: String,
    },
    /// Workspace added or any field changed.
    WorkspaceChanged(WorkspaceInfo),
    WorkspaceRemoved {
        output: String,
        index: u32,
    },
    /// Window opened or any field changed (title, workspace, urgent, ...).
    WindowChanged(WindowInfo),
    WindowClosed {
        id: WindowId,
    },
    FocusChanged {
        window: Option<WindowId>,
        output: Option<String>,
    },
    Theme(ThemeSnapshot),
    /// Result of a config reload. `warnings` are the config ladder's messages.
    ConfigReloaded {
        ok: bool,
        warnings: Vec<String>,
    },
}

impl Event {
    /// The topic a client must subscribe to for this event. `Snapshot` belongs to none
    /// and is always delivered after `Subscribe`.
    pub fn topic(&self) -> Option<Topic> {
        match self {
            Event::Snapshot(_) => None,
            Event::OutputChanged(_) | Event::OutputRemoved { .. } => Some(Topic::Outputs),
            Event::WorkspaceChanged(_) | Event::WorkspaceRemoved { .. } => Some(Topic::Workspaces),
            Event::WindowChanged(_) | Event::WindowClosed { .. } => Some(Topic::Windows),
            Event::FocusChanged { .. } => Some(Topic::Focus),
            Event::Theme(_) => Some(Topic::Theme),
            Event::ConfigReloaded { .. } => Some(Topic::Config),
        }
    }
}

// ---- requests ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverviewAction {
    Toggle,
    Open,
    Close,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    GetSnapshot,
    ListWindows,
    ListOutputs,
    GetTheme,
    /// Show workspace `index` (1-based) on `output`, or on the active output if `None`.
    SwitchWorkspace {
        output: Option<String>,
        index: u32,
    },
    FocusWindow {
        id: WindowId,
    },
    /// Close `id`, or the focused window if `None`.
    CloseWindow {
        id: Option<WindowId>,
    },
    /// Run a program through the compositor's spawn (activation token, `spawn_env`).
    /// `argv[0]` is the program, looked up in `PATH`; no shell is involved.
    Spawn {
        argv: Vec<String>,
    },
    ReloadConfig,
    Overview(OverviewAction),
    /// Replace the live theme (not persisted).
    SetTheme(Theme),
    /// Ask the compositor to start the lock client. Fire and forget.
    Lock,
    /// Unlocking is only ever honored from the session-lock client itself; the
    /// compositor answers every IPC `Unlock` with `ErrorCode::Denied` unless the
    /// connection is that client's. Present so the lock service can tell the compositor
    /// its authentication succeeded before it destroys the lock object.
    Unlock,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Snapshot(Snapshot),
    Windows(Vec<WindowInfo>),
    Outputs(Vec<OutputInfo>),
    Theme(ThemeSnapshot),
}
