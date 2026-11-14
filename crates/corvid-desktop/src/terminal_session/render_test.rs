use std::sync::mpsc as blocking;

use super::*;

struct PauseOnBell {
    parsing: blocking::Sender<()>,
    resume: blocking::Receiver<()>,
}

impl vt100::Callbacks for PauseOnBell {
    fn audible_bell(&mut self, _: &mut Screen) {
        self.parsing.send(()).unwrap();
        self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
    }
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
        shared.apply_ansi(&mut parser, b"before");
        changes.borrow_and_update();
        let shared = &shared;
        let parsing = scope.spawn(move || {
            let mut ansi = b"\x1b[2J\x1b[Hpartial\x07".to_vec();
            ansi.resize(corvid_protocol::RENDER_LIMIT - 1024, b'\0');
            ansi.extend_from_slice(b"\x1b[2J\x1b[Hafter ");
            let korean = "한".as_bytes();
            ansi.extend_from_slice(&korean[..1]);
            shared.apply_ansi(&mut parser, &ansi);
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
            session.screen("pane", Screen::contents).as_deref(),
            Some("after ")
        );

        shared.apply_ansi(&mut parser, &"한".as_bytes()[1..]);
        assert_eq!(
            session.screen("pane", Screen::contents).as_deref(),
            Some("after 한")
        );
    });
}
