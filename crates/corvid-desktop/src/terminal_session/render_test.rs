use std::sync::mpsc as blocking;

use super::*;

#[cfg(windows)]
impl Session {
    pub(crate) fn screen_change_fixture() -> (Self, watch::Sender<()>) {
        let shared = Arc::new(Shared::new(&Destination::Local, session_runtime().unwrap()));
        let changes = shared.screen_changed.clone();
        let (actions, _) = mpsc::channel(1);
        let (size, _) = watch::channel((0, 0, Grid::new(2, 24)));
        let session = Self {
            shared,
            actions,
            size,
        };
        (session, changes)
    }
}

#[test]
fn preview_snapshot_allows_publication_during_rendering() {
    let shared = Arc::new(Shared::new(&Destination::Local, session_runtime().unwrap()));
    let (actions, _) = mpsc::channel(1);
    let grid = Grid::new(2, 24);
    let (size, _) = watch::channel((0, 0, grid));
    let session = Session {
        shared: Arc::clone(&shared),
        actions,
        size,
    };
    let mut parser = Decoder::new(grid.rows, grid.cols);
    parser.process(b"before");
    let screen = Arc::new(parser.screen().clone());
    shared.edit(move |state| {
        Arc::make_mut(&mut state.previews).insert("pane".into(), screen);
        Changed::Ui
    });

    thread::scope(|scope| {
        let (reading, started) = blocking::channel();
        let (published, resume) = blocking::channel();
        let session = &session;
        let reader = scope.spawn(move || {
            session.preview("pane", |screen| {
                reading.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
                screen.contents()
            })
        });

        started.recv_timeout(Duration::from_secs(5)).unwrap();
        parser.process(b" after");
        let screen = Arc::new(parser.screen().clone());
        shared.edit(move |state| {
            Arc::make_mut(&mut state.previews).insert("pane".into(), screen);
            Changed::Ui
        });
        published.send(()).unwrap();
        assert_eq!(reader.join().unwrap().as_deref(), Some("before"));
    });

    assert_eq!(
        session.preview("pane", Screen::contents).as_deref(),
        Some("before after")
    );
}

#[test]
fn clipboard_write_is_single_use_and_matches_the_current_view() {
    let shared = Arc::new(Shared::new(&Destination::Local, session_runtime().unwrap()));
    let (actions, _) = mpsc::channel(1);
    let (size, _) = watch::channel((0, 0, Grid::new(2, 24)));
    let session = Session {
        shared: Arc::clone(&shared),
        actions,
        size,
    };
    shared.edit(|snapshot| {
        let ui = Arc::make_mut(&mut snapshot.ui);
        ui.connected = true;
        ui.changing = false;
        ui.selected = Some(ViewSelection {
            pane_id: "pane".into(),
            ..Default::default()
        });
        snapshot.view_epoch = 2;
        Changed::View
    });
    let publish = |view_epoch| {
        shared.clipboard.send_replace(Some(ClipboardWrite {
            view_epoch,
            text: "한글 🐦".into(),
        }));
    };
    publish(1);
    assert!(session.take_clipboard("pane").is_none());
    publish(2);
    assert!(session.take_clipboard("another pane").is_none());
    assert_eq!(session.take_clipboard("pane").as_deref(), Some("한글 🐦"));
    assert!(session.take_clipboard("pane").is_none());

    publish(2);
    let change = shared.reserve_view_change().unwrap();
    assert!(session.take_clipboard("pane").is_none());
    drop(change);
    assert!(session.take_clipboard("pane").is_none());
}

#[test]
fn snapshot_reads_do_not_wait_for_the_state_actor() {
    let shared = Shared::new(&Destination::Local, session_runtime().unwrap());
    thread::scope(|scope| {
        let (editing, started) = blocking::channel();
        let (publish, ready) = blocking::channel();
        let producer = scope.spawn(|| {
            shared.edit(move |state| {
                Arc::make_mut(&mut state.ui).connected = true;
                editing.send(()).unwrap();
                ready.recv_timeout(Duration::from_secs(5)).unwrap();
                state.revision = 7;
                Changed::View
            });
        });

        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let previous = shared.snapshot();
        assert!(!previous.ui.connected);
        assert_eq!(previous.revision, 0);
        publish.send(()).unwrap();
        producer.join().unwrap();

        let current = shared.snapshot();
        assert!(current.ui.connected);
        assert_eq!(current.revision, 7);
        assert!(!previous.ui.connected);
    });
}

#[test]
fn rejected_view_change_releases_its_input_gate() {
    let shared = Shared::new(&Destination::Local, session_runtime().unwrap());
    shared.update_ui(|ui| {
        ui.connected = true;
        ui.changing = false;
    });
    let (actions, mut incoming) = mpsc::channel(1);
    let (size, _) = watch::channel((0, 0, Grid::new(2, 24)));
    let session = Session {
        shared: Arc::new(shared),
        actions,
        size,
    };
    session.select(ViewSelection::default(), false);
    assert!(session.ui().changing);
    session.select(ViewSelection::default(), false);
    let queued = incoming.try_recv().unwrap();
    assert!(matches!(queued, Action::Change { .. }));
    assert!(incoming.try_recv().is_err());

    drop(queued);
    assert!(!session.ui().changing);
    session.commit_navigation();
    session.select(ViewSelection::default(), false);
    assert!(
        !session.ui().changing,
        "a full queue cannot reserve the view"
    );
}

