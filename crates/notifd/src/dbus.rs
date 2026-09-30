//! `org.freedesktop.Notifications` (spec 1.2) over zbus.
//!
//! Threading: zbus runs the object server on its own executor thread, which sleeps in
//! `epoll` until a message arrives. Method calls never touch UI state; they do the two
//! things that must be synchronous (assign the id, convert the arguments) and hand a
//! [`Command`] to the `sink`, which in the daemon posts it into the calloop loop. The
//! Wayland thread therefore wakes only for real work and the paint path stays idle.
//! Signals go the other way: the main loop calls [`Server::emit_closed`] and
//! [`Server::emit_action`], which write a few bytes to the socket.
//!
//! Name policy: the well-known name is requested with `DoNotQueue` and never
//! `ReplaceExisting` unless `replace` is set, so starting notifd next to a running
//! notification daemon fails loudly ([`StartError::NameTaken`]) instead of stealing the
//! name. We always offer `AllowReplacement` so a later daemon can take over cleanly; when
//! that happens the sink receives [`Command::NameLost`].

use std::collections::HashMap;
use std::sync::Arc;

use zbus::blocking::{Connection, connection::Builder, fdo::DBusProxy};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::interface;
use zbus::names::BusName;
use zbus::zvariant::{Structure, Value};

use crate::hints::{Hint, Hints, RawImage};
use crate::state::{CloseReason, IdGen, NotifyArgs};

pub const NAME: &str = "org.freedesktop.Notifications";
pub const PATH: &str = "/org/freedesktop/Notifications";
pub const SPEC_VERSION: &str = "1.2";

/// Capabilities advertised. No `persistence`: notifications are not kept past their toast.
pub const CAPABILITIES: [&str; 4] = ["body", "actions", "body-markup", "icon-static"];

/// Inputs from the bus, consumed on the main loop.
#[derive(Debug)]
pub enum Command {
    Notify(Box<NotifyArgs>),
    /// `CloseNotification(id)`.
    Close(u32),
    /// Another daemon replaced us as owner of the name.
    NameLost,
}

pub type Sink = Arc<dyn Fn(Command) + Send + Sync>;

