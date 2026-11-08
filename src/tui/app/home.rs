//! Opening the home dashboard.

use crate::tui::views::home::HomeView;

use super::App;

/// Opens the dashboard from the browser (the views keep their own keys).
pub(super) const HOME_KEY: char = 'H';

impl App {
    /// Shows the dashboard over the browser, fresh, in place of any view.
    pub(crate) fn open_home(&mut self) {
        self.views.clear();
        self.push_view(Box::new(HomeView::new()));
    }
}
