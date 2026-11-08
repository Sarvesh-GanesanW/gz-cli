//! Session events of a turn: streamed text, tools, subagents, and how they
//! become finished transcript items.

use std::time::Instant;

use crate::assistant::attachments::PreparedTurn;
use crate::assistant::{AssistantEvent, TurnEnd};
use crate::code_ui::auto_mode::AutoContext;
use crate::code_ui::blocks;
use crate::code_ui::items::{Item, ToolItem, ToolStatus};
use crate::code_ui::labels::{self, Label};
use crate::code_ui::usage;
use crate::subagents::{Status as SubagentStatus, Subagent, SubagentEvent};

use super::{Effect, UiState, SCRIPT_CHILD_PREFIX};

impl UiState {
    /// Begins a turn the driver is about to send.
    #[cfg(test)]
    pub fn start_turn(&mut self, prompt: &str) {
        self.start_prepared_turn(&PreparedTurn::plain(prompt));
    }

    /// Shows the user's message (with its attachment notes) and starts the
    /// spinner.
    pub fn start_prepared_turn(&mut self, turn: &PreparedTurn) {
        let item = if turn.notes.is_empty() {
            Item::User(turn.display.clone())
        } else {
            Item::UserWithContext {
                text: turn.display.clone(),
                notes: turn.notes.clone(),
            }
        };
        self.record(item);
        self.with_auto(|auto| auto.user_said(&turn.display));
        self.begin_turn();
    }

    /// Starts the spinner and the turn's stats.
    pub(super) fn begin_turn(&mut self) {
        self.busy_since = Some(Instant::now());
        self.received_chars = 0;
        self.subagents.keep_running(&self.background);
        self.turns += 1;
        self.usage.start_turn(self.model_label());
        self.spend.meter.guard.turn_started();
    }

    pub fn apply(&mut self, event: AssistantEvent) -> Vec<Effect> {
        match event {
            AssistantEvent::Text(text) => {
                self.close_group();
                let chars = text.chars().count();
                self.received_chars += chars;
                self.usage.turn.streamed(chars);
                self.streaming.push_str(&text);
                self.answer_full.push_str(&text);
                self.commit_finished_blocks();
            }
            AssistantEvent::Usage(report) => {
                self.subagents.record_usage(&report.subagents);
                self.usage.turn.record(&report);
                // The run's cost includes its inline subagents' shares.
                if let Some(cost) = report.cost_usd {
                    self.spend_reported(cost, self.model_name());
                }
            }
            AssistantEvent::Context(context) => self.usage.context = Some(context),
            AssistantEvent::SteerSent(note) => self.notes_sent(vec![note]),
            AssistantEvent::SteerApplied(steer_id) => self.note_read(&steer_id),
            // Unread notes are requeued when the turn ends.
            AssistantEvent::SteerUnapplied(_) => {}
            AssistantEvent::Subagent(event) => self.subagent_event(&event),
            AssistantEvent::ForSubagent { id, event } => self.subagent_tool(&id, *event),
            AssistantEvent::ToolStarted {
                name,
                summary,
                local,
            } => self.tool_started(&name, &summary, local),
            AssistantEvent::ToolFinished {
                name,
                summary,
                is_error,
                detail,
                output,
                diff,
            } => self.tool_finished(&name, &summary, is_error, &detail, output, diff),
            AssistantEvent::Notice(text) => {
                self.close_text();
                self.close_group();
                self.record(Item::Notice(text));
            }
            AssistantEvent::Folders(result) => {
                self.close_text();
                self.close_group();
                self.record(match result {
                    Ok(text) => Item::Notice(text),
                    Err(why) => Item::Error(why),
                });
            }
            AssistantEvent::SubagentRequested(request) => return self.subagent_requested(request),
            AssistantEvent::SubagentReport(report) => return self.subagent_reported(report),
            AssistantEvent::Done(end) => return self.finish_turn(end),
        }
        Vec::new()
    }

