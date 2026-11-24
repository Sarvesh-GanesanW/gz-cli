//! The live region's place on the terminal: a fixed-position viewport under
//! normal scrollback that gz moves itself, so the screen stays consistent
//! after any sequence of resizes and pop-ups.
//!
//! Two things a plain inline viewport gets wrong are handled here:
//! - A terminal resize reflows (or crops) everything already on screen,
//!   including the old input box, and nothing redraws it. Any size change
//!   therefore clears the screen and scrollback and prints the transcript
//!   again at the new size, once the size has settled.
//! - A pop-up that grows the region would push transcript rows into
//!   scrollback, and closing it would leave the input box over empty rows.
//!   Instead a pop-up covers the transcript rows above it, and gz keeps
//!   the transcript rows that are on screen, so closing it paints them
//!   back. Rows only reach scrollback when new lines are printed, and they
//!   are painted first, so scrollback holds the transcript exactly once.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

/// A resize is acted on once the size has stayed the same this long, so
/// dragging a window edge reprints once, not on every step.
pub const RESIZE_SETTLE: Duration = Duration::from_millis(150);

/// Clearing the scrollback, which [`Backend`] has no call for.
pub trait ClearScrollback {
    fn clear_scrollback(&mut self) -> io::Result<()>;
}

impl<W: io::Write> ClearScrollback for ratatui::backend::CrosstermBackend<W> {
    fn clear_scrollback(&mut self) -> io::Result<()> {
        crossterm::queue!(
            self,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge)
        )
    }
}

impl ClearScrollback for ratatui::backend::TestBackend {
    fn clear_scrollback(&mut self) -> io::Result<()> {
        // The test backend's scrollback can't be emptied; tests check the
        // visible screen.
        Ok(())
    }
}

/// The live region and where it sits.
///
/// The screen holds, top to bottom: rows gz didn't print (`origin` of
/// them, from before gz code started), the transcript rows still on screen
/// (`shown`), then the region. A pop-up may cover the end of `shown`; it
/// never covers the rows gz can't paint back.
pub struct LiveScreen<B: Backend + ClearScrollback> {
    terminal: Terminal<B>,
    screen: Size,
    /// Rows above the transcript that gz didn't print.
    origin: u16,
    /// The transcript has reached the screen's bottom: the region stays
    /// there, and rows it frees above it stay blank until the next print
    /// (they were scrolled off and can't come back without doubling them).
    anchored: bool,
    /// The transcript rows on screen, covered ones included.
    shown: VecDeque<Line<'static>>,
    /// First row of the region.
    top: u16,
    height: u16,
    /// When the terminal last changed size, while a reprint is due.
    resized_at: Option<Instant>,
    /// Something else has the terminal (see [`suspend`](Self::suspend)).
    suspended: bool,
}

