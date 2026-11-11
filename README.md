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

## Query cost preview (`gz sql estimate`)

Before a lakehouse query runs, gz can say how much data it will scan.
Nothing runs and no session starts.

```sh
gz sql estimate "SELECT * FROM DemoDB.user_events WHERE eventType = 'click'"
gz sql estimate @report.sql --warehouse GZWarehousePROD -o json
```

- Where the lakehouse provider plans estimates (`POST /lakehouse/scanestimate`),
  each table's files are pruned with the query's filters (partition columns
  and column bounds) and the answer reads "about 1.2 GB". Tables with data
  policies are counted whole.
- Otherwise gz adds up the current snapshot of every table the query reads
  (CTEs, aliases, joins and subqueries are followed) and says "up to 9.0 GB":
  an upper bound.
- Bytes are whole data files; Spark reads only the columns it needs, so the
  real scan can be smaller. Tables named without a database are listed as
  not sized.
- No money is shown: Groundzero has no price for a lakehouse query (per byte
  scanned or per session hour), only for job runs.

**A scan limit.** `--max-scan 10GB` on `gz sql run`, `submit`, `run-file`
or SQL piped into `gz sql` refuses a query estimated to scan more, before
anything runs, and prints the estimate and the override (`--max-scan 12GB`
or `--max-scan off`). `gz sql max-scan 10GB` saves the limit on the profile
(`sql_max_scan`), `gz sql max-scan off` removes it, and `gz sql max-scan`
shows it. With a limit set, a query gz cannot estimate is refused too.

## Table history (`gz diff`, `gz bisect`)

Every write to a lakehouse table is a commit that keeps the table as it was
before. gz reads that history, so "what changed?" and "when did this break?"
are one command each, answered from the data itself.

```bash
gz table versions sales.orders                       # snapshots, branches and tags
gz diff sales.orders                                 # the last commit, from the history
gz diff sales.orders --rows                          # …row by row
gz diff sales.orders --from 2026-09-01 --key order_id
gz bisect sales.orders --where "amount < 0"          # the commit that broke it
```

The table is `database.table`; the warehouse is `--warehouse`, else the one
`gz sql` remembers. A point in the history is a branch, a tag, a snapshot
id, `current`, `previous`, or a UTC date or time (`2026-09-16`,
`2026-09-16T08:00`: the snapshot that was current then).

**`gz diff`** compares two snapshots (`--from`, default `previous`; `--to`,
default `current`).

- Without `--rows` it reads the history only, which is instant and starts
  nothing: the commits in between, what each recorded (operation, records
  added and deleted, the Spark application that wrote it), and the record
  and file totals before and after.
- `--rows` compares the rows themselves on a SQL session: columns added,
  dropped or retyped, rows added and removed, and a few sample rows
  (`--sample N`, 0 for none). Whole rows are compared, so a changed row
  counts as one removed and one added.
- `--key order_id` (or `--key a,b`) matches rows by those columns, so gz
  also tells which rows changed, how many rows changed in each column, and
  shows the values before and after. A key that repeats is refused, since
  the counts would be wrong.
- `-o json` gives all of it as one value for a script or a CI check.

**`gz bisect`** finds the snapshot where a table started failing a check. A
check is SQL that returns the bad rows; a snapshot fails when the check
returns any. Give it as `--where "amount < 0"`, or as a whole query that
reads the table as `{table}`:

```bash
gz bisect sales.orders --check "SELECT order_id FROM {table} GROUP BY order_id HAVING count(*) > 1"
gz bisect sales.orders --check @checks/orders.sql --good 2026-09-01 --code
```

gz tests the newest snapshot (`--bad`) and the oldest one kept (or
`--good`), then halves the commits in between, so a history of a thousand
commits takes about ten queries. It prints the first failing snapshot, when
it was committed, what that commit changed, the Spark application that
wrote it, and the `gz diff` command that shows the change. `--code` opens
`gz code` with that evidence as a prepared message. It finds one change
from passing to failing: if the data broke, was repaired and broke again,
give `--good` a snapshot after the repair.

Row comparisons and checks run on the session `gz sql` uses (`--compute`,
`--keep-session`); gz stops it afterwards unless you keep it. Both commands
only read: they never change the table. Snapshots that have been expired
can't be compared or searched.

