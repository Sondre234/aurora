//! The compositor side of the Aurora IPC (see the `aurora-ipc` crate for the protocol).
//!
//! A `Generic<UnixListener>` calloop source accepts clients; each client is a nonblocking
//! socket with its own calloop source, a frame `Decoder` and a bounded outbound queue
//! (`queue.rs`). Nothing here runs on the paint path, and nothing in the window manager
//! calls into it: once per loop turn, while someone is subscribed, `ipc_update` builds a
//! cheap snapshot (`snapshot.rs`), diffs it with the last one and broadcasts only what
//! changed to the clients subscribed to that topic.
//!
//! Safety of the socket: the directory is 0700 and the socket 0600, a stale socket from a
//! dead compositor is replaced but a live one (or anything that is not our socket) is never
//! touched, and every peer's `SO_PEERCRED` uid must be ours.
use std::{
    collections::HashMap,
    fs,
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

use aurora_ipc::{
    Body, Decoder, ErrorCode, Event, Frame, FrameError, HandshakeError, Topic, check_first_frame,
    encode,
};
use aurora_theme::ThemeSnapshot;
use queue::{Key, OutQueue, Push};
use smithay::reexports::calloop::{
    Interest, Mode, PostAction, RegistrationToken,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use snapshot::{Homes, assemble, collect, diff};

use crate::state::Aurora;

pub mod queue;
mod requests;
pub mod snapshot;
pub mod theme;

/// A client that has not said hello within this long is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Enough for every service and a few tools; a runaway client cannot take more.
const MAX_CLIENTS: usize = 32;
/// Bytes read from one client per loop turn; more is picked up by an idle callback so a
/// flooding client cannot starve the compositor.
const READ_BUDGET: usize = 256 * 1024;
/// Undecodable frames tolerated before the connection is dropped.
const MAX_BAD_FRAMES: u32 = 8;
const SERVER_NAME: &str = "aurora-comp";

/// A set of topics, one bit each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TopicSet(u8);

impl TopicSet {
    fn bit(topic: Topic) -> u8 {
        match topic {
            Topic::Workspaces => 1,
            Topic::Windows => 2,
            Topic::Focus => 4,
            Topic::Outputs => 8,
            Topic::Theme => 16,
            Topic::Config => 32,
        }
    }

    pub fn add(&mut self, topics: &[Topic]) {
        for t in topics {
            self.0 |= Self::bit(*t);
        }
    }

    pub fn remove(&mut self, topics: &[Topic]) {
        for t in topics {
            self.0 &= !Self::bit(*t);
        }
    }

    pub fn has(self, topic: Topic) -> bool {
        self.0 & Self::bit(topic) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

struct Client {
    stream: UnixStream,
    /// Absent only in tests, which have no loop.
    token: Option<RegistrationToken>,
    decoder: Decoder,
    /// Peer process id from `SO_PEERCRED`.
    pid: i32,
    /// Set by the hello; a client without one has not been announced.
    name: Option<String>,
    topics: TopicSet,
    out: OutQueue,
    /// To be dropped at the next sweep.
    closing: bool,
    bad_frames: u32,
}

/// The server's whole state.
pub struct Ipc {
    path: PathBuf,
    clients: HashMap<u64, Client>,
    next_key: u64,
    /// The state as last broadcast; `None` while nobody is subscribed.
    last: Option<aurora_ipc::Snapshot>,
    homes: Homes,
    /// The client whose callback is running: it cannot be removed from inside that callback.
    current: Option<u64>,
}

impl Drop for Ipc {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The key that identifies which piece of state an event is about, for coalescing.
fn key_of(event: &Event) -> Key {
    match event {
        Event::Snapshot(_) => Key::Snapshot,
        Event::OutputChanged(o) => Key::Output(o.name.clone()),
        Event::OutputRemoved { name } => Key::Output(name.clone()),
        Event::WorkspaceChanged(w) => Key::Workspace(w.output.clone(), w.index),
        Event::WorkspaceRemoved { output, index } => Key::Workspace(output.clone(), *index),
        Event::WindowChanged(w) => Key::Window(w.id),
        Event::WindowClosed { id } => Key::Window(*id),
        Event::FocusChanged { .. } => Key::Focus,
        Event::Theme(_) => Key::Theme,
        Event::ConfigReloaded { .. } => Key::Config,
    }
}

/// Free-form client names end up in logs: printable, bounded.
fn clean_name(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    if cleaned.trim().is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// Checks the first frame of a connection. `Ok` is the client's cleaned name; `Err` is the
/// error frame to answer with before closing.
pub fn handshake(frame: &Frame) -> Result<String, Box<Frame>> {
    match check_first_frame(frame) {
        Ok(hello) => Ok(clean_name(&hello.client)),
        Err(err @ HandshakeError::VersionMismatch { .. }) => Err(Box::new(Frame::error(
            0,
            ErrorCode::VersionMismatch,
            err.to_string(),
        ))),
        Err(err @ HandshakeError::NotHello) => Err(Box::new(Frame::error(
            0,
            ErrorCode::BadRequest,
            err.to_string(),
        ))),
    }
}

impl Ipc {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            clients: HashMap::new(),
            next_key: 0,
            last: None,
            homes: Homes::new(),
            current: None,
        }
    }

    fn add_client(
        &mut self,
        stream: UnixStream,
        pid: i32,
        token: Option<RegistrationToken>,
    ) -> u64 {
        self.next_key += 1;
        let key = self.next_key;
        self.clients.insert(
            key,
            Client {
                stream,
                token,
                decoder: Decoder::new(),
                pid,
                name: None,
                topics: TopicSet::default(),
                out: OutQueue::default(),
                closing: false,
                bad_frames: 0,
            },
        );
        key
    }

    fn has_subscribers(&self) -> bool {
        self.clients
            .values()
            .any(|c| !c.closing && !c.topics.is_empty())
    }

    fn announced(&self) -> usize {
        self.clients.values().filter(|c| c.name.is_some()).count()
    }

    /// Tries to write the client's queue; a failed socket marks it closing.
    fn flush(&mut self, key: u64) {
        let Some(c) = self.clients.get_mut(&key) else {
            return;
        };
        let mut w = &c.stream;
        if c.out.flush(&mut w).is_err() {
            c.closing = true;
        }
    }

    fn push_result(c: &mut Client, result: Push) {
        if result == Push::Close {
            c.closing = true;
        }
    }

    /// Queues a reply or error for `key`.
    fn send(&mut self, key: u64, frame: &Frame) {
        let Some(c) = self.clients.get_mut(&key) else {
            return;
        };
        match encode(frame) {
            Ok(bytes) => {
                let result = c.out.push_reliable(bytes);
                Self::push_result(c, result);
            }
            Err(err) => {
                tracing::warn!("ipc: cannot encode a reply: {err}");
                c.closing = true;
            }
        }
        self.flush(key);
    }

    /// A full snapshot for `key` (and the theme if it wants it), superseding whatever
    /// events are still queued.
    fn send_state(&mut self, key: u64, snapshot: &aurora_ipc::Snapshot, theme: &ThemeSnapshot) {
        let Some(c) = self.clients.get_mut(&key) else {
            return;
        };
        let snap = encode(&Frame::event(Event::Snapshot(snapshot.clone())));
        match snap {
            Ok(bytes) => {
                let result = c.out.push_snapshot(bytes);
                Self::push_result(c, result);
            }
            Err(err) => {
                tracing::warn!("ipc: cannot encode a snapshot: {err}");
                c.closing = true;
            }
        }
        if c.topics.has(Topic::Theme)
            && let Ok(bytes) = encode(&Frame::event(Event::Theme(theme.clone())))
        {
            let result = c.out.push_latest(Key::Theme, bytes);
            Self::push_result(c, result);
        }
        self.flush(key);
    }

    /// `Subscribe`: adds the topics and answers with the full state.
    fn subscribe(
        &mut self,
        key: u64,
        topics: &[Topic],
        snapshot: &aurora_ipc::Snapshot,
        theme: &ThemeSnapshot,
    ) {
        if let Some(c) = self.clients.get_mut(&key) {
            c.topics.add(topics);
        }
        self.send_state(key, snapshot, theme);
    }

    fn unsubscribe(&mut self, key: u64, topics: &[Topic]) {
        if let Some(c) = self.clients.get_mut(&key) {
            c.topics.remove(topics);
        }
    }

    /// Sends `events` to every client subscribed to their topic. A client whose queue
    /// overflows is resynced with the current snapshot (`self.last`) instead; one that
    /// cannot even take that is marked closing.
    fn broadcast(&mut self, events: &[Event], theme: &ThemeSnapshot) {
        let mut encoded = Vec::with_capacity(events.len());
        for e in events {
            match encode(&Frame::event(e.clone())) {
                Ok(bytes) => encoded.push((e.topic(), key_of(e), bytes)),
                Err(err) => tracing::warn!("ipc: cannot encode an event: {err}"),
            }
        }
        for topic in Topic::ALL {
            if !events.iter().any(|e| e.topic() == Some(topic)) {
                continue;
            }
            let clients = self
                .clients
                .values()
                .filter(|c| !c.closing && c.topics.has(topic))
                .count();
            if clients > 0 {
                tracing::info!("ipc: broadcast topic={topic:?} clients={clients}");
            }
        }
        let snapshot = self.last.clone().unwrap_or_default();
        let keys: Vec<u64> = self.clients.keys().copied().collect();
        for key in keys {
            let Some(c) = self.clients.get_mut(&key) else {
                continue;
            };
            if c.closing || c.topics.is_empty() {
                continue;
            }
            let mut resync = false;
            for (topic, k, bytes) in &encoded {
                if !topic.is_some_and(|t| c.topics.has(t)) {
                    continue;
                }
                match c.out.push_latest(k.clone(), bytes.clone()) {
                    Push::Queued | Push::Coalesced => {}
                    Push::Resync => {
                        resync = true;
                        break;
                    }
                    Push::Close => {
                        c.closing = true;
                        break;
                    }
                }
            }
            if resync {
                tracing::info!(
                    "ipc: slow client name={} resynced",
                    c.name.as_deref().unwrap_or("?")
                );
                self.send_state(key, &snapshot, theme);
            } else {
                self.flush(key);
            }
        }
    }
}

/// `SO_PEERCRED` of a connected unix socket: the peer's process id and uid.
fn peer_cred(stream: &UnixStream) -> io::Result<(i32, u32)> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // Safety: `cred` and `len` are valid for the size getsockopt writes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc == 0 {
        Ok((cred.pid, cred.uid))
    } else {
        Err(io::Error::last_os_error())
    }
}

fn our_uid() -> u32 {
    // Safety: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Creates the listening socket at `path`: parent directory 0700 if we create it (or if it
/// is our own `aurora` directory), a stale socket replaced, the socket itself 0600.
fn bind_socket(path: &Path) -> Result<UnixListener, String> {
    let uid = our_uid();
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        match fs::metadata(dir) {
            Ok(meta) => {
                let ours_by_name = dir.file_name().is_some_and(|n| n == "aurora");
                if ours_by_name {
                    if meta.uid() != uid {
                        return Err(format!("{} belongs to another user", dir.display()));
                    }
                    if meta.permissions().mode() & 0o077 != 0 {
                        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                            .map_err(|e| format!("cannot chmod {}: {e}", dir.display()))?;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
            Err(e) => return Err(format!("cannot inspect {}: {e}", dir.display())),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == uid => {
            // A compositor that is still alive answers; only a dead one's leftover goes.
            match UnixStream::connect(path) {
                Ok(_) => return Err("another compositor is listening there".into()),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(path)
                        .map_err(|e| format!("cannot remove the stale socket: {e}"))?;
                }
                Err(e) => return Err(format!("cannot probe the existing socket: {e}")),
            }
        }
        Ok(_) => return Err("the path exists and is not a socket of ours".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot inspect the socket path: {e}")),
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("cannot bind: {e}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot chmod the socket: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("cannot set nonblocking: {e}"))?;
    Ok(listener)
}

/// What became of a client after its socket was serviced.
enum Io {
    Keep,
    /// Dropped; the token is the source to remove when not inside its own callback.
    Gone(Option<RegistrationToken>),
}

enum Incoming {
    Frame(Frame),
    Bad(String),
}

impl Aurora {
    /// Opens the IPC socket and starts accepting. On any failure IPC stays off and the
    /// compositor runs without it.
    pub fn ipc_start(&mut self) {
        let Some(path) = aurora_ipc::socket_path() else {
            return tracing::warn!(
                "ipc: disabled, neither AURORA_IPC_SOCK nor XDG_RUNTIME_DIR is set"
            );
        };
        let listener = match bind_socket(&path) {
            Ok(l) => l,
            Err(err) => {
                return tracing::warn!("ipc: disabled, {}: {err}", path.display());
            }
        };
        let source = Generic::new(listener, Interest::READ, Mode::Level);
        let result = self.handle.insert_source(source, |_, listener, state| {
            state.ipc_accept(listener);
            Ok(PostAction::Continue)
        });
        if let Err(err) = result {
            let _ = fs::remove_file(&path);
            return tracing::warn!("ipc: disabled, cannot register the listener: {err}");
        }
        tracing::info!("ipc: listening path={}", path.display());
        self.ipc = Some(Ipc::new(path));
    }

    pub fn ipc_socket_path(&self) -> Option<PathBuf> {
        self.ipc.as_ref().map(|i| i.path.clone())
    }

    /// Closes every client and removes the socket.
    pub fn ipc_shutdown(&mut self) {
        self.ipc = None;
    }

    fn ipc_accept(&mut self, listener: &UnixListener) {
        loop {
            match listener.accept() {
                Ok((stream, _)) => self.ipc_add_client(stream),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return tracing::warn!("ipc: accept failed: {e}"),
            }
        }
    }

    fn ipc_add_client(&mut self, stream: UnixStream) {
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        let pid = match peer_cred(&stream) {
            Ok((pid, uid)) if uid == our_uid() => pid,
            Ok((_, uid)) => return tracing::warn!("ipc: refused a client of uid {uid}"),
            Err(e) => return tracing::warn!("ipc: cannot read peer credentials: {e}"),
        };
        if ipc.clients.len() >= MAX_CLIENTS {
            return tracing::warn!("ipc: refused a client, {MAX_CLIENTS} are connected");
        }
        let reader = match stream
            .set_nonblocking(true)
            .and_then(|()| stream.try_clone())
        {
            Ok(r) => r,
            Err(e) => return tracing::warn!("ipc: cannot set up a client socket: {e}"),
        };
        let key = ipc.add_client(stream, pid, None);
        // Edge triggered: every readiness is drained until the socket would block.
        let source = Generic::new(reader, Interest::BOTH, Mode::Edge);
        let token = self.handle.insert_source(source, move |_, _, state| {
            Ok(match state.ipc_client_io(key) {
                Io::Keep => PostAction::Continue,
                Io::Gone(_) => PostAction::Remove,
            })
        });
        match token {
            Ok(token) => {
                if let Some(c) = self.ipc.as_mut().and_then(|i| i.clients.get_mut(&key)) {
                    c.token = Some(token);
                }
            }
            Err(e) => {
                if let Some(ipc) = self.ipc.as_mut() {
                    ipc.clients.remove(&key);
                }
                return tracing::warn!("ipc: cannot register a client: {e}");
            }
        }
        // Nobody who stays silent keeps a slot.
        let timer = self.handle.insert_source(
            Timer::from_duration(HANDSHAKE_TIMEOUT),
            move |_, _, state| {
                state.ipc_handshake_expired(key);
                TimeoutAction::Drop
            },
        );
        if let Err(e) = timer {
            tracing::warn!("ipc: cannot arm the handshake timer: {e}");
        }
    }

    fn ipc_handshake_expired(&mut self, key: u64) {
        if let Some(c) = self.ipc.as_mut().and_then(|i| i.clients.get_mut(&key))
            && c.name.is_none()
        {
            c.closing = true;
            tracing::info!("ipc: dropped a client that sent no hello");
        }
        self.ipc_sweep();
    }

    /// An idle callback continuing a read that hit its budget.
    fn ipc_continue(&mut self, key: u64) {
        if let Io::Gone(Some(token)) = self.ipc_client_io(key) {
            self.handle.remove(token);
        }
    }

    /// Services one client: flush what is queued, read and decode what arrived, run the
    /// frames. Called from the client's readiness callback (and its continuation).
    fn ipc_client_io(&mut self, key: u64) -> Io {
        let mut incoming = Vec::new();
        let mut more = false;
        {
            let Some(c) = self.ipc.as_mut().and_then(|i| i.clients.get_mut(&key)) else {
                return Io::Gone(None);
            };
            let mut w = &c.stream;
            if c.out.flush(&mut w).is_err() {
                c.closing = true;
            }
            let mut buf = [0u8; 16 * 1024];
            let mut total = 0;
            while !c.closing {
                if total >= READ_BUDGET {
                    more = true;
                    break;
                }
                let mut r = &c.stream;
                match r.read(&mut buf) {
                    Ok(0) => {
                        c.closing = true;
                    }
                    Ok(n) => {
                        total += n;
                        c.decoder.feed(&buf[..n]);
                        loop {
                            match c.decoder.next_frame() {
                                Ok(Some(frame)) => incoming.push(Incoming::Frame(frame)),
                                Ok(None) => break,
                                Err(FrameError::Decode(e)) => {
                                    incoming.push(Incoming::Bad(format!("undecodable frame: {e}")));
                                }
                                Err(e) => {
                                    // Oversize: the stream cannot be resynchronized.
                                    incoming.push(Incoming::Bad(e.to_string()));
                                    c.closing = true;
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => c.closing = true,
                }
            }
        }

        if let Some(ipc) = self.ipc.as_mut() {
            ipc.current = Some(key);
        }
        for item in incoming {
            let closing = self
                .ipc
                .as_ref()
                .and_then(|i| i.clients.get(&key))
                .is_none_or(|c| c.closing);
            // A poisoned stream still gets its error frame out below.
            match item {
                Incoming::Frame(_) if closing => break,
                Incoming::Frame(frame) => self.ipc_frame(key, frame),
                Incoming::Bad(msg) => self.ipc_bad_frame(key, msg),
            }
        }
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.current = None;
        }

        let mut gone = None;
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.flush(key);
            match ipc.clients.get(&key) {
                Some(c) if c.closing => {
                    gone = Some(c.token);
                    Self::log_gone(ipc, key);
                }
                Some(_) => {}
                None => return Io::Gone(None),
            }
        }
        if let Some(token) = gone {
            return Io::Gone(token);
        }
        if more {
            self.handle
                .insert_idle(move |state| state.ipc_continue(key));
        }
        self.ipc_sweep();
        Io::Keep
    }

    /// Removes `key` from the table and logs it (only announced clients are logged).
    fn log_gone(ipc: &mut Ipc, key: u64) {
        if let Some(c) = ipc.clients.remove(&key)
            && let Some(name) = c.name
        {
            tracing::info!("ipc: client gone name={name}");
        }
    }

    /// Drops every client marked closing except the one whose callback is running (that
    /// one is dropped by its own callback returning).
    fn ipc_sweep(&mut self) {
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        let dead: Vec<u64> = ipc
            .clients
            .iter()
            .filter(|(k, c)| c.closing && Some(**k) != ipc.current)
            .map(|(k, _)| *k)
            .collect();
        for key in dead {
            let token = ipc.clients.get(&key).and_then(|c| c.token);
            Self::log_gone(ipc, key);
            if let Some(token) = token {
                self.handle.remove(token);
            }
        }
    }

    fn ipc_bad_frame(&mut self, key: u64, msg: String) {
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        let Some(c) = ipc.clients.get_mut(&key) else {
            return;
        };
        c.bad_frames += 1;
        if c.bad_frames > MAX_BAD_FRAMES {
            c.closing = true;
        }
        ipc.send(key, &Frame::error(0, ErrorCode::BadRequest, msg));
    }

    fn ipc_frame(&mut self, key: u64, frame: Frame) {
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        let Some(c) = ipc.clients.get_mut(&key) else {
            return;
        };
        if c.name.is_none() {
            match handshake(&frame) {
                Ok(name) => {
                    let proto = match &frame.body {
                        Body::Hello(h) => h.proto_version,
                        _ => 0,
                    };
                    tracing::info!("ipc: client connected name={name} proto={proto}");
                    c.name = Some(name);
                    ipc.send(key, &Frame::hello(SERVER_NAME));
                }
                Err(error) => {
                    c.closing = true;
                    ipc.send(key, &error);
                }
            }
            return;
        }
        let pid = c.pid;
        let id = frame.id;
        match frame.body {
            Body::Request(request) => {
                if id == 0 {
                    ipc.send(
                        key,
                        &Frame::error(0, ErrorCode::BadRequest, "a request needs a nonzero id"),
                    );
                    return;
                }
                let reply = match self.ipc_request(pid, request) {
                    Ok(response) => Frame::new(id, Body::Response(response)),
                    Err((code, message)) => Frame::error(id, code, message),
                };
                if let Some(ipc) = self.ipc.as_mut() {
                    ipc.send(key, &reply);
                }
            }
            Body::Subscribe(topics) => {
                // Bring everyone else up to date first, so the snapshot and the deltas
                // that follow it neither overlap nor leave a gap.
                let snapshot = self.ipc_current_snapshot();
                let theme = self.theme.clone();
                if let Some(ipc) = self.ipc.as_mut() {
                    ipc.subscribe(key, &topics, &snapshot, &theme);
                }
            }
            Body::Unsubscribe(topics) => ipc.unsubscribe(key, &topics),
            Body::Hello(_) => ipc.send(
                key,
                &Frame::error(id, ErrorCode::BadRequest, "hello was already sent"),
            ),
            Body::Response(_) | Body::Error(_) | Body::Event(_) => ipc.send(
                key,
                &Frame::error(id, ErrorCode::BadRequest, "clients send requests only"),
            ),
        }
    }

    /// A fresh snapshot of the compositor state, without touching the broadcast baseline.
    pub(super) fn ipc_fresh_snapshot(&mut self) -> aurora_ipc::Snapshot {
        let inputs = collect(self);
        let mut homes = self
            .ipc
            .as_mut()
            .map(|i| std::mem::take(&mut i.homes))
            .unwrap_or_default();
        let snapshot = assemble(&inputs, &mut homes);
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.homes = homes;
        }
        snapshot
    }

    /// The state clients are consistent with: everything broadcast so far is applied first.
    pub(super) fn ipc_current_snapshot(&mut self) -> aurora_ipc::Snapshot {
        self.ipc_update();
        if let Some(last) = self.ipc.as_ref().and_then(|i| i.last.clone()) {
            return last;
        }
        let snapshot = self.ipc_fresh_snapshot();
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.last = Some(snapshot.clone());
        }
        snapshot
    }

    /// Runs once per loop turn (next to `finish_slides`): if anyone is subscribed, builds a
    /// snapshot, diffs it with the last broadcast and sends the changes.
    pub fn ipc_update(&mut self) {
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        if !ipc.has_subscribers() {
            ipc.last = None;
            self.ipc_sweep();
            return;
        }
        let inputs = collect(self);
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        let new = assemble(&inputs, &mut ipc.homes);
        let events = match ipc.last.replace(new.clone()) {
            Some(old) => diff(&old, &new),
            None => Vec::new(),
        };
        if !events.is_empty() {
            ipc.broadcast(&events, &self.theme);
        }
        self.ipc_sweep();
    }

    /// Sends a one-off event (theme, config reload) to its subscribers.
    pub fn ipc_push(&mut self, event: Event) {
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        // The baseline a resync falls back to must exist and be current.
        if ipc.last.is_none() && ipc.has_subscribers() {
            let snapshot = self.ipc_fresh_snapshot();
            if let Some(ipc) = self.ipc.as_mut() {
                ipc.last = Some(snapshot);
            }
        }
        if let Some(ipc) = self.ipc.as_mut() {
            ipc.broadcast(&[event], &self.theme);
        }
        self.ipc_sweep();
    }

    /// Tells subscribers a reload finished. The messages are bounded so a config with
    /// hundreds of warnings cannot make a huge frame.
    pub fn ipc_config_reloaded(&mut self, ok: bool, warnings: Vec<String>) {
        let warnings = warnings
            .into_iter()
            .take(64)
            .map(|w| w.chars().take(512).collect())
            .collect();
        self.ipc_push(Event::ConfigReloaded { ok, warnings });
    }

    /// `dump: ipc clients=<n>`.
    pub fn dump_ipc(&self) {
        let clients = self.ipc.as_ref().map_or(0, Ipc::announced);
        tracing::info!("dump: ipc clients={clients}");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::atomic::{AtomicU32, Ordering},
    };

    use aurora_ipc::{OutputInfo, Request, WindowInfo};

    use super::*;

    fn pair() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        (a, b)
    }

    /// Everything the server sent, decoded.
    fn read_all(peer: &mut UnixStream) -> Vec<Frame> {
        read_with(peer, &mut Decoder::new())
    }

    /// Like `read_all`, for a stream read in several calls (a frame may be cut between).
    fn read_with(peer: &mut UnixStream, dec: &mut Decoder) -> Vec<Frame> {
        let mut buf = [0u8; 4096];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => dec.feed(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        let mut frames = Vec::new();
        while let Some(f) = dec.next_frame().unwrap() {
            frames.push(f);
        }
        frames
    }

    fn window(id: u64, title: &str) -> WindowInfo {
        WindowInfo {
            id,
            app_id: "a".into(),
            title: title.into(),
            workspace: 1,
            output: "A".into(),
            floating: false,
            fullscreen: false,
            urgent: false,
        }
    }

    fn server() -> Ipc {
        Ipc::new(PathBuf::from("/nonexistent/not-a-real.sock"))
    }

    fn events_of(frames: Vec<Frame>) -> Vec<Event> {
        frames
            .into_iter()
            .filter_map(|f| match f.body {
                Body::Event(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn topic_sets() {
        let mut t = TopicSet::default();
        assert!(t.is_empty());
        t.add(&[Topic::Windows, Topic::Theme]);
        assert!(t.has(Topic::Windows) && t.has(Topic::Theme) && !t.has(Topic::Focus));
        t.remove(&[Topic::Windows]);
        assert!(!t.has(Topic::Windows) && t.has(Topic::Theme));
        t.remove(&[Topic::Theme]);
        assert!(t.is_empty());
    }

    #[test]
    fn the_first_frame_must_be_a_matching_hello() {
        assert_eq!(handshake(&Frame::hello("shell")).unwrap(), "shell");
        assert_eq!(
            handshake(&Frame::hello("sh\u{1b}[31mell")).unwrap(),
            "sh[31mell"
        );
        assert_eq!(handshake(&Frame::hello("")).unwrap(), "unnamed");
        assert_eq!(handshake(&Frame::hello("x".repeat(500))).unwrap().len(), 64);

        let mut old = aurora_ipc::Hello::new("x");
        old.proto_version += 1;
        let Err(err) = handshake(&Frame::new(0, Body::Hello(old))) else {
            panic!("a newer protocol must be refused");
        };
        assert!(matches!(
            err.body,
            Body::Error(aurora_ipc::Error {
                code: ErrorCode::VersionMismatch,
                ..
            })
        ));
        let Err(err) = handshake(&Frame::request(1, Request::GetSnapshot)) else {
            panic!("a request before hello must be refused");
        };
        assert!(matches!(
            err.body,
            Body::Error(aurora_ipc::Error {
                code: ErrorCode::BadRequest,
                ..
            })
        ));
    }

    #[test]
    fn subscribing_sends_a_snapshot_then_only_subscribed_topics() {
        let mut ipc = server();
        let (a, mut a_peer) = pair();
        let (b, mut b_peer) = pair();
        let (ka, kb) = (ipc.add_client(a, 1, None), ipc.add_client(b, 2, None));
        let theme = ThemeSnapshot::default();
        let mut state = aurora_ipc::Snapshot::default();
        state.windows.push(window(1, "one"));
        ipc.last = Some(state.clone());
        ipc.subscribe(ka, &[Topic::Windows], &state, &theme);
        ipc.subscribe(kb, &[Topic::Focus, Topic::Theme], &state, &theme);

        let first_a = events_of(read_all(&mut a_peer));
        assert_eq!(first_a, vec![Event::Snapshot(state.clone())]);
        let first_b = events_of(read_all(&mut b_peer));
        assert_eq!(
            first_b,
            vec![Event::Snapshot(state.clone()), Event::Theme(theme.clone())]
        );

        ipc.broadcast(
            &[
                Event::WindowChanged(window(1, "renamed")),
                Event::FocusChanged {
                    window: Some(1),
                    output: None,
                },
                Event::OutputRemoved { name: "A".into() },
            ],
            &theme,
        );
        assert_eq!(
            events_of(read_all(&mut a_peer)),
            vec![Event::WindowChanged(window(1, "renamed"))]
        );
        assert_eq!(
            events_of(read_all(&mut b_peer)),
            vec![Event::FocusChanged {
                window: Some(1),
                output: None
            }]
        );

        ipc.unsubscribe(ka, &[Topic::Windows]);
        ipc.broadcast(&[Event::WindowClosed { id: 1 }], &theme);
        assert!(read_all(&mut a_peer).is_empty());
        assert!(ipc.has_subscribers());
    }

    #[test]
    fn replies_reach_the_client_in_order() {
        let mut ipc = server();
        let (a, mut peer) = pair();
        let k = ipc.add_client(a, 1, None);
        ipc.send(k, &Frame::hello(SERVER_NAME));
        ipc.send(k, &Frame::new(7, Body::Response(aurora_ipc::Response::Ok)));
        ipc.send(k, &Frame::error(8, ErrorCode::NotFound, "nope"));
        let frames = read_all(&mut peer);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1].id, 7);
        assert_eq!(frames[2].id, 8);
    }

    #[test]
    fn a_slow_subscriber_is_resynced_not_grown_without_bound() {
        let mut ipc = server();
        let (a, mut peer) = pair();
        let k = ipc.add_client(a, 1, None);
        let theme = ThemeSnapshot::default();
        let mut state = aurora_ipc::Snapshot::default();
        ipc.last = Some(state.clone());
        ipc.subscribe(k, &[Topic::Windows, Topic::Outputs], &state, &theme);

        // The peer never reads. Many distinct windows change: the socket buffer fills, the
        // queue passes its soft limit and the client is resynced.
        let mut last_title = String::new();
        for round in 0..40 {
            let mut events = Vec::new();
            last_title = format!("{round}-{}", "t".repeat(2000));
            for id in 0..200 {
                let w = window(id, &last_title);
                if round == 39 {
                    state.windows.retain(|x| x.id != id);
                    state.windows.push(w.clone());
                }
                events.push(Event::WindowChanged(w));
            }
            ipc.last = Some(state.clone());
            ipc.broadcast(&events, &theme);
            assert!(
                !ipc.clients[&k].closing,
                "a slow reader must be resynced, not cut off (round {round})"
            );
            assert!(ipc.clients[&k].out.bytes() <= queue::HARD_LIMIT);
        }
        // Once it reads, the stream is whole and ends consistent with the newest state.
        let mut local = aurora_ipc::Snapshot::default();
        let mut seen_snapshot = false;
        let mut dec = Decoder::new();
        for _ in 0..200 {
            ipc.flush(k);
            let events = events_of(read_with(&mut peer, &mut dec));
            for e in &events {
                seen_snapshot |= matches!(e, Event::Snapshot(_));
                local.apply(e);
            }
            if ipc.clients[&k].out.is_empty() && events.is_empty() {
                break;
            }
        }
        assert!(seen_snapshot, "an overflow is answered with a snapshot");
        assert_eq!(local.windows.len(), 200);
        assert!(local.windows.iter().all(|w| w.title == last_title));
    }

    #[test]
    fn a_client_that_never_reads_replies_is_cut_off() {
        let mut ipc = server();
        let (a, _peer) = pair();
        let k = ipc.add_client(a, 1, None);
        for _ in 0..200 {
            ipc.send(
                k,
                &Frame::new(
                    1,
                    Body::Response(aurora_ipc::Response::Outputs(vec![
                        OutputInfo {
                            name: "x".repeat(60_000),
                            x: 0,
                            y: 0,
                            width: 1,
                            height: 1,
                            scale: 1.0,
                        };
                        1
                    ])),
                ),
            );
            if ipc.clients[&k].closing {
                return;
            }
        }
        panic!("unread replies must end in a close");
    }

    #[test]
    fn a_dead_peer_marks_the_client_closing() {
        let mut ipc = server();
        let (a, peer) = pair();
        let k = ipc.add_client(a, 1, None);
        drop(peer);
        ipc.send(k, &Frame::hello(SERVER_NAME));
        assert!(ipc.clients[&k].closing);
    }

    #[test]
    fn peer_credentials_are_ours_on_a_socketpair() {
        let (a, _b) = pair();
        let (pid, uid) = peer_cred(&a).unwrap();
        assert_eq!(uid, our_uid());
        assert_eq!(pid, std::process::id() as i32);
    }

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "aurora-ipc-test-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bind_creates_a_private_directory_and_socket() {
        let root = scratch("bind");
        let path = root.join("run").join("aurora").join("ipc.sock");
        let listener = bind_socket(&path).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        drop(listener);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bind_tightens_an_existing_aurora_directory() {
        let root = scratch("tighten");
        let dir = root.join("aurora");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let _l = bind_socket(&dir.join("ipc.sock")).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bind_replaces_a_stale_socket() {
        let root = scratch("stale");
        let path = root.join("ipc.sock");
        drop(UnixListener::bind(&path).unwrap()); // leaves the file, nobody listens
        assert!(path.exists());
        let l = bind_socket(&path).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        drop(l);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bind_never_takes_over_a_live_socket() {
        let root = scratch("live");
        let path = root.join("ipc.sock");
        let live = bind_socket(&path).unwrap();
        let err = bind_socket(&path).unwrap_err();
        assert!(err.contains("another compositor"), "{err}");
        // And the live one still works.
        assert!(UnixStream::connect(&path).is_ok());
        drop(live);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bind_refuses_to_replace_anything_that_is_not_a_socket() {
        let root = scratch("file");
        let path = root.join("ipc.sock");
        fs::write(&path, b"precious").unwrap();
        assert!(bind_socket(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"precious");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dropping_the_server_removes_the_socket_file() {
        let root = scratch("drop");
        let path = root.join("ipc.sock");
        let listener = bind_socket(&path).unwrap();
        let ipc = Ipc::new(path.clone());
        drop(ipc);
        assert!(!path.exists());
        drop(listener);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn keys_identify_state_not_event_kind() {
        let w = window(3, "t");
        assert_eq!(
            key_of(&Event::WindowChanged(w)),
            key_of(&Event::WindowClosed { id: 3 })
        );
        assert_ne!(
            key_of(&Event::WindowClosed { id: 3 }),
            key_of(&Event::WindowClosed { id: 4 })
        );
        assert_eq!(
            key_of(&Event::WorkspaceRemoved {
                output: "A".into(),
                index: 2
            }),
            Key::Workspace("A".into(), 2)
        );
    }

    #[test]
    fn writing_to_a_full_socket_just_queues() {
        let mut ipc = server();
        let (a, _peer) = pair();
        let k = ipc.add_client(a, 1, None);
        // Fill the kernel buffer by hand, then queue more: nothing fails, it waits.
        let c = ipc.clients.get_mut(&k).unwrap();
        let chunk = vec![0u8; 64 * 1024];
        loop {
            match (&c.stream).write(&chunk) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        ipc.send(k, &Frame::hello(SERVER_NAME));
        let c = &ipc.clients[&k];
        assert!(!c.closing);
        assert!(c.out.bytes() > 0);
    }
}
