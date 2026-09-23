use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use crate::config::redact_token;
use crate::tui::app::{App, Screen};
use crate::tui::model::Grid;

const ACCENT: Color = Color::Cyan;
const ACCENT2: Color = Color::Magenta;
const MUTED: Color = Color::DarkGray;
const GOOD: Color = Color::Green;
const BAD: Color = Color::Red;
const WARN: Color = Color::Yellow;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);
    render_header(app, frame, rows[0]);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(18), Constraint::Min(20)])
        .split(rows[1]);
    render_nav(app, frame, cols[0]);
    match app.screen {
        Screen::Home => render_home(app, frame, cols[1]),
        Screen::Warehouses => render_browser(app, frame, cols[1], BrowserKind::Warehouses),
        Screen::Sql => render_sql(app, frame, cols[1]),
        Screen::Jobs => render_jobs(app, frame, cols[1]),
        Screen::Files => render_files(app, frame, cols[1]),
        Screen::Agents => render_browser(app, frame, cols[1], BrowserKind::Agents),
        Screen::Logs => render_browser(app, frame, cols[1], BrowserKind::Logs),
    }
    render_footer(app, frame, rows[2]);
    if app.help {
        render_help(frame, area);
    } else if app.modal.is_some() {
        render_modal(app, frame, area);
    } else if app.editing_filter {
        render_filter(app, frame, area);
    }
}

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let token = app.resolved.token();
    let (dot, auth) = match &token {
        Some(t) => (GOOD, format!("auth {}", redact_token(t))),
        None => (BAD, "no token".to_string()),
    };
    let spin = if app.pending > 0 {
        SPINNER[(app.tick as usize) % SPINNER.len()]
    } else {
        " "
    };
    let title = Line::from(vec![
        Span::styled(
            " ▚ GZ ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " GROUNDZERO ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{spin} {auth} "), Style::default().fg(dot)),
        Span::styled(
            format!(
                "{} · {} · {}",
                app.resolved.profile_name,
                app.resolved.client().unwrap_or_else(|| "?".to_string()),
                app.resolved.site().unwrap_or_else(|| "?".to_string()),
            ),
            Style::default().fg(MUTED),
        ),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "command center for lakehouse · warehouses · etl · mlops · agents",
            Style::default().fg(MUTED),
        )])),
        inner,
    );
}

fn render_nav(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = Screen::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let active = *s == app.screen;
            let style = if active {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", i + 1),
                    Style::default().fg(if active { Color::Black } else { MUTED }),
                ),
                Span::styled(s.title(), style),
            ]))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(MUTED))
            .title("screens"),
    );
    frame.render_widget(list, area);
}

fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let keys = screen_keys(app);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(Paragraph::new(Line::from(keys)), rows[0]);
    let status_style = if app.status.contains("failed") || app.status.contains("error") {
        Style::default().fg(BAD)
    } else {
        Style::default().fg(Color::Gray)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate(&app.status, inner.width as usize),
            status_style,
        )),
        rows[1],
    );
}

fn key_hint(key: &str, action: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default().fg(Color::Black).bg(MUTED),
        ),
        Span::styled(format!("{action} "), Style::default().fg(Color::Gray)),
    ]
}

fn screen_keys(app: &App) -> Vec<Span<'static>> {
    let mut spans = vec![];
    let mut add = |k: &str, a: &str| spans.extend(key_hint(k, a));
    match app.screen {
        Screen::Home => {
            add("1-7", "screens");
            add("?", "help");
            add("q", "quit");
        }
        Screen::Warehouses | Screen::Agents => {
            add("enter", "drill-in");
            add("bksp", "up");
            add("/", "filter");
            add("v", "view row");
            add("r", "refresh");
        }
        Screen::Sql => {
            if app.sql_editing {
                add("tab", "next field");
                add("ctrl+r", "run");
                add("esc", "browse results");
            } else {
                add("i", "edit");
                add("ctrl+r", "run");
                add("v", "view row");
            }
        }
        Screen::Jobs => {
            add("enter", "status/logs");
            add("t", "toggle view");
            add("a", "auto 5s");
            add("s", "submit file");
            add("v", "view row");
        }
        Screen::Files => {
            add("enter", "open dir");
            add("bksp", "up");
            add("d", "download");
            add("i", "connection");
            add("r", "refresh");
        }
        Screen::Logs => {
            add("enter", "detail");
            add("v", "view row");
            add("r", "refresh");
        }
    }
    add("tab", "next screen");
    add("q", "quit");
    spans
}

