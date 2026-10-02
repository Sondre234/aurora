use std::io::Cursor;

use crate::*;

fn output(name: &str) -> OutputInfo {
    OutputInfo {
        name: name.into(),
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
        scale: 1.25,
    }
}

fn workspace(out: &str, index: u32) -> WorkspaceInfo {
    WorkspaceInfo {
        output: out.into(),
        index,
        active: index == 1,
        windows: 2,
        urgent: index == 2,
    }
}

fn window(id: u64) -> WindowInfo {
    WindowInfo {
        id,
        app_id: "kitty".into(),
        title: "~ \u{1f680} zsh".into(),
        workspace: 3,
        output: "DP-1".into(),
        floating: true,
        fullscreen: false,
        urgent: true,
    }
}

fn snapshot() -> Snapshot {
    Snapshot {
        outputs: vec![output("DP-1"), output("HDMI-A-1")],
        workspaces: vec![workspace("DP-1", 1), workspace("DP-1", 2)],
        windows: vec![window(7), window(9)],
        focused_window: Some(7),
        active_output: Some("DP-1".into()),
    }
}

fn theme() -> ThemeSnapshot {
    ThemeSnapshot::new(4, Theme::default())
}

fn all_requests() -> Vec<Request> {
    vec![
        Request::GetSnapshot,
        Request::ListWindows,
        Request::ListOutputs,
        Request::GetTheme,
        Request::SwitchWorkspace {
            output: None,
            index: 4,
        },
        Request::SwitchWorkspace {
            output: Some("DP-1".into()),
            index: 10,
        },
        Request::FocusWindow { id: u64::MAX },
        Request::CloseWindow { id: None },
        Request::CloseWindow { id: Some(3) },
        Request::Spawn {
            argv: vec!["kitty".into(), "--title".into(), "x y".into()],
        },
        Request::ReloadConfig,
        Request::Overview(OverviewAction::Toggle),
        Request::Overview(OverviewAction::Open),
        Request::Overview(OverviewAction::Close),
        Request::SetTheme(Theme::default()),
        Request::Lock,
        Request::Unlock,
        Request::PowerOffMonitors,
        Request::PowerOnMonitors,
    ]
}

fn all_responses() -> Vec<Response> {
    vec![
        Response::Ok,
        Response::Snapshot(snapshot()),
        Response::Windows(vec![window(1), window(2)]),
        Response::Outputs(vec![output("eDP-1")]),
        Response::Theme(theme()),
    ]
}

fn all_events() -> Vec<Event> {
    vec![
        Event::Snapshot(snapshot()),
        Event::Snapshot(Snapshot::default()),
        Event::OutputChanged(output("DP-2")),
        Event::OutputRemoved {
            name: "DP-2".into(),
        },
        Event::WorkspaceChanged(workspace("DP-1", 5)),
        Event::WorkspaceRemoved {
            output: "DP-1".into(),
            index: 5,
        },
        Event::WindowChanged(window(11)),
        Event::WindowClosed { id: 11 },
        Event::FocusChanged {
            window: Some(1),
            output: Some("DP-1".into()),
        },
        Event::FocusChanged {
            window: None,
            output: None,
        },
        Event::Theme(theme()),
        Event::ConfigReloaded {
            ok: false,
            warnings: vec!["a".into(), "b".into()],
        },
    ]
}

fn all_errors() -> Vec<Error> {
    [
        ErrorCode::BadRequest,
        ErrorCode::NotFound,
        ErrorCode::Denied,
        ErrorCode::Unsupported,
        ErrorCode::Internal,
        ErrorCode::VersionMismatch,
    ]
    .into_iter()
    .map(|code| Error {
        code,
        message: format!("{code:?} happened"),
    })
    .collect()
}

fn all_frames() -> Vec<Frame> {
    let mut frames = vec![
        Frame::hello("tests"),
        Frame::new(0, Body::Subscribe(Topic::ALL.to_vec())),
        Frame::new(0, Body::Subscribe(vec![])),
        Frame::new(0, Body::Unsubscribe(vec![Topic::Theme, Topic::Focus])),
    ];
    frames.extend(
        all_requests()
            .into_iter()
            .enumerate()
            .map(|(i, r)| Frame::request(i as u64 + 1, r)),
    );
    frames.extend(
        all_responses()
            .into_iter()
            .map(|r| Frame::new(5, Body::Response(r))),
    );
    frames.extend(all_events().into_iter().map(Frame::event));
    frames.extend(
        all_errors()
            .into_iter()
            .map(|e| Frame::new(9, Body::Error(e))),
    );
    frames
}

