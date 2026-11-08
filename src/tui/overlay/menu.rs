//! The `a` menu: every action and link the selected row offers.

use crossterm::event::{KeyCode, KeyEvent};

use serde_json::Value;

use crate::tui::screen::{ActionSpec, LinkSpec, ScreenSpec};
use crate::tui::views::RowContext;

#[derive(Debug, Clone, Copy)]
pub enum MenuItem {
    Action(&'static ActionSpec),
    Link(&'static LinkSpec),
}

impl MenuItem {
    pub fn key(&self) -> char {
        match self {
            MenuItem::Action(action) => action.key,
            MenuItem::Link(link) => link.key,
        }
    }

    pub fn label(&self) -> String {
        match self {
            MenuItem::Action(action) => action.label.to_string(),
            MenuItem::Link(link) => format!("Go to {}", link.label),
        }
    }

    pub fn is_destructive(&self) -> bool {
        matches!(self, MenuItem::Action(action) if action.destructive)
    }
}

/// The actions, then the links, `spec` offers for `row`.
pub fn items_for(spec: &'static ScreenSpec, row: &Value) -> Vec<MenuItem> {
    spec.actions_for(row)
        .map(MenuItem::Action)
        .chain(spec.links_for(row).map(MenuItem::Link))
        .collect()
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Open,
    Picked(MenuItem),
    Closed,
}

#[derive(Debug, Clone)]
pub struct ActionMenu {
    pub title: String,
    pub items: Vec<MenuItem>,
    pub selected: usize,
    pub row: RowContext,
}

impl ActionMenu {
    pub fn new(title: impl Into<String>, items: Vec<MenuItem>, row: RowContext) -> Self {
        Self {
            title: title.into(),
            items,
            selected: 0,
            row,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc | KeyCode::Char('a') | KeyCode::Char('q') => Outcome::Closed,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Outcome::Open
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
                Outcome::Open
            }
            KeyCode::Enter => self
                .items
                .get(self.selected)
                .map_or(Outcome::Closed, |item| Outcome::Picked(*item)),
            KeyCode::Char(c) => self
                .items
                .iter()
                .find(|item| item.key() == c)
                .map_or(Outcome::Open, |item| Outcome::Picked(*item)),
            _ => Outcome::Open,
        }
    }
}