fn truncate(text: &str, width: usize) -> String {
    if width < 4 || text.chars().count() <= width {
        return text.to_string();
    }
    format!("{}…", text.chars().take(width - 1).collect::<String>())
}

// ----- home -----

fn render_home(app: &App, frame: &mut Frame, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(6)])
        .split(cols[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(6)])
        .split(cols[1]);

    let resolved = &app.resolved;
    let profile = vec![
        Line::from(vec![
            Span::styled("profile  ", Style::default().fg(ACCENT)),
            Span::raw(&resolved.profile_name),
        ]),
        Line::from(vec![
            Span::styled("client   ", Style::default().fg(ACCENT)),
            Span::raw(resolved.client().unwrap_or_else(|| "—".to_string())),
        ]),
        Line::from(vec![
            Span::styled("site     ", Style::default().fg(ACCENT)),
            Span::raw(resolved.site().unwrap_or_else(|| "—".to_string())),
        ]),
        Line::from(vec![
            Span::styled("domain   ", Style::default().fg(ACCENT)),
            Span::raw(resolved.domain()),
        ]),
        Line::from(vec![
            Span::styled("authHost ", Style::default().fg(ACCENT)),
            Span::raw(resolved.auth_host().unwrap_or_else(|| "—".to_string())),
        ]),
    ];
    frame.render_widget(card("profile", profile), left[0]);

    let token = resolved.token();
    let auth = vec![
        match &token {
            Some(t) => Line::from(vec![
                Span::styled("● logged in  ", Style::default().fg(GOOD)),
                Span::raw(redact_token(t)),
            ]),
            None => Line::from(Span::styled(
                "○ logged out — run `gz auth login` outside the TUI",
                Style::default().fg(BAD),
            )),
        },
        Line::from(vec![
            Span::styled("source  ", Style::default().fg(ACCENT)),
            Span::raw(resolved.token_source),
        ]),
        Line::from(vec![
            Span::styled("scope   ", Style::default().fg(ACCENT)),
            Span::raw(
                resolved
                    .profile
                    .scope
                    .clone()
                    .unwrap_or_else(|| "—".to_string()),
            ),
        ]),
        Line::from(vec![
            Span::styled("expired ", Style::default().fg(ACCENT)),
            Span::raw(
                if token.is_some() && resolved.token_expired() {
                    "yes — re-login"
                } else {
                    "no"
                }
                .to_string(),
            ),
        ]),
    ];
    frame.render_widget(card("auth", auth), right[0]);

    let services = ["lakehouse", "etl", "files", "agents", "chat", "logs"]
        .iter()
        .map(|s| {
            let (url, style) = match resolved.base_for(s) {
                Ok(u) => (truncate(&u, 52), Style::default().fg(Color::Gray)),
                Err(_) => (
                    "(needs --service override)".to_string(),
                    Style::default().fg(WARN),
                ),
            };
            Line::from(vec![
                Span::styled(format!("{s:<10}"), Style::default().fg(ACCENT2)),
                Span::styled(url, style),
            ])
        })
        .collect();
    frame.render_widget(card("services (derived)", services), left[1]);

    let hints = vec![
        Line::from("2  browse warehouses → namespaces → tables"),
        Line::from("3  write SQL, Ctrl+R to run, results as a grid"),
        Line::from("4  watch a job: id, Enter, then a for auto-refresh"),
        Line::from("5  browse storage, Enter to descend, d to download"),
        Line::from("6  agents → runs → o for observability"),
        Line::from(""),
        Line::from(Span::styled(
            "every grid: j/k move · / filter · v view row · PgUp/PgDn",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(card("start here", hints), right[1]);
}

fn card<'a>(title: &'a str, lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(MUTED))
            .title(Span::styled(
                format!(" {title} "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
    )
}

// ----- generic browser -----

#[derive(Clone, Copy)]
enum BrowserKind {
    Warehouses,
    Agents,
    Logs,
}

fn render_browser(app: &mut App, frame: &mut Frame, area: Rect, kind: BrowserKind) {
    let (title, trail) = match kind {
        BrowserKind::Warehouses => ("warehouses", app.wh_trail.clone()),
        BrowserKind::Agents => ("agents", app.agent_trail()),
        BrowserKind::Logs => ("log schedules", Vec::<String>::new()),
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(4)])
        .split(area);
    let crumb = if trail.is_empty() {
        "·".to_string()
    } else {
        trail.join(" › ")
    };
    frame.render_widget(
        Paragraph::new(Span::styled(crumb, Style::default().fg(ACCENT2))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .title(format!(" {title} ")),
        ),
        rows[0],
    );
    match kind {
        BrowserKind::Warehouses => render_grid(frame, rows[1], &mut app.wh, "rows"),
        BrowserKind::Agents => render_grid(frame, rows[1], &mut app.agent_grid, "rows"),
        BrowserKind::Logs => render_grid(frame, rows[1], &mut app.log_grid, "rows"),
    }
}

pub fn render_grid(frame: &mut Frame, area: Rect, grid: &mut Grid, title: &str) {
    let height = area.height.saturating_sub(3) as usize;
    if height > 0 && !grid.matches.is_empty() {
        if grid.selected < grid.row_offset {
            grid.row_offset = grid.selected;
        } else if grid.selected >= grid.row_offset + height {
            grid.row_offset = grid.selected + 1 - height;
        }
    }
    let visible = grid.visible_rows();
    let end = (grid.row_offset + height).min(visible.len());
    let cols: Vec<usize> = (grid.col_offset..grid.cols.len()).collect();
    let widths: Vec<Constraint> = cols.iter().map(|_| Constraint::Fill(1)).collect();
    let header = Row::new(cols.iter().map(|c| grid.cols[*c].clone()))
        .style(Style::default().fg(ACCENT2).add_modifier(Modifier::BOLD))
        .height(1);
    let body: Vec<Row> = visible[grid.row_offset..end]
        .iter()
        .map(|r| {
            Row::new(
                cols.iter()
                    .map(|c| grid.rows[*r].get(*c).cloned().unwrap_or_default()),
            )
        })
        .collect();
    let count = format!(" {title} {}/{} ", end.min(visible.len()), visible.len());
    let filter = if grid.filter.is_empty() {
        String::new()
    } else {
        format!(" /{} ", grid.filter)
    };
    let mut state = TableState::default();
    state.select(Some(grid.selected.saturating_sub(grid.row_offset)));
    let table = Table::new(body, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .title(format!("{count}{filter}")),
        )
        .row_highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    if grid.visible_len() == 0 {
        let msg = Paragraph::new(grid.empty_note.clone())
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(MUTED))
                    .title(format!(" {title} ")),
            );
        frame.render_widget(msg, area);
    } else {
        frame.render_stateful_widget(table, area, &mut state);
    }
}