## Where a number comes from (`gz explain`)

`gz explain` traces a figure on a dashboard back through the platform, so
you can see what it is computed from and whether its inputs have moved.

```bash
gz explain dashboard Executive-Working-Capital-Overview     # every figure on it
gz explain dashboard 1114 --viz WC-YearChart --at 2026-09-16
gz explain viz 1717
gz explain dataset WC-YeartoQuarter --depth 2
gz explain sql "SELECT sum(total) FROM DemoDB.INVOICE_EXTRACTS" --warehouse GZWarehousePROD
```

Dashboards, visualizations and datasets are named by id or by name. The
trace reads top to bottom:

- **Figures**: each visualization's measures and the dataset SQL that
  computes them (`Revenue = ROUND(SUM(revenue27), 0)`), what it is drawn
  by, and the filters its dataset query uses with their starting values.
  Charts plot dataset columns as they are, so the aggregation is in the
  SQL; pivot tables, which aggregate in the chart, say so.
- **Datasets**: the definition, with SQL shown (long SQL is cut; `-o json`
  has all of it), the connection and the warehouse it points at, and the
  lakehouse tables it reads. gz finds the tables in the SQL itself: CTEs,
  aliases, quoted names, subqueries and joins. For a dataset that is code,
  it reads the SQL written in the code's strings.
- **Tables**: for each table, the snapshot it was at (id, commit time,
  records, the Spark application that wrote it), the run that most likely
  wrote that snapshot with its status, times, and how sure the match is,
  the code version that run used, and the tables upstream (`--depth N`
  levels, default 1).
- **`--at TIME`** (a UTC date or time) uses the snapshot each table was at
  then and lists the commits made to it since, with the record count
  before and after, so you can tell whether the inputs moved.
- **What gz cannot tell**: gaps stated plainly, such as the Designer
  keeping no history of definitions.

How the run is matched. The platform does not record the run id on a
table commit, so gz matches by evidence and says which it used:

| Match | Meaning |
|---|---|
| `recorded` | the ETL lineage store recorded this snapshot id against the run |
| `strong` | the only run going at commit time, and the commit's Spark application started inside it (standalone Spark puts its start time in the application id) |
| `likely` | the only run going at commit time, with nothing more to confirm it |
| `ambiguous` | several runs fit equally; gz shows the one that started last and lists the others |

A run on a different engine from the commit (DuckDB against Spark) is
ruled out. A run whose end was never recorded counts as going for at most a
day. The snapshot's `app-name` is whatever the job's code named its Spark
session, so it only breaks ties. The code version comes from the job's
lineage (`version`, code hash, when it was saved, and where the code is
kept).

`gz explain` only reads. It never calls the routes that can settle a live
run (`current-run`, run logs). A table you cannot read is reported as
such, and the rest of the trace goes on.

### `-o json`

One object:

```jsonc
{
  "subject": {"kind": "dashboard", "id": "1114", "name": "Executive-Working-Capital-Overview"},
  "at": "2026-09-16T00:00:00",            // null for now
  "figures": [{
    "id": "1717", "name": "WC-YearChart", "chartType": "ColumnChart", "datasetId": "1873",
    "dimensions": ["Year"],
    "measures": [{"column": "Revenue", "expression": "ROUND(SUM(revenue27), 0)", "chartAggregation": null}],
    "chartNote": null                       // e.g. "shows the first row of the dataset only"
  }],
  "datasets": [{
    "id": "1873", "name": "WC-YeartoQuarter", "kind": "sql",   // or "code"
    "connections": ["GZLakehouseProd"], "runtime": null,
    "sql": "SELECT …", "code": null,        // whole text
    "filters": [{"id": "1273", "name": "WC-Region", "filterType": "multiSelect",
                 "column": "supplier_region", "defaults": ["APAC"],
                 "placeholders": ["{{WC-Region.value}}"]}],
    "connectionDetails": [{"name": "GZLakehouseProd", "type": "GZLAKEHOUSE",
                           "warehouse": "GZWarehousePROD", "database": "DemoDB"}],
    "tables": ["GZWarehousePROD.DemoDB.bi_working_capital"],
    "unresolved": []                        // sources gz could not place, e.g. "read_parquet(...)"
  }],
  "tables": [{
    "warehouse": "GZWarehousePROD", "database": "DemoDB", "table": "bi_working_capital",
    "readBy": ["1873"],
    "snapshot": {"id": "…", "committedAt": "…", "operation": "overwrite",
                 "addedRecords": 120000, "totalRecords": 120000, "appName": "S3",
                 "sparkAppId": "app-…", "engine": "spark", "names": ["current", "branch main"]},
    "run": {                                // null when no run fits
      "run": {"executionId": "…", "jobId": 1825, "jobName": "…", "datasetId": null,
              "datasetName": null, "projectId": 1405, "projectName": "…",
              "status": "succeeded", "startedAt": "…", "completedAt": "…", "provider": "spark"},
      "certainty": "strong",                // recorded | strong | likely | ambiguous
      "reasons": ["…"], "alternatives": []
    },
    "codeVersion": {"versionId": 1399, "number": 6, "codeHash": "…",
                    "compiledOn": "…", "sourceUri": "s3://…"},
    "upstream": [{"source": "db.t", "target": "db.t2", "operation": "…",
                  "eventTime": "…", "correlationId": "…"}],   // null when lineage can't be read
    "changesSince": {"commits": [/* snapshots, newest first */],
                     "recordsThen": 98, "recordsNow": 100},   // --at only
    "problems": []                          // what could not be read for this table, and why
  }],
  "gaps": ["…"]
}
```

