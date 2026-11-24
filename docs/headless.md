# Scripted `gz code` runs

`gz code -p` runs the assistant once, without a person at the terminal, and
prints the result in a form a script can read. Nothing in a scripted run
ever prompts: an action nothing allowed ahead of time is refused at once,
the model is told why and carries on, and the refusal is reported in
`permission_denials`.

```bash
gz code -p "why does the build fail?"                    # the final answer only
git diff | gz code -p "review this change"               # the prompt from stdin
gz code -p @task.md --output-format json > run.json      # one result object
gz code -p "fix the test" --output-format stream-json    # one JSON event per line
```

## Which runs are scripted

| You run | What happens |
|---|---|
| `gz code` in a terminal | the interactive interface |
| `gz code "<prompt>"` | one turn, the answer streamed, progress on stderr, permission prompts in the terminal (refused without one) |
| `gz code -p "<prompt>"` (or `--print`) | scripted, `--output-format text` |
| `gz code --output-format …` | scripted, implies `-p` |
| `gz code "<prompt>" -o json\|yaml` | scripted, the run report described in the README (kept as it was) |
| `gz code "<prompt>" --max-turns N` (or `--max-budget-usd`) | scripted, `text` |

The prompt is the argument (`@file` reads a file), or, with no argument, all
of standard input. `-p` with neither says how to pass one.

`-o json|yaml` and `--output-format` are two ways to ask for machine output;
use one. `-o` gives the camelCase run report scripts already rely on;
`--output-format` gives the schemas below.

## Flags

| Flag | Effect |
|---|---|
| `-p`, `--print` | run once, scripted |
| `--output-format text\|json\|stream-json` | see below |
| `--input-format stream-json` | read user messages from stdin, one JSON line each; needs `--output-format stream-json` |
| `--include-partial-messages` | with `--output-format stream-json`: also write the answer's text as it streams, as `partial_assistant` events (see below) |
| `--max-turns N` | stop when a message would need more than N model turns. Every round of client tool calls is a new turn, so `--max-turns 1` lets the model answer but not use tools. Counted per message |
| `--max-budget-usd X` | stop before the next model turn once this run's cost reaches X (US dollars), counted as `cost_usd` is (every leg of each turn). A turn already running finishes, so the total can pass X by one turn's cost |
| `--tools LIST` | the client tools the model is offered, comma- or space-separated; `none` offers none, `all` every one. Names: `read_file`, `list_directory`, `find_files`, `search_files`, `edit_file`, `write_file`, `run_command`, `run_script`, `change_directory`, `load_skill`, `memory`, `command_output`, `stop_command`, `notebook_edit`, `web_fetch`, `ask_user_question`, `enter_plan_mode`, `exit_plan_mode`, `todo_write`, `local_mcp_tools`, `local_mcp_call`. A tool also needs the server to offer it; a call to a tool not offered is refused |
| `--append-system-prompt TEXT` | extra instructions for this run, sent as your own standing instructions (`clientContext.instructions`, `source: "user"`, path `--append-system-prompt`). Needs a chat server with local context; at most 20,000 characters |
| `--append-system-prompt-file PATH` | the same from a file (repeatable); the file's path is the entry's path |
| `--add-dir PATH` | open another folder too, as `/add-dir` does (repeatable). A relative path is read from the folder gz started in, also with `--worktree` |
| `--bare` | skip hooks, skills, MCP servers, memories and instruction files (`instructions.md`, `GZ.md`, `AGENTS.md`, `CLAUDE.md`): faster and the same on every machine. Appended instructions still go out |
| `--verbose` | progress lines (tools, notices) on stderr, plus each HTTP request |
| `--worktree [NAME]` | run in the git worktree `.gz/worktrees/NAME` on branch `gz/NAME` (see the README); the notes on how to merge or remove it go to stderr, and `-q` leaves them out |

The chat server takes at most 8 instruction entries of at most 20,000
characters each (or the lower limits it advertises). More appended entries
than that, or a longer one, are refused before the run starts. Appended
entries come first, so with many instruction files the farthest files are
left out.

`--tools`, `--append-system-prompt*`, `--add-dir` and `--bare` also work for
interactive sessions, and background subagents follow them too. An
interactive session with `--append-system-prompt` asks the server first and
refuses to open when it can't send them.

Flag errors, a missing prompt and an unusable `--add-dir` are found before a
worktree is made. In a scripted run they come out in the chosen format
(`json`: the result object with `subtype: "error"`; `stream-json`: a
`result` line), like any other failure. Flags that can't go together or have no
usable value (`-o json` with `--output-format`, a misplaced
`--input-format`, `--max-budget-usd 0`) are reported the way any gz command
reports a usage error, before the format is settled.

