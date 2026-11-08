//! Row actions: the menu, forms and confirmations in front of them, the
//! request they send, and how its answer is reported.

use crossterm::event::KeyEvent;
use reqwest::Method;
use serde_json::Value;

use crate::tui::errors::{self, Subject};
use crate::tui::fetch::{ActionDone, FetchReq, FetchTarget, Request};
use crate::tui::overlay::confirm::{self, Confirm};
use crate::tui::overlay::form::{self, FormState};
use crate::tui::overlay::menu::{self, ActionMenu, MenuItem};
use crate::tui::screen::{allowed, ActionKind, ActionSpec, After, Choices, FieldKind, RequestSpec};
use crate::tui::template::{self, Encoding};
use crate::tui::views::RowContext;

use super::{App, Tone};

/// A request ready to send once confirmed.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub service: &'static str,
    pub method: Method,
    pub path: String,
    pub body: Option<Value>,
    pub done: ActionDone,
}

/// The dialog in front of everything.
#[derive(Debug, Clone)]
pub enum Overlay {
    Menu(ActionMenu),
    Confirm {
        confirm: Confirm,
        request: Prepared,
    },
    Form {
        form: FormState,
        action: &'static ActionSpec,
        request: RequestSpec,
        row: RowContext,
    },
}

impl App {
    /// Runs `action` on `row`: straight away, or after its form or
    /// confirmation.
    pub(super) fn start_action(&mut self, action: &'static ActionSpec, row: RowContext) {
        if !allowed(action.when, &row.row) {
            self.set_status(
                Tone::Info,
                format!("{} isn't available for this row", action.label),
            );
            return;
        }
        match action.kind {
            ActionKind::Request(request) => self.prepare(action, request, row, None),
            ActionKind::Form(spec, request) => {
                let site = self.resolved.site();
                let scope = row.scope(None, site.as_deref());
                let form = FormState::new(spec, |default| {
                    template::fill(default, &scope, Encoding::Text).ok()
                });
                self.open_form(form, action, request, row);
            }
            ActionKind::Navigate(goto) => self.follow(goto, &row),
            ActionKind::Custom(run) => self.run_detached(|ctx| run(&row.row, ctx)),
            ActionKind::CustomRow(run) => self.run_detached(|ctx| run(&row, ctx)),
        }
    }

    fn open_form(
        &mut self,
        mut form: FormState,
        action: &'static ActionSpec,
        request: RequestSpec,
        row: RowContext,
    ) {
        let site = self.resolved.site();
        for field in form.endpoint_fields() {
            let FieldKind::Select(Choices::Endpoint { source, .. }) = form.field(field).kind else {
                continue;
            };
            let scope = row.scope(None, site.as_deref());
            match source.resolve(&scope) {
                Ok((path, body)) => {
                    let id = self.next_id();
                    self.outbox.push(Request::Fetch(FetchReq {
                        id,
                        target: FetchTarget::FormChoices,
                        service: source.service.to_string(),
                        method: source.verb.method(),
                        path,
                        body,
                    }));
                    form.set_loading(field, id);
                }
                Err(err) => form.set_failed(field, err.to_string()),
            }
        }
        self.overlay = Some(Overlay::Form {
            form,
            action,
            request,
            row,
        });
    }

    /// Fills the request; asks first when the action says so.
    fn prepare(
        &mut self,
        action: &'static ActionSpec,
        request: RequestSpec,
        row: RowContext,
        form_values: Option<Value>,
    ) {
        let site = self.resolved.site();
        let scope = row.scope(form_values.as_ref(), site.as_deref());
        let (path, body) = match request.source.resolve(&scope) {
            Ok(resolved) => resolved,
            Err(err) => {
                self.set_status(
                    Tone::Error,
                    format!("Can't {}: {err}", action.label.to_lowercase()),
                );
                return;
            }
        };
        let prepared = Prepared {
            service: request.source.service,
            method: request.source.verb.method(),
            path,
            body,
            done: ActionDone {
                label: action.label,
                success: request.success,
                after: request.after,
                noun: row.noun,
                service: request.source.service,
                resource: row.resource,
                view: row.view,
            },
        };
        if !action.destructive && action.confirm.is_none() {
            return self.send_action(prepared);
        }
        let message = action
            .confirm
            .map(|text| {
                template::fill(text, &scope, Encoding::Text).unwrap_or_else(|_| text.to_string())
            })
            .unwrap_or_else(|| format!("{}?", action.label));
        let spelled_out = format!("{} {} {}", prepared.method, prepared.service, prepared.path);
        let confirm = Confirm::new(
            action.label,
            message,
            spelled_out,
            action.destructive,
            row.title.clone(),
        );
        self.overlay = Some(Overlay::Confirm {
            confirm,
            request: prepared,
        });
    }

