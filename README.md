# gz - the Groundzero CLI

One small Rust binary for the whole Groundzero platform: lakehouse, SQL,
files, ETL jobs, notebooks, models, agents, and chat. No runtime to install,
no URLs to memorize.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.ps1 | iex
```

Both install to `~/.local/bin` (`%USERPROFILE%\.local\bin` on Windows),
verify the checksum, and fix up your `PATH`. macOS needs nothing extra;
Windows may show a one-time SmartScreen prompt (the binary isn't signed yet).

Prefer manual? Grab your platform from the
[releases page](https://github.com/Sarvesh-GanesanW/gz-cli/releases),
drop it on `PATH` as `gz`, run `gz --version`. From source:
`make install-local` (needs Rust 1.88+).

## Update

```bash
gz update          # self-update to the latest release (checksum-verified)
gz update --check  # just report whether an update exists
```

`gz update` fetches the newest release binary for your platform, verifies
its sha256 checksum, and swaps it in place — no reinstall, no PATH changes.

## Setup

```bash
gz configure            # prompts: client, site, domain (your web app is https://<site>.<client>.<domain>)
gz auth login           # username, password, and authenticator code
gz doctor               # checks every service and your sign-in
```

That's it. `gz auth login` signs in exactly like the web app (Cognito) and
reads the web app's own settings, so every service URL is found for you on
any environment. Sessions renew automatically; `gz auth logout` revokes
them. Override a single service only if you need to:

```bash
gz configure --service data=https://<gateway-url>
```

For CI and scripts, `gz auth login --api-key` (or `GZ_API_KEY`) uses a platform
API key instead; keys work for ETL jobs, data connections/storage, and agent
metrics only. See [docs/CI.md](docs/CI.md).

Tokens live in `~/.config/gz/config.toml`, readable only by you.
`gz mcp setup claude|codex|cursor|vscode` connects an AI tool to your site's
Groundzero MCP server (it signs in through the tool's own OAuth flow);
`gz mcp setup print` shows every snippet.

## Daily use

```bash
gz warehouse list
gz table describe prod analytics orders
gz sql run --sql @query.sql --wait --data @conn.json
gz fs ls --connection s3conn --prefix raw/2024/
gz jobs submit --data @job.json
gz jobs logs --job nightly-etl
gz notebooks list 7
gz models list-all
gz agents list
gz projects list
gz projects jobs myproject
gz designer datasets
gz designer dashboards
gz workspaces list
gz connections list
gz schedules executions
gz permissions api-key list <user>
gz jobs run <project> <job> --compute <id> --rte <image> --follow
gz rte images
gz chat conversations
```

`--output table|json|yaml` on everything (`--json` for scripts).
Writes take `--data @file.json`; secrets belong in files or stdin (`@-`),
never on the command line. `gz request <service> <method> <path>` hits any
endpoint the shortcuts don't cover yet.

`conn.json` for SQL looks like this:

```json
{
  "connectionConfig": {"config": {"userName": "u", "password": "p", "warehouseName": "prod"}},
  "computeId": 6
}
```

## Local dev loop

Edit job code locally, run it on Groundzero compute, keep a folder in sync:

```bash
gz run etl/clean_orders.py --project 1000 --compute 1003 \
  --rte gz-rte-spark-pytorch:1.0.0 --follow       # upload → run → stream logs
