//! Saving the screen in front as a named view, and what the palette lists
//! ahead of the screens: Recent, then saved views.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::palette::{Action, Entry, Palette};
use crate::tui::saved_views::{SavedView, SavedViews};

use super::{App, Place, Tone};

/// Longest view name accepted.
const MAX_NAME_CHARS: usize = 60;

impl App {
    /// Uses the views read from disk at start-up (tests keep the empty,
    /// unstored default). Unreadable views turn saving off and say why.
    pub fn use_saved_views(&mut self, views: anyhow::Result<SavedViews>) {
        match views {
            Ok(views) => self.saved = views,
            Err(err) => self.set_status(Tone::Error, format!("Saved views are off: {err:#}")),
        }
    }

    /// The palette: Recent first, then saved views, then everything else.
    pub(super) fn open_palette(&mut self) {
        let mut lead = self.recent.clone();
        for view in self.saved.all() {
            let Some(place) = view.place() else { continue };
            lead.push(Entry {
                label: view.name.clone(),
                hint: "saved view",
                action: Action::Go(place),
            });
        }
        lead.push(Entry {
            label: "Save this view…".to_string(),
            hint: "action",
            action: Action::SaveView,
        });
        lead.extend(self.saved.all().iter().map(|view| Entry {
            label: format!("Delete saved view “{}”", view.name),
            hint: "action",
            action: Action::DeleteView(view.name.clone()),
        }));
        self.palette = Some(Palette::new(&lead));
    }

    /// Ctrl+E: just the Recent list.
    pub(super) fn open_recent(&mut self) {
        if self.recent.is_empty() {
            return self.set_status(Tone::Info, "Nothing opened yet");
        }
        self.palette = Some(Palette::titled("Recent", &self.recent));
    }

    /// Asks for a name for the screen in front.
    pub(super) fn start_saving_view(&mut self) {
        if self.view().is_none() {
            return self.set_status(Tone::Info, "Open a table to save it as a view");
        }
        self.naming = Some(String::new());
    }

    /// What a saved view of the screen in front keeps: its trail, filter
    /// and sort, but not the row that happens to be selected.
    pub fn view_to_save(&self) -> Place {
        let mut place = self.place_here();
        place.steps.retain(|s| s.drill);
        place
    }

    pub(super) fn on_naming_key(&mut self, key: KeyEvent) {
        let Some(name) = self.naming.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.naming = None,
            KeyCode::Enter => self.save_view(),
            KeyCode::Backspace => {
                name.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => name.clear(),
            KeyCode::Char(c) if name.chars().count() < MAX_NAME_CHARS => name.push(c),
            _ => {}
        }
    }

    fn save_view(&mut self) {
        let name = self
            .naming
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            return self.set_status(Tone::Error, "Type a name for the view");
        }
        self.naming = None;
        let view = SavedView::new(&name, &self.view_to_save());
        match self.saved.save(view) {
            Ok(()) => self.set_status(
                Tone::Ok,
                format!("Saved view “{name}” · Ctrl+K and type its name to open it"),
            ),
            Err(err) => self.set_status(Tone::Error, format!("Couldn't save the view: {err:#}")),
        }
    }

    pub(super) fn delete_view(&mut self, name: &str) {
        match self.saved.delete(name) {
            Ok(()) => self.set_status(Tone::Ok, format!("Deleted saved view “{name}”")),
            Err(err) => self.set_status(Tone::Error, format!("Couldn't delete the view: {err:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::app::{SortBy, Step};

    use crossterm::event::KeyCode;
    use serde_json::json;

    fn ctrl(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, &[KeyCode::Char(c)]);
        }
    }

    fn choose(app: &mut App, query: &str) {
        ctrl(app, 'k');
        type_text(app, query);
        press(app, &[KeyCode::Enter]);
    }

    /// Warehouses › wh2, filtered to "s" and sorted by name, descending.
    fn filtered_namespaces(app: &mut App) {
        app.open_and_focus(resource("Lakehouse", "Warehouses"));
        answer(app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        press(app, &[KeyCode::Down, KeyCode::Enter]);
        answer(
            app,
            json!([{"namespace": "sales"}, {"namespace": "hr"}, {"namespace": "ops"}]),
        );
        press(app, &[KeyCode::Char('o'), KeyCode::Char('O')]);
        ctrl(app, 'f');
        type_text(app, "s");
        press(app, &[KeyCode::Enter]);
    }

    #[test]
    fn a_saved_view_reopens_its_trail_filter_and_sort() {
        let mut app = app();
        filtered_namespaces(&mut app);
        choose(&mut app, "save this");
        assert!(draw(&mut app, 100, 30).contains("Save view"));
        type_text(&mut app, "sales dbs");
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.naming.is_none());
        assert!(app.status.text.contains("Saved view “sales dbs”"));
        let saved = &app.saved.all()[0];
        assert_eq!(saved.steps, [Step::drill(&["name", ""], "wh2")]);
        assert_eq!(saved.filter, "s");
        assert_eq!(
            saved.sort,
            Some(SortBy {
                column: "Database".into(),
                descending: true
            })
        );

        app.open_and_focus(resource("Designer", "Datasets"));
        choose(&mut app, "sales dbs");
        answer(&mut app, json!([{"name": "wh1"}, {"name": "wh2"}]));
        answer(
            &mut app,
            json!([{"namespace": "hr"}, {"namespace": "sales"}, {"namespace": "ops"}]),
        );
        assert_eq!(
            app.breadcrumb(),
            ["Lakehouse", "Warehouses", "wh2", "Databases"]
        );
        let grid = app.view().unwrap().grid();
        assert_eq!(grid.filter, "s");
        let shown: Vec<&str> = grid
            .matches
            .iter()
            .map(|&i| grid.rows[i][0].as_str())
            .collect();
        assert_eq!(shown, ["sales", "ops"]);
    }

    #[test]
    fn naming_can_be_cancelled_and_needs_a_name() {
        let mut app = app();
        app.open_and_focus(resource("Designer", "Datasets"));
        choose(&mut app, "save this");
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.naming.is_some(), "an empty name is refused");
        type_text(&mut app, "q");
        assert!(!app.quit, "letters are the name");
        press(&mut app, &[KeyCode::Esc]);
        assert!(app.naming.is_none());
        assert!(app.saved.all().is_empty());
    }

    #[test]
    fn saved_views_can_be_deleted_from_the_palette() {
        let mut app = app();
        app.open_and_focus(resource("Designer", "Datasets"));
        app.saved
            .save(SavedView::new("old", &app.view_to_save()))
            .unwrap();
        choose(&mut app, "delete saved");
        assert!(app.saved.all().is_empty());
    }

    #[test]
    fn ctrl_e_lists_only_recent_places() {
        let mut app = app();
        ctrl(&mut app, 'e');
        assert!(app.palette.is_none());
        assert_eq!(app.status.text, "Nothing opened yet");
        app.run_action(Action::Open(resource("Designer", "Datasets")));
        ctrl(&mut app, 'e');
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("Recent"));
        assert!(screen.contains("Designer › Datasets"));
        assert!(!screen.contains("Lakehouse › Warehouses"), "only recent");
    }
}
