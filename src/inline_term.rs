//! Terminal plumbing shared by the inline interfaces (`gz code`, `gz sql`):
//! raw mode with a guaranteed restore, and an inline viewport pinned under
//! normal scrollback that grows and shrinks with its content.

use std::io::stdout;

use anyhow::{Context, Result};
use crossterm::event::{
    self, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

pub type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Raw mode (and richer key reporting where supported) for a session;
/// restored on drop and on panic.
pub struct TerminalGuard {
    enhanced: bool,
}

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        enable_raw_mode().context("cannot enter raw mode (need a terminal)")?;
        let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
        if enhanced {
            execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .ok();
        }
        execute!(stdout(), event::EnableBracketedPaste).ok();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(enhanced);
            previous(info);
        }));
        Ok(Self { enhanced })
    }
}

fn restore(enhanced: bool) {
    if enhanced {
        execute!(stdout(), PopKeyboardEnhancementFlags).ok();
    }
    execute!(stdout(), event::DisableBracketedPaste, cursor::Show).ok();
    disable_raw_mode().ok();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore(self.enhanced);
    }
}

/// An inline viewport kept exactly as tall as its content, recreated in
/// place when that changes, with finished lines printed above it.
#[derive(Default)]
pub struct InlineTerm {
    terminal: Option<Term>,
    height: u16,
}

impl InlineTerm {
    /// Terminal size as (columns, rows).
    pub fn size() -> Result<(u16, u16)> {
        terminal::size().context("cannot read the terminal size")
    }

    /// The viewport, resized to `wanted` rows first if needed.
    pub fn sized(&mut self, wanted: u16) -> Result<&mut Term> {
        if self.terminal.is_none() || wanted != self.height {
            if let Some(old) = self.terminal.as_mut() {
                old.clear().ok();
            }
            let options = TerminalOptions {
                viewport: Viewport::Inline(wanted),
            };
            let term = Terminal::with_options(CrosstermBackend::new(stdout()), options)
                .context("cannot open the terminal")?;
            self.terminal = Some(term);
            self.height = wanted;
        }
        Ok(self.terminal.as_mut().expect("terminal was just created"))
    }

    /// Prints `lines` into scrollback above the viewport.
    pub fn print(&mut self, lines: Vec<Line<'static>>) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let height = self.height.max(1);
        let term = self.sized(height)?;
        let rows = lines.len() as u16;
        term.insert_before(rows, |buffer| {
            Paragraph::new(lines).render(buffer.area, buffer);
        })
        .context("cannot print to the terminal")
    }

    pub fn draw(&mut self, wanted: u16, draw: impl FnOnce(&mut Frame)) -> Result<()> {
        self.sized(wanted)?.draw(draw).context("cannot draw")?;
        Ok(())
    }

    /// Clears the live region before exiting so the shell prompt lands cleanly.
    pub fn clear(&mut self) {
        if let Some(term) = self.terminal.as_mut() {
            term.clear().ok();
        }
    }
}
