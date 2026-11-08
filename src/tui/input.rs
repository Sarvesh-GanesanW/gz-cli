//! The terminal reader thread. It can be paused while the TUI hands the
//! terminal to something else (gz code from `F`), so the two never race for
//! the same keys.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use tokio::sync::mpsc::UnboundedSender;

/// How long a read waits before checking for a pause.
const POLL: Duration = Duration::from_millis(50);
/// A pause is acknowledged within one poll; this is the give-up time.
const PAUSE_ACK: Duration = Duration::from_secs(2);

/// Terminal input the app reacts to.
pub enum Input {
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize,
}

enum Control {
    /// Stop reading; the sender hears back once nothing is being read.
    Pause(mpsc::Sender<()>),
    Resume,
}

/// Handle on the reader thread.
pub struct Reader {
    control: mpsc::Sender<Control>,
}

impl Reader {
    pub fn spawn(inputs: UnboundedSender<Input>) -> Self {
        let (control, orders) = mpsc::channel();
        std::thread::spawn(move || read_loop(&inputs, &orders));
        Self { control }
    }

    /// Returns once the thread has stopped reading the terminal.
    pub fn pause(&self) -> Result<()> {
        let (ack, acked) = mpsc::channel();
        self.control
            .send(Control::Pause(ack))
            .ok()
            .context("the terminal reader has stopped")?;
        acked
            .recv_timeout(PAUSE_ACK)
            .context("the terminal reader didn't pause")
    }

    pub fn resume(&self) {
        let _ = self.control.send(Control::Resume);
    }
}

fn read_loop(inputs: &UnboundedSender<Input>, orders: &mpsc::Receiver<Control>) {
    loop {
        match orders.try_recv() {
            Ok(Control::Pause(ack)) => {
                let _ = ack.send(());
                if !wait_for_resume(orders) {
                    return;
                }
            }
            Ok(Control::Resume) | Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return,
        }
        let Ok(ready) = event::poll(POLL) else {
            return;
        };
        if !ready {
            continue;
        }
        let Ok(event) = event::read() else {
            return;
        };
        let input = match event {
            // Windows reports key releases too; act on presses only.
            Event::Key(key) if key.kind != KeyEventKind::Release => Input::Key(key),
            Event::Mouse(mouse) => Input::Mouse(mouse),
            Event::Resize(..) => Input::Resize,
            _ => continue,
        };
        if inputs.send(input).is_err() {
            return;
        }
    }
}

/// Blocks until resumed; `false` when the TUI is gone.
fn wait_for_resume(orders: &mpsc::Receiver<Control>) -> bool {
    loop {
        match orders.recv() {
            Ok(Control::Resume) => return true,
            Ok(Control::Pause(ack)) => {
                let _ = ack.send(());
            }
            Err(_) => return false,
        }
    }
}