Allowing more without a person: `--yes-reads` (every read in the folder),
`--auto` (auto mode's review decides), and saved rules in
`permissions.toml`.

## `--output-format text`

The final answer (the text after the model's last tool call) on stdout,
nothing else. Errors go to stderr. Progress goes to stderr only with
`--verbose`.

## `--output-format json`

One object on stdout when the run ends:

```json
{
  "type": "result",
  "subtype": "success",
  "is_error": false,
  "result": "All done.",
  "conversation_id": "b42bf8ee-8dcf-4359-bfae-20fd509fa6c4",
  "model": "sonnet55",
  "num_turns": 2,
  "duration_ms": 5210,
  "cost_usd": 0.0042,
  "estimated_cost_usd": 0,
  "unpriced_legs": 0,
  "usage": {
    "input_tokens": 100,
    "output_tokens": 20,
    "cache_read_input_tokens": 0,
    "cache_write_input_tokens": 0
  },
  "permission_denials": [
    {"tool": "run_command", "summary": "run_command git push", "reason": "this action needs the user's approval …"}
  ],
  "tool_calls": [
    {"tool": "read_file", "summary": "read_file hello.txt", "decision": "preapproved", "is_error": false}
  ],
  "error": null,
  "exit_code": 4
}
```

| Field | Meaning |
|---|---|
| `subtype` | `success`, `error`, `error_max_turns`, `error_max_budget_usd`, `interrupted` |
| `is_error` | the run did not complete |
| `result` | the final answer (may be partial when the run stopped) |
| `conversation_id` | continue with `gz code -p "…" -c <id>`; `null` when the run failed before a conversation was set up |
| `model` | the key of the model this message ran on (a command file's `model:` included), `null` when the list could not be loaded |
| `num_turns` | model turns this message took (the start plus each resume) |
| `cost_usd`, `usage` | totals for every model turn of the message; `null` until the server reports a figure. The server reports each run's cost for its last leg only (every client-tools resume is a new run), so gz prices the earlier legs' tokens at the rate that leg showed, as the interactive cost line does; those earlier legs are estimates |
| `estimated_cost_usd` | the part of `cost_usd` gz estimated for those earlier legs; `0` when the server priced everything |
| `unpriced_legs` | earlier legs left out of `cost_usd` because no rate was known yet (no model rate learned and the server gave no token counts); `0` normally |
| `permission_denials` | actions refused because no one could approve them, or auto mode's review blocked them |
| `tool_calls[].decision` | `preapproved`, `auto`, `once`, `blocked` or `server` (the server ran it) |
| `error` | why the run failed or stopped |
| `exit_code` | the process exit code |

## `--output-format stream-json`

One JSON object per line, as things happen, ending with the `result` object
above. Every line has a `type`:

| `type` | Fields |
|---|---|
| `init` | `conversation_id`, `cwd`, `roots`, `model`, `tools` (client tools offered), `gz_version` |
| `assistant` | `text`: the model's text since the last other event (a subagent's text since its last event, with `subagent`) |
| `partial_assistant` | only with `--include-partial-messages`: `text`, one piece of the answer as it arrived (with `subagent` for a subagent's). The pieces in order add up to the `assistant` event that follows them, which still comes, so skip one kind or the other |
| `tool_use` | `tool`, `input` (the call's arguments; `null` for a server tool), `summary` (a server tool's name and one-line input, e.g. `Query lakehouse select 1`, the same text its `tool_result` carries), `local` (gz runs it on this machine) |
| `tool_result` | `tool`, `summary`, `is_error`, `output` (what the model received) |
| `permission_denied` | `tool`, `summary`, `reason` |
| `notice` | `text`: a warning, refusal or server tool error |
| `usage` | `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_write_input_tokens`, `cost_usd` for the message's turn just finished, every leg counted, plus `estimated_cost_usd` and `unpriced_legs` for that turn (see `cost_usd` above; a subagent's `usage` always has `0` for both) |
| `subagent` | `id`, `name` (when started), `status` |
| `result` | as in `json` |

Events from a subagent's work carry `"subagent": "<id>"`. New event types
and fields may be added; skip what you don't know.

## `--input-format stream-json`

Each line on stdin is one user message; each runs as a turn in the same
conversation, and each ends with its own `result` line. Blank lines are
skipped. A line is:

```json
{"type": "user", "message": {"role": "user", "content": "fix the failing test"}}
```

`content` may also be a list of text blocks:
`[{"type": "text", "text": "…"}]`. A line that isn't one of these ends the
run with an `error` result naming the line. The run stops after a message
that failed, was stopped by a limit, or was interrupted; refused actions
don't stop it. Ctrl+C (SIGINT) while gz waits for the next line ends the
run with an `interrupted` result and exit code 130.

```bash
printf '%s\n' \
  '{"type":"user","message":{"role":"user","content":"read the README"}}' \
  '{"type":"user","message":{"role":"user","content":"now summarize it"}}' \
| gz code --input-format stream-json --output-format stream-json --yes-reads
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | completed |
| `1` | failed (the result's `error`, or stderr, says why) |
| `3` | a `/budget` cap is spent; nothing was sent |
| `4` | completed, but an action was refused because it needed approval |
| `5` | stopped by `--max-turns` |
| `6` | stopped by `--max-budget-usd` |
| `130` | interrupted (Ctrl+C) |

With stream input, the code is the last message's.
