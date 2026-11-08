//! `/subagents`: whether the chat may start helper agents, stopping the
//! background ones, and the user's settings for them (model, effort,
//! context, how many per run, how many at once).

use crate::assistant::subagent_settings::{self, Choices, Limits, Setting, SubagentSettings};
use crate::code_ui::items::Item;
use crate::code_ui::panel::{Panel, Row};

use super::{Effect, Picker, PickerRow, UiState};

const USAGE: &str = "use /subagents model|effort|context|max|parallel <value>, /subagents reset, or /subagents on|off|stop";

impl UiState {
    /// `/subagents` shows the overview; the words after it toggle, stop, or
    /// change a setting.
    pub(super) fn subagents_command(&mut self, command: &str) -> Vec<Effect> {
        let mut words = command.split_whitespace().skip(1);
        let first = words.next();
        // Model labels have spaces (`Claude Haiku 5.5`).
        let value = words.collect::<Vec<_>>().join(" ");
        match first {
            None => {
                self.record(Item::Panel(self.subagent_overview()));
                Vec::new()
            }
            Some("stop") => self.stop_background(),
            Some(switch @ ("on" | "off")) => self.switch_subagents(switch == "on"),
            Some("reset") => self.change_subagent_settings(SubagentSettings::default(), None),
            Some(word) => match Setting::parse(word) {
                Some(Setting::Model) if value.is_empty() => {
                    self.open_subagent_model_picker();
                    Vec::new()
                }
                Some(_) if value.is_empty() => {
                    self.record(Item::Error(format!(
                        "/subagents {word} needs a value · {USAGE}"
                    )));
                    Vec::new()
                }
                Some(setting) => self.set_subagent_setting(setting, &value),
                None => {
                    self.record(Item::Error(format!("Unknown option {word} · {USAGE}")));
                    Vec::new()
                }
            },
        }
    }

    fn switch_subagents(&mut self, enabled: bool) -> Vec<Effect> {
        self.subagents_enabled = enabled;
        let note = if enabled {
            "Subagents on: the chat may start helper agents in parallel"
        } else {
            "Subagents off: the chat works alone"
        };
        self.record(Item::Notice(note.into()));
        vec![Effect::SetSubagents(enabled)]
    }

    /// Checks `value` against the model list and the server's limits, then
    /// applies it; an invalid value changes nothing and names the choices.
    pub(super) fn set_subagent_setting(&mut self, setting: Setting, value: &str) -> Vec<Effect> {
        let choices = Choices {
            catalog: self.catalog.as_ref(),
            main: self.model.as_ref(),
            limits: self.subagent_limits(),
        };
        match subagent_settings::update(&self.subagent_settings, setting, value, &choices) {
            Ok(updated) => self.change_subagent_settings(updated.settings, updated.note),
            Err(err) => {
                self.picker = None;
                self.record(Item::Error(format!("{err}")));
                Vec::new()
            }
        }
    }

    fn subagent_limits(&self) -> Limits {
        self.catalog
            .as_ref()
            .and_then(|c| c.subagent_settings)
            .unwrap_or_default()
    }

    /// `Some(false)` once the model list shows the server ignores them.
    fn subagent_settings_supported(&self) -> Option<bool> {
        self.catalog.as_ref().map(|c| c.subagent_settings.is_some())
    }

    fn change_subagent_settings(
        &mut self,
        settings: SubagentSettings,
        note: Option<String>,
    ) -> Vec<Effect> {
        self.subagent_settings = settings.clone();
        self.picker = None;
        if let Some(note) = note {
            self.record(Item::Notice(note));
        }
        self.record(Item::Notice(format!("Subagents: {}", self.settings_line())));
        if self.subagent_settings_supported() == Some(false) {
            self.record(Item::Notice(
                "This chat server doesn't take subagent settings yet; gz keeps them for when it does"
                    .into(),
            ));
        }
        let mut effects = vec![Effect::SetSubagentSettings(settings)];
        // A higher `parallel` lets waiting subagents start now.
        effects.extend(self.start_queued());
        effects
    }

