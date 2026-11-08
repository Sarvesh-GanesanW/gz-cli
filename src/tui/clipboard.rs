//! Copying to the user's clipboard through the terminal (OSC 52), which
//! works over SSH and needs no platform clipboard library.

use std::io::Write;

use base64::Engine;

/// The escape sequence that asks the terminal to set the clipboard.
pub fn osc52(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{encoded}\x07")
}

/// Writes the sequence to the terminal. Terminals that don't support OSC 52
/// ignore it, so a failed write is not worth interrupting the user for.
pub fn copy(text: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_base64_text_in_the_osc52_sequence() {
        assert_eq!(osc52("1000"), "\x1b]52;c;MTAwMA==\x07");
    }
}