#[test]
fn every_variant_roundtrips() {
    for frame in all_frames() {
        let bytes = encode(&frame).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4, "{frame:?}");
        let mut dec = Decoder::new();
        dec.feed(&bytes);
        assert_eq!(dec.next_frame().unwrap(), Some(frame.clone()));
        assert_eq!(dec.next_frame().unwrap(), None);
        assert_eq!(dec.pending(), 0);
        let got = read_frame(&mut Cursor::new(&bytes)).unwrap();
        assert_eq!(got, Some(frame));
    }
}

#[test]
fn topics_cover_events() {
    assert_eq!(Event::Snapshot(snapshot()).topic(), None);
    let mut seen: Vec<Topic> = all_events().iter().filter_map(Event::topic).collect();
    seen.sort_by_key(|t| *t as u8 as usize);
    seen.dedup();
    assert_eq!(seen.len(), Topic::ALL.len());
}

#[test]
fn partial_reads_byte_by_byte() {
    let frames = all_frames();
    let mut stream = Vec::new();
    for f in &frames {
        encode_into(f, &mut stream).unwrap();
    }
    let mut dec = Decoder::new();
    let mut got = Vec::new();
    for b in &stream {
        dec.feed(std::slice::from_ref(b));
        while let Some(f) = dec.next_frame().unwrap() {
            got.push(f);
        }
    }
    assert_eq!(got, frames);
    assert_eq!(dec.pending(), 0);
}

#[test]
fn many_frames_in_one_read_and_odd_chunks() {
    let frames = all_frames();
    let mut stream = Vec::new();
    for f in &frames {
        encode_into(f, &mut stream).unwrap();
    }
    for chunk in [1usize, 3, 7, 64, 1000, stream.len()] {
        let mut dec = Decoder::new();
        let mut got = Vec::new();
        for piece in stream.chunks(chunk) {
            dec.feed(piece);
            while let Some(f) = dec.next_frame().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got, frames, "chunk {chunk}");
    }
}

#[test]
fn header_split_needs_more() {
    let bytes = encode(&Frame::hello("x")).unwrap();
    let mut dec = Decoder::new();
    dec.feed(&bytes[..2]);
    assert_eq!(dec.next_frame().unwrap(), None);
    dec.feed(&bytes[2..bytes.len() - 1]);
    assert_eq!(dec.next_frame().unwrap(), None);
    dec.feed(&bytes[bytes.len() - 1..]);
    assert!(dec.next_frame().unwrap().is_some());
}

#[test]
fn oversize_header_is_rejected_and_poisons() {
    let mut dec = Decoder::new();
    dec.feed(&((MAX_FRAME as u32) + 1).to_le_bytes());
    assert!(matches!(dec.next_frame(), Err(FrameError::TooLarge(n)) if n == MAX_FRAME + 1));
    assert!(dec.is_poisoned());
    dec.feed(&encode(&Frame::hello("x")).unwrap());
    assert!(matches!(dec.next_frame(), Err(FrameError::TooLarge(_))));
    assert_eq!(dec.pending(), 0);

    let mut dec = Decoder::new();
    dec.feed(&u32::MAX.to_le_bytes());
    assert!(matches!(dec.next_frame(), Err(FrameError::TooLarge(_))));
}

#[test]
fn exactly_max_length_is_not_rejected_by_header() {
    let mut dec = Decoder::new();
    dec.feed(&(MAX_FRAME as u32).to_le_bytes());
    assert!(dec.next_frame().unwrap().is_none());
    assert!(!dec.is_poisoned());
}

