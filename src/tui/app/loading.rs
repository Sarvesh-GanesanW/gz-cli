//! Loading a grid level that is more than one plain request: several
//! requests composed into one list (`ScreenSpec::compose`), and long lists
//! loaded a page at a time (`ScreenSpec::paging`).

use reqwest::Method;
use serde_json::Value;

use crate::tui::compose::{Compose, ComposeRun, Step};
use crate::tui::fetch::FetchTarget;
use crate::tui::nav::{ResourceId, Source, Verb};
use crate::tui::shape;
use crate::tui::template::Scope;

use super::{App, Tone};

/// The page a paged level's request asks for.
#[derive(Debug, Clone, Copy)]
pub struct PageLoad {
    pub offset: usize,
    pub limit: usize,
    /// Add the rows to those shown (a next page) instead of replacing them.
    pub appending: bool,
}

/// What a paged level knows about the rest of its list.
#[derive(Debug, Clone, Default)]
pub struct Pages {
    /// Every row loaded so far, as the server sent them.
    pub rows: Vec<Value>,
    /// Where the next page starts; None after the last.
    pub next_offset: Option<usize>,
    /// The list's full length, when the server says.
    pub total: Option<u64>,
}

impl Pages {
    /// "100 of 1,441" or "100+", for the row count; None when everything
    /// is loaded.
    pub fn more_hint(&self) -> Option<String> {
        self.next_offset?;
        let shown = self.rows.len();
        Some(match self.total {
            Some(total) => format!(
                "{} of {}",
                crate::tui::format::group_digits(&shown.to_string()),
                crate::tui::format::group_digits(&total.to_string())
            ),
            None => format!("{shown}+"),
        })
    }
}

impl App {
    /// Sends a composed level's requests: its own `source` (already filled
    /// as `path`/`body`) and every `with` source.
    pub(super) fn load_composed(
        &mut self,
        resource: ResourceId,
        compose: &'static Compose,
        own: (Source, String, Option<Value>),
        scope: &Scope,
    ) {
        let (source, path, body) = own;
        let target = FetchTarget::Grid(resource);
        let first = self.fetch_resolved(
            target.clone(),
            source.service,
            source.verb.method(),
            path,
            body,
        );
        let mut ids = vec![first];
        for with in compose.with {
            match with.resolve(scope) {
                Ok((path, body)) => ids.push(self.fetch_resolved(
                    target.clone(),
                    with.service,
                    with.verb.method(),
                    path,
                    body,
                )),
                // An id nobody answers would stall the run; a source that
                // can't be filled is a spec bug, so say so loudly.
                Err(err) => {
                    return self.set_status(Tone::Error, format!("Can't load this view: {err}"))
                }
            }
        }
        if let Some(view) = self.grids.get_mut(&resource) {
            view.compose = Some(ComposeRun::new(compose, ids));
            view.awaiting = Some(first);
        }
    }

    /// An answer for a composed level; the rows show once every part is in.
    pub(super) fn composed_fetched(
        &mut self,
        resource: ResourceId,
        id: u64,
        result: Result<Value, String>,
    ) {
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        let Some(run) = view.compose.as_mut() else {
            return;
        };
        let step = match run.accept(id, result) {
            Step::Expand(rows) => self.expand(resource, rows),
            step => step,
        };
        if step != Step::Done {
            return;
        }
        let Some(view) = self.grids.get_mut(&resource) else {
            return;
        };
        let Some(run) = view.compose.take() else {
            return;
        };
        view.awaiting = None;
        let parent = view
            .levels
            .last()
            .map(|l| l.row.clone())
            .unwrap_or_default();
        let combined = run.combine(&parent);
        match run.own_error() {
            Some(err) if combined.rows.is_empty() => self.grid_failed(resource, err),
            _ => self.grid_loaded(resource, &Value::Array(combined.rows), &combined.failures),
        }
    }