gz sync jobs/ --project 1000 --pull                # first: download the project's jobs
gz sync jobs/ --project 1000                       # push changed files to their jobs
gz sync jobs/ --project 1000 --watch               # push on every save (Ctrl+C stops)
```

- `gz run` saves the file as the job's code (asking before it replaces
  existing code; `--yes` skips), submits the run, and with `--follow` exits
  non-zero unless it ends in `Success`. Without `--job` it uses a scratch job
  `gz-run-<file>` (created after you confirm).
- `gz sync` maps `clean_orders.py` → job `clean-orders`, or use a
  `gz-sync.toml` (`[jobs] "etl/clean_orders.py" = "clean-orders"`). It never
  deletes jobs, never overwrites a change made in the web app since the last
  sync (it says so; `--force` overrides), and `--dry-run` shows the plan.
  It records what it synced in `.gz-sync.json`; add that to `.gitignore`.
  `--only <file>` syncs just that file.
- Jobs run Python; wrap SQL in `spark.sql(...)`.

### Fix a failed run (`gz fix`)

```bash
gz fix clean-orders                  # newest failed run → gz code with a ready task
gz jobs fix 1006 --project 1000 --dir fixes --compute 1003 --rte gz-rte-spark-pytorch:1.0.0
gz fix clean-orders --run <run-id>   # a specific run
gz fix clean-orders --print          # just print the evidence and the prompt
```

`gz fix` (same as `gz jobs fix`) finds the job's newest failed run, reads its
status, a log excerpt (the end of the log plus the lines around errors and
tracebacks, size-capped, credentials redacted) and the job's config, and puts
the job's code in the folder the way `gz sync --pull` does. It never
overwrites local edits silently: when both sides changed it asks, and
without a terminal it keeps yours. Then it opens `gz code` there with a
prepared message in the input box: find the root cause, make the smallest
fix, show the diff, then — with your approval — `gz sync --only` the file,
rerun with `gz jobs run … --follow` and report. Nothing is sent until you
press Enter, and gz code asks before every edit and command.

- The project comes from `--project`, else the folder's `.gz-sync.json`,
  else the one project that has the job.
- A run that is still going is never touched: if the latest run is running
  gz says so and stops (`gz jobs logs --job <id> --follow` follows it), and it
  reads logs only after the run's own record says it is over. Reading a live
  run's logs can mark it Failed.
- Pass `--compute`/`--rte` to put the exact rerun command in the task;
  otherwise gz code asks you for them.
- In `gz tui`, press `F` on a failed run (DE/ML → Job runs, or its log view)
  or on a job to do the same; the TUI steps aside for gz code and comes back
  when it exits.

## Assistant (`gz code`)

`gz code` is Groundzero chat that can work on the folder you run it in:
read and search files, edit them, and run commands — always with your
permission.

```bash
cd my-project
gz code                          # interactive session
gz code "why does the build fail?" --yes-reads   # one question, reads pre-approved
gz code -c <conversation-id>     # continue a conversation
```

- Reads and searches ask once per folder; every edit shows its diff first;
  every command asks (answer `a` to allow it for the session).
- Everything stays inside the folders you opened (see **Folders** below);
  secrets (`.env*`, keys, `.ssh/`, `.aws/`, git credentials) are always
  blocked.
- Each tool call is logged (tool, target, decision — never contents) to
  `~/.config/gz/audit.log`.
- `Ctrl+C` stops the current answer; `/new` starts over; `/exit` quits.

**Auto mode.** Shift+Tab turns on auto mode (when the chat server reviews
actions): reads and edits run at once, and a review decides each command,
script, protected edit, web fetch or Groundzero action from your messages,
blocking what you didn't ask for; three blocks in a row pause it until you
approve a prompt, and `/auto` shows the recent blocks and your rules.
Telling gz "I authorize it" after a block, even while it is working, lets
that same action run (on servers advertising
`features.actionReviewAuthorizations`).

**Folders.** Like Claude Code, gz works in the folder you start it in and
can move. Started in `~`, the assistant can read `~/code/<repo>` and move
into it; relative paths and commands then start there. The status line shows
the current folder once it differs from the start folder.

- The assistant's `change_directory` switches at once inside an open folder.
  Outside, gz asks: for this session, always (saved per profile as
  `code_allowed_dirs`, honored in later sessions), or no. An allowed folder
  stays open for the session.
- `/cd <path>` moves (your own action, so no prompt; `~` and relative paths
  work), `/add-dir <path>` opens another folder without moving, `/dirs` lists
  the open folders, `/dirs remove <n|path>` closes an added one.
- `/` and credential folders (`.ssh`, `.aws`, …) can never be opened; saved
  "always" rules for edits, commands and scripts hold only in the folder they
  were given in. Background subagents start in the current folder with the
  same open folders. Folder changes are in the audit log.

**Instructions.** gz sends instruction files with every message (read
again each time, so edits apply to the next one):

- your personal `~/.config/gz/instructions.md` (honors `XDG_CONFIG_HOME`);
  `/instructions edit` opens it in `$EDITOR`, creating it if needed;
- `GZ.md`, `AGENTS.md` and `CLAUDE.md` from the current folder up to your
  home folder (or the top open folder outside home), nearest first.

At most 8 files go out, each cut at 20,000 characters (marked as cut);
identical files go once, and secret files never. `/instructions` shows which
files are in use, their size, and whether one was cut.

**Memory.** The assistant remembers short facts about how you work ("Uses
snake_case for Python") and gets them with every message. It saves, updates
and deletes them itself, without a prompt, and each change shows in the
transcript (`Saved memory: …`) and the audit log (by id). Memories live in
`~/.config/gz/memory.json`, one list per profile: at most 200, each up to 500
characters, 20,000 in all. Text that looks like a key, token or password, or
holds control or invisible characters, is refused.

- `/memory` lists them with their ids; `/memory forget <id>` deletes one;
  `/memory clear` deletes all (after confirming with `/memory clear yes`).
- `/memory off` stops saving and sending memories (remembered per profile);
  `/memory on` turns it back on.

Folders, instructions and memory need a chat server that advertises
`features.localContext`; older servers get neither the extra tools nor the
context.

**Context and compaction.** Like Claude Code, the chat summarizes older
messages once the conversation reaches a size you choose, so long sessions
don't resend a huge prompt on every call. The footer counts down
(`120k until auto-compact`).

- `/autocompact` picks the threshold in 100k steps up to the model's window
  (or `/autocompact 200k`, `/autocompact default`); `gz code --auto-compact
  200k` does the same from the command line. The choice is remembered per
  profile (`code_auto_compact`); unset means the server default (85% of the
  window). GPT bills whole calls at about twice the price above 272K input
  tokens, so 200k suits GPT models.
- `/compact [what to keep]` summarizes the conversation so far now and shows
  how much smaller the resent history became.

**Subagents.** The chat can hand parts of a task to helper agents that run
in parallel; each shows under the input box with the model it runs on
(`Task(Check logs · analyst · Claude Haiku 5.5 · 200K)`), and `shift+↓`
opens its page. You can ask for a model or context size in plain words
("use haiku subagents"), or set defaults with `/subagents`:

- `/subagents` shows the settings, the profiles, what runs, and whether the
  chat server applies the settings.
- `/subagents model <mode>|main` picks the subagents' model (`/subagents
  model` alone opens a picker); `main` means the chat's own model.
- `/subagents effort <effort>|default` sets their reasoning effort (one the
  subagent model offers).
- `/subagents context <N>k|default` sets where each subagent summarizes its
  older messages, in 100K steps from 100K (capped at the model's window).
- `/subagents max <n>|off` caps subagents per run (nested ones count);
  `/subagents parallel <n>|off` caps how many run at once — extra ones wait
  as `queued` and start as others finish; interrupting the chat (esc) or
  `/subagents stop` drops the queued ones.
- `/subagents reset` clears them all; `/subagents on|off` lets the chat use
  subagents or not; `/subagents stop` ends background ones.

Settings are remembered per profile (`code_subagent_settings`) and sent only
to chat servers that advertise them; on a saved LLM connection the model and
effort are left out (connections choose their own).

**Costs, budgets and caps.** Groundzero prices every model call on the
server and reports each run's cost; gz adds those up where you work. The
status line shows this session's spend (main turns, inline subagents and
background subagents) and, when your admin set a budget, what is left of it:
`$1.24 this session · $38.10 of $50 left (30d)`. It turns yellow past 80%
and red when a limit is spent.

- The Groundzero budget is a rolling 30-day limit an admin sets for you or
  your group (`gz admin budgets`). gz refreshes it after each turn (cached
  for 20 s, in the background, so a failed refresh never holds up a turn),
  warns once past 80%, and once it is used up says so before you send —
  with who it is set for — since the server will refuse the turn. Press
  enter again to send anyway (the status may be out of date).
- Your own caps for gz code: `/budget session <usd>` (this session),
  `/budget day <usd>` (today, all gz code sessions on this machine),
  `/budget session|day off`, `/budget off`. They are saved per profile
  (`code_spend_caps`). At 80% gz warns once; at 100% the next turn waits
  for your choice: raise the cap (to twice as high), continue once (that
  turn and the background subagents it starts), or cancel (the message goes
  back into the input). Background subagents don't start past a cap until
  you choose; inline subagents run inside the chat's own turn, so they finish
  with it.
- `/budget` shows this session, today and this month (per model), your caps,
  and the Groundzero budget.
- One-shot and piped `gz code` can't ask, so past a cap it refuses with a
  message and exit code 3, before anything is sent.
- gz code's spend is kept per profile, local day and model in
  `~/.config/gz/code_spend.json` (dates, model names and dollar amounts
  only; honors `XDG_CONFIG_HOME`). Days are local time on macOS and Linux,
  UTC on Windows.
- `gz usage budget` prints the Groundzero budget, gz code's spend today and
  this month, and your caps (`-o json` for scripts); `gz usage` shows the
  site's tokens and cost. In `gz tui`, Home and Usage & cost show a budget
  meter and gz code's spend from the same ledger.

**Scripts (programmatic tool calling).** For work that would take many tool
calls — scanning a codebase, counting, batch edits — the assistant can write
one short Python or Node script that calls the file tools as functions and
prints only the answer. You see the full script and approve it first (`a`
allows scripts for the session); it then runs in the folder with `python3` or
`node`. Inside, `import gz` (Python) or `require("gz")` (Node) gives
`read_file`, `list_directory`, `find_files`, `search_files`, `edit_file`, and
`write_file` — the same sandbox, secret blocking, and audit log as single
calls, and every edit still shows its diff unless you allow all edits from
that script. Scripts can't run commands through `gz`, stop after 120 s by
default, and are limited to 500 helper calls.

Needs a chat backend with client tools enabled (`CHAT_CLIENT_TOOLS_ENABLED`).

### Skills and plugins

A skill is a folder with a `SKILL.md` (frontmatter `name` and `description`,
then instructions) plus any docs, templates or scripts it refers to by
relative path, as in the Agent Skills format. The assistant sees the name and
description of each skill that is switched on and loads the rest with the
`load_skill` tool when a request matches, or when you write `@skill-name`.
A plugin is a bundle of skills installed together; installing one installs
only its skills.

```bash
gz skills install github:owner/repo                 # everything in the repo
gz skills install https://git.example.com/team/skills.git#skills/sql@v1.2
gz skills install ./my-skills --skills sql,charts   # a subset from a folder
gz skills list                                      # status, source, bundle
gz skills disable charts                            # or enable
gz skills update                                    # reinstall from the source
gz skills folders add ~/team-skills                 # an extra folder of skills
gz plugins list                                     # installed bundles
gz plugins remove lakehouse
```

- Skills live in `~/.config/gz/skills/<name>/`. Extra folders are additive;
  each must hold skill folders and can't be a skill itself. A personal skill
  wins a name clash, then folders in the order you added them; the losers
  show as `shadowed`.
- Sources: `https://` git URLs (any host, optional `#subdir` and `@branch`,
  tag or commit), `github:owner/repo[/subdir][@ref]`, and local folders.
  ssh, `git@`, `file://` and `http://` are refused.
