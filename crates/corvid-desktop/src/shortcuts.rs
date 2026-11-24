use corvid_core::shortcuts::dubeolsik_key;
use gpui::Keystroke;

/// Use GPUI's key identity for shortcuts, with a fallback for layouts that report jamo.
pub fn key(stroke: &Keystroke) -> &str {
    dubeolsik_key(&stroke.key).unwrap_or(&stroke.key)
}