#[test]
fn oversize_frame_is_not_encoded() {
    let big = Frame::request(
        1,
        Request::Spawn {
            argv: vec!["x".repeat(MAX_FRAME + 1)],
        },
    );
    let mut out = vec![1, 2, 3];
    assert!(matches!(
        encode_into(&big, &mut out),
        Err(FrameError::TooLarge(_))
    ));
    assert_eq!(out, [1, 2, 3]);
    assert!(matches!(encode(&big), Err(FrameError::TooLarge(_))));
    assert!(matches!(
        write_frame(&mut Vec::new(), &big),
        Err(FrameError::TooLarge(_))
    ));
    // A frame just under the cap still goes through.
    let ok = Frame::request(
        1,
        Request::Spawn {
            argv: vec!["x".repeat(MAX_FRAME - 64)],
        },
    );
    let bytes = encode(&ok).unwrap();
    let mut dec = Decoder::new();
    dec.feed(&bytes);
    assert_eq!(dec.next_frame().unwrap(), Some(ok));
}

#[test]
fn garbage_body_errors_but_stays_in_sync() {
    let good = Frame::hello("after");
    let mut stream = Vec::new();
    let garbage = [0xffu8; 9];
    stream.extend_from_slice(&(garbage.len() as u32).to_le_bytes());
    stream.extend_from_slice(&garbage);
    encode_into(&good, &mut stream).unwrap();

    let mut dec = Decoder::new();
    dec.feed(&stream);
    assert!(matches!(dec.next_frame(), Err(FrameError::Decode(_))));
    assert!(!dec.is_poisoned());
    assert_eq!(dec.next_frame().unwrap(), Some(good));
}

#[test]
fn empty_and_truncated_bodies_fail_to_decode() {
    let mut dec = Decoder::new();
    dec.feed(&0u32.to_le_bytes());
    assert!(matches!(dec.next_frame(), Err(FrameError::Decode(_))));

    let full = encode(&Frame::event(Event::Snapshot(snapshot()))).unwrap();
    let body_len = full.len() - 4;
    let mut cut = ((body_len - 3) as u32).to_le_bytes().to_vec();
    cut.extend_from_slice(&full[4..full.len() - 3]);
    let mut dec = Decoder::new();
    dec.feed(&cut);
    assert!(matches!(dec.next_frame(), Err(FrameError::Decode(_))));
}

#[test]
fn unknown_variant_is_a_decode_error() {
    // Body discriminant 200 does not exist: what a newer peer's variant looks like.
    let mut body = vec![0u8]; // id 0
    body.push(200);
    let mut stream = (body.len() as u32).to_le_bytes().to_vec();
    stream.extend_from_slice(&body);
    let mut dec = Decoder::new();
    dec.feed(&stream);
    assert!(matches!(dec.next_frame(), Err(FrameError::Decode(_))));
}

#[test]
fn pseudo_random_garbage_never_panics() {
    let mut x = 0x1234_5678_9abc_def0u64;
    for _ in 0..200 {
        let mut junk = Vec::new();
        for _ in 0..64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            junk.push(x as u8);
        }
        let mut dec = Decoder::new();
        dec.feed(&junk);
        for _ in 0..70 {
            match dec.next_frame() {
                Ok(None) | Err(FrameError::TooLarge(_)) => break,
                _ => {}
            }
        }
    }
}

#[test]
fn blocking_reader_eof_and_truncation() {
    let mut empty = Cursor::new(Vec::new());
    assert!(read_frame(&mut empty).unwrap().is_none());

    let bytes = encode(&Frame::hello("x")).unwrap();
    let mut cut = Cursor::new(bytes[..bytes.len() - 1].to_vec());
    assert!(matches!(read_frame(&mut cut), Err(FrameError::Truncated)));
    let mut cut = Cursor::new(bytes[..2].to_vec());
    assert!(matches!(read_frame(&mut cut), Err(FrameError::Truncated)));

    let mut over = Cursor::new(u32::MAX.to_le_bytes().to_vec());
    assert!(matches!(
        read_frame(&mut over),
        Err(FrameError::TooLarge(_))
    ));
}

#[test]
fn blocking_write_then_read() {
    let mut wire = Vec::new();
    for f in all_frames() {
        write_frame(&mut wire, &f).unwrap();
    }
    let mut r = Cursor::new(wire);
    let mut n = 0;
    while let Some(_f) = read_frame(&mut r).unwrap() {
        n += 1;
    }
    assert_eq!(n, all_frames().len());
}

