use anyhow::{Result, anyhow};
use corvid_deck::{Deck, Frame, KEY_COUNT, Press};

#[derive(Default)]
pub struct KeyEdges {
    previous: [bool; KEY_COUNT],
}

impl KeyEdges {
    pub fn seed(&mut self, levels: &[bool]) -> Result<()> {
        self.previous = button_levels(levels)?;
        Ok(())
    }

    pub fn apply(&mut self, levels: &[bool], shown: &Frame, deck: &mut Deck) -> Result<Vec<Press>> {
        Ok(self
            .transitions(levels)?
            .into_iter()
            .filter(|&(_, down)| down)
            .filter_map(|(index, _)| shown.capture(index).and_then(|capture| deck.press(capture)))
            .collect())
    }

    pub fn transitions(&mut self, levels: &[bool]) -> Result<Vec<(usize, bool)>> {
        let levels = button_levels(levels)?;
        let previous = std::mem::replace(&mut self.previous, levels);
        Ok(levels
            .into_iter()
            .zip(previous)
            .enumerate()
            .filter_map(|(index, (down, was_down))| (down != was_down).then_some((index, down)))
            .collect())
    }
}

fn button_levels(levels: &[bool]) -> Result<[bool; KEY_COUNT]> {
    levels
        .try_into()
        .map_err(|_| anyhow!("unexpected Stream Deck report length: {}", levels.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvid_deck::Outcome;

    #[test]
    fn usb_input_triggers_only_on_press_edges() {
        let mut edges = KeyEdges::default();
        let mut deck = Deck::default();
        let shown = deck.frame();
        let mut levels = [false; KEY_COUNT];
        levels[0] = true;

        assert_eq!(
            edges.apply(&levels, &shown, &mut deck).unwrap()[0].outcome,
            Outcome::Enabled
        );
        assert!(edges.apply(&levels, &shown, &mut deck).unwrap().is_empty());
        assert!(edges.apply(&[], &shown, &mut deck).is_err());
        assert!(edges.apply(&levels, &shown, &mut deck).unwrap().is_empty());
        assert!(
            edges
                .apply(&[false; KEY_COUNT], &shown, &mut deck)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            edges.apply(&levels, &shown, &mut deck).unwrap()[0].outcome,
            Outcome::Disabled
        );
    }

    #[test]
    fn seeded_held_key_cannot_change_pages_until_released() {
        let mut edges = KeyEdges::default();
        let mut deck = Deck::default();
        let shown = deck.frame();
        let mut levels = [false; KEY_COUNT];
        levels[KEY_COUNT - 1] = true;
        edges.seed(&levels).unwrap();

        assert!(edges.apply(&levels, &shown, &mut deck).unwrap().is_empty());
        edges.apply(&[false; KEY_COUNT], &shown, &mut deck).unwrap();
        assert_eq!(
            edges.apply(&levels, &shown, &mut deck).unwrap()[0].outcome,
            Outcome::PageChanged
        );
        assert!(
            edges
                .apply(&levels, &deck.frame(), &mut deck)
                .unwrap()
                .is_empty()
        );
        assert_eq!(deck.frame().page, 1);
    }
}
