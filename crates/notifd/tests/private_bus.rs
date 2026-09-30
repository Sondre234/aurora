//! D-Bus integration test against a PRIVATE `dbus-daemon` started here with its own
//! config and socket. It never touches the user's session bus (the address is always passed
//! explicitly; `DBUS_SESSION_BUS_ADDRESS` is removed for the child) and never connects to
//! Wayland. Skipped when `dbus-daemon` is not installed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command as Proc, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use aurora_notifd::dbus::{CAPABILITIES, Command, NAME, PATH, Server, StartError};
use aurora_notifd::hints::Urgency;
use aurora_notifd::state::CloseReason;
use zbus::blocking::{Connection, connection::Builder};
use zbus::zvariant::{Structure, Value};

struct Daemon {
    child: Child,
    address: String,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Only the pid we spawned ourselves.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_daemon() -> Option<Daemon> {
    let dir = std::env::temp_dir().join(format!("aurora-notifd-bus-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let conf = dir.join("bus.conf");
    std::fs::write(
        &conf,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
            dir.display()
        ),
    )
    .ok()?;
    let mut child = Proc::new("dbus-daemon")
        .arg(format!("--config-file={}", conf.display()))
        .arg("--nofork")
        .arg("--print-address")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut line = String::new();
    BufReader::new(child.stdout.take()?)
        .read_line(&mut line)
        .ok()?;
    let address = line.trim().to_string();
    if address.is_empty() {
        let _ = child.kill();
        return None;
    }
    Some(Daemon {
        child,
        address,
        dir,
    })
}

fn sink() -> (aurora_notifd::dbus::Sink, mpsc::Receiver<Command>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    (
        Arc::new(move |c| {
            let _ = tx.lock().unwrap().send(c);
        }),
        rx,
    )
}

fn client(addr: &str) -> Connection {
    Builder::address(addr).unwrap().build().unwrap()
}

#[allow(clippy::too_many_arguments)]
fn notify(
    c: &Connection,
    replaces: u32,
    summary: &str,
    actions: Vec<&str>,
    hints: HashMap<&str, Value<'_>>,
    timeout: i32,
) -> u32 {
    c.call_method(
        Some(NAME),
        PATH,
        Some(NAME),
        "Notify",
        &(
            "test",
            replaces,
            "icon",
            summary,
            "<b>body</b>",
            actions,
            hints,
            timeout,
        ),
    )
    .unwrap()
    .body()
    .deserialize::<u32>()
    .unwrap()
}

#[test]
fn notifications_interface_on_a_private_bus() {
    let Some(daemon) = start_daemon() else {
        eprintln!("skipped: dbus-daemon not available");
        return;
    };
    let (sink_a, rx) = sink();
    let server = Server::start(Some(&daemon.address), false, sink_a).expect("name is free");
    assert!(server.owner.starts_with(':'));
    let c = client(&daemon.address);

    // GetCapabilities / GetServerInformation.
    let caps: Vec<String> = c
        .call_method(Some(NAME), PATH, Some(NAME), "GetCapabilities", &())
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    assert_eq!(caps, CAPABILITIES.map(String::from).to_vec());
    let info: (String, String, String, String) = c
        .call_method(Some(NAME), PATH, Some(NAME), "GetServerInformation", &())
        .unwrap()
        .body()
        .deserialize()
        .unwrap();
    assert_eq!(info.0, "aurora-notifd");
    assert_eq!(info.3, "1.2");

    // Notify: fresh ids, hints, actions, image-data.
    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(2));
    hints.insert("resident", Value::Bool(true));
    hints.insert("image-path", Value::new("/tmp/x.png"));
    let image = Structure::from((1i32, 1i32, 3i32, false, 8i32, 3i32, vec![10u8, 20, 30]));
    hints.insert("image-data", Value::Structure(image));
    let id = notify(&c, 0, "Hello", vec!["default", "Open", "a", "A"], hints, -1);
    assert_eq!(id, 1);
    let Command::Notify(n) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected Notify");
    };
    assert_eq!(n.id, 1);
    assert_eq!(n.summary, "Hello");
    assert_eq!(n.body, "<b>body</b>");
    assert_eq!(n.expire_timeout, -1);
    assert_eq!(n.actions, ["default", "Open", "a", "A"]);
    assert_eq!(n.hints.urgency, Urgency::Critical);
    assert!(n.hints.resident);
    assert_eq!(n.hints.image_path.as_deref(), Some("/tmp/x.png"));
    let raw = n.hints.image.as_ref().expect("image-data parsed");
    assert_eq!(
        (raw.width, raw.height, raw.data.clone()),
        (1, 1, vec![10, 20, 30])
    );

    // A second notification gets the next id; replaces_id is honoured.
    assert_eq!(notify(&c, 0, "Two", vec![], HashMap::new(), 0), 2);
    let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(notify(&c, 1, "Again", vec![], HashMap::new(), 100), 1);
    let Command::Notify(n) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected Notify");
    };
    assert_eq!((n.id, n.summary.as_str()), (1, "Again"));

    // CloseNotification reaches the main loop.
    c.call_method(Some(NAME), PATH, Some(NAME), "CloseNotification", &(2u32,))
        .unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Command::Close(2)
    ));

    // Signals reach other bus clients.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(NAME)
        .unwrap()
        .build();
    let mut stream = zbus::blocking::MessageIterator::for_match_rule(rule, &c, None).unwrap();
    let (stx, srx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let Some(Ok(msg)) = stream.next() else { return };
            let member = msg.header().member().map(|m| m.to_string());
            let _ = stx.send((member, msg));
        }
    });
    server.emit_closed(1, CloseReason::Expired);
    server.emit_action(1, "a");
    let (member, msg) = srx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(member.as_deref(), Some("NotificationClosed"));
    assert_eq!(msg.body().deserialize::<(u32, u32)>().unwrap(), (1, 1));
    let (member, msg) = srx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(member.as_deref(), Some("ActionInvoked"));
    assert_eq!(
        msg.body().deserialize::<(u32, String)>().unwrap(),
        (1, "a".to_string())
    );

    // The name is taken: a second daemon without --replace must fail, not steal it.
    let (sink_b, rx_b) = sink();
    match Server::start(Some(&daemon.address), false, sink_b) {
        Err(StartError::NameTaken { owner }) => {
            assert_eq!(owner.as_deref(), Some(server.owner.as_str()));
        }
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("a daemon without --replace took a name that was owned"),
    }
    // ... and the original still serves.
    assert_eq!(notify(&c, 0, "Still", vec![], HashMap::new(), 0), 3);
    let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(rx_b.try_recv().is_err());

    // --replace takes over, and the old owner is told it lost the name.
    let (sink_c, _rx_c) = sink();
    let second = Server::start(Some(&daemon.address), true, sink_c).expect("replace works");
    assert_ne!(second.owner, server.owner);
    let lost = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(lost, Command::NameLost), "got {lost:?}");
}