impl<B: Backend + ClearScrollback> LiveScreen<B> {
    /// Starts the region at the cursor's row, under whatever is on screen.
    pub fn open(mut backend: B) -> io::Result<Self> {
        let screen = backend.size()?;
        let top = backend.get_cursor_position()?.y.min(last_row(screen));
        let area = Rect::new(0, top, screen.width, 0);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )?;
        Ok(Self {
            terminal,
            screen,
            origin: top,
            anchored: false,
            shown: VecDeque::new(),
            top,
            height: 0,
            resized_at: None,
            suspended: false,
        })
    }

    pub fn width(&self) -> u16 {
        self.screen.width
    }

    #[cfg(test)]
    pub fn backend(&self) -> &B {
        self.terminal.backend()
    }

    #[cfg(test)]
    pub fn backend_mut(&mut self) -> &mut B {
        self.terminal.backend_mut()
    }

    /// Notes a size change; true once the new size has settled and the
    /// caller should [`reprint`](Self::reprint).
    pub fn reprint_due(&mut self, now: Instant) -> io::Result<bool> {
        let size = self.terminal.backend().size()?;
        if size != self.screen {
            self.screen = size;
            self.resized_at = Some(now);
            return Ok(false);
        }
        Ok(self
            .resized_at
            .is_some_and(|at| now.duration_since(at) >= RESIZE_SETTLE))
    }

    /// True between a size change and its reprint: what is on screen is
    /// the terminal's reflow of the old layout, so nothing should be drawn
    /// over it.
    pub fn resize_pending(&self) -> bool {
        self.resized_at.is_some()
    }

    /// Clears the screen and scrollback and prints `lines` (the whole
    /// transcript, rendered at the current width) from the top, with the
    /// region right under them.
    pub fn reprint(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        // The scrollback holds the terminal's reflowed copies of the old
        // layout (old input boxes among them); only purging removes them.
        self.restart(true)?;
        self.insert(lines)
    }

    /// Clears the visible screen and starts the region at the top
    /// (`/clear`). The scrollback is left alone: nothing in it is stale.
    pub fn clear_all(&mut self) -> io::Result<()> {
        // Terminals that keep a cleared screen in scrollback keep only the
        // transcript, never the input box.
        self.clear_rows(self.top, self.height)?;
        self.restart(false)?;
        self.place()
    }

    /// Empties the screen (and, with `purge`, the scrollback) and forgets
    /// what was on it, ready for the region to start again at the top.
    fn restart(&mut self, purge: bool) -> io::Result<()> {
        self.resized_at = None;
        self.suspended = false;
        self.screen = self.terminal.backend().size()?;
        let backend = self.terminal.backend_mut();
        backend.set_cursor_position(Position::ORIGIN)?;
        backend.clear_region(ClearType::All)?;
        if purge {
            backend.clear_scrollback()?;
        }
        self.origin = 0;
        self.anchored = false;
        self.shown.clear();
        self.top = 0;
        self.height = self.height.min(self.max_height());
        Ok(())
    }

    /// The tallest the region may be: one row is always left for
    /// scrollback, so the terminal never scrolls the region itself.
    pub fn max_height(&self) -> u16 {
        self.screen.height.saturating_sub(1).max(1)
    }

    /// Prints `lines` right above the region (into scrollback once they
    /// scroll off), uncovering any rows a pop-up covered.
    pub fn insert(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        self.append(lines, self.height)?;
        let end = self.transcript_end();
        let rows = self.screen.height;
        self.top = if self.anchored {
            rows - self.height
        } else {
            end
        };
        self.place()
    }

    /// Makes the region `wanted` rows tall, of which the last `pop_up`
    /// belong to a menu or picker. The rest always has room under the
    /// transcript (the screen scrolls for it); a pop-up may instead cover
    /// the transcript's last rows, which are painted back when it closes.
    pub fn fit(&mut self, wanted: u16, pop_up: u16) -> io::Result<()> {
        let wanted = wanted.min(self.max_height());
        if wanted == self.height {
            return Ok(());
        }
        let rows = self.screen.height;
        let base = wanted.saturating_sub(pop_up);
        if self.transcript_end() + base > rows {
            self.append(Vec::new(), base)?;
            let end = self.transcript_end();
            self.clear_rows(end, rows - end)?;
            self.top = end;
            self.height = 0;
        }
        // Rows gz didn't print can't be painted back, so they scroll away
        // instead of being covered.
        let lowest_top = rows - wanted;
        if lowest_top < self.origin {
            let scroll = self.origin - lowest_top;
            self.scroll_up(scroll)?;
            self.origin -= scroll;
            self.top = self.top.saturating_sub(scroll);
        }
        let end = self.transcript_end();
        let top = if self.anchored {
            lowest_top
        } else {
            end.min(lowest_top)
        };
        if top > self.top {
            self.paint_shown(self.top, top)?;
            let blank_from = self.top.max(end);
            if top > blank_from {
                self.clear_rows(blank_from, top - blank_from)?;
            }
        }
        let old_bottom = self.top + self.height;
        if top + wanted < old_bottom {
            self.clear_rows(top + wanted, old_bottom - (top + wanted))?;
        }
        self.top = top;
        self.height = wanted;
        self.place()
    }

    /// Draws the region.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        self.terminal.draw(|frame| {
            render(frame);
            crate::tui::theme::recolor(frame.buffer_mut());
        })?;
        Ok(())
    }

    /// Empties the region, paints back the transcript it covered and leaves
    /// the cursor on the free row under it, for a full-screen view or an
    /// editor to take the terminal (or for the shell, on exit); the next
    /// [`resume`](Self::resume) starts the region again at the cursor.
    pub fn suspend(&mut self) -> io::Result<()> {
        if self.suspended {
            return Ok(());
        }
        // One free row for the cursor.
        self.append(Vec::new(), 1)?;
        let end = self.transcript_end();
        self.clear_rows(end, self.screen.height - end)?;
        self.top = end;
        self.height = 0;
        self.suspended = true;
        let backend = self.terminal.backend_mut();
        backend.set_cursor_position(Position::new(0, end))?;
        backend.show_cursor()?;
        backend.flush()
    }

    /// Takes the terminal back after [`suspend`](Self::suspend): the region
    /// starts at the cursor's row. A size change meanwhile still reprints.
    pub fn resume(&mut self) -> io::Result<()> {
        if !self.suspended {
            return Ok(());
        }
        self.suspended = false;
        let size = self.terminal.backend().size()?;
        let row = self
            .terminal
            .backend_mut()
            .get_cursor_position()?
            .y
            .min(size.height.saturating_sub(1));
        if row != self.transcript_end() {
            // Something printed meanwhile: what is on screen is no longer
            // the transcript gz knows.
            self.shown.clear();
            self.origin = row;
            self.anchored = false;
        }
        self.top = row;
        self.place()
    }

    /// The row under the transcript's last row.
    fn transcript_end(&self) -> u16 {
        self.origin + self.shown.len() as u16
    }

    /// Adds `lines` to the transcript and paints every transcript row the
    /// region covered or will no longer cover, scrolling the screen until
    /// `room` rows are free under the transcript. Rows are always painted
    /// before they scroll off, so scrollback gets the transcript once.
    fn append(&mut self, lines: Vec<Line<'static>>, room: u16) -> io::Result<()> {
        let rows = self.screen.height;
        let first_unpainted = self.top.min(self.transcript_end());
        self.shown.extend(lines);
        let mut painted_from = first_unpainted;
        loop {
            self.paint_shown(painted_from, rows)?;
            let end = self.origin as usize + self.shown.len();
            let overflow = (end + room as usize).saturating_sub(rows as usize);
            if overflow == 0 {
                return Ok(());
            }
            self.anchored = true;
            let scroll = overflow.min(rows as usize) as u16;
            self.scroll_up(scroll)?;
            let dropped = scroll.saturating_sub(self.origin) as usize;
            self.origin = self.origin.saturating_sub(scroll);
            self.shown.drain(..dropped.min(self.shown.len()));
            painted_from = rows - scroll;
        }
    }

    /// Paints the transcript rows from screen row `from` up to (not
    /// including) `to`, as far as the transcript reaches.
    fn paint_shown(&mut self, from: u16, to: u16) -> io::Result<()> {
        let from = from.max(self.origin);
        let to = to.min(self.transcript_end());
        if from >= to {
            return Ok(());
        }
        let first = (from - self.origin) as usize;
        let last = (to - self.origin) as usize;
        let lines: Vec<Line<'static>> = self.shown.range(first..last).cloned().collect();
        self.draw_lines(from, self.screen.width, &lines)
    }

    fn place(&mut self) -> io::Result<()> {
        let area = Rect::new(0, self.top, self.screen.width, self.height);
        // A fixed viewport's resize clears its rows and redraws in full.
        self.terminal.resize(area)
    }

    fn scroll_up(&mut self, lines: u16) -> io::Result<()> {
        let backend = self.terminal.backend_mut();
        backend.set_cursor_position(Position::new(0, last_row(self.screen)))?;
        backend.append_lines(lines)?;
        backend.flush()
    }

    fn clear_rows(&mut self, top: u16, height: u16) -> io::Result<()> {
        let backend = self.terminal.backend_mut();
        for y in top..top.saturating_add(height).min(self.screen.height) {
            backend.set_cursor_position(Position::new(0, y))?;
            backend.clear_region(ClearType::CurrentLine)?;
        }
        backend.flush()
    }

    fn draw_lines(&mut self, top: u16, width: u16, lines: &[Line<'static>]) -> io::Result<()> {
        let area = Rect::new(0, top, width, lines.len() as u16);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines.to_vec()).render(area, &mut buffer);
        crate::tui::theme::recolor(&mut buffer);
        self.clear_rows(top, area.height)?;
        let backend = self.terminal.backend_mut();
        backend.draw(visible_cells(&buffer).into_iter())?;
        backend.flush()
    }
}