    fn tool_started(&mut self, name: &str, summary: &str, local: bool) {
        let tools = &mut self.usage.turn.tools;
        match (summary.starts_with(SCRIPT_CHILD_PREFIX), local) {
            (true, _) => tools.helper += 1,
            (false, true) => tools.local += 1,
            (false, false) => tools.server += 1,
        }
        if let Some(child) = summary.strip_prefix(SCRIPT_CHILD_PREFIX) {
            if let Some(script) = self
                .running
                .iter_mut()
                .rev()
                .find(|t| t.label.verb == "Script")
            {
                script.children.push(child.to_string());
                return;
            }
        }
        self.close_text();
        let label = labels::label(name, summary);
        if !label.read_only {
            self.close_group();
        }
        let item = ToolItem {
            label,
            status: ToolStatus::Running,
            result: None,
            diff: None,
            children: Vec::new(),
            output: None,
        };
        if local {
            self.running.push(item);
        } else {
            // Server-side tools report only that they started.
            self.record(Item::Tool(ToolItem {
                status: ToolStatus::Ok,
                ..item
            }));
        }
    }

    fn tool_finished(
        &mut self,
        name: &str,
        summary: &str,
        failed: bool,
        detail: &str,
        output: String,
        diff: Option<String>,
    ) {
        let label = labels::label(name, summary);
        let position = self.running.iter().position(|t| t.label == label);
        let mut item = match position {
            Some(i) => self.running.remove(i),
            None => ToolItem {
                label: label.clone(),
                status: ToolStatus::Running,
                result: None,
                diff: None,
                children: Vec::new(),
                output: None,
            },
        };
        item.status = if failed {
            ToolStatus::Failed
        } else {
            ToolStatus::Ok
        };
        item.result = Some(labels::result_line(name, summary, detail, diff.as_deref()));
        item.diff = diff;
        // Everything a local tool returns is sent back to the model.
        self.usage.turn.tools.context_bytes += output.len();
        item.output = Some(output).filter(|o| !o.trim().is_empty());
        if label.read_only && !failed {
            self.group.push(item);
        } else {
            self.close_group();
            self.record(Item::Tool(item));
        }
    }

    /// Running subagents stay in the live region; a finished one prints
    /// once and joins the turn's analytics.
    fn subagent_event(&mut self, event: &SubagentEvent) {
        if matches!(event, SubagentEvent::Started { .. }) {
            self.close_text();
            self.close_group();
        }
        let done = self.subagents.apply(event, Instant::now());
        self.pages.on_subagent(event, &self.subagents);
        if let Some(done) = done {
            if self.is_busy() {
                self.usage.turn.subagents.push(done.clone());
            } else {
                // A background subagent that ends between turns counts
                // toward the session only.
                self.usage.session_subagents += 1;
            }
            self.record(Item::Subagent(Box::new(done)));
        }
    }

    /// A local tool gz ran for a subagent: it goes on that subagent's page,
    /// and counts toward the turn's analytics.
    fn subagent_tool(&mut self, id: &str, event: AssistantEvent) {
        // A background subagent's spend is its own run's, not the turn's.
        if let AssistantEvent::Usage(report) = &event {
            self.usage.session.record(report);
            if let Some(cost) = report.cost_usd {
                let model = self
                    .subagents
                    .get(id)
                    .and_then(|agent| agent.runs_on.label.clone())
                    .or_else(|| self.model_name());
                self.spend_reported(cost, model);
            }
            return;
        }
        let tools = &mut self.usage.turn.tools;
        match &event {
            AssistantEvent::ToolStarted { summary, .. }
                if summary.starts_with(SCRIPT_CHILD_PREFIX) =>
            {
                tools.helper += 1
            }
            AssistantEvent::ToolStarted { .. } => tools.local += 1,
            AssistantEvent::ToolFinished { output, .. } => tools.context_bytes += output.len(),
            _ => {}
        }
        self.pages.on_local(id, event, &self.subagents);
    }

    /// Drops what belonged to the conversation being left (`/new`,
    /// `/clear`, `/resume`): subagent pages, the context fill, and its
    /// background subagents, whose reports would mean nothing elsewhere.
    pub(super) fn leave_conversation(&mut self) -> Vec<Effect> {
        self.pages.clear();
        self.agent_view = None;
        self.usage.context = None;
        self.with_auto(AutoContext::new_conversation);
        self.reports.clear();
        self.drop_queued();
        if self.background.is_empty() {
            return Vec::new();
        }
        self.drop_prompts(|background| background.is_some());
        vec![Effect::StopSubagents]
    }