- Layouts: one skill, a folder of skills (`skills/*/SKILL.md` or
  `*/SKILL.md`), a Claude Code plugin (`.claude-plugin/plugin.json`) or a
  marketplace (`.claude-plugin/marketplace.json`, `plugins/*/skills/*`).
  Hooks, MCP servers, agents, commands and binaries in a bundle are listed
  as skipped: gz plugins carry skills only; tools come from your Groundzero
  MCP connections.
- In `gz code`: `/skills` lists them with on/off toggles; `/skills install`,
  `update`, `remove`, `enable`, `disable` and `folders` work as above.

**What is guaranteed.**

- Nothing in a skill runs at install or load. gz copies text files; git
  fetches run with hooks, submodules, LFS filters, extra transports and
  prompts off.
- Every install is checked and shown before anything is copied: source,
  resolved commit, skills, every file with its size, skipped parts and all
  findings. Symlinks, hard links, special files, binaries, files over 1 MB,
  skills over 10 MB or 500 files, and invalid `SKILL.md` files always fail.
  Installed files get no execute bit.
- Installs are confirmed: interactively, or with `--yes` when there are no
  warnings and `--yes --accept-warnings` when there are. HIGH findings
  always refuse.
- Installs are pinned: `~/.config/gz/skills.lock.toml` records the source,
  ref, commit and the sha256 of every file. gz re-hashes installed skills at
  every load; a changed one is disabled until you run `gz skills verify
  --accept <name>` (which re-scans it first) or reinstall it. Skills you
  make by hand are scanned at every load instead.