fn last_row(screen: Size) -> u16 {
    screen.height.saturating_sub(1)
}

/// Every cell to write, skipping the ones a wide character covers.
fn visible_cells(buffer: &Buffer) -> Vec<(u16, u16, &Cell)> {
    let area = buffer.area;
    let mut cells = Vec::with_capacity(buffer.content.len());
    let mut covered = 0usize;
    for (i, cell) in buffer.content.iter().enumerate() {
        if covered > 0 {
            covered -= 1;
            continue;
        }
        let x = area.x + (i as u16 % area.width);
        let y = area.y + (i as u16 / area.width);
        cells.push((x, y, cell));
        covered = cell.symbol().width().saturating_sub(1);
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;

    fn rows(screen: &LiveScreen<TestBackend>) -> Vec<String> {
        let buffer = screen.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut covered = 0;
                for x in 0..buffer.area.width {
                    if covered > 0 {
                        covered -= 1;
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    covered = symbol.width().saturating_sub(1);
                    row.push_str(symbol);
                }
                row.trim_end().to_string()
            })
            .collect()
    }

    fn numbered(from: usize, count: usize) -> Vec<Line<'static>> {
        (from..from + count)
            .map(|n| Line::from(format!("line {n}")))
            .collect()
    }

    /// Draws a region of `height` rows: a rule, `› input` lines, a rule.
    fn draw_box(screen: &mut LiveScreen<TestBackend>, height: u16) {
        draw_region(screen, height, 0);
    }

    /// Draws a `base`-row box with a pop-up under it, `height` rows in all.
    fn draw_pop_up(screen: &mut LiveScreen<TestBackend>, height: u16, base: u16) {
        draw_region(screen, height, height - base);
    }

    fn draw_region(screen: &mut LiveScreen<TestBackend>, height: u16, pop_up: u16) {
        screen.fit(height, pop_up).unwrap();
        screen
            .draw(|frame| {
                let area = frame.area();
                let width = area.width as usize;
                let mut lines = vec![Line::from("─".repeat(width))];
                for _ in 2..area.height {
                    lines.push(Line::from("› input"));
                }
                lines.push(Line::from("─".repeat(width)));
                frame.render_widget(Paragraph::new(lines), area);
            })
            .unwrap();
    }

    fn open(width: u16, height: u16) -> LiveScreen<TestBackend> {
        LiveScreen::open(TestBackend::new(width, height)).unwrap()
    }

    fn count(rows: &[String], needle: &str) -> usize {
        rows.iter().filter(|row| row.contains(needle)).count()
    }

    #[test]
    fn printed_lines_sit_above_the_region_and_scroll_off_the_top() {
        let mut screen = open(30, 8);
        draw_box(&mut screen, 3);
        screen.insert(numbered(1, 10)).unwrap();
        draw_box(&mut screen, 3);
        let out = rows(&screen);
        assert_eq!(
            out[..5],
            ["line 6", "line 7", "line 8", "line 9", "line 10"]
        );
        assert_eq!(out[6], "› input");
        assert_eq!(count(&out, "› input"), 1);
    }

    /// The rows the terminal scrolled off, oldest first.
    fn scrollback(screen: &LiveScreen<TestBackend>) -> Vec<String> {
        let buffer = screen.backend().scrollback();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_closed_pop_up_paints_back_the_rows_it_covered() {
        let mut screen = open(30, 10);
        screen.insert(numbered(1, 20)).unwrap();
        draw_box(&mut screen, 3);
        let before = rows(&screen);
        assert_eq!(before[9], "─".repeat(30));
        draw_pop_up(&mut screen, 7, 3);
        assert_eq!(rows(&screen)[3], "─".repeat(30), "it covers the transcript");
        draw_box(&mut screen, 3);
        assert_eq!(rows(&screen), before, "no blank rows are left");
        // The next print goes right under the transcript.
        screen.insert(numbered(21, 2)).unwrap();
        draw_box(&mut screen, 3);
        let out = rows(&screen);
        assert_eq!(out[5..7], ["line 21", "line 22"], "{out:?}");
        assert_eq!(out[9], "─".repeat(30));
    }

    #[test]
    fn scrollback_holds_the_transcript_once_through_pop_ups_and_prints() {
        let mut screen = open(30, 10);
        screen.insert(numbered(1, 3)).unwrap();
        draw_box(&mut screen, 3);
        let mut next = 4;
        for height in [7, 3, 9, 5, 3] {
            draw_pop_up(&mut screen, height, 3);
            // Output arrives while the pop-up is open.
            screen.insert(numbered(next, 4)).unwrap();
            draw_pop_up(&mut screen, height, 3);
            next += 4;
        }
        draw_box(&mut screen, 3);
        let mut transcript: Vec<String> = scrollback(&screen)
            .into_iter()
            .filter(|row| !row.is_empty())
            .collect();
        transcript.extend(
            rows(&screen)
                .into_iter()
                .take_while(|row| row.starts_with("line")),
        );
        let expected: Vec<String> = (1..next).map(|n| format!("line {n}")).collect();
        assert_eq!(transcript, expected);
        assert_eq!(rows(&screen)[9], "─".repeat(30));
    }

    #[test]
    fn suspending_paints_back_covered_rows_and_frees_a_row_for_the_cursor() {
        let mut screen = open(30, 10);
        screen.insert(numbered(1, 20)).unwrap();
        draw_pop_up(&mut screen, 7, 3);
        screen.suspend().unwrap();
        let out = rows(&screen);
        assert_eq!(out[6], "line 20", "{out:?}");
        assert!(out[7..].iter().all(String::is_empty), "{out:?}");
        let cursor = screen.backend_mut().get_cursor_position().unwrap();
        assert_eq!(cursor.y, 7);
        screen.resume().unwrap();
        draw_box(&mut screen, 3);
        let out = rows(&screen);
        assert_eq!(out[6], "line 20", "{out:?}");
        assert_eq!(out[9], "─".repeat(30), "{out:?}");
    }

    #[test]
    fn a_pop_up_in_an_empty_screen_shrinks_back_in_place() {
        let mut screen = open(30, 20);
        screen.insert(numbered(1, 2)).unwrap();
        draw_box(&mut screen, 3);
        draw_pop_up(&mut screen, 9, 3);
        draw_box(&mut screen, 3);
        let out = rows(&screen);
        assert_eq!(out[2], "─".repeat(30));
        assert_eq!(out[3], "› input");
        assert!(out[5..].iter().all(String::is_empty), "{out:?}");
    }

    #[test]
    fn a_pop_up_that_pushed_a_short_transcript_up_closes_right_under_it() {
        let mut screen = open(30, 12);
        screen.insert(numbered(1, 6)).unwrap();
        draw_box(&mut screen, 3);
        draw_pop_up(&mut screen, 9, 3);
        draw_box(&mut screen, 3);
        let out = rows(&screen);
        let last = out.iter().rposition(|r| r.starts_with("line")).unwrap();
        assert_eq!(out[last + 1], "─".repeat(30), "{out:?}");
        assert!(out[last + 4..].iter().all(String::is_empty), "{out:?}");
    }

    #[test]
    fn reopening_a_pop_up_covers_the_same_rows_again() {
        let mut screen = open(30, 10);
        screen.insert(numbered(1, 20)).unwrap();
        draw_box(&mut screen, 3);
        draw_pop_up(&mut screen, 7, 3);
        let before = rows(&screen);
        draw_box(&mut screen, 3);
        draw_pop_up(&mut screen, 7, 3);
        assert_eq!(rows(&screen), before);
    }

    #[test]
    fn any_resize_sequence_reprints_one_box_after_the_size_settles() {
        let mut screen = open(70, 12);
        let transcript = |width: u16| {
            let mut lines = numbered(1, 15);
            lines.push(Line::from("=".repeat(width as usize)));
            lines
        };
        screen.insert(transcript(70)).unwrap();
        draw_box(&mut screen, 3);
        let start = Instant::now();
        for (step, (w, h)) in [(200, 12), (40, 9), (140, 30), (60, 12)]
            .into_iter()
            .enumerate()
        {
            screen.backend_mut().resize(w, h);
            let at = start + Duration::from_millis(step as u64 * 10);
            assert!(
                !screen.reprint_due(at).unwrap(),
                "waits for the size to settle"
            );
            assert!(screen.resize_pending());
        }
        let settled = start + Duration::from_secs(1);
        assert!(screen.reprint_due(settled).unwrap());
        screen.reprint(transcript(screen.width())).unwrap();
        draw_box(&mut screen, 3);
        let out = rows(&screen);
        assert_eq!(out.len(), 12);
        assert_eq!(count(&out, "› input"), 1, "{out:?}");
        assert_eq!(out[11], "─".repeat(60));
        assert_eq!(
            out[8],
            "=".repeat(60),
            "the transcript is drawn at the new width"
        );
        assert!(
            !screen.reprint_due(settled).unwrap(),
            "one reprint per resize"
        );
        assert!(!screen.resize_pending());
    }

    #[test]
    fn the_region_never_takes_the_whole_screen() {
        let mut screen = open(30, 6);
        draw_box(&mut screen, 40);
        assert_eq!(count(&rows(&screen), "› input"), 3);
        screen.insert(numbered(1, 3)).unwrap();
        draw_box(&mut screen, 40);
        assert_eq!(rows(&screen)[0], "line 3");
    }

    #[test]
    fn a_resize_while_suspended_still_reprints_after_resuming() {
        let mut screen = open(40, 10);
        screen.insert(numbered(1, 3)).unwrap();
        draw_box(&mut screen, 3);
        screen.suspend().unwrap();
        assert!(rows(&screen)[3].is_empty(), "the region is cleared");
        screen.backend_mut().resize(50, 10);
        let start = Instant::now();
        screen.reprint_due(start).unwrap();
        screen.resume().unwrap();
        assert!(screen.reprint_due(start + RESIZE_SETTLE).unwrap());
    }

    /// The app's real order: a short banner under the cursor, the base
    /// box, then enough output to fill the screen.
    fn opened_like_the_app(width: u16, height: u16) -> LiveScreen<TestBackend> {
        let mut screen = open(width, height);
        screen.insert(numbered(1, 3)).unwrap();
        draw_box(&mut screen, 4);
        screen.insert(numbered(4, 30)).unwrap();
        draw_box(&mut screen, 4);
        screen
    }

    fn box_bottom(out: &[String]) -> usize {
        out.iter().rposition(|row| row.starts_with('─')).unwrap()
    }

    #[test]
    fn after_a_turn_and_a_menu_the_box_stays_at_the_bottom_in_the_apps_order() {
        let mut screen = opened_like_the_app(40, 20);
        assert_eq!(box_bottom(&rows(&screen)), 19);
        // A turn's activity rows grow the region and shrink it again.
        draw_box(&mut screen, 8);
        draw_box(&mut screen, 4);
        let out = rows(&screen);
        assert_eq!(box_bottom(&out), 19, "after a turn: {out:?}");
        // A menu opens and closes.
        draw_pop_up(&mut screen, 14, 4);
        draw_box(&mut screen, 4);
        let out = rows(&screen);
        assert_eq!(box_bottom(&out), 19, "after a menu: {out:?}");
        assert_eq!(count(&out, "› input"), 2, "{out:?}");
    }

    #[test]
    fn the_last_answer_printed_as_a_turn_ends_fills_the_rows_its_activity_freed() {
        let mut screen = opened_like_the_app(40, 20);
        draw_box(&mut screen, 8);
        screen.insert(numbered(100, 6)).unwrap();
        draw_box(&mut screen, 8);
        // The turn ends: the region shrinks, then its answer is printed.
        draw_box(&mut screen, 4);
        screen.insert(numbered(200, 4)).unwrap();
        draw_box(&mut screen, 4);
        let out = rows(&screen);
        assert_eq!(out[15], "line 203", "no blank rows: {out:?}");
        assert_eq!(box_bottom(&out), 19, "{out:?}");
    }

    #[test]
    fn output_that_fills_the_screen_during_a_turn_leaves_the_box_at_the_bottom() {
        let mut screen = open(40, 20);
        screen.insert(numbered(1, 3)).unwrap();
        draw_box(&mut screen, 4);
        // The turn starts while the box sits under a short transcript…
        draw_box(&mut screen, 8);
        // …and its output scrolls the screen.
        screen.insert(numbered(4, 30)).unwrap();
        draw_box(&mut screen, 8);
        draw_box(&mut screen, 4);
        let out = rows(&screen);
        assert_eq!(box_bottom(&out), 19, "{out:?}");
    }

    #[test]
    fn a_menu_after_a_resize_reprint_closes_back_at_the_bottom() {
        let mut screen = opened_like_the_app(60, 20);
        screen.backend_mut().resize(45, 20);
        let start = Instant::now();
        screen.reprint_due(start).unwrap();
        assert!(screen.reprint_due(start + RESIZE_SETTLE).unwrap());
        screen.reprint(numbered(1, 33)).unwrap();
        draw_box(&mut screen, 4);
        assert_eq!(box_bottom(&rows(&screen)), 19);
        draw_pop_up(&mut screen, 14, 4);
        draw_box(&mut screen, 4);
        let out = rows(&screen);
        assert_eq!(box_bottom(&out), 19, "{out:?}");
    }

    #[test]
    fn a_resize_after_clear_redraws_the_whole_box() {
        let mut screen = opened_like_the_app(60, 20);
        screen.clear_all().unwrap();
        draw_box(&mut screen, 4);
        assert_eq!(box_bottom(&rows(&screen)), 3);
        screen.backend_mut().resize(50, 20);
        let start = Instant::now();
        screen.reprint_due(start).unwrap();
        assert!(screen.reprint_due(start + RESIZE_SETTLE).unwrap());
        screen.reprint(Vec::new()).unwrap();
        draw_box(&mut screen, 4);
        let out = rows(&screen);
        assert_eq!(out[0], "─".repeat(50), "{out:?}");
        assert_eq!(out[1..3], ["› input", "› input"], "{out:?}");
        assert_eq!(out[3], "─".repeat(50), "{out:?}");
    }

    #[test]
    fn wide_characters_print_whole() {
        let mut screen = open(20, 6);
        screen.insert(vec![Line::from("日本語 ok")]).unwrap();
        assert_eq!(rows(&screen)[0], "日本語 ok");
    }
}