    /// `model Claude Haiku 5.5 · effort default · context 200K · …`.
    fn settings_line(&self) -> String {
        self.subagent_settings
            .rows(self.catalog.as_ref())
            .into_iter()
            .map(|(setting, value)| format!("{setting} {value}"))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn open_subagent_model_picker(&mut self) {
        let Some(catalog) = &self.catalog else {
            let message = if self.models_loading {
                "The model list is still loading; try /subagents model again in a moment"
            } else {
                "The model list couldn't be loaded; check `gz doctor`"
            };
            self.record(Item::Error(message.into()));
            return;
        };
        let mut rows = vec![PickerRow::SubagentModel(None)];
        rows.extend(
            catalog
                .modes
                .iter()
                .map(|mode| PickerRow::SubagentModel(Some(mode.value.clone()))),
        );
        let current = PickerRow::SubagentModel(self.subagent_settings.model.clone());
        let selected = rows.iter().position(|row| *row == current).unwrap_or(0);
        self.picker = Some(Picker {
            title: "Subagent model".into(),
            rows,
            selected,
            model: None,
        });
    }

    pub(super) fn subagent_overview(&self) -> Panel {
        let (state, switch) = if self.subagents_enabled {
            ("on", "off")
        } else {
            ("off", "on")
        };
        let profiles = self
            .catalog
            .as_ref()
            .map(|c| c.subagent_profiles.as_slice())
            .unwrap_or_default();
        let mut rows: Vec<Row> = profiles
            .iter()
            .map(|profile| Row::entry(&profile.id, profile.description.clone()))
            .collect();
        if profiles.is_empty() {
            rows.push(Row::note(if self.models_loading {
                "The profile list is still loading; try again in a moment"
            } else {
                "This chat server doesn't list subagent profiles"
            }));
        }
        let mut settings: Vec<Row> = self
            .subagent_settings
            .rows(self.catalog.as_ref())
            .into_iter()
            .map(|(setting, value)| Row::field(setting, value))
            .collect();
        settings.push(Row::note(match self.subagent_settings_supported() {
            Some(true) => "This chat server applies these settings",
            Some(false) => "This chat server doesn't take subagent settings yet",
            None => "Checking whether the chat server takes these settings…",
        }));
        let running: Vec<Row> = self
            .subagents
            .running()
            .filter(|a| self.background.contains(&a.id))
            .map(|a| Row::note(crate::subagents::render::target(a)))
            .collect();
        let queued: Vec<Row> = self.queued_targets().into_iter().map(Row::note).collect();
        let mut footer = format!("/subagents {switch} to turn them {switch}");
        if !running.is_empty() || !queued.is_empty() {
            footer.push_str(" · /subagents stop ends the background ones");
        }
        Panel::new("Subagents")
            .aside(state)
            .section(Some("Settings"), settings)
            .section(Some("Profiles"), rows)
            .section(Some("Running in the background"), running)
            .section(Some("Queued"), queued)
            .footer(footer)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::assistant::{AssistantEvent, TurnEnd};
    use crate::code_ui::state::test_support::*;
    use crate::subagents::{Status, SubagentReport, SubagentRequest};

    fn run_command(s: &mut UiState, command: &str) -> Vec<Effect> {
        type_text(s, command);
        s.on_key(key(KeyCode::Enter))
    }

    fn last_error(s: &UiState) -> String {
        match s.flush.last() {
            Some(Item::Error(e)) => e.clone(),
            other => panic!("no error: {other:?}"),
        }
    }

    fn request(id: &str) -> SubagentRequest {
        SubagentRequest {
            id: id.into(),
            name: format!("Check {id}"),
            profile: "analyst".into(),
            prompt: "look".into(),
            model: None,
            reasoning_effort: None,
            context_tokens: None,
        }
    }

    fn started(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::StartSubagent(plan) => Some(plan.request.id.clone()),
                _ => None,
            })
            .collect()
    }