- Skills never grant permissions. Frontmatter such as `allowed-tools` is
  shown but ignored, and every read, edit and command still goes through
  gz's approval prompts.

**Team skills.** Site admins can publish skills (runbooks, house style,
data conventions) for everyone on the site, like workspace skills in
Databricks. They show in `gz skills list` and `/skills` with the source
`team:<site>.<client>` and who updated them last, and the assistant uses them like
your own skills (`load_skill`, `@skill-name`). This works only when the
site's chat server offers team skills (`features.teamSkills`).

```bash
gz skills team list                  # name, description, updated by/at, local status
gz skills team sync                  # sync now (gz code also syncs on its own)
gz skills trust api                  # turn on a team skill that has warnings
gz skills publish oncall             # admins: publish a personal skill
gz skills publish api --yes --accept-warnings
gz skills unpublish oncall --yes     # admins: remove it for everyone
```

- gz code syncs in the background when it starts and when you open
  `/skills`, at most every 10 minutes per site and environment, and never holds up a turn.
  Offline or storage trouble just keeps the copies you have.
- Copies live in `~/.config/gz/team-skills/<site>.<client>/<name>/` (one folder per site and environment, so `preview` on dev and on stage stay apart). Personal
  skills win a name clash, then team skills, then extra folders; the losers
  show as `shadowed`.