    /// Subagents still in flight when the turn ends stopped with it: as
    /// cancelled when the user interrupted, as failed when the run failed.
    fn stop_subagents(&mut self, end: &TurnEnd) {
        let now = Instant::now();
        let stopped: Vec<Subagent> = self
            .subagents
            .running()
            .filter(|a| !self.is_background(&a.id))
            .cloned()
            .collect();
        for mut agent in stopped {
            agent.status = match end {
                TurnEnd::Failed(_) => SubagentStatus::Failed,
                _ => SubagentStatus::Cancelled,
            };
            if agent.summary.is_empty() && matches!(end, TurnEnd::Failed(_)) {
                agent.summary = "Stopped because the run failed (see the error below).".into();
            }
            agent.duration = Some(agent.elapsed(now));
            self.pages.stopped(&agent);
            self.usage.turn.subagents.push(agent.clone());
            self.record(Item::Subagent(Box::new(agent)));
        }
        self.subagents.keep_running(&self.background);
    }

    fn finish_turn(&mut self, end: TurnEnd) -> Vec<Effect> {
        self.close_text();
        self.close_group();
        self.stop_subagents(&end);
        let stopped: Vec<ToolItem> = self.running.drain(..).collect();
        for mut tool in stopped {
            tool.status = ToolStatus::Failed;
            tool.result.get_or_insert_with(|| "Stopped".to_string());
            self.record(Item::Tool(tool));
        }
        let completed = end == TurnEnd::Completed;
        match end {
            TurnEnd::Completed => {}
            TurnEnd::Cancelled => {
                self.record(Item::Notice("⎿  Interrupted by user".into()));
                // Subagents that never started end with the turn the user
                // stopped; running ones carry on.
                let queued = self.drop_queued();
                if queued > 0 {
                    self.record(Item::Notice(format!(
                        "⎿  {queued} queued subagent{} won't start",
                        if queued == 1 { "" } else { "s" }
                    )));
                }
            }
            TurnEnd::Failed(why) => self.record(Item::Error(crate::http::readable_error(&why))),
        }
        let stats = self.usage.end_turn().clone();
        if completed {
            self.record(Item::Summary(usage::turn_summary(&stats)));
            self.transcript
                .push(Item::Panel(usage::turn_details(&stats)));
        }
        self.busy_since = None;
        // The run ended, so its prompts can no longer matter; background
        // subagents' prompts stay.
        self.drop_prompts(|background| background.is_none());
        self.requeue_unread_notes();
        self.spend.meter.guard.turn_ended();
        self.next_message()
    }

    /// The answer chunk still streaming, as it will print once finished.
    pub fn streaming_item(&self) -> Option<Item> {
        let text = self.streaming.trim_end();
        (!text.trim().is_empty()).then(|| self.answer_chunk(text.to_string()))
    }

    fn answer_chunk(&self, text: String) -> Item {
        if self.answer_started {
            Item::AssistantMore(text)
        } else {
            Item::Assistant(text)
        }
    }

    /// Prints the answer's finished blocks now, keeping only the block in
    /// progress live, so long answers scroll like they do in the transcript.
    fn commit_finished_blocks(&mut self) {
        let Some(cut) = blocks::last_safe_cut(&self.streaming) else {
            return;
        };
        let done = self.streaming[..cut.commit].trim_end().to_string();
        self.streaming = self.streaming[cut.rest..].to_string();
        if done.trim().is_empty() {
            return;
        }
        let chunk = self.answer_chunk(done);
        self.flush.push(chunk);
        self.answer_started = true;
    }

    pub(super) fn close_text(&mut self) {
        let text = std::mem::take(&mut self.streaming);
        if !text.trim().is_empty() {
            let chunk = self.answer_chunk(text.trim_end().to_string());
            self.flush.push(chunk);
        }
        let full = std::mem::take(&mut self.answer_full);
        if !full.trim().is_empty() {
            self.transcript
                .push(Item::Assistant(full.trim_end().to_string()));
        }
        self.answer_started = false;
    }

