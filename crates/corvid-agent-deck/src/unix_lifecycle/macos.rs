use anyhow::{Result, ensure};
use semver::Version;

unsafe extern "C" {
    fn corvid_retain_deck_process(pid: u32) -> usize;
    fn corvid_release_deck_process(process: usize);
    fn corvid_deck_process_visible(process: usize) -> bool;
    fn corvid_retire_legacy_deck(process: usize) -> bool;
}

pub(super) struct Peer(usize);

impl Peer {
    pub(super) fn new(pid: u32) -> Self {
        Self(unsafe { corvid_retain_deck_process(pid) })
    }

    pub(super) fn visible(&self) -> bool {
        unsafe { corvid_deck_process_visible(self.0) }
    }

    pub(super) async fn retire_legacy(&self, _: &Version) -> Result<Option<bool>> {
        let visible = self.visible();
        // An older macOS Deck has no version response. AppKit retains the
        // process identity and sends its normal application termination event.
        ensure!(
            unsafe { corvid_retire_legacy_deck(self.0) },
            "could not retire the old Agent Deck"
        );
        Ok(Some(visible))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        unsafe { corvid_release_deck_process(self.0) };
    }
}