// ----- sql -----

fn render_sql(app: &mut App, frame: &mut Frame, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(38),
            Constraint::Min(6),
        ])
        .split(area);
    let fields = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 4); 4])
        .split(rows[0]);
    for (i, field) in ["warehouse", "username", "password", "compute"]
        .iter()
        .enumerate()
    {
        let focused = app.sql_editing && app.sql_focus == i;
        let area = &mut app.sql_fields[i].area;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
            .title(format!(" {field} "));
        let inner = block.inner(fields[i]);
        frame.render_widget(block, fields[i]);
        frame.render_widget(&*area, inner);
    }
    let editor_focused = app.sql_editing && app.sql_focus == 4;
    let editor_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if editor_focused { ACCENT } else { MUTED }))
        .title(" sql — Ctrl+R to run ");
    let inner = editor_block.inner(rows[1]);
    frame.render_widget(editor_block, rows[1]);
    frame.render_widget(&app.sql_editor, inner);
    let title = if app.sql_status.is_empty() {
        "results".to_string()
    } else {
        format!("results · {}", app.sql_status)
    };
    render_grid(frame, rows[2], &mut app.sql_results, &title);
}

// ----- jobs -----

fn render_jobs(app: &mut App, frame: &mut Frame, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(6)])
        .split(area);
    let fields = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    for (i, label) in ["job id — Enter=status", "submit file — s=submit"]
        .iter()
        .enumerate()
    {
        let focused = app.job_editing && app.job_focus == i;
        let area_widget = &mut app.job_fields[i].area;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
            .title(format!(" {label} "));
        let inner = block.inner(fields[i]);
        frame.render_widget(block, fields[i]);
        frame.render_widget(&*area_widget, inner);
    }
    let title = format!(
        "{} {}",
        app.job_view_title(),
        if app.job_auto { "· auto 5s" } else { "" }
    );
    render_grid(frame, rows[1], &mut app.job_grid, title.trim());
}

