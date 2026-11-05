//! Platform clipboard surface (text only). Implementations live in the platform crates.

pub trait ClipboardOs {
    /// Monotonic counter that changes whenever any process writes the clipboard.
    fn change_count(&self) -> Result<u64, String>;
    /// Current text, or `None` when the clipboard holds no text.
    fn read_text(&self) -> Result<Option<String>, String>;
    fn write_text(&self, s: &str) -> Result<(), String>;
}