#[test]
fn wheel_reports_use_the_child_encoding_and_terminal_coordinates() {
    let cell = (1, 254);
    assert!(wheel_report(WheelEncoding::Disabled, cell, -3, 0).is_none());
    assert_eq!(
        wheel_report(WheelEncoding::Sgr, cell, -2, 0).unwrap(),
        b"\x1b[<64;255;2M".repeat(2)
    );
    assert_eq!(
        wheel_report(WheelEncoding::Sgr, cell, 1, 16).unwrap(),
        b"\x1b[<81;255;2M"
    );
    assert_eq!(
        wheel_report(WheelEncoding::Legacy, cell, 1, 0).unwrap(),
        b"\x1b[Ma\xff\x22"
    );
    assert_eq!(
        wheel_report(WheelEncoding::Utf8, cell, -1, 0).unwrap(),
        "\x1b[M\u{60}\u{11f}\u{22}".as_bytes()
    );
}

#[test]
fn large_ansi_update_keeps_previous_screen_and_input_available() {
    let shared = Arc::new(Shared::new(&Destination::Local, session_runtime().unwrap()));
    let (actions, mut incoming) = mpsc::channel(1);
    let grid = Grid::new(2, 24);
    let (size, _) = watch::channel((0, 7, grid));
    let session = Session {
        shared: Arc::clone(&shared),
        actions,
        size,
    };
    shared.edit(|state| {
        let ui = Arc::make_mut(&mut state.ui);
        ui.selected = Some(ViewSelection {
            pane_id: "pane".into(),
            ..Default::default()
        });
        ui.connected = true;
        ui.changing = false;
        state.revision = 7;
        Changed::View
    });
    assert_eq!(session.screen("pane", Screen::contents), None);
    assert!(!session.send("pane", b"key".to_vec(), false));

    let mut changes = session.screen_changes();
    thread::scope(|scope| {
        let (parsing, parsing_started) = blocking::channel();
        let (resume_parsing, resume) = blocking::channel();
        let mut parser = Decoder::new(grid.rows, grid.cols);
        shared.apply_ansi(&mut parser, b"before", WheelEncoding::Sgr, Some(false));
        assert!(session.wheel_target("other").is_none());
        let route = session.wheel_target("pane").unwrap();
        session.send_wheel("pane", route, (3, 4), -1, 0);
        assert!(matches!(incoming.try_recv().unwrap(),
            Action::Input { revision: 7, bytes, paste: false, .. } if bytes == b"\x1b[<64;5;4M"
        ));
        changes.borrow_and_update();
        let shared = &shared;
        let parsing = scope.spawn(move || {
            shared.apply_frame(WheelEncoding::Disabled, Some(false), || {
                parser.process(b"\x1b[2J\x1b[Hpartial");
                parsing.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
                let mut ansi = vec![0; corvid_protocol::RENDER_LIMIT - 1024];
                ansi.extend_from_slice(b"\x1b[2J\x1b[Hafter ");
                ansi.extend_from_slice(&"한".as_bytes()[..1]);
                parser.process(&ansi);
                parser.screen().clone()
            });
            parser
        });

        parsing_started
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(!changes.has_changed().unwrap());
        assert_eq!(
            session.screen("pane", Screen::contents).as_deref(),
            Some("before")
        );
        assert_eq!(session.screen("other", Screen::contents), None);
        assert!(session.send("pane", b"key".to_vec(), false));
        assert!(matches!(
            incoming.try_recv().unwrap(),
            Action::Input { revision: 7, bytes, paste: false, .. } if bytes == b"key"
        ));

        resume_parsing.send(()).unwrap();
        let mut parser = parsing.join().unwrap();
        assert!(changes.has_changed().unwrap());
        assert_eq!(
            session.wheel_target("pane").unwrap().1,
            WheelTarget::Scrollback
        );
        session.send_wheel("pane", route, (3, 4), -1, 0);
        assert!(incoming.try_recv().is_err());
        assert_eq!(
            session.screen("pane", Screen::contents).as_deref(),
            Some("after ")
        );

        shared.apply_ansi(
            &mut parser,
            &"한".as_bytes()[1..],
            WheelEncoding::Disabled,
            Some(false),
        );
        assert_eq!(
            session.screen("pane", Screen::contents).as_deref(),
            Some("after 한")
        );
    });
}

#[test]
fn history_rejects_non_sgr_payloads_and_broken_character_offsets() {
    let mut history = TerminalHistory {
        columns: 10,
        lines: vec!["한글".into()],
        styles: vec![corvid_protocol::wire::HistoryStyle {
            row: 0,
            offset: 3,
            sgr: "38;5;2".into(),
        }],
        ..Default::default()
    };
    assert!(validate_history(&history).is_ok());
    history.styles[0].offset = 2;
    assert!(validate_history(&history).is_err());
    history.styles[0].offset = 3;
    history.styles[0].sgr = "0m\x1b[2J".into();
    assert!(validate_history(&history).is_err());
}