## Impact (`gz impact`)

Before you change data code, see what depends on it. gz follows the lineage
the platform records (the same graph as the catalog's lineage pages): the
tables written from a table, the jobs and datasets that read it, the
visualizations and dashboards on those datasets, and the agents grounded on
ontology models over them.

```bash
gz impact sales.orders                    # everything downstream, 3 hops
gz impact sales.orders.amount --depth 5   # one column
gz impact sales.orders -o json            # for a script
gz impact --changed etl/clean_orders.py   # what this job's tables feed
gz impact --git origin/main --fail-on dashboards,agents   # for CI
```

- Each entry has its kind, name, id, owner, and how it depends: a direct
  read, or via a table, dataset or ontology model. `--depth` (1-10) limits
  the table hops; cycles in the lineage end the walk.
- A column follows column lineage where it exists. Where a hop has only
  table-level lineage, the entry says the column-level answer is not known
  instead of guessing.
- `--changed <file>...` and `--git <base-ref>` (files changed since the
  branch left that ref) map files to jobs the way `gz sync` does
  (`gz-sync.toml`, or one job per `.py` file), then report what the tables
  those jobs write feed. Files that map to no job are listed as unmapped.
  `--dir` is the synced folder (default `.`).
- Exit codes: `0` nothing protected is affected, `2` dependents of a
  `--fail-on` kind exist (a short summary goes to stderr), `1` gz could not
  tell. Kinds: `tables`, `jobs`, `datasets`, `visualizations`, `dashboards`,
  `agents`.

The synced folder's `gz-sync.toml` can hold the CI settings:

```toml
[impact]
fail_on = ["dashboards", "agents"]   # used when --fail-on isn't given
project = 1603                       # else the folder's last `gz sync`

[impact.jobs]                        # job ids by name, so a run with only
"clean-orders" = 2233                # an API key needn't list the project
```

**With an API key.** Lineage lives in the ETL provider, which accepts API
keys, so tables, jobs, datasets and dashboards all work with `GZ_API_KEY`.
Visualizations (Designer) and agents (agent builder) need a signed-in user;
with only a key gz skips them and says so. Mapping job names to ids reads
the project's job list, which also needs a user, so list the jobs under
`[impact.jobs]` for a key-only CI. A GitHub Actions check:

```yaml
name: impact
on: pull_request

jobs:
  impact:
    runs-on: ubuntu-latest
    env:
      GZ_CLIENT: stage
      GZ_SITE: preview
      GZ_DOMAIN: groundzerodev.cloud
      GZ_API_KEY: ${{ secrets.GZ_API_KEY }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0          # --git needs the base branch's history
      - name: Install gz
        run: |
          curl -fsSL https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Check what the change affects
        run: gz impact --git origin/${{ github.base_ref }} --dir jobs --fail-on dashboards
```

In `gz code`, editing a file that maps to a job shows the same summary in
the permission prompt ("Affects 2 dashboards, 3 datasets downstream (gz
impact sales.orders)"), or as a notice in auto mode and accept-edits mode,
and auto mode's review sees it too. The lookup takes at most 3 seconds, is
remembered per job for the session, and never holds up or fails the edit.

## Try a job change on a branch (`gz jobs try`)

Run a changed job against the real data without changing the real tables,
look at what it wrote, then publish it or throw it away:

```bash
gz jobs try build-oee --compute 1003 --rte gz-rte-spark-pytorch:1.0.0
gz jobs publish 1324 --project 1138 --branch try-build-oee-20261111-093005   # asks first; --yes skips
gz jobs discard 1324 --project 1138 --branch try-build-oee-20261111-093005
```

- `gz jobs try` finds the lakehouse tables the job writes from its lineage
  (`--table warehouse.database.table` adds one it doesn't show), makes a
  branch of each at its current snapshot (`--branch`, default
  `try-<job>-<UTC time>`), and runs the job with every write going to those
  branches; reads of other tables still see main. It runs the job's file in
  the `gz sync` folder (`--dir`, default `.`) when the folder maps the job,
  else the saved code. It follows the run, prints the `gz diff` summary of
  each table from main to the branch, and the publish and discard commands.
- The ETL provider refuses the run, and gz removes the branches again, when
  the code writes a table without the branch, writes outside the lakehouse,
  or builds SQL or table names at run time where the writes can't be seen.
  Only single Spark jobs can be tried. Running local code needs edit
  permission on the job, though the code isn't saved.
- `gz jobs publish` moves each table's main to its branch and removes the
  branch. Main must not have moved on since the branch was made; when it
  has, nothing is published and you try again on a new branch. Each table is
  published on its own, so a failure part way says which were done.
- `gz jobs discard` drops the branches; main is never touched.
- Tables with row filters, column masks or other data policies can't be
  tried on a branch.
- `-o json` gives each command's result as one value.

This needs the lakehouse provider and ETL provider updates deployed; until
then gz says so and changes nothing.

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
- `/export` saves the conversation as Markdown (`gz-<date>-<first words>.md` in the current folder, never replacing a file); `/export <path>` picks the file inside an open folder, `/export clipboard` copies it. A turn that failed keeps its error, and a resumed conversation exports every earlier message, not just the ones `/resume` showed.

**Granting access.** `/permissions` lists what gz does without asking and
`/permissions revoke <id>` takes one back. `/permissions allow <what>` grants
something before gz ever asks, saved for the folder you are in and counted
at once, in a running turn too:

| `/permissions allow …` | gz stops asking about |
|---|---|
| `edits` | every edit in this folder |
| `scripts` | scripts in this folder |
| `reads <folder>` | reads and searches in that open folder |
| `pages <host>` | web pages from that host |
| `command cargo test` | that exact command line |
| `command git status *` | `git status` with any arguments after it |

A rule ending in `*` covers one program and its arguments only: a line that
chains, pipes, redirects or substitutes (`;` `&&` `|` `>` `$(…)`) is never
covered and gz asks as usual.

**Auto mode.** Shift+Tab turns on auto mode (when the chat server reviews
actions): reads and edits run at once, and a review decides each command,
script, protected edit, web fetch or Groundzero action from your messages,
blocking what you didn't ask for; three blocks in a row pause it until you
approve a prompt, and `/auto` shows the recent blocks and your rules.
A block is not final. `/allow` lets the action that was just blocked run:
that exact action, once, with no second review, and gz is asked to run it
again (`/allow <n>` picks an earlier block by its number in `/auto`;
`/auto allow-blocked <n>` allows it from now on). Telling gz "I authorize
it" in your own words works too, even while it is working, on servers
advertising `features.actionReviewAuthorizations`: the model runs the action
again and the review counts your message. The review reads your answers to
gz's questions, your plan feedback and what you type when denying a prompt
as your words too.

**Query scan cap.** In auto mode, a `gz sql` command the model runs (with
`--sql`, `run-file`, `< file.sql`, a here-document, or `echo`/`cat` piped in)
is estimated first (see [Query cost preview](#query-cost-preview-gz-sql-estimate)).
Above the cap (10 GB unless you change it) it is blocked without a review,
and `/allow` lets it run once. Within the cap the review decides and is told
the estimate; when gz can't estimate it (the shell builds the query, the
lakehouse doesn't answer within 10 seconds), the review decides and is told
that the scan wasn't checked. `/auto` shows the cap; `/auto scan cap 20GB`,
`/auto scan cap off` and `/auto scan cap default` change it for the session
and save it on the profile (`code_scan_cap`).

**Folders.** gz works in the folder you start it in and can move. Started in `~`, the assistant can read `~/code/<repo>` and move
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

**Model.** `/models` chooses what answers next (a Groundzero model or one of
your LLM connections) and `/effort` its reasoning effort. The line under the
input box always names the model in use, a change prints what it replaced,
and each answer's summary line ends with the model that wrote it. A model
chosen while gz is working takes over from your next message.

**Shells.** Commands the assistant leaves running in the background, and
shell commands you run yourself, share one list.

- `/shells` lists the background commands: id, state (running for how long,
  exit code, stopped), the command and the last line it printed.
  `/shells <id>` prints the end of its output (the last 200 lines);
  `/shells stop <id>` and `/shells stop all` stop them. While any run, a
  line under the input box says so (`1 command running · /shells`). The
  assistant is told when one of its commands ends, including one you stopped.
- `!command` runs a shell command yourself in the current folder (`!git
  status`), with no permission prompt, even while gz is working. Its output
  and exit code print when it ends. `esc` leaves a long one running in the
  background (it then shows in `/shells`); `ctrl+c` stops it.
- Your commands are never sent to the assistant by themselves. Your next
  message carries what you ran since the last one (command, exit code and
  the end of the output, at most 10 commands and 20,000 characters, with
  credentials redacted) as context, and the message shows `Shared 1 shell
  command you ran`. A message typed while gz is working carries it too; a
  note to the running turn holds 2,000 characters in all, so it takes the
  newest commands and the end of their output, says when output was cut,
  and leaves older commands that don't fit for your next message.
  `!commands` are in the audit log, marked `user`.

**Rewind and diff.** Just before gz's own tools change a file (edits,
writes, notebook edits, and writes from a script's `gz` helpers, background
subagents' included), gz keeps what the file held, tagged with your message.

- `/rewind` lists your messages that led to file changes, newest first, with
  how many files each changed. Choosing one puts every file gz changed since
  that message back as it was before it, and deletes files gz created. If a
  file changed after gz last wrote it (by you or a command), gz lists it and
  asks: put it back too, or leave it and put back the rest. Your next
  message tells the assistant which files went back (`Told gz you rewound 2
  files`), so it re-reads them.
- Only files go back; the conversation stays as it is. Changes made by
  commands (the assistant's or your own `!commands`) are not tracked.
- `/diff` shows every file gz changed in this conversation as a diff from
  before gz first touched it to now; `/diff <path>` shows one. Long diffs
  open in the full-screen view (`esc` returns); very long ones are cut and
  say so.
- The history is kept per conversation in `~/.config/gz/code_edits/`
  (owner-only; honors `XDG_CONFIG_HOME`), so it survives `/resume` and
  `gz code -c`. Files over 5 MB are noted but not copied, and `/rewind`
  refuses them. Histories of conversations not edited for 30 days are
  deleted when `gz code` starts.

**Sandbox for commands.** gz's file tools only reach the open folders, but a
command or script you allow (by a prompt, a saved rule or auto mode) runs as
you. So commands and scripts the assistant runs, its subagents' included,
run under OS limits; your own `!commands` do not.

- **Writes** only in the open folders, the temp folders (`$TMPDIR`, `/tmp`),
  `/dev/null` and the terminal, the download and build caches builds and
  installs need (`~/.cargo/registry`, `~/.cargo/git`, `~/.npm`, `~/.cache`,
  `~/Library/Caches`, `~/.gradle`, `~/.m2`, `~/go/pkg`, `~/.pnpm-store`,
  `~/.yarn/berry`) and folders you add. Folders that hold programs or files
  your shell runs later (`~/.cargo/bin`, `~/.cargo/env`, `~/.rustup`,
  `~/go/bin`) stay closed.
- **Git hooks** are closed too: a hook runs at your next commit, outside
  the sandbox, and shows in neither `git status` nor diffs. That covers the
  open folder's repo and repos nested up to four folders below it
  (`node_modules`, `target`, `.venv`, `vendor` and dot-folders are not
  searched), and a worktree's or submodule's git dir when it lies in an
  open folder.
- **Git config** stays writable, since `git push -u`, `git remote add` and
  `git config` need it. But gz saves each protected repo's config before a
  command or script and checks it after: if the command added or changed a
  key that makes git run a program (`core.hooksPath`, `core.fsmonitor`,
  `core.sshCommand`, `core.pager`, `core.editor`, `alias.*` running `!…`,
  `filter.*`, `diff.*.textconv`, `credential.helper`, `include.path` and
  the like), gz puts the old file back and tells you and the assistant which
  key it reverted. Remotes, branches and `user.*` changes stay, unless the
  same command also set such a key: the whole file goes back, and the
  assistant is told which of its other changes were undone so it can redo
  them. If you meant it, run the command yourself with `!command`.
- **New repos** a command creates (`git init`, `git clone`) are checked when
  it ends: their hooks other than git's `*.sample` templates are deleted
  and those keys removed from their config; the rest of the config stays.
- **Reads** everywhere except the credential folders `~/.aws`, `~/.gnupg`,
  `~/.kube` and `~/.docker`.
- **Network** is on, because installs, fetches and `git push` need it.
  `/sandbox network off` blocks it except localhost.
- **Other apps:** on macOS a command can't send Apple events, so it can't
  ask Terminal, Finder or System Events to act for it.
- When the sandbox stops a command, the assistant is told so and asks you
  rather than trying another way. Commands that write elsewhere fail until
  you widen it: `go install` (`~/go/bin`), `cargo install` and `rustup`
  toolchain installs (`/sandbox allow-write ~/.cargo/bin` or
  `/sandbox allow-write ~/.rustup`), global `npm -g` installs, `brew`, and
  editing your dotfiles or `~/.config`.

What it does not stop: `~/.ssh` stays readable so git over SSH works, so
with the network on a command could read a key and send it out; local
services' sockets (ssh-agent, Docker, databases) stay reachable; and the
project's own files, `Makefile` and `package.json` scripts included, are
writable by design, so read what the assistant changed before you run it.

`/sandbox` shows the status: on or off, how it is applied, the writable
folders and network. `/sandbox allow-write <path>` adds a folder,
`/sandbox allow-read <path>` opens a path in a credential folder,
`/sandbox forget <n>` removes one you added, and `/sandbox on|off` and
`/sandbox network on|off` switch it. Settings apply from the next command
and are saved on the profile. On macOS it uses `sandbox-exec`; on Linux,
bubblewrap (`bwrap`) when it is installed. On Linux without bubblewrap and on
Windows commands run without limits, and gz says so once per session (in a
one-shot `gz code "<prompt>"`, on stderr unless `-q`).

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

**Context and compaction.** The chat summarizes older
messages once the conversation reaches a size you choose, so long sessions
don't resend a huge prompt on every call. The footer counts down
(`120k until auto-compact`).

- `/autocompact` picks the threshold in 100k steps up to the model's window
  (or `/autocompact 200k`, `/autocompact default`); `gz code --auto-compact
  200k` does the same from the command line. The choice is remembered per
  profile (`code_auto_compact`); unset means the server default (85% of the
  window).
- **Models with two prices.** Some models bill a whole call at a higher
  long-context rate once its prompt passes a size (272k tokens for the GPT
  models). You choose which one to use, and gz never picks for you: when a
  conversation on such a model reaches the point where staying on the
  standard price means compacting, gz asks once before sending your
  message — *Standard price · auto-compact at 200k* or *Full window ·
  long-context price past 272k (2× input, 1.5× output)*. The answer is
  remembered per profile (`code_long_context`) and applies only to models
  with two prices; `/autocompact` shows both choices and changes it.
  One-shot runs (`gz code -p`) follow the saved answer.
- `/compact [what to keep]` summarizes the conversation so far now and shows
  how much smaller the resent history became.

**Subagents.** The chat can hand parts of a task to helper agents that run
in parallel; each shows under the input box with the model it runs on
(`Task(Check logs · analyst · Claude Haiku 5.5 · 200K)`), and `shift+↓`
opens its page. A subagent started by another subagent is indented under
it, and its tab comes right after its parent's, marked `↳`. You can ask
for a model or context size in plain words ("use haiku subagents"), or set
defaults with `/subagents`:

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

**Scripts and CI (`-o json`).** A one-shot `gz code "<prompt>"` with
`-o json` (or `-o yaml`) is script mode: it never waits for input, prints
progress on stderr (nothing with `-q`), and prints one object on stdout
when the run ends:

```bash
gz code "fix the failing test" --auto -o json > run.json
gz code "and add a changelog entry" -c "$(jq -r .conversationId run.json)" --auto -o json
git diff | gz code -o json    # no prompt argument: everything piped in is the prompt
```

```json
{
  "conversationId": "b42bf8ee-8dcf-4359-bfae-20fd509fa6c4",
  "model": "sonnet55",
  "answer": "All done.",
  "toolCalls": [
    {"tool": "read_file", "summary": "read_file hello.txt", "decision": "preapproved", "isError": false},
    {"tool": "run_command", "summary": "run_command git push", "decision": "blocked", "isError": true,
     "reason": "unattended run: this action needs the user's approval and no one is at a terminal to give it. …"}
  ],
  "usage": {"inputTokens": 100, "outputTokens": 20, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0, "costUsd": 0.0042},
  "durationMs": 5210,
  "status": "completed"
}
```

- `answer` is what the model wrote after its last tool call; `model` the
  model it ran on (`null` when the model list couldn't be loaded).
- `toolCalls[].decision` is the audit log's word: `preapproved` (ran
  without asking: a saved rule, `--yes-reads`, or a tool that needs no
  permission), `auto` (auto mode's review allowed it), `blocked` (refused,
  with a `reason`), or `server` (a tool the chat server ran itself).
- `subagents` (`[{name, status}]`) appears when subagents ran; `error` when
  `status` is `failed`. `status` is `completed`, `failed` or `interrupted`.
- Exit codes: `0` completed; `1` failed (the report's `error` says why);
  `3` a `/budget` cap is spent, nothing sent; `4` completed, but an action
  was refused because it needed approval (no one to ask, or auto mode's
  review blocked it); `130` interrupted (Ctrl+C).

An action nothing allowed ahead of time is refused at once, never waited
on: the model is told no one was there to approve it and carries on. Allow
what a CI job needs before it runs:

- `--auto`: auto mode's review decides each command, script, protected
  edit, web fetch and Groundzero action from your prompt; reads and edits in
  the folder run at once. Needs a chat server with `features.actionReview`;
  anything the review would ask a person about is refused.
- `--yes-reads`: every read and search in the folder.
- Saved rules in `$XDG_CONFIG_HOME/gz/permissions.toml`, written by
  `/permissions allow` or by hand, keyed by the absolute checkout folder:

  ```toml
  [workspaces."/home/runner/work/app/app"]
  edits = true
  commands = ["cargo test"]
  command_starts = ["git diff"]
  ```

`-c <conversation-id>` continues a conversation, so a script can ask a
follow-up; `--continue` continues the last one started in the folder.

**Hooks.** Your own commands that run around the assistant's tool calls.
Put them in `~/.config/gz/hooks.toml` (honours `XDG_CONFIG_HOME`) or, for a
project, in `.gz/hooks.toml` in the folder you open:

```toml
# Format Rust files after the assistant edits them.
[[after_tool]]
tools = ["edit_file", "write_file"]
path = "*.rs"                       # a glob; with a "/" it is a path from the current folder
command = 'rustfmt "$GZ_HOOK_PATH"'

# Refuse commands that push.
[[before_tool]]
tools = ["run_command"]
command = '''
if jq -r .input.command | grep -Eq 'git push'; then
  echo "pushing is for humans" >&2; exit 2
fi
'''
timeout = 5                         # seconds; 30 by default

[[turn_end]]
command = "cargo check -q 2>&1 | tail -5"
```

- `before_tool` runs after you allowed a call and before it runs (a denied
  call runs no hook). Exit 2 blocks the call: its stderr (else stdout) is
  what the model gets back, and you see a notice. Any other non-zero exit
  warns and carries on.
- `after_tool` runs after the call. Its output shows dimmed; on exit 2 its
  stderr is added to the result the model gets (a lint or test failure
  after an edit, say).
- `turn_end` runs when a turn completes; its output shows.
- Each hook gets JSON on stdin: `{event, tool, summary, input, cwd}`, plus
  `result: {isError, output}` for `after_tool`; and `GZ_HOOK_EVENT`,
  `GZ_HOOK_TOOL` and `GZ_HOOK_PATH` (the file the call targets) in its
  environment. `tools` and `path` narrow which calls a hook runs for.
- Hooks run as you, unsandboxed, through `sh -c` in the session's current
  folder, around background subagents' calls and script helper calls too.
  One that runs past its timeout is stopped. Every run is in the audit log
  (decision `hook`, the outcome only).
- A project `hooks.toml` comes from the repository, so its hooks run only
  after you trust that exact file: gz shows them and asks once, and asks
  again when the file changes. Trust is kept by content hash in
  `~/.config/gz/hooks-trust.json`.
- `/hooks` lists them with their source, trust and last result;
  `/hooks off|on` switches them for the session; `/hooks trust` reviews
  the project file again. A broken `hooks.toml` is reported by name and gz
  starts without it.

**Local MCP servers.** The interactive `gz code` can use MCP servers that run on your machine.
List them in
`~/.config/gz/mcp.toml` (honors `XDG_CONFIG_HOME`):

```toml
[servers.files]
command = "npx"
args = ["-y", "@scope/server", "/Users/me/notes"]

[servers.tracker]
command = "tracker-mcp"
args = ["stdio"]
env = { TRACKER_TOKEN = "${TRACKER_TOKEN}" }
timeout_seconds = 120   # per request; default 60, at most 600
```

- `${NAME}` in `command`, `args` and `env` is filled from gz's own
  environment when the server starts, so the file needs no secrets. Server
  names are letters, digits, `_` and `-`.
- gz talks to each server over stdio. A server starts the first time the
  model uses it and stops when gz exits. Its stderr goes to
  `~/.config/gz/mcp-logs/<name>.log` (capped at 1 MB), never the screen.
- The model lists the servers' tools without asking (`local_mcp_tools`).
  Each call (`local_mcp_call`) asks first, like a command, with the tool's
  arguments: "for this chat" allows that server's tool for the chat, and
  "always" saves a rule for that tool in this folder, listed and revocable
  in `/permissions`. Auto mode reviews every call; plan mode refuses them.
  Hooks run around calls too: name `local_mcp_call` in a hook's `tools`.
- Text results go back to the model, cut at 40,000 characters; images and
  other content are described in one line.
- If a server crashes, gz starts it again once on the next use; after a
  second crash it stays failed until `/mcp restart <name>`.
- `/mcp` lists the servers, their state (not started, running, crashed or
  failed with the reason) and tool counts, with env values masked. A bad
  `mcp.toml` is reported with the file and the problem when gz starts, and
  gz runs without those servers.

Needs a chat server that lists `local_mcp_tools` and `local_mcp_call` in its
client tools; with an older one gz doesn't offer them. This version reads only the user-level `mcp.toml`, not a
project file.

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
data conventions) for everyone on the site. They show in
`gz skills list` and `/skills` with the source
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

## What AI models may read (`gz govern model-access`)

The chat server filters every tool result by the site's model read policy
before a model reads it: in the web chat, in `gz code` (including what your
`gz` commands print back to it) and for AI tools connected with `gz mcp`.
For each data level, Groundzero models and customer-brought models (your own
LLM connections, and MCP clients) either read the data (`allow`), get its
sensitive columns masked (`mask`: columns tagged `pii`/`sensitive` or under a
mask policy), or get no rows and are told the policy kept them back (`deny`).

```sh
gz govern model-access                                    # the policy, per level
gz govern model-access set customer confidential deny     # site admins; asks first (--yes to skip)
```

Levels are the lakehouse classifications (`public`, `internal`,
`confidential`, `restricted`), plus `unclassified` and `ungoverned`
(external connections, uploaded CSVs, MCP connections). By default
customer-brought models get `mask` on confidential, unclassified and
ungoverned data and `deny` on restricted; Groundzero models get `mask` on
restricted only.

In `gz code`, a masked or withheld result shows a dim notice such as
`data policy: masked email in run_command's result before the model read it
(lakehouse table Main.sales.customers)`. Printed tables can't be masked by
column, so a `gz` command whose output holds a masked column is withheld;
the model is told to retry with `-o json`.

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
