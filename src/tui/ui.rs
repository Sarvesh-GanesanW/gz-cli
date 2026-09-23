use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use crate::tui::app::{App, Screen};

fn title_case(app: &App) -> String {
    let client = app.resolved.client().unwrap_or_default();
    let site = app.resolved.site().unwrap_or_default();
    if client.is_empty() && site.is_empty() {
        "groundzero".to_string()
    } else {
        format!("groundzero · {client} · {site}")
    }
}

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(area);
    render_tabs(app, frame, rows[0]);
    render_kinds(app, frame, rows[1]);
    render_grid(app, frame, rows[2]);
    render_status(app, frame, rows[3]);
    if let Some(modal) = app.modal.as_ref() {
        render_modal(frame, area, &modal.title, &modal.lines, modal.scroll);
    }
    if app.help {
        render_help(frame, area);
    }
}

fn render_tabs(app: &App, frame: &mut Frame, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    for (i, screen) in Screen::ALL.iter().enumerate() {
        let active = *screen == app.screen;
        let label = format!(" {}:{} ", i + 1, screen.title());
        let style = if active {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", title_case(app)));
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn render_kinds(app: &App, frame: &mut Frame, area: Rect) {
    let screen = app.screen;
    let kinds = screen.kinds();
    let selected = app.module().kind;
    let mut spans: Vec<Span> = Vec::new();
    for (i, kind) in kinds.iter().enumerate() {
        let style = if i == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(format!(" {} ", kind.label), style));
    }
    if kinds.len() > 1 {
        spans.push(Span::styled(
            "  ←/→ or Tab to switch resource",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let crumb = app.module().breadcrumb(screen);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {crumb} "));
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn render_grid(app: &mut App, frame: &mut Frame, area: Rect) {
    let pending = app.pending;
    let grid = app.module_mut().grid_mut();
    let height = area.height.saturating_sub(3) as usize;
    if grid.visible_len() == 0 {
        let note = if pending > 0 {
            "loading…".to_string()
        } else {
            grid.empty_note.clone()
        };
        let block = Block::default().borders(Borders::ALL);
        frame.render_widget(Paragraph::new(note).block(block), area);
        return;
    }
    if grid.selected >= grid.row_offset + height.max(1) {
        grid.row_offset = grid.selected + 1 - height.max(1);
    }
    if grid.selected < grid.row_offset {
        grid.row_offset = grid.selected;
    }
    let offset = grid.row_offset;
    let visible: Vec<usize> = grid.visible_rows();
    let end = (offset + height.max(1)).min(visible.len());
    let header = Row::new(
        grid.cols
            .iter()
            .map(|c| Cell::from(c.clone()).style(Style::default().add_modifier(Modifier::BOLD))),
    );
    let rows: Vec<Row> = visible[offset..end]
        .iter()
        .map(|row_idx| {
            let cells: Vec<Cell> = grid.rows[*row_idx]
                .iter()
                .map(|c| Cell::from(truncate(c, 60)))
                .collect();
            Row::new(cells)
        })
        .collect();
    let widths: Vec<Constraint> = grid
        .cols
        .iter()
        .map(|c| Constraint::Length((c.len() + 2).clamp(8, 62) as u16))
        .collect();
    let selected = grid.selected.saturating_sub(offset);
    let mut state = TableState::new().with_selected(Some(selected));
    let filter = if grid.filter.is_empty() {
        String::new()
    } else {
        format!(" · filter: {}", grid.filter)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} rows{filter} ", visible.len(),));
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_status(app: &App, frame: &mut Frame, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);
    let status = if app.editing_filter {
        format!("filter: {}", app.filter_area.lines().join(" "))
    } else {
        app.status.clone()
    };
    let pending = if app.pending > 0 {
        format!("{}…", ".".repeat((app.tick / 4 % 3 + 1) as usize))
    } else {
        "idle".to_string()
    };
    let block = Block::default().borders(Borders::ALL);
    frame.render_widget(Paragraph::new(status).block(block), cols[0]);
    let keys = if app.editing_filter {
        "Enter apply · Esc cancel"
    } else {
        "↑↓ select · Enter open · ⌫ back · / filter · r reload · ? help · q quit"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {pending} "));
    frame.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(Color::DarkGray))).block(block),
        cols[1],
    );
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let width = area.width * width_pct / 100;
    let height = area.height * height_pct / 100;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.max(10), height.max(5))
}

fn render_modal(frame: &mut Frame, area: Rect, title: &str, lines: &[String], scroll: usize) {
    let modal = centered(area, 80, 80);
    frame.render_widget(Clear, modal);
    let text: Vec<Line> = lines
        .iter()
        .skip(scroll)
        .map(|l| Line::from(l.clone()))
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} (Esc to close) "));
    frame.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        modal,
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let modal = centered(area, 60, 60);
    frame.render_widget(Clear, modal);
    let lines = vec![
        Line::from("modules: 1 Chat · 2 Workspaces · 3 Connections · 4 Designer"),
        Line::from("         5 DE/ML · 6 Lakehouse · 7 Catalog · 8 Schedules"),
        Line::from(""),
        Line::from("↑/↓ or j/k    move selection"),
        Line::from("←/→ or Tab    switch resource picker"),
        Line::from("Enter         open / drill into selection"),
        Line::from("Backspace     go back up one level"),
        Line::from("/             filter rows"),
        Line::from("r             reload current resource"),
        Line::from("PgUp/PgDn     page through rows"),
        Line::from("?             toggle this help"),
        Line::from("q             quit"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" gz help (Esc to close) ");
    frame.render_widget(Paragraph::new(lines).block(block), modal);
}
