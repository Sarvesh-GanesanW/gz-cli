//! Clicks and the scroll wheel.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::{App, Focus, SidebarRow, WHEEL_STEP};

impl App {
    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        let overlay = self.palette.is_some() || self.help || self.overlay.is_some();
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -WHEEL_STEP
                } else {
                    WHEEL_STEP
                };
                if let Some(modal) = self.modal.as_mut() {
                    modal.scroll_by(delta);
                } else if overlay {
                } else if let Some((_, view)) = self.views.top_mut() {
                    view.on_scroll(delta);
                } else if inside(self.hits.transcript) {
                    self.chat.scroll = (self.chat.scroll as isize - delta).max(0) as usize;
                } else if inside(self.hits.chat_list) {
                    self.chat.move_sel(delta.signum());
                } else if inside(self.hits.sidebar_rows) {
                    self.move_cursor(delta.signum());
                } else if let Some(view) = self.view_mut() {
                    view.grid_mut().move_sel(delta);
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if !overlay && self.modal.is_none() && self.views.is_empty() =>
            {
                self.click(
                    x,
                    y,
                    inside(self.hits.sidebar_rows),
                    inside(self.hits.chat_list),
                    inside(self.hits.grid_rows),
                );
            }
            _ => {}
        }
    }

    fn click(&mut self, _x: u16, y: u16, on_sidebar: bool, on_chat_list: bool, on_grid: bool) {
        if on_sidebar {
            let row = (y - self.hits.sidebar_rows.y) as usize + self.hits.sidebar_offset;
            let rows = self.sidebar_rows();
            match rows.get(row) {
                Some(SidebarRow::Module(m)) => {
                    self.cursor = row;
                    self.expanded[*m] = !self.expanded[*m];
                }
                Some(SidebarRow::Resource(id)) => {
                    self.cursor = row;
                    self.open_and_focus(*id);
                }
                None => {}
            }
        } else if on_chat_list {
            let index = (y - self.hits.chat_list.y) as usize + self.hits.chat_list_offset;
            if let Some(conversation) = self.chat.conversations.get(index).cloned() {
                self.focus = Focus::Content;
                self.chat.selected = index;
                self.open_conversation(conversation);
            }
        } else if on_grid {
            self.focus = Focus::Content;
            let offset = (y - self.hits.grid_rows.y) as usize;
            if let Some(view) = self.view_mut() {
                let grid = view.grid_mut();
                let target = grid.row_offset + offset;
                if target < grid.visible_len() {
                    grid.selected = target;
                }
            }
        } else if self.hits.content.width > 0
            && y >= self.hits.content.y
            && y < self.hits.content.y + self.hits.content.height
        {
            self.focus = Focus::Content;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    use crate::tui::nav::ResourceId;

    #[test]
    fn clicking_a_sidebar_resource_opens_it() {
        let mut app = app();
        app.hits.sidebar_rows = Rect::new(1, 2, 20, 30);
        let row = app
            .row_of(ResourceId {
                module: 0,
                resource: 1,
            })
            .unwrap();
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 2 + row as u16,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.active,
            ResourceId {
                module: 0,
                resource: 1
            }
        );
        assert_eq!(app.focus, Focus::Content);
    }
}