    fn report(id: &str, status: Status) -> AssistantEvent {
        AssistantEvent::SubagentReport(SubagentReport {
            id: id.into(),
            name: format!("Check {id}"),
            status,
            text: "done".into(),
        })
    }

    #[test]
    fn overview_lists_settings_profiles_and_support() {
        let mut s = with_catalog();
        s.subagent_settings.model = Some("opus".into());
        s.subagent_settings.max_concurrent = Some(3);
        run_command(&mut s, "/subagents");
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("no overview");
        };
        let lines = panel.plain_lines().join("\n");
        assert!(lines.starts_with("Subagents · on"), "{lines}");
        assert!(lines.contains("Claude Opus 5"), "{lines}");
        assert!(lines.contains("no limit") && lines.contains('3'), "{lines}");
        assert!(lines.contains("applies these settings"), "{lines}");
        assert!(lines.contains("analyst") && lines.contains("lakehouse and SQL reads"));
        s.catalog.as_mut().unwrap().subagent_settings = None;
        run_command(&mut s, "/subagents");
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("no overview");
        };
        assert!(panel
            .plain_lines()
            .join("\n")
            .contains("doesn't take subagent settings yet"));
    }

    #[test]
    fn toggles_and_rejects_unknown_options() {
        let mut s = with_catalog();
        assert_eq!(
            run_command(&mut s, "/subagents off"),
            vec![Effect::SetSubagents(false)]
        );
        assert!(!s.subagents_enabled);
        assert!(run_command(&mut s, "/subagents maybe").is_empty());
        assert!(last_error(&s).contains("model|effort|context|max|parallel"));
        assert!(run_command(&mut s, "/subagents max").is_empty());
        assert!(last_error(&s).contains("needs a value"));
    }

    #[test]
    fn settings_change_by_command_and_are_remembered() {
        let mut s = with_catalog();
        let effects = run_command(&mut s, "/subagents model Claude Opus 5");
        let expected = SubagentSettings {
            model: Some("opus".into()),
            ..SubagentSettings::default()
        };
        assert_eq!(effects, vec![Effect::SetSubagentSettings(expected)]);
        run_command(&mut s, "/subagents effort high");
        run_command(&mut s, "/subagents context 200k");
        run_command(&mut s, "/subagents max 6");
        run_command(&mut s, "/subagents parallel 3");
        assert_eq!(
            s.subagent_settings,
            SubagentSettings {
                model: Some("opus".into()),
                reasoning_effort: Some("high".into()),
                context_tokens: Some(200_000),
                max_subagents: Some(6),
                max_concurrent: Some(3),
            }
        );
        assert!(
            matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("context 200K") && n.contains("parallel 3"))
        );
        // Switching to a model without that effort drops it, and says so.
        run_command(&mut s, "/subagents model sonnet55");
        assert_eq!(s.subagent_settings.reasoning_effort, None);
        assert!(s
            .flush
            .iter()
            .any(|i| matches!(i, Item::Notice(n) if n.contains("doesn't offer effort `high`"))));
        assert_eq!(
            run_command(&mut s, "/subagents reset"),
            vec![Effect::SetSubagentSettings(SubagentSettings::default())]
        );
        assert!(s.subagent_settings.is_empty());
    }

    #[test]
    fn invalid_values_change_nothing_and_name_the_choices() {
        let mut s = with_catalog();
        for (command, hint) in [
            ("/subagents model gpt-9", "opus, sonnet55"),
            ("/subagents context 150k", "100K steps"),
            ("/subagents max 99", "from 1 to 50"),
            ("/subagents parallel 0", "from 1 to 20"),
            ("/subagents effort max", "has no reasoning-effort levels"),
            ("/subagents model my-gpt", "isn't a model offered in chat"),
        ] {
            assert!(run_command(&mut s, command).is_empty(), "{command}");
            let err = last_error(&s);
            assert!(err.contains(hint), "{command}: {err}");
        }
        assert!(s.subagent_settings.is_empty());
    }

    #[test]
    fn a_server_without_settings_still_keeps_them_and_says_so() {
        let mut s = with_catalog();
        s.catalog.as_mut().unwrap().subagent_settings = None;
        let effects = run_command(&mut s, "/subagents max 4");
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("doesn't take subagent settings yet"))
        );
    }

    #[test]
    fn the_model_picker_chooses_the_subagent_model() {
        let mut s = with_catalog();
        assert!(run_command(&mut s, "/subagents model").is_empty());
        let picker = s.picker.as_ref().unwrap();
        assert_eq!(picker.rows[0], PickerRow::SubagentModel(None));
        assert_eq!(picker.selected, 0, "the chat's model is current");
        assert!(
            !picker.rows.iter().any(|r| matches!(r, PickerRow::Model(_))),
            "connections can't run subagents"
        );
        s.on_key(key(KeyCode::Down));
        let effects = s.on_key(key(KeyCode::Enter));
        assert!(
            matches!(&effects[..], [Effect::SetSubagentSettings(set)] if set.model.as_deref() == Some("opus")),
            "{effects:?}"
        );
        assert!(s.picker.is_none());
        assert_eq!(s.model, None, "the chat's own model is untouched");
    }

    #[test]
    fn parallel_queues_extra_background_subagents_until_one_finishes() {
        let mut s = with_catalog();
        run_command(&mut s, "/subagents parallel 2");
        let mut effects = Vec::new();
        for id in ["a", "b", "c", "d"] {
            effects.extend(s.apply(AssistantEvent::SubagentRequested(request(id))));
        }
        assert_eq!(started(&effects), vec!["a", "b"]);
        assert_eq!(s.queued_targets().len(), 2);
        assert!(s.queued_targets()[0].starts_with("Check c · analyst"));
        let effects = s.apply(report("a", Status::Completed));
        assert_eq!(started(&effects), vec!["c"], "a slot freed up");
        // Raising the limit starts the rest at once.
        let effects = run_command(&mut s, "/subagents parallel 5");
        assert_eq!(started(&effects), vec!["d"]);
        assert!(s.queued.is_empty());
    }

    #[test]
    fn stopping_or_interrupting_drops_queued_subagents() {
        let mut s = with_catalog();
        run_command(&mut s, "/subagents parallel 1");
        s.start_turn("go");
        s.apply(AssistantEvent::SubagentRequested(request("a")));
        s.apply(AssistantEvent::SubagentRequested(request("b")));
        assert_eq!(s.queued.len(), 1);
        s.apply(AssistantEvent::Done(TurnEnd::Cancelled));
        assert!(s.queued.is_empty(), "esc drops what never started");
        assert!(s
            .flush
            .iter()
            .any(|i| matches!(i, Item::Notice(n) if n.contains("1 queued subagent won't start"))));
        assert_eq!(s.background.len(), 1, "the running one carries on");
        s.apply(AssistantEvent::SubagentRequested(request("c")));
        assert_eq!(
            run_command(&mut s, "/subagents stop"),
            vec![Effect::StopSubagents]
        );
        assert!(s.queued.is_empty());
        let effects = s.apply(report("a", Status::Cancelled));
        assert!(started(&effects).is_empty(), "nothing left to start");
    }

    #[test]
    fn a_requested_subagent_runs_on_the_model_the_event_names() {
        let mut s = with_catalog();
        s.auto_compact = Some(300_000);
        let asked = SubagentRequest {
            model: Some("opus".into()),
            reasoning_effort: Some("low".into()),
            ..request("a")
        };
        let effects = s.apply(AssistantEvent::SubagentRequested(asked));
        let [Effect::StartSubagent(plan)] = &effects[..] else {
            panic!("{effects:?}");
        };
        let model = plan.model.as_ref().unwrap();
        assert_eq!(
            (model.model.key().as_str(), model.effort.as_deref()),
            ("opus", Some("low"))
        );
        assert_eq!(plan.auto_compact, Some(300_000), "the chat's threshold");
    }
}