#[derive(Debug)]
pub enum StartError {
    /// Could not connect to the bus or export the object.
    Connect(String),
    /// Somebody else owns the name and we were not asked (or not allowed) to replace it.
    NameTaken {
        owner: Option<String>,
    },
    Request(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Connect(e) => write!(f, "cannot connect to the D-Bus session bus: {e}"),
            StartError::NameTaken { owner } => write!(
                f,
                "{NAME} is already owned by {}; not taking it over (pass --replace to ask \
                 the owner to leave)",
                owner.as_deref().unwrap_or("another process")
            ),
            StartError::Request(e) => write!(f, "cannot request {NAME}: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

struct Iface {
    ids: Arc<IdGen>,
    sink: Sink,
}

/// Largest `image-data` payload accepted (bytes).
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

fn int(v: &Value<'_>) -> Option<i64> {
    Some(match v {
        Value::U8(n) => *n as i64,
        Value::I16(n) => *n as i64,
        Value::U16(n) => *n as i64,
        Value::I32(n) => *n as i64,
        Value::U32(n) => *n as i64,
        Value::I64(n) => *n,
        Value::U64(n) => i64::try_from(*n).unwrap_or(i64::MAX),
        _ => return None,
    })
}

fn image(s: &Structure<'_>) -> Option<RawImage> {
    let f = s.fields();
    if f.len() != 7 {
        return None;
    }
    let i32_at = |i: usize| int(f.get(i)?).and_then(|n| i32::try_from(n).ok());
    let Value::Bool(has_alpha) = f[3] else {
        return None;
    };
    let Value::Array(bytes) = &f[6] else {
        return None;
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let data = bytes
        .iter()
        .map(|b| match b {
            Value::U8(n) => Some(*n),
            _ => None,
        })
        .collect::<Option<Vec<u8>>>()?;
    Some(RawImage {
        width: i32_at(0)?,
        height: i32_at(1)?,
        rowstride: i32_at(2)?,
        has_alpha,
        bits_per_sample: i32_at(4)?,
        channels: i32_at(5)?,
        data,
    })
}

/// Reduces a D-Bus variant to a [`Hint`].
pub fn to_hint(v: &Value<'_>) -> Hint {
    match v {
        Value::Bool(b) => Hint::Bool(*b),
        Value::Str(s) => Hint::Str(s.to_string()),
        Value::Value(inner) => to_hint(inner),
        Value::Structure(s) => image(s).map_or(Hint::Other, Hint::Image),
        other => int(other).map_or(Hint::Other, Hint::Int),
    }
}

#[interface(name = "org.freedesktop.Notifications")]
impl Iface {
    fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES.iter().map(|s| s.to_string()).collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        expire_timeout: i32,
    ) -> u32 {
        let id = self.ids.assign(replaces_id);
        let hints: HashMap<String, Hint> =
            hints.iter().map(|(k, v)| (k.clone(), to_hint(v))).collect();
        (self.sink)(Command::Notify(Box::new(NotifyArgs {
            id,
            app_name,
            app_icon,
            summary,
            body,
            actions,
            hints: Hints::parse(&hints),
            expire_timeout,
        })));
        id
    }

    fn close_notification(&self, id: u32) {
        (self.sink)(Command::Close(id));
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "aurora-notifd".into(),
            "aurora".into(),
            env!("CARGO_PKG_VERSION").into(),
            SPEC_VERSION.into(),
        )
    }
}

/// A live bus connection that owns the name.
pub struct Server {
    conn: Connection,
    /// Unique bus name of this connection (what `GetNameOwner` returns for [`NAME`]).
    pub owner: String,
}

impl Server {
    /// Connects (to `bus` when given, else the session bus from the environment), exports
    /// the interface and requests the name as described in the module docs.
    pub fn start(bus: Option<&str>, replace: bool, sink: Sink) -> Result<Server, StartError> {
        let builder = match bus {
            Some(addr) => Builder::address(addr),
            None => Builder::session(),
        }
        .map_err(|e| StartError::Connect(e.to_string()))?;
        let conn = builder
            .serve_at(
                PATH,
                Iface {
                    ids: Arc::new(IdGen::default()),
                    sink: sink.clone(),
                },
            )
            .and_then(Builder::build)
            .map_err(|e| StartError::Connect(e.to_string()))?;

        let flags = if replace {
            RequestNameFlags::DoNotQueue
                | RequestNameFlags::AllowReplacement
                | RequestNameFlags::ReplaceExisting
        } else {
            RequestNameFlags::DoNotQueue | RequestNameFlags::AllowReplacement
        };
        match conn.request_name_with_flags(NAME, flags) {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
            Ok(RequestNameReply::Exists | RequestNameReply::InQueue)
            | Err(zbus::Error::NameTaken) => {
                return Err(StartError::NameTaken {
                    owner: name_owner(&conn),
                });
            }
            Err(e) => return Err(StartError::Request(e.to_string())),
        }
        let owner = conn
            .unique_name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| NAME.to_string());
        watch_name_lost(&conn, sink);
        Ok(Server { conn, owner })
    }

    pub fn emit_closed(&self, id: u32, reason: CloseReason) {
        let r = self.conn.emit_signal(
            None::<&str>,
            PATH,
            NAME,
            "NotificationClosed",
            &(id, reason.code()),
        );
        if let Err(e) = r {
            tracing::warn!("notifd: cannot emit NotificationClosed: {e}");
        }
    }

    pub fn emit_action(&self, id: u32, key: &str) {
        let r = self
            .conn
            .emit_signal(None::<&str>, PATH, NAME, "ActionInvoked", &(id, key));
        if let Err(e) = r {
            tracing::warn!("notifd: cannot emit ActionInvoked: {e}");
        }
    }
}

fn name_owner(conn: &Connection) -> Option<String> {
    let proxy = DBusProxy::new(conn).ok()?;
    let name = BusName::try_from(NAME).ok()?;
    proxy.get_name_owner(name).ok().map(|o| o.to_string())
}

/// A sleeping thread that turns the bus's `NameLost` (sent only to the connection that
/// lost a name) into [`Command::NameLost`].
fn watch_name_lost(conn: &Connection, sink: Sink) {
    let Ok(proxy) = DBusProxy::new(conn) else {
        return;
    };
    let Ok(mut lost) = proxy.receive_name_lost() else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("notifd-namelost".into())
        .spawn(move || {
            if lost.next().is_some() {
                sink(Command::NameLost);
            }
        });
}
