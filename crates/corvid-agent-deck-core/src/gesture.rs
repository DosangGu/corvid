use crate::actions::FocusAction;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Pointer,
    Keyboard,
}

#[derive(Default)]
pub struct Gesture {
    pressed: Option<(Input, usize, FocusAction)>,
}

impl Gesture {
    pub fn press(&mut self, input: Input, index: usize, action: FocusAction) {
        // Repeated key-down events cannot replace the captured agent.
        if self.pressed.is_none() {
            self.pressed = Some((input, index, action));
        }
    }

    pub fn release(&mut self, input: Input, index: usize) -> Option<FocusAction> {
        let (source, pressed, action) = self.pressed.take()?;
        (source == input && pressed == index).then_some(action)
    }

    pub fn cancel(&mut self) {
        self.pressed = None;
    }

    pub fn active(&self) -> bool {
        self.pressed.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use corvid_presentation::model::Snapshot;
    use corvid_protocol::wire::PresentedRunKey;
    use uuid::Uuid;

    fn action(run: &str) -> FocusAction {
        FocusAction::Desktop(
            Arc::new(Snapshot {
                link: Uuid::nil(),
                host: Uuid::nil(),
                page: 0,
                data: Default::default(),
                #[cfg(windows)]
                can_pair_wezterm: false,
            }),
            PresentedRunKey {
                host_id: "host".into(),
                remote_epoch: "epoch".into(),
                agent_run_id: run.into(),
            },
        )
    }

    #[test]
    fn repeated_keys_keep_the_original_agent_until_release() {
        let mut gesture = Gesture::default();
        gesture.press(Input::Keyboard, 0, action("original"));
        gesture.press(Input::Keyboard, 0, action("replacement"));
        let Some(FocusAction::Desktop(_, key)) = gesture.release(Input::Keyboard, 0) else {
            panic!("the original press should activate");
        };
        assert_eq!(key.agent_run_id, "original");
        assert!(gesture.release(Input::Keyboard, 0).is_none());
    }

    #[test]
    fn mismatched_or_cancelled_releases_cannot_activate_an_agent() {
        let mut gesture = Gesture::default();
        gesture.press(Input::Pointer, 0, action("original"));
        assert!(gesture.release(Input::Pointer, 1).is_none());
        gesture.press(Input::Pointer, 0, action("original"));
        assert!(gesture.release(Input::Keyboard, 0).is_none());
        gesture.press(Input::Pointer, 0, action("original"));
        gesture.cancel();
        assert!(gesture.release(Input::Pointer, 0).is_none());
    }
}
