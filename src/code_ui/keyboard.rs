//! Reads the terminal's input without leaving keys stranded after a resize.
//!
//! crossterm 0.28's Unix reader waits on an edge-triggered poller for both
//! the terminal and the window-size signal. When a size change and typed
//! keys are ready in the same wait, it returns the resize first and drops
//! the terminal's readiness edge: the keys stay unread in the terminal until
//! more bytes arrive, so an Enter typed just after a resize did nothing until
//! the next key. Asking the terminal for the cursor position after every
//! resize makes it send a reply, which raises a fresh edge, and the reader
//! then takes the held keys together with the reply.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event};

/// Where terminal events come from; the real one is [`Crossterm`].
pub trait TerminalInput {
    /// True when an event can be read without waiting.
    fn has_event(&mut self) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
    /// Makes the reader look at the terminal again (see the module docs).
    fn wake_reader(&mut self);
}

/// The process's terminal, read through crossterm.
pub struct Crossterm;

impl TerminalInput for Crossterm {
    fn has_event(&mut self) -> io::Result<bool> {
        // A failed look ends this frame's reading, as it always has; the
        // next frame looks again, and a failed read still ends the session.
        Ok(event::poll(Duration::ZERO).unwrap_or(false))
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }

    fn wake_reader(&mut self) {
        // Only the reply's arrival matters, not the position it carries; a
        // terminal that never answers costs crossterm's own short timeout,
        // and the next key wakes the reader as before.
        let _ = crossterm::cursor::position();
    }
}

/// Hands every event that is ready to `handle`. When any of them was a
/// resize, the reader is woken once after the ready events run out, so keys
/// typed alongside it are handled now, not later. Waking is a round trip to
/// the terminal, so a burst of resizes from a drag pays for it only once.
pub fn drain(
    input: &mut impl TerminalInput,
    mut handle: impl FnMut(Event) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut resized = false;
    loop {
        while input.has_event()? {
            let event = input.read()?;
            resized |= matches!(event, Event::Resize(..));
            handle(event)?;
        }
        if !std::mem::take(&mut resized) {
            return Ok(());
        }
        input.wake_reader();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    /// Models the stranded-edge reader: keys that arrived with a resize are
    /// invisible until something wakes the reader.
    struct EdgeReader {
        ready: VecDeque<Event>,
        stranded: Vec<Event>,
        wakes: usize,
    }

    impl TerminalInput for EdgeReader {
        fn has_event(&mut self) -> io::Result<bool> {
            Ok(!self.ready.is_empty())
        }

        fn read(&mut self) -> io::Result<Event> {
            Ok(self.ready.pop_front().expect("has_event said ready"))
        }

        fn wake_reader(&mut self) {
            self.wakes += 1;
            self.ready.extend(self.stranded.drain(..));
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn keys_typed_with_a_resize_are_handled_in_the_same_pass() {
        let mut input = EdgeReader {
            ready: VecDeque::from([Event::Resize(80, 24)]),
            stranded: vec![key(KeyCode::Char('h')), key(KeyCode::Enter)],
            wakes: 0,
        };
        let mut seen = Vec::new();
        drain(&mut input, |event| {
            seen.push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![
                Event::Resize(80, 24),
                key(KeyCode::Char('h')),
                key(KeyCode::Enter)
            ]
        );
    }

    #[test]
    fn plain_keys_do_not_wake_the_reader() {
        let mut input = EdgeReader {
            ready: VecDeque::from([key(KeyCode::Char('a'))]),
            stranded: vec![key(KeyCode::Char('b'))],
            wakes: 0,
        };
        let mut seen = Vec::new();
        drain(&mut input, |event| {
            seen.push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![key(KeyCode::Char('a'))]);
    }

    #[test]
    fn a_burst_of_resizes_wakes_the_reader_once() {
        let mut input = EdgeReader {
            ready: VecDeque::from([
                Event::Resize(120, 40),
                Event::Resize(100, 30),
                Event::Resize(80, 24),
            ]),
            stranded: vec![key(KeyCode::Char('h')), key(KeyCode::Enter)],
            wakes: 0,
        };
        let mut seen = Vec::new();
        drain(&mut input, |event| {
            seen.push(event);
            Ok(())
        })
        .unwrap();
        assert_eq!(input.wakes, 1);
        assert_eq!(seen.len(), 5);
        assert_eq!(seen[3..], [key(KeyCode::Char('h')), key(KeyCode::Enter)]);
    }
}