    fn send_action(&mut self, prepared: Prepared) {
        let id = self.next_id();
        self.set_status(Tone::Info, format!("{}…", prepared.done.label));
        self.outbox.push(Request::Fetch(FetchReq {
            id,
            target: FetchTarget::Action(prepared.done),
            service: prepared.service.to_string(),
            method: prepared.method,
            path: prepared.path,
            body: prepared.body,
        }));
    }

    pub(super) fn action_finished(&mut self, done: ActionDone, result: Result<Value, String>) {
        let answer = match result {
            Ok(answer) => answer,
            Err(err) => {
                let what = format!("this {}", done.noun);
                let reason = errors::explain(
                    &err,
                    Subject {
                        what: &what,
                        service: done.service,
                    },
                );
                self.set_status(Tone::Error, format!("{} failed: {reason}", done.label));
                return;
            }
        };
        self.refresh_after(&done);
        // Set last so a refresh's "Loading…" doesn't hide the result.
        let message = server_message(&answer).unwrap_or_else(|| done.success.to_string());
        self.set_status(Tone::Ok, message);
    }

    fn refresh_after(&mut self, done: &ActionDone) {
        match (done.after, done.view) {
            (After::Stay, _) => return,
            (After::RefreshAndClose, Some(view)) => {
                if self.views.top().is_some_and(|(top, _)| top == view) {
                    self.views.pop();
                }
            }
            (After::Refresh, Some(view)) => {
                self.with_view(view, |v, ctx| v.refresh(ctx));
            }
            (_, None) => {}
        }
        if let Some(resource) = done.resource {
            self.reload_resource(resource);
        }
    }

    pub(super) fn on_overlay_key(&mut self, key: KeyEvent) {
        let Some(overlay) = self.overlay.take() else {
            return;
        };
        match overlay {
            Overlay::Menu(mut menu) => match menu.on_key(key) {
                menu::Outcome::Open => self.overlay = Some(Overlay::Menu(menu)),
                menu::Outcome::Closed => {}
                menu::Outcome::Picked(MenuItem::Action(action)) => {
                    self.start_action(action, menu.row)
                }
                menu::Outcome::Picked(MenuItem::Link(link)) => self.follow(link.goto, &menu.row),
            },
            Overlay::Confirm {
                mut confirm,
                request,
            } => match confirm.on_key(key) {
                confirm::Outcome::Waiting => {
                    self.overlay = Some(Overlay::Confirm { confirm, request })
                }
                confirm::Outcome::Cancelled => self.set_status(Tone::Info, "Cancelled"),
                confirm::Outcome::Confirmed => self.send_action(request),
            },
            Overlay::Form {
                mut form,
                action,
                request,
                row,
            } => match form.on_key(key) {
                form::Outcome::Editing => {
                    self.overlay = Some(Overlay::Form {
                        form,
                        action,
                        request,
                        row,
                    })
                }
                form::Outcome::Cancelled => self.set_status(Tone::Info, "Cancelled"),
                form::Outcome::Submitted(values) => {
                    self.prepare(action, request, row, Some(values))
                }
            },
        }
    }

    pub(super) fn form_choices_fetched(&mut self, id: u64, result: Result<Value, String>) {
        if let Some(Overlay::Form { form, .. }) = self.overlay.as_mut() {
            form.choices_loaded(id, result);
        }
    }

    /// Action and link keys on the selected row; false when `c` is neither.
    pub(super) fn row_shortcut(&mut self, c: char) -> bool {
        let (Some(spec), Some(row)) = (self.screen(), self.row_context()) else {
            return false;
        };
        let action = spec.actions_for(&row.row).find(|a| a.key == c);
        let link = spec.links_for(&row.row).find(|l| l.key == c);
        match (action, link) {
            (Some(action), _) => self.start_action(action, row),
            (None, Some(link)) => self.follow(link.goto, &row),
            (None, None) => return false,
        }
        true
    }
}

/// The success message a server sent, if any (`{message}` or
/// `{response: {message}}`).
fn server_message(answer: &Value) -> Option<String> {
    [
        &answer["message"],
        &answer["response"]["message"],
        &answer["response"],
    ]
    .into_iter()
    .find_map(|v| {
        v.as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    })
}

#[cfg(test)]
mod tests;