    /// Prints pending read-only calls: one in full, several as a summary.
    pub(super) fn close_group(&mut self) {
        match self.group.len() {
            0 => {}
            1 => {
                let item = self.group.remove(0);
                self.record(Item::Tool(item));
            }
            _ => {
                let tools: Vec<ToolItem> = self.group.drain(..).collect();
                let labels: Vec<Label> = tools.iter().map(|t| t.label.clone()).collect();
                self.flush.push(Item::Group(labels::group_summary(&labels)));
                // The transcript keeps each call with its full output.
                self.transcript.extend(tools.into_iter().map(Item::Tool));
            }
        }
    }

    /// The transcript plus what the current turn hasn't finished yet
    /// (pending reads, running tools, the answer so far).
    pub fn transcript_items(&self) -> Vec<Item> {
        let mut items = self.transcript.clone();
        items.extend(
            self.group
                .iter()
                .chain(self.running.iter())
                .cloned()
                .map(Item::Tool),
        );
        items.extend(
            self.subagents
                .running()
                .cloned()
                .map(|agent| Item::Subagent(Box::new(agent))),
        );
        if !self.answer_full.trim().is_empty() {
            items.push(Item::Assistant(self.answer_full.trim_end().to_string()));
        }
        items
    }
}

#[cfg(test)]
mod tests {

    use crate::code_ui::state::test_support::*;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::assistant::{AssistantEvent, TurnEnd};

    use crate::code_ui::items::{Item, ToolItem};

    use crate::code_ui::usage;

    use crate::subagents::{Status as SubagentStatus, Subagent};

    #[test]
    fn text_and_tools_flush_in_order_with_reads_grouped() {
        let mut s = state();
        s.start_turn("look");
        s.apply(AssistantEvent::Text("Let me look.".into()));
        s.apply(started("read_file", "read_file a.rs"));
        s.apply(finished("read_file", "read_file a.rs", "3 lines", None));
        s.apply(started("read_file", "read_file b.rs"));
        s.apply(finished("read_file", "read_file b.rs", "4 lines", None));
        assert_eq!(s.group.len(), 2, "reads wait to be grouped");
        s.apply(AssistantEvent::Text("Done.".into()));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        let summary = s.flush.pop();
        assert_eq!(
            s.flush,
            vec![
                Item::User("look".into()),
                Item::Assistant("Let me look.".into()),
                Item::Group("Read 2 files".into()),
                Item::Assistant("Done.".into()),
            ]
        );
        assert!(
            matches!(&summary, Some(Item::Summary(line))
                if line.starts_with("✻ Worked for") && line.ends_with("· 2 tools")),
            "{summary:?}"
        );
        assert!(!s.is_busy());
    }

    #[test]
    fn edits_flush_with_their_diff_and_script_calls_nest() {
        let mut s = state();
        s.start_turn("go");
        s.apply(started("run_script", "run_script python (2 lines)"));
        s.apply(started("read_file", "script · read_file a.rs"));
        assert_eq!(s.running[0].children, vec!["read_file a.rs"]);
        s.apply(finished(
            "run_script",
            "run_script python (2 lines)",
            "2 lines",
            None,
        ));
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n";
        s.apply(started("edit_file", "edit_file a.rs"));
        s.apply(finished(
            "edit_file",
            "edit_file a.rs",
            "edited",
            Some(diff),
        ));
        let Item::Tool(edit) = s.flush.last().unwrap() else {
            panic!()
        };
        assert_eq!(edit.label.title(), "Update(a.rs)");
        assert_eq!(
            edit.result.as_deref(),
            Some("Updated a.rs with 1 addition and 1 removal")
        );
        assert!(edit.diff.is_some());
        let Item::Tool(script) = &s.flush[1] else {
            panic!()
        };
        assert_eq!(script.children, vec!["read_file a.rs"]);
    }