    /// Sends one request per expanded row, filled from that row.
    fn expand(&mut self, resource: ResourceId, rows: Vec<Value>) -> Step {
        let Some(spec) = resource.grid_spec() else {
            return Step::Done;
        };
        let Some(view) = self.grids.get(&resource) else {
            return Step::Done;
        };
        let depth = view.depth();
        let Some(expand) = spec
            .screen_at(depth)
            .and_then(|s| s.compose)
            .and_then(|c| c.expand)
        else {
            return Step::Done;
        };
        let trail = view.trail_rows();
        let mut sent = Vec::new();
        let mut unfilled = Vec::new();
        for row in rows {
            let mut scope = Scope::default();
            for parent in trail.iter().rev() {
                scope = scope.nearest(parent);
            }
            match expand.source.resolve(&scope.nearest(&row)) {
                Ok((path, body)) => {
                    let id = self.fetch_resolved(
                        FetchTarget::Grid(resource),
                        expand.source.service,
                        expand.source.verb.method(),
                        path,
                        body,
                    );
                    sent.push((id, row));
                }
                Err(err) => unfilled.push((row, err.to_string())),
            }
        }
        let Some(run) = self
            .grids
            .get_mut(&resource)
            .and_then(|v| v.compose.as_mut())
        else {
            return Step::Done;
        };
        let step = run.expanding(sent);
        for (row, reason) in unfilled {
            run.expansion_failed(row, reason);
        }
        step
    }

    /// The path for a paged level's first page, and what it asks for.
    pub(super) fn first_page(
        &self,
        resource: ResourceId,
        source: Source,
        path: String,
    ) -> (String, Option<PageLoad>) {
        let paging = resource
            .grid_spec()
            .zip(self.grids.get(&resource))
            .and_then(|(spec, view)| spec.screen_at(view.depth()))
            .and_then(|screen| screen.paging);
        match paging {
            Some(paging) if source.verb == Verb::Get => {
                let shown = self
                    .grids
                    .get(&resource)
                    .map_or(0, |v| v.levels[v.depth()].pages.rows.len());
                let limit = paging.reload_limit(shown);
                let page = PageLoad {
                    offset: 0,
                    limit,
                    appending: false,
                };
                (paging.page_path(&path, 0, limit), Some(page))
            }
            _ => (path, None),
        }
    }

    /// Folds a page into the level's rows; the whole list so far comes back.
    pub(super) fn paged(&mut self, resource: ResourceId, page: PageLoad, answer: Value) -> Value {
        let Some(view) = self.grids.get_mut(&resource) else {
            return answer;
        };
        let depth = view.depth();
        let pages = &mut view.levels[depth].pages;
        let rows = shape::rows_of(&answer);
        if page.appending {
            pages.rows.extend(rows);
        } else {
            pages.rows = rows;
        }
        pages.next_offset = shape::next_offset(&answer, page.offset, page.limit);
        pages.total = shape::total_of(&answer).filter(|total| *total > 0);
        Value::Array(pages.rows.clone())
    }

    /// Loads the next page when the selection reaches the last row of a
    /// paged list that has more (and no filter hides rows).
    pub(super) fn load_more_if_at_end(&mut self) {
        let Some(view) = self.view() else { return };
        let grid = view.grid();
        let at_end = grid.visible_len() > 0 && grid.selected + 1 >= grid.visible_len();
        if !at_end || !grid.filter.is_empty() || view.is_loading() {
            return;
        }
        let depth = view.depth();
        let Some(offset) = view.levels[depth].pages.next_offset else {
            return;
        };
        let Some(spec) = self.spec() else { return };
        let Some(paging) = spec.screen_at(depth).and_then(|s| s.paging) else {
            return;
        };
        let resource = self.active;
        let source = spec.source_at(depth);
        let mut scope = Scope::with_ids(&view.trail_ids());
        let trail = view.trail_rows();
        for row in trail.iter().rev() {
            scope = scope.nearest(row);
        }
        let site = self.resolved.site();
        if let Some(site) = site {
            scope = scope.var("site", site);
        }
        let path = match source.resolve(&scope) {
            Ok((path, _)) => paging.page_path(&path, offset, paging.size),
            Err(err) => return self.set_status(Tone::Error, format!("Can't load more: {err}")),
        };
        let id = self.fetch_resolved(
            FetchTarget::Grid(resource),
            source.service,
            Method::GET,
            path,
            None,
        );
        if let Some(view) = self.view_mut() {
            view.awaiting = Some(id);
            view.page_load = Some(PageLoad {
                offset,
                limit: paging.size,
                appending: true,
            });
        }
        self.set_status(Tone::Info, "Loading more…");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn more_hint_counts_what_is_loaded() {
        let pages = Pages {
            rows: vec![Value::Null; 100],
            next_offset: Some(100),
            total: Some(1441),
        };
        assert_eq!(pages.more_hint().as_deref(), Some("100 of 1,441"));
        let open_ended = Pages {
            total: None,
            ..pages.clone()
        };
        assert_eq!(open_ended.more_hint().as_deref(), Some("100+"));
        let done = Pages {
            next_offset: None,
            ..pages
        };
        assert_eq!(done.more_hint(), None);
    }
}
