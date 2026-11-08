//! A form inside a view, for flows the action overlay can't express: a
//! form whose request is built in code (a job run), or that shows a live
//! preview under its fields (a schedule's timing).

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, text::Line, widgets::Paragraph, Frame};
use serde_json::Value;

use super::Ctx;
use crate::tui::overlay::form::{FormState, Outcome};
use crate::tui::screen::{Choices, FieldKind, FormSpec};
use crate::tui::template::{self, Encoding, Scope};
use crate::tui::ui::overlays;

pub struct HostedForm {
    pub form: FormState,
}

impl HostedForm {
    /// Fills defaults from `scope` and starts loading endpoint choices.
    pub fn open(spec: &'static FormSpec, scope: &Scope, ctx: &mut Ctx) -> Self {
        let mut form = FormState::new(spec, |default| {
            template::fill(default, scope, Encoding::Text).ok()
        });
        for field in form.endpoint_fields() {
            let FieldKind::Select(Choices::Endpoint { source, .. }) = form.field(field).kind else {
                continue;
            };
            match ctx.fetch(&source, scope) {
                Ok(id) => form.set_loading(field, id),
                Err(err) => form.set_failed(field, err.to_string()),
            }
        }
        Self { form }
    }

    /// Lands choices; false when `id` wasn't for this form.
    pub fn on_fetched(&mut self, id: u64, result: Result<Value, String>) -> bool {
        self.form.choices_loaded(id, result)
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        self.form.on_key(key)
    }

    /// The selected choice's label for field `name`.
    pub fn choice_label(&self, name: &str) -> Option<String> {
        let index = self.form.spec.fields.iter().position(|f| f.name == name)?;
        let shown = self.form.display(index);
        let label = shown.strip_prefix("‹ ")?.strip_suffix(" ›")?;
        Some(label.to_string())
    }

    /// The form in a dialog, with `extra` lines under the fields.
    pub fn render(&self, frame: &mut Frame, area: Rect, extra: Vec<Line<'static>>) {
        let mut lines = overlays::form_lines(&self.form);
        lines.extend(extra);
        lines.push(overlays::form_footer(&self.form));
        let inner = overlays::dialog(frame, area, self.form.spec.title, lines.len() as u16, false);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}