// ----- files -----

fn render_files(app: &mut App, frame: &mut Frame, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
        ])
        .split(area);
    let focused = app.fs_conn_editing;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
        .title(" connection — i to edit, Enter to load ");
    let inner = block.inner(rows[0]);
    frame.render_widget(block, rows[0]);
    frame.render_widget(&app.fs_conn.area, inner);
    let prefix = if app.fs_prefix.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", app.fs_prefix)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(prefix, Style::default().fg(ACCENT2))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(MUTED))
                .title(" path "),
        ),
        rows[1],
    );
    render_grid(frame, rows[2], &mut app.fs_grid, "objects");
}

// ----- modal / help / filter -----

fn centered(area: Rect, w_pct: u16, h_pct: u16) -> Rect {
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - w_pct) / 2),
            Constraint::Percentage(w_pct),
            Constraint::Percentage((100 - w_pct) / 2),
        ])
        .split(area);
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - h_pct) / 2),
            Constraint::Percentage(h_pct),
            Constraint::Percentage((100 - h_pct) / 2),
        ])
        .split(h[1]);
    v[1]
}

fn render_modal(app: &mut App, frame: &mut Frame, area: Rect) {
    let Some(modal) = app.modal.as_mut() else {
        return;
    };
    let area = centered(area, 80, 76);
    frame.render_widget(Clear, area);
    let text: Vec<Line> = modal.lines.iter().map(|l| Line::from(l.clone())).collect();
    let total = text.len();
    let visible = area.height.saturating_sub(2) as usize;
    modal.scroll = modal.scroll.min(total.saturating_sub(visible).max(0));
    let start = modal.scroll;
    let end = (start + visible).min(total);
    let title = format!(
        " {} ({}/{}) ",
        modal.title,
        if total == 0 { 0 } else { start + 1 },
        total
    );
    let para = Paragraph::new(text[start..end].to_vec())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(title),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_filter(app: &mut App, frame: &mut Frame, area: Rect) {
    let area = centered(area, 60, 20);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(" filter — Enter to apply, Esc to cancel ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&app.filter_area, inner);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let area = centered(area, 70, 80);
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "global",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  1-7 / Tab      switch screens"),
        Line::from("  ?              this help"),
        Line::from("  q              quit"),
        Line::from(""),
        Line::from(Span::styled(
            "grids",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  j/k · ↑/↓      move selection"),
        Line::from("  h/l · ←/→      scroll columns"),
        Line::from("  PgUp/PgDn·Home/End  jump"),
        Line::from("  /  filter · c  clear filter"),
        Line::from("  v  view row as JSON · r  refresh"),
        Line::from(""),
        Line::from(Span::styled(
            "screens",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  warehouses  Enter drill-in · Bksp up · d describe · s sample"),
        Line::from("              (table name = / filter text or selected row)"),
        Line::from("  sql         Tab cycle fields · Ctrl+R run · Esc browse"),
        Line::from("  jobs        Enter status · t toggle logs · a auto · s submit"),
        Line::from("  files       i connection · Enter open · Bksp up · d download"),
        Line::from("  agents      Enter runs · o observability · e events"),
        Line::from("  logs        Enter schedule detail"),
        Line::from(""),
        Line::from(Span::styled(
            "modal",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  j/k scroll · Enter/Esc close"),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" gz help "),
        ),
        area,
    );
}