    #[test]
    fn subagents_run_live_print_when_done_and_roll_into_analytics() {
        use crate::subagents::{SubagentEvent, SubagentUsage};
        let mut s = state();
        s.start_turn("investigate");
        for (id, name) in [("a", "Logs"), ("b", "Schema")] {
            s.apply(subagent(SubagentEvent::Started {
                id: id.into(),
                name: name.into(),
                profile: Some("data_engineer".into()),
                agent_id: None,
                task: None,
                runs_on: Default::default(),
            }));
        }
        assert_eq!(s.subagents.running().count(), 2);
        assert!(!s.flush.iter().any(|i| matches!(i, Item::Subagent(_))));
        s.apply(subagent(SubagentEvent::Finished {
            id: "a".into(),
            status: SubagentStatus::Completed,
            summary: "job 4 failed on a null key".into(),
            usage: SubagentUsage::default(),
            duration: Some(std::time::Duration::from_secs(12)),
        }));
        let printed: Vec<&Subagent> = s
            .flush
            .iter()
            .filter_map(|i| match i {
                Item::Subagent(agent) => Some(agent.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(printed.len(), 1);
        assert_eq!(printed[0].name, "Logs");
        let report = crate::cmd::stream::Usage {
            subagents: vec![(
                "a".into(),
                SubagentUsage {
                    input_tokens: 900,
                    output_tokens: 120,
                    cost_usd: Some(0.002),
                },
            )],
            ..Default::default()
        };
        s.apply(AssistantEvent::Usage(report));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        let summary = s
            .flush
            .iter()
            .find_map(|i| match i {
                Item::Summary(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert!(summary.contains("2 subagents"), "{summary}");
        let stopped = s.flush.iter().any(|i| {
            matches!(i, Item::Subagent(agent) if agent.name == "Schema" && agent.status == SubagentStatus::Cancelled)
        });
        assert!(
            stopped,
            "a subagent still running at the end is shown as stopped"
        );
        let details = usage::turn_details(s.usage.last.as_ref().unwrap())
            .plain_lines()
            .join("\n");
        assert!(details.contains("  Subagents · 2\n"), "{details}");
        assert!(
            details.contains("Logs (data_engineer)    12s · ↑ 900 ↓ 120 · $0.0020 · done"),
            "{details}"
        );
        assert_eq!(s.subagents.running().count(), 0);
    }

    #[test]
    fn subagent_work_goes_to_its_page_not_the_main_one() {
        use crate::subagents::SubagentEvent;
        let mut s = state();
        s.start_turn("check the repo");
        s.apply(subagent(SubagentEvent::Started {
            id: "sa_1".into(),
            name: "Edit config".into(),
            profile: Some("general".into()),
            agent_id: None,
            task: Some("Fix the port in config.toml".into()),
            runs_on: Default::default(),
        }));
        s.apply(AssistantEvent::ForSubagent {
            id: "sa_1".into(),
            event: Box::new(started("edit_file", "Update config.toml")),
        });
        s.apply(AssistantEvent::ForSubagent {
            id: "sa_1".into(),
            event: Box::new(finished(
                "edit_file",
                "Update config.toml",
                "1 line changed",
                Some("--- a/config.toml\n+++ b/config.toml\n@@ -1 +1 @@\n-port = 1\n+port = 2\n"),
            )),
        });
        assert!(
            !s.flush.iter().any(|i| matches!(i, Item::Tool(_))),
            "subagent tools stay off the main page"
        );
        assert!(s.running.is_empty());
        let page = s.pages.get(0).expect("a page per subagent");
        let shown: String = page
            .items()
            .iter()
            .flat_map(|i| i.render_expanded(80))
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains("Fix the port in config.toml"), "{shown}");
        assert!(
            shown.contains("port = 2"),
            "the edit's diff is on the page: {shown}"
        );
    }

    /// Streams `text` one character at a time and returns everything the
    /// answer printed into scrollback.
    fn stream_answer(text: &str) -> Vec<Item> {
        let mut s = state();
        s.start_turn("q");
        s.flush.clear();
        for ch in text.chars() {
            s.apply(AssistantEvent::Text(ch.to_string()));
        }
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        s.flush
            .into_iter()
            .filter(|i| matches!(i, Item::Assistant(_) | Item::AssistantMore(_)))
            .collect()
    }

    const LONG_ANSWER: &str = "## Plan\n\nFirst we read the files.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\n1. one\n\n2. two\n\n- tight\n- list\n\n| file | lines |\n|---|---:|\n| a.rs | 3 |\n\n> a quote\n\nDone with **bold** and `code`.";

    #[test]
    fn streamed_chunks_print_exactly_like_the_whole_answer() {
        let chunks = stream_answer(LONG_ANSWER);
        assert!(
            chunks.len() > 3,
            "long answers commit in pieces: {chunks:?}"
        );
        for width in [40, 80, 120] {
            let pieces: Vec<String> = chunks
                .iter()
                .flat_map(|item| item.render(width))
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            let whole: Vec<String> = Item::Assistant(LONG_ANSWER.into())
                .render(width)
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            assert_eq!(pieces, whole, "width {width}");
        }
    }

    #[test]
    fn finished_blocks_print_before_the_answer_ends() {
        let mut s = state();
        s.start_turn("q");
        s.flush.clear();
        s.apply(AssistantEvent::Text(
            "Para one.\n\nPara two starts\n".into(),
        ));
        assert_eq!(s.flush, vec![Item::Assistant("Para one.".into())]);
        assert_eq!(
            s.streaming_item(),
            Some(Item::AssistantMore("Para two starts".into())),
            "the rest stays live, without a second bullet"
        );
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        assert_eq!(
            s.transcript
                .iter()
                .rfind(|i| matches!(i, Item::Assistant(_))),
            Some(&Item::Assistant("Para one.\n\nPara two starts".into())),
            "the transcript keeps the answer whole"
        );
    }

    #[test]
    fn usage_reports_feed_the_summary_and_last() {
        let mut s = state();
        s.start_turn("q");
        s.apply(AssistantEvent::Text("hello".into()));
        s.apply(AssistantEvent::Usage(crate::cmd::stream::Usage {
            tokens: Some(crate::cmd::stream::TokenCounts {
                input: 900,
                output: 120,
                cache_read: 100,
                cache_write: 0,
            }),
            cost_usd: Some(0.004),
            ..Default::default()
        }));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        let summary = s.flush.iter().find_map(|i| match i {
            Item::Summary(line) => Some(line.clone()),
            _ => None,
        });
        assert!(summary.unwrap().contains("↑ 1.0k ↓ 120 tokens · $0.0040"));
        s.flush.clear();
        type_text(&mut s, "/last");
        s.on_key(key(KeyCode::Enter));
        let Some(Item::Panel(panel)) = s.flush.last() else {
            panic!("no analytics: {:?}", s.flush);
        };
        let lines = panel.plain_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("cost") && l.ends_with("$0.0040")),
            "{lines:?}"
        );
    }

    #[test]
    fn transcript_keeps_full_detail_and_ctrl_o_toggles_it() {
        let mut s = state();
        s.start_turn("look");
        s.apply(started("read_file", "read_file a.rs"));
        s.apply(finished(
            "read_file",
            "read_file a.rs",
            "line one\nline two",
            None,
        ));
        s.apply(started("read_file", "read_file b.rs"));
        s.apply(finished("read_file", "read_file b.rs", "b body", None));
        s.apply(AssistantEvent::Done(TurnEnd::Completed));
        let tools: Vec<&ToolItem> = s
            .transcript
            .iter()
            .filter_map(|i| match i {
                Item::Tool(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 2, "grouped reads are kept one by one");
        assert_eq!(tools[0].output.as_deref(), Some("line one\nline two"));
        assert!(s
            .transcript
            .iter()
            .any(|i| matches!(i, Item::Panel(p) if p.title == "Turn analytics")));
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        s.start_turn("more");
        s.apply(started("read_file", "read_file c.rs"));
        s.apply(finished("read_file", "read_file c.rs", "c body", None));
        s.apply(AssistantEvent::Text("partial".into()));
        let live = s.transcript_items();
        assert!(
            live.iter()
                .any(|i| matches!(i, Item::Tool(t) if t.output.as_deref() == Some("c body"))),
            "pending reads show while the turn runs"
        );
        assert_eq!(live.last(), Some(&Item::Assistant("partial".into())));
        s.on_key(ctrl_o);
        assert!(s.viewer.is_some());
        type_text(&mut s, "x");
        assert_eq!(s.input_text(), "", "keys go to the viewer while it's open");
        s.on_key(key(KeyCode::Esc));
        assert!(s.viewer.is_none());
    }
}