- Every file is checked against the sha256 in the server's index, and the
  whole skill against the index's skill sha256. A download that doesn't
  match, or that fails the install checks (unsafe paths, binaries, sizes,
  invalid `SKILL.md`), is never written, and the last good copy stays.
  Files are written without execute bits and pinned in
  `team-skills/<site>.<client>/.gz/lock.toml`, like installs.
- Trust: clean skills turn on as soon as they sync. A skill with HIGH
  findings is blocked, shown with the rule, and never loaded. A skill with
  MEDIUM findings stays off until you trust that exact version, with
  `gz skills trust <name>` or from `/skills` (enter on it, or `/skills
  trust <name>`); the prompt shows the findings and which files were added,
  changed or removed since the version you trusted before. Any new version
  with warnings asks again; a version without warnings needs no trust.
- A copy changed on disk is disabled; the next sync restores the team's
  version. Skills removed from the site are removed from your machine.
- Publishing runs the same checks and scanner locally before uploading
  anything: HIGH findings refuse, MEDIUM findings need `--yes
  --accept-warnings` (or a yes at the prompt), and the site's limits (100
  skills, 50 files and 1 MB per skill, 256 KB per file by default) apply.
  Only site admins can publish or unpublish; anyone else gets a plain
  "only site admins can publish team skills".

**The scanner is a heuristic.** It looks for risky shapes in all text:
HIGH (blocks) for downloads piped into a shell, decoded payloads that run,
reverse shells, reading credential stores (`~/.ssh`, `~/.aws/credentials`,
keychains, browser data, `.env` sent anywhere), exfiltration endpoints,
persistence (cron, launch agents, systemd, shell rc files), `sudo`, disk or
home wipes, instructions to act behind your back or ignore rules, and hidden
text (zero-width or bidi characters, instructions in HTML comments); MEDIUM
(warns) for scripts, network calls, `chmod +x`, package installs and reading
environment variables. It can miss things, so only install skills from
sources you trust; the guarantees above don't depend on it.

## TUI

`gz tui` opens a full-screen console mirroring the app: Chat, Lakehouse,
DE/ML, Agents, Designer, Workspaces, Connections, Files, Catalog, Schedules.

- `↑↓` move through the sidebar tree, `→`/`Enter` open, `←` back, `Tab`
  switches sidebar ↔ content, `Ctrl+K` (or `:`) jumps anywhere.
- Chat: pick a conversation or press `n`, type, `Enter` sends, replies
  stream in with Markdown (tables, code, lists) rendered; `Esc` cancels.
- Tables: `/` filters live, `[` `]` scroll columns, `r` reloads.
- `F` on a failed job run (or a job) hands it to gz code to fix; see
  [Fix a failed run](#fix-a-failed-run-gz-fix).
- Mouse clicks and scrolling work; `?` shows every key; `q` quits.

## Reference

`gz --help` lists every command; each group has its own help
(`gz sql --help`). Shell completions: `gz completion bash|zsh|fish`.

| Service key | What it talks to              | URL source          |
|-------------|-------------------------------|---------------------|
| lakehouse   | warehouses, tables, backups   | derived             |
| etl         | jobs, runs, lineage           | derived             |
| files       | file manager                  | derived             |
| agents      | agent builder API             | derived             |
| chat, auth  | chat, MCP tokens              | derived             |
| data, mlops, etlprojects, datasets, dashboards, visualizations, filters, workspaces, schedules, rtes, permissions, connections, admin, adminqueries, notifications, logexports | gateway APIs | read from the web app at `gz auth login` |

Config layers: flags beat env (`GZ_PROFILE`, `GZ_TOKEN`, `GZ_CLIENT`,
`GZ_SITE`, `GZ_DOMAIN`) beat the config file.

## Troubleshooting

| Symptom                   | Fix                                              |
|---------------------------|--------------------------------------------------|
| `no client configured`    | run `gz configure`                               |
| `401/403` from a provider | `gz auth login`; check `gz auth status`          |
| install script 404s       | releases page directly, or check your network    |
| `gz auth mcp-token` port busy | `--port 0` picks a free port                  |
| a service shows ✗ in `gz doctor` | check VPN/network; re-run `gz auth login` |

## Development

```bash
make build | make test | make check   # check = fmt + clippy -D warnings
make install-local
```

Releases: push a `v*` tag and CI builds Linux, macOS (ARM + Intel), and
Windows, attaches them with checksums, and generates notes.

## License

Proprietary. Copyright (c) 2026 Groundzero. All rights reserved. The source
is visible for transparency only; see [LICENSE](LICENSE) for the limited
right to install and run the official releases with a Groundzero account.
