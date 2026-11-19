use std::sync::mpsc as blocking;

use super::*;

#[cfg(windows)]
impl Session {
    pub(crate) fn screen_change_fixture() -> (Self, watch::Sender<()>) {
        let shared = Arc::new(Shared::new(&Destination::Local));
        let changes = shared.screen_changed.clone();
        let (actions, _) = mpsc::channel(1);
        let (size, _) = watch::channel((0, Grid::new(2, 24)));
        let session = Self {
            shared,
            actions,
            size,
        };
        (session, changes)
    }
}

struct PauseOnBell {
    parsing: blocking::Sender<()>,
    resume: blocking::Receiver<()>,
}

#[test]
fn preview_snapshot_allows_publication_during_rendering() {
    let shared = Arc::new(Shared::new(&Destination::Local));
    let (actions, _) = mpsc::channel(1);
    let grid = Grid::new(2, 24);
    let (size, _) = watch::channel((0, grid));
    let session = Session {
        shared: Arc::clone(&shared),
        actions,
        size,
    };
    let mut parser = Parser::new(grid.rows, grid.cols, 0);
    parser.process(b"before");
    shared
        .buffer()
        .previews
        .insert("pane".into(), Arc::new(parser.screen().clone()));

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
        shared
            .buffer
            .try_lock()
            .expect("preview rendering must not hold the publication lock")
            .previews
            .insert("pane".into(), Arc::new(parser.screen().clone()));
        published.send(()).unwrap();
        assert_eq!(reader.join().unwrap().as_deref(), Some("before"));
    });

    assert_eq!(
        session.preview("pane", Screen::contents).as_deref(),
        Some("before after")
    );
}

impl vt100::Callbacks for PauseOnBell {
    fn audible_bell(&mut self, _: &mut Screen) {
        self.parsing.send(()).unwrap();
        self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
    }
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
    let shared = Arc::new(Shared::new(&Destination::Local));
    let (actions, mut incoming) = mpsc::channel(1);
    let grid = Grid::new(2, 24);
    let (size, _) = watch::channel((7, grid));
    let session = Session {
        shared: Arc::clone(&shared),
        actions,
        size,
    };
    {
        let mut buffer = shared.buffer();
        buffer.ui.selected = Some(ViewSelection {
            pane_id: "pane".into(),
            ..Default::default()
        });
        buffer.ui.connected = true;
        buffer.ui.changing = false;
        buffer.revision = 7;
    }
    assert_eq!(session.screen("pane", Screen::contents), None);
    assert!(!session.send("pane", b"key".to_vec(), false));

    let mut changes = session.screen_changes();
    thread::scope(|scope| {
        let (parsing, parsing_started) = blocking::channel();
        let (resume_parsing, resume) = blocking::channel();
        let mut parser =
            Parser::new_with_callbacks(grid.rows, grid.cols, 0, PauseOnBell { parsing, resume });
        shared.apply_ansi(&mut parser, b"before", WheelEncoding::Sgr, Some(false));
        assert!(session.wheel_target("other").is_none());
        let route = session.wheel_target("pane").unwrap();
        session.send_wheel("pane", route, (3, 4), -1, 0);
        assert!(matches!(incoming.try_recv().unwrap(),
            Action::Input { revision: 7, bytes, paste: false } if bytes == b"\x1b[<64;5;4M"
        ));
        changes.borrow_and_update();
        let shared = &shared;
        let parsing = scope.spawn(move || {
            let mut ansi = b"\x1b[2J\x1b[Hpartial\x07".to_vec();
            ansi.resize(corvid_protocol::RENDER_LIMIT - 1024, b'\0');
            ansi.extend_from_slice(b"\x1b[2J\x1b[Hafter ");
            let korean = "한".as_bytes();
            ansi.extend_from_slice(&korean[..1]);
            shared.apply_ansi(&mut parser, &ansi, WheelEncoding::Disabled, Some(false));
            parser
        });

        parsing_started
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(shared.buffer.try_lock().is_ok());
        assert!(!changes.has_changed().unwrap());
        assert_eq!(
            session.screen("pane", Screen::contents).as_deref(),
            Some("before")
        );
        assert_eq!(session.screen("other", Screen::contents), None);
        assert!(session.send("pane", b"key".to_vec(), false));
        assert!(matches!(
            incoming.try_recv().unwrap(),
            Action::Input { revision: 7, bytes, paste: false } if bytes == b"key"
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
