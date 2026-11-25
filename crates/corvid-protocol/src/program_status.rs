use crate::wire::{ProgramPhase, ProgramStatus, ProgramWaitKind};

impl ProgramPhase {
    pub fn summary_priority(&self) -> u8 {
        match self {
            ProgramPhase::Blocked => 5,
            ProgramPhase::Error => 4,
            ProgramPhase::Working => 3,
            ProgramPhase::Done => 2,
            ProgramPhase::Idle => 1,
            ProgramPhase::Unspecified => 0,
        }
    }
}

impl ProgramStatus {
    pub fn summary_priority(&self) -> u8 {
        self.phase().summary_priority()
    }

    pub fn label(&self) -> &'static str {
        match (self.phase(), self.wait_kind()) {
            (ProgramPhase::Idle, _) => "App idle",
            (ProgramPhase::Working, _) => "App working",
            (ProgramPhase::Blocked, ProgramWaitKind::Permission) => "App waiting for permission",
            (ProgramPhase::Blocked, ProgramWaitKind::Question) => "App waiting for an answer",
            (ProgramPhase::Blocked, ProgramWaitKind::Auth) => "App waiting for login",
            (ProgramPhase::Blocked, _) => "App waiting",
            (ProgramPhase::Done, _) => "App finished",
            (ProgramPhase::Error, _) => "App error",
            (ProgramPhase::Unspecified, _) => "App status",
        }
    }

    pub fn summary(&self) -> String {
        let mut text = self.label().to_owned();
        if let Some(progress) = self.progress.filter(|progress| *progress <= 100) {
            use std::fmt::Write as _;
            let _ = write!(text, " {progress}%");
        }
        text
    }

    pub fn detail(&self) -> String {
        let mut text = self.summary();
        for part in [&self.title, &self.message] {
            if !part.is_empty() {
                text.push_str(" · ");
                text.push_str(part);
            }
        }
        if self.title.is_empty() && !self.app.is_empty() {
            text.push_str(" · ");
            text.push_str(&self.app);
        }
        text
    }
}
