//! Drawing the action menu, confirmations and forms.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Frame,
};

use crate::tui::app::Overlay;
use crate::tui::overlay::confirm::{Confirm, Mode};
use crate::tui::overlay::form::FormState;
use crate::tui::overlay::menu::ActionMenu;
use crate::tui::screen::FieldKind;
use crate::tui::theme::{self, BadgeTone};

const DIALOG_WIDTH: u16 = 64;

pub fn render(overlay: &Overlay, frame: &mut Frame, area: Rect) {
    match overlay {
        Overlay::Menu(menu) => render_menu(menu, frame, area),
        Overlay::Confirm { confirm, .. } => render_confirm(confirm, frame, area),
        Overlay::Form { form, .. } => render_form(form, frame, area),
    }
}

/// A box of `height` text rows centred in `area`, cleared, with a title;
/// returns the inside.
pub fn dialog(frame: &mut Frame, area: Rect, title: &str, height: u16, danger: bool) -> Rect {
    let width = DIALOG_WIDTH.min(area.width);
    let height = (height + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 3,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let border = if danger {
        theme::badge(BadgeTone::Bad)
    } else {
        theme::border(true)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), theme::title()))
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn render_menu(menu: &ActionMenu, frame: &mut Frame, area: Rect) {
    let inner = dialog(frame, area, &menu.title, menu.items.len() as u16 + 2, false);
    let mut lines: Vec<Line> = menu
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == menu.selected {
                theme::selected_row()
            } else if item.is_destructive() {
                theme::badge(BadgeTone::Bad)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!(" {} ", item.key()), theme::key_hint()),
                Span::styled(
                    format!(" {:<width$}", item.label(), width = inner.width as usize),
                    style,
                ),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "key or ⏎ to run · esc close",
        theme::muted(),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_confirm(confirm: &Confirm, frame: &mut Frame, area: Rect) {
    let typing = matches!(confirm.mode, Mode::TypeName(_));
    let height = 6 + u16::from(typing) + u16::from(confirm.mismatch);
    let inner = dialog(frame, area, &confirm.title, height, confirm.destructive);
    let mut lines = vec![
        Line::from(Span::styled(confirm.message.clone(), theme::title())),
        Line::from(""),
        Line::from(Span::styled(confirm.request.clone(), theme::muted())),
        Line::from(""),
    ];
    if typing {
        lines.push(Line::from(vec![
            Span::styled("> ", theme::key_hint()),
            Span::raw(confirm.typed.clone()),
            Span::styled("▏", Style::default().fg(theme::ACCENT)),
        ]));
    }
    if confirm.mismatch {
        lines.push(Line::from(Span::styled(
            "That doesn't match the name",
            theme::badge(BadgeTone::Bad),
        )));
    }
    lines.push(Line::from(Span::styled(confirm.prompt(), theme::muted())));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_form(form: &FormState, frame: &mut Frame, area: Rect) {
    let mut lines = form_lines(form);
    lines.push(form_footer(form));
    let inner = dialog(frame, area, form.spec.title, lines.len() as u16, false);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A form's fields, values, errors and hints, for views that host a form
/// with extra lines of their own (a live preview).
pub fn form_lines(form: &FormState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for index in 0..form.values.len() {
        let field = form.field(index);
        let focused = index == form.focus;
        let marker = if field.required { " *" } else { "" };
        let label_style = if focused {
            theme::key_hint()
        } else {
            theme::muted()
        };
        lines.push(Line::from(Span::styled(
            format!("{}{marker}", field.label),
            label_style,
        )));
        let value = form.display(index);
        let cursor = focused
            && matches!(
                field.kind,
                FieldKind::Text | FieldKind::Multiline | FieldKind::Number { .. }
            );
        for (n, text) in value.split('\n').enumerate() {
            let mut spans = vec![Span::raw("  "), Span::raw(text.to_string())];
            if cursor && n == value.split('\n').count() - 1 {
                spans.push(Span::styled("▏", Style::default().fg(theme::ACCENT)));
            }
            lines.push(Line::from(spans));
        }
        if let Some(err) = &form.errors[index] {
            lines.push(Line::from(Span::styled(
                format!("  {err}"),
                theme::badge(BadgeTone::Bad),
            )));
        } else if let Some(hint) = field.hint.filter(|_| focused) {
            lines.push(Line::from(Span::styled(
                format!("  {hint}"),
                theme::muted(),
            )));
        }
    }
    lines.push(Line::from(""));
    lines
}

/// The submit and key hints under a form.
pub fn form_footer(form: &FormState) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("⏎ {}", form.spec.submit),
            theme::key_hint().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  tab next · ←→ choose · space toggle · esc cancel",
            theme::muted(),
        ),
    ])
}