#[test]
fn handshake_versions() {
    let ok = Frame::hello("shell");
    assert_eq!(check_first_frame(&ok).unwrap().client, "shell");
    assert_eq!(ok.body, Body::Hello(Hello::new("shell")));

    let old = Hello {
        proto_version: PROTO_VERSION + 1,
        client: "future".into(),
    };
    assert_eq!(
        check_hello(&old),
        Err(HandshakeError::VersionMismatch {
            ours: PROTO_VERSION,
            theirs: PROTO_VERSION + 1
        })
    );
    let zero = Frame::new(
        0,
        Body::Hello(Hello {
            proto_version: 0,
            client: String::new(),
        }),
    );
    assert!(check_first_frame(&zero).is_err());
    let not_hello = Frame::request(1, Request::GetSnapshot);
    assert_eq!(check_first_frame(&not_hello), Err(HandshakeError::NotHello));
    assert!(!HandshakeError::NotHello.to_string().is_empty());
}

#[test]
fn wire_format_is_pinned() {
    // Guards the append-only rule: these bytes change only with a PROTO_VERSION bump.
    let bytes = encode(&Frame::request(
        1,
        Request::Spawn {
            argv: vec!["a".into()],
        },
    ))
    .unwrap();
    // len, id=1, Body::Request=1, Request::Spawn=7, vec len 1, str len 1, 'a'
    assert_eq!(bytes, [6, 0, 0, 0, 1, 1, 7, 1, 1, b'a']);
    let bytes = encode(&Frame::hello("c")).unwrap();
    assert_eq!(bytes, [5, 0, 0, 0, 0, 0, 1, 1, b'c']);
}

#[test]
fn snapshot_apply_deltas() {
    let mut s = Snapshot::default();
    s.apply(&Event::Snapshot(snapshot()));
    assert_eq!(s, snapshot());

    let mut moved = window(7);
    moved.title = "new".into();
    s.apply(&Event::WindowChanged(moved.clone()));
    assert_eq!(s.windows[0], moved);
    assert_eq!(s.windows.len(), 2);
    s.apply(&Event::WindowChanged(window(12)));
    assert_eq!(s.windows.len(), 3);

    s.apply(&Event::WindowClosed { id: 7 });
    assert_eq!(s.focused_window, None);
    s.apply(&Event::WindowClosed { id: 4242 });
    assert_eq!(s.windows.len(), 2);

    let mut ws = workspace("DP-1", 2);
    ws.urgent = false;
    s.apply(&Event::WorkspaceChanged(ws.clone()));
    assert_eq!(s.workspaces[1], ws);
    s.apply(&Event::WorkspaceRemoved {
        output: "DP-1".into(),
        index: 1,
    });
    assert_eq!(s.workspaces, vec![ws]);

    s.apply(&Event::OutputRemoved {
        name: "DP-1".into(),
    });
    assert_eq!(s.outputs.len(), 1);
    s.apply(&Event::OutputChanged(output("DP-3")));
    assert_eq!(s.outputs.len(), 2);

    s.apply(&Event::FocusChanged {
        window: Some(9),
        output: Some("HDMI-A-1".into()),
    });
    assert_eq!(s.focused_window, Some(9));
    assert_eq!(s.active_output.as_deref(), Some("HDMI-A-1"));

    let before = s.clone();
    s.apply(&Event::Theme(theme()));
    s.apply(&Event::ConfigReloaded {
        ok: true,
        warnings: vec![],
    });
    assert_eq!(s, before);
}

#[test]
fn theme_snapshot_survives_postcard() {
    let mut t = Theme::default();
    t.palette.accent = aurora_theme::Rgba::new(1, 2, 3, 4);
    t.fonts.family = "Inter".into();
    let ev = Frame::event(Event::Theme(ThemeSnapshot::new(99, t.clone())));
    let mut dec = Decoder::new();
    dec.feed(&encode(&ev).unwrap());
    match dec.next_frame().unwrap().unwrap().body {
        Body::Event(Event::Theme(s)) => assert_eq!((s.rev, s.theme), (99, t)),
        other => panic!("{other:?}"),
    }
}
