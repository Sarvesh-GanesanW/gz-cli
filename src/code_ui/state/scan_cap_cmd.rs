//! `/auto scan cap [<size>|off|default]`: shows or sets auto mode's query
//! scan cap (see [`crate::code_ui::scan_cap`]). It applies from the next
//! action, for the main agent and every subagent, and is saved on the
//! profile.

use crate::code_ui::items::Item;
use crate::code_ui::panel::Row;
use crate::code_ui::scan_cap::{ScanCap, ScanGate};

use super::UiState;

const USAGE: &str = "Use /auto scan cap, or /auto scan cap <size>|off|default (e.g. 20GB)";

impl UiState {
    pub(super) fn scan_cap_command(&mut self, argument: &str) {
        let Some(gate) = self.scan_gate() else {
            self.record(Item::Error("Auto mode is off in this session.".into()));
            return;
        };
        let value = match argument.split_once(char::is_whitespace) {
            Some(("cap", value)) => value.trim(),
            None if argument == "cap" || argument.is_empty() => "",
            _ => {
                self.record(Item::Error(USAGE.into()));
                return;
            }
        };
        if value.is_empty() {
            self.record(Item::Notice(format!("Scan cap {}", gate.cap().describe())));
            return;
        }
        let cap = match ScanCap::parse(value) {
            Ok(cap) => cap,
            Err(err) => {
                self.record(Item::Error(format!("{err:#}. {USAGE}")));
                return;
            }
        };
        match gate.set_cap(cap) {
            Ok(()) => self.record(Item::Notice(format!("✓ Scan cap {}", cap.describe()))),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    /// The `/auto` panel row for the cap, when auto mode has one.
    pub(super) fn scan_cap_rows(&self) -> Option<Vec<Row>> {
        let gate = self.scan_gate()?;
        Some(vec![Row::field("Scan cap", gate.cap().describe())])
    }

    fn scan_gate(&self) -> Option<ScanGate> {
        let mut gate = None;
        self.with_auto(|auto| gate = auto.scan_gate());
        gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_ui::auto_mode::AutoContext;
    use crate::code_ui::scan_cap::{EstimateFuture, QueryEstimator};
    use crate::code_ui::state::test_support::*;
    use crossterm::event::KeyCode;
    use std::sync::{Arc, Mutex};

    struct NoEstimates;

    impl QueryEstimator for NoEstimates {
        fn estimate<'a>(&'a self, _: Option<&'a str>, _: &'a str) -> EstimateFuture<'a> {
            Box::pin(async { anyhow::bail!("not in this test") })
        }
    }

    fn with_gate() -> (UiState, ScanGate, Arc<Mutex<Vec<ScanCap>>>) {
        let saved = Arc::new(Mutex::new(Vec::new()));
        let record = saved.clone();
        let gate = ScanGate::new(
            ScanCap::default(),
            Arc::new(NoEstimates),
            Arc::new(move |cap| {
                record.lock().unwrap().push(cap);
                Ok(())
            }),
        );
        let mut context = AutoContext::default();
        context.set_scan_gate(gate.clone());
        let mut s = with_catalog();
        s.auto = Some(Arc::new(Mutex::new(context)));
        (s, gate, saved)
    }

    fn run(s: &mut UiState, command: &str) -> String {
        type_text(s, command);
        s.on_key(key(KeyCode::Enter));
        match s.flush.last() {
            Some(Item::Notice(text) | Item::Error(text)) => text.clone(),
            other => panic!("nothing recorded: {other:?}"),
        }
    }

    #[test]
    fn the_cap_is_shown_set_saved_and_turned_off() {
        let (mut s, gate, saved) = with_gate();

        assert!(run(&mut s, "/auto scan cap").contains("10.0 GB"));
        let set = run(&mut s, "/auto scan cap 20GB");
        assert!(set.starts_with("✓ Scan cap 20.0 GB"), "{set}");
        assert_eq!(
            gate.cap(),
            ScanCap::Bytes(20 << 30),
            "the shared gate changes at once"
        );
        run(&mut s, "/auto scan cap off");
        assert_eq!(
            *saved.lock().unwrap(),
            [ScanCap::Bytes(20 << 30), ScanCap::Off]
        );
        assert!(run(&mut s, "/auto scan cap lots").contains("is not a size"));
        assert!(run(&mut s, "/auto scan limit 5GB").starts_with("Use /auto scan cap"));
    }

    #[test]
    fn the_auto_panel_shows_the_cap() {
        let (mut s, _, _) = with_gate();
        let dir = crate::assistant::sandbox::tests::temp_dir("scan-panel");
        s.auto_store = Some(crate::assistant::auto_rules::AutoStore::in_folder(&dir));
        type_text(&mut s, "/auto");
        s.on_key(key(KeyCode::Enter));
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("no panel");
        };
        let lines = panel.plain_lines().join("\n");
        assert!(
            lines.contains("Scan cap") && lines.contains("10.0 GB: auto mode blocks"),
            "{lines}"
        );
    }
}
