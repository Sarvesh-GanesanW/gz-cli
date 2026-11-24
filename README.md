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

The installer says what it is doing at each step:
1. It finds the release: the latest, unless you name a version.
2. It downloads it, with a progress bar.
3. It checks the download's sha256 against the release.
4. It puts `gz` in `~/.local/bin` (`%USERPROFILE%\.local\bin` on Windows).

It ends by printing:
- where the program went;
- where gz will keep its settings: `~/.config/gz`, or `%APPDATA%\Groundzero\gz` on Windows;
- how to update gz and how to remove it.

The Windows installer adds the folder to your user `PATH`. The macOS and
Linux installer prints the line to add it, if it isn't on `PATH` yet.

Other versions and folders:
- **A version:** `curl -fsSL …/install.sh | sh -s -- v0.8.1`.
- **A folder:** `GZ_INSTALL_DIR=/some/dir`, or `-InstallDir` on Windows.

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

## Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.sh | sh -s -- --uninstall
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.ps1))) -Uninstall
```

**What it removes:** the `gz` program. It keeps your settings, sign-in and
gz code history, so a reinstall picks up where you left off.
- **To delete those too:** add `--purge` (`-Purge` on Windows).
- **If you installed to another folder:** pass the same `GZ_INSTALL_DIR`
  (`-InstallDir`).
- **On Windows:** the folder also comes off your user `PATH` once nothing else is in it.

**By hand:** delete `~/.local/bin/gz`, and `~/.config/gz` for the settings.
On Windows, delete `%USERPROFILE%\.local\bin\gz.exe` and
`%APPDATA%\Groundzero\gz`.

**AI tools you connected with `gz mcp`** keep their `groundzero` server
entry. Remove it in each tool, for example
`claude mcp remove groundzero`.

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
`gz mcp setup print` shows every snippet. `gz mcp add | list | login` manage
the MCP servers `gz code` itself uses (see "Your own MCP servers" below).

## Daily use

```bash
gz warehouse list
gz table describe prod analytics orders
gz sql run --sql @query.sql --wait --data @conn.json
gz fs ls --connection s3conn --prefix raw/2024/
gz jobs submit --data @job.json
gz jobs logs --job nightly-etl
gz notebooks list myproject
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
gz jobs run <job> --compute <size> --rte <image> --follow
gz rte images
gz chat conversations
```

### Names instead of ids

Wherever a command takes the id of a project, job, notebook, dataset,
dashboard, visualization, filter, agent, connection, compute size,
warehouse, workspace, saved query, ontology model or lineage asset, its name
works too:

```bash
gz jobs run DQC --compute small --rte gz-rte-spark-sklearn:1.0.0   # = gz jobs run 1999 3013 --compute 1006 …
gz projects job-runs Sales DQC
gz designer delete dashboard "Revenue overview"
gz rte compute get small
```

- A value of digits only is always an id (a UUID, for agents).
- Names match exactly, in any case; a match in the same case wins.
- A job (or notebook) given without its project is looked for in all your
  projects, and its project filled in.
- When a name matches several things, gz asks which one in a terminal.
  With `-q`, `--no-input` or no terminal it fails instead and prints the
  exact command for each, with its ids.
- Each kind is listed once per command (jobs once per project).

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
  existing code; `--yes` skips) and submits the run. Without `--job` it uses
  a scratch job `gz-run-<file>` (created after you confirm). Waiting works
  as for `gz jobs run` (below).
- `gz sync` maps `clean_orders.py` → job `clean-orders`, or use a
  `gz-sync.toml` (`[jobs] "etl/clean_orders.py" = "clean-orders"`). It never
  deletes jobs, never overwrites a change made in the web app since the last
  sync (it says so; `--force` overrides), and `--dry-run` shows the plan.
  It records what it synced in `.gz-sync.json`; add that to `.gitignore`.
  `--only <file>` syncs just that file.
- Jobs run Python; wrap SQL in `spark.sql(...)`.

### Waiting on a run (`--wait`, `--follow`)

`gz jobs run`, `gz jobs submit` and `gz run` start the run and return. To
wait for it:

```bash
gz jobs run 1999 3013 --compute 1006 --rte gz-rte-spark-sklearn:1.0.0 --wait
gz jobs run 1999 3013 --compute 1006 --rte gz-rte-spark-sklearn:1.0.0 --follow
gz jobs run 1999 3013 --compute 1006 --rte gz-rte-spark-sklearn:1.0.0 --wait-timeout 30m
```

- `--wait` waits for the run to end; `--follow` also prints its log.
- `--wait-timeout 90s|30m|2h|1d` stops waiting (it implies `--wait`); the run
  keeps going. (`--timeout` is the HTTP timeout of each request.)
- Ctrl-C stops waiting, not the run, and prints how to follow it again
  (`gz jobs logs --job <id> --follow`).
- While the run is going gz reads only its status (`/etl/jobStatus`) and,
  with `--follow`, its pod's buffer (`/etl/ekslogs`); the full log is read
  once the run has ended, without repeating lines already shown. Job runs
  whose pod serves no lines print their log when they end.
- `-o json` prints `{"jobIdentifier", "status", "exitCode"}` at the end.

| Exit code | Meaning |
|---|---|
| 0 | the run succeeded |
| 1 | gz could not start or check the run (bad input, network, auth) |
| 2 | the run failed |
| 3 | the run was aborted (cancelled) |
| 4 | `--wait-timeout` passed first; the run keeps going |
| 130 | you pressed Ctrl-C; the run keeps going |

`--notify` also shows a desktop notification when the wait ends, saying how
the run ended (succeeded, failed, was aborted, or the wait timed out).
`gz jobs try` and `gz quality check` take it too, and
`gz configure --notify on` makes it the profile's default (`--no-notify`
turns it off for one command). macOS uses `osascript`, Linux `notify-send`,
Windows a PowerShell toast; where none works gz says so once and rings the
terminal bell (plus an OSC 9/777 notification in terminals that show one).

### Compare two runs (`gz jobs compare`)

```bash
gz projects job-runs 1999 3013                     # pick two finished runs
gz jobs compare c9d6338a7ad6475a8c9e1e52991f45ee 1921893c14bb46dbb8a5595edb9a2cee
```

Side by side, with changed rows marked: status, start, end and duration,
engine; requested CPU and memory, peak memory and CPU core-hours; rows and
bytes read and written; the tables each run read and wrote (lineage); the
job version and code hash each ran; and every line of the
`===== JOB METRICS =====` block a Spark run prints. Runs still going are
refused. `-o json` gives both runs' full records too.

What the platform does not provide is said in the output: no API returns a
version's code (so gz shows whether the code changed, not the lines), the
job's config is not kept per run, and no API returns a run's compute,
runtime image or worker count.

### Backfill (`gz jobs backfill`)

```bash
gz jobs backfill 1999 3013 --from 2026-11-01 --to 2026-11-30 \
  --compute 1006 --rte gz-rte-spark-sklearn:1.0.0 --dry-run
gz jobs backfill 1999 3013 --from 2026-11-01 --to 2026-11-30 \
  --compute 1006 --rte gz-rte-spark-sklearn:1.0.0 --concurrency 3
```

Runs the job once per slice (`--step day`, the default; `hour`; `month`),
`--from` and `--to` both included. Each run reads its slice from a
placeholder in the job's code, which the ETL provider fills in before the
run starts:

```python
run_date = $RUN_DATE          # runs as run_date = '2026-11-01'
run_end = $RUN_DATE_END       # the next slice's start: '2026-11-02'
```

Formats: day `2026-11-01`, hour `2026-11-01T05:00`, month `2026-11`.
`--param DAY` uses `$DAY` instead. A job whose code never reads the
placeholder is refused, since every run would do the same thing.

- A state file per backfill (`<gz config folder>/backfills/`) records each
  slice's run. Running the same command again skips slices that succeeded
  and picks up runs still going; `--retry` runs failed or aborted slices
  again.
- `--dry-run` lists the slices, the values each gets and what would happen.
- Progress prints as runs start and end, then a summary table.
- Exit codes: 0 every slice succeeded, 1 a run could not start, 2 a slice
  failed or was aborted, 130 Ctrl-C (runs keep going).

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

## SQL shell (`gz sql`)

`gz sql` in a terminal opens a shell on a lakehouse session (`--warehouse`,
`--compute`, `--keep-session`). Statements end with `;`.

- **Tab completes** keywords, warehouses, databases, tables and columns. After
  `FROM`/`JOIN` it offers databases, then `db.` offers that database's tables;
  after `SELECT`, `WHERE`, `ON` or `BY` it offers the columns of the tables in
  the statement, and `alias.` the columns of that table. Names are read once per
  session from the warehouse's schema; `\refresh` reads them again.
- **Lookups:** `\dn` databases, `\dt` every table (`\dt db` one database),
  `\d db.table` columns, types, partitioning and properties.
- **Display:** `\x` expanded rows, `\timing` elapsed time, space or `\more`
  for the next page, `\h` for every command.
- **Saving:** `\save out.csv` writes the last result; `\o out.parquet` writes
  every following result until `\o off`. The extension picks the format:
  `.csv`, `.jsonl` or `.parquet` (typed columns). All pages are fetched, with
  progress, up to 1,000,000 rows; past that the save stops and says so.
- **History** is kept per profile (`~/.config/gz/sql_history-<profile>`);
  statements that look like they hold a credential are never stored.
- **A stopped session** is noticed; press `y` to start a new one on the same
  warehouse and compute (or `\restart`).

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
  added and deleted, the job run that wrote it), and the record and file
  totals before and after.
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
it was committed, what that commit changed, the job run that wrote it, and
the `gz diff` and `gz jobs logs` commands that show the change and the run. `--code` opens
`gz code` with that evidence as a prepared message. It finds one change
from passing to failing: if the data broke, was repaired and broke again,
give `--good` a snapshot after the repair.

**`gz table rollback`** moves a table's main back to an earlier snapshot.
It first prints what changes, from the history: the commits it undoes and
how the record and file totals move (`--rows` or `--key` also compares the
rows, as `gz diff` does). Then it asks, unless `--yes`.

```bash
gz table rollback Lineage.sales.orders --to previous
gz table rollback sales.orders --warehouse Lineage --to 2026-09-16 --rows --key order_id
```

- The undone snapshots are kept until they expire, and gz prints the
  command that puts main back.
- A target that is not an earlier snapshot of main (another branch, or one
  an earlier rollback left out) is refused unless `--allow-non-ancestor`,
  since main would also gain commits it never had. The table's quality gate
  and data policies then apply as they do to publishing a branch.
- It needs what publishing a branch needs: AlterTable, Update for the
  commits undone, Insert or Update for any brought in. The catalog audit
  and the table's commit history record who rolled back, from where to
  where. If main moved after the preview, the rollback is refused.

**`gz table profile`** shows per-column statistics of a table: type, nulls,
an approximate distinct count, min, max and mean for numbers, dates and
timestamps, and the length range for text. `--top N` adds each column's
most common values, `--columns a,b` picks columns, `--at` profiles an
earlier snapshot, and `-o json` gives it all as one value.

```bash
gz table profile GZWarehousePROD.DemoDB.ProductionMetrics --top 3
```

It runs one aggregate query per 40 columns (plus one per batch of top
values) on a SQL session, all at one snapshot. The scan estimate is printed
first, and `--max-scan` or the profile's `sql_max_scan` refuses a profile
over the limit before anything runs.

**Which run wrote a snapshot.** A commit a job (or code dataset) run makes
through the lakehouse catalog names the run in the snapshot: the lakehouse
provider checks the run is real and going, then records its id, job,
project and site (`gz.run-id`, `gz.job-id`, `gz.job-name`, `gz.project-id`,
`gz.project-name`, `gz.run-site` in the summary). `gz table versions` shows
the job and run id, `gz diff` lists them per commit, and `gz bisect` and
`gz explain` print `gz jobs logs --job <run id>` for the run's logs. Older
snapshots, and commits made outside a job run (a notebook, `gz sql`), name
no run; for those gz shows the Spark `app-name` the writer chose, and
`gz explain` matches a run by time.

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
  records, the Spark application that wrote it), the run that wrote that
  snapshot with its status, times, how sure the match is and the command
  for its logs,
  the code version that run used, and the tables upstream (`--depth N`
  levels, default 1).
- **`--at TIME`** (a UTC date or time) uses the snapshot each table was at
  then and lists the commits made to it since, with the record count
  before and after, so you can tell whether the inputs moved.
- **What gz cannot tell**: gaps stated plainly, such as the Designer
  keeping no history of definitions.

How the run is matched. A snapshot committed by a job run names the run
(see "Which run wrote a snapshot" above); otherwise gz matches by evidence.
It says which it used:

| Match | Meaning |
|---|---|
| `named by the commit` | the snapshot records the run, checked by the lakehouse provider at commit time |
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

**Drawing the lineage.** `gz jobs lineage graph` draws one asset's upstream
and downstream as a tree, each edge named by the job or dataset that moves
the data, with dashboards under the datasets they read:

```bash
gz jobs lineage graph home_health_rcm.gold_fact_payment_transactions
gz jobs lineage graph 3601 --upstream --depth 3
gz jobs lineage graph 3601 --format mermaid      # or dot, for Graphviz
```

The asset is a lineage asset id or a name. `--depth` (1-5) is hops each
way; a node drawn earlier is marked "see above" and a loop "cycle"; a node
with more than `--fan-out` (default 10) neighbours shows the first ones and
counts the rest by type.

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
  each table from main to the branch, and the next commands for how the run
  ended: publish and discard when it passed; the run's logs, discard and
  the same try again when it failed; and when a table's quality gate will
  refuse the branch, why, `gz quality show` for that table, discard and try
  again. It never suggests publishing what would be refused.
- The ETL provider refuses the run, and gz removes the branches again, when
  the code writes a table without the branch, writes outside the lakehouse,
  or builds SQL or table names at run time where the writes can't be seen.
  gz keeps the provider's reason for each write and adds what to change,
  with the read and write forms it can follow. Only single Spark jobs can
  be tried. Running local code needs edit
  permission on the job, though the code isn't saved.
- `gz jobs publish` moves each table's main to its branch and removes the
  branch. Main must not have moved on since the branch was made; when it
  has, nothing is published and you try again on a new branch. Each table is
  published on its own, so a failure part way says which were done. When a
  table requires a quality verdict (`gz quality require`), the branch's
  head needs a passing one; otherwise nothing is published and gz says
  which tables stopped it and why.
- `gz jobs discard` drops the branches; main is never touched.
- Tables with row filters, column masks or other data policies can't be
  tried on a branch.
- `-o json` gives each command's result as one value.

This needs the lakehouse provider and ETL provider updates deployed; until
then gz says so and changes nothing.

### Quality rules as code (`quality.toml`)

A `quality.toml` next to the job files of a `gz sync` folder declares checks
per output table; `gz quality lint` lists every problem with its line and
column, and `gz quality lint --help` shows every rule.

```toml
[tables."Manufacturing.mfg_ops.oee_daily"]
rows = { min = 1 }
not_null = ["line_id"]
unique = [["line_id", "day"]]
accepted_values = { shift = ["Morning", "Evening", "Night"] }
max_commit_age = "24h"

[[tables."Manufacturing.mfg_ops.oee_daily".sql]]
name = "no_negative_oee"
query = "SELECT * FROM {table} WHERE oee < 0"   # fails when rows come back
```

- `gz jobs try` reads the file before it makes any branch (a broken file
  stops it), turns on the publish gate of each declared table the job
  writes, and after the run checks those tables on the try's branch with
  one temporary Spark job. The results show with the try's outcome; a
  failing check fails the try (exit 2) and gz never suggests publishing it.
- The check records the branch's verdict, stamped with the rules it used,
  so the provider refuses to publish the branch without a passing verdict
  for its head, from gz and the web app alike. `gz jobs publish` also
  refuses when the verdict is missing, failed, stale or from other rules
  than the file's now, and prints the `gz quality check … --rules` command
  that checks the branch again.
- `gz quality check <table> --project <id> --rules [path]` runs a table's
  rules on demand (`--branch` to check a branch).

## Review a job change (`gz review`)

One command for a change in a `gz sync` folder, locally or in CI:

```bash
gz review --compute 1003 --rte gz-rte-spark-pytorch:1.0.0 --sql-compute small
gz review etl/clean_orders.py --compute 1003 --rte gz-rte-spark-pytorch:1.0.0
gz review --format markdown > review.md      # for a pull request comment
gz review -o json --strict                   # for a script
```

For each changed job (its file against the code saved on the platform;
with no files named, every job in the folder whose code differs):

1. **Code:** the diff against the saved code.
2. **Impact:** the tables the job writes and what depends on them
   downstream, as `gz impact` finds it.
3. **Try:** `gz jobs try` runs the local code on a write branch of each
   table it writes, then each table's rows and schema are compared, branch
   against main, as `gz diff --rows` compares them. The comparison uses the
   SQL session `gz sql` uses; gz stops it afterwards only if review started
   it. `--no-rows` skips it.
4. **Quality:** what the try run's quality checks and gates said.
5. **Verdict:** pass, warn or fail, with the reasons. It fails on a failed
   try run, a quality refusal, or dependents of a `--fail-on` kind (or the
   folder's `[impact] fail_on`). It warns on a step that could not run, a
   dropped or retyped column, a job not in the project yet, or a removed
   file.

The branches review makes are dropped at the end (`--keep-branches` keeps
them). A job that can't be tried (it writes no lakehouse table, or the ETL
provider refuses the branch run) is reported with the reason, and its other
steps still run. Without `--compute` and `--rte` no job is tried.

Exit codes: 0 pass (or warn without `--strict`), 1 gz could not finish a
step, 2 fail, 3 warn with `--strict`.

### In a pull request (GitHub Action)

The repository is also a GitHub Action. It installs the gz release it is
pinned to (checking its sha256), signs in, runs `gz review` on the job files
the pull request changes, writes the review to the job summary, keeps one
comment on the pull request up to date, and fails the check when the
review fails.

```yaml
name: gz review
on: pull_request

permissions:
  contents: read
  pull-requests: write     # for the review comment

jobs:
  review:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0     # the diff needs the base branch's history
      - uses: Sarvesh-GanesanW/gz-cli@v0.13.0   # a release that has gz review
        with:
          client: stage
          site: preview
          domain: groundzerodev.cloud
          username: ${{ secrets.GZ_USERNAME }}
          password: ${{ secrets.GZ_PASSWORD }}
          dir: jobs          # the gz sync folder
          compute: "1003"
          rte: gz-rte-spark-pytorch:1.0.0
          sql-compute: small
          fail-on: dashboards,agents
```

Secrets:
- `GZ_USERNAME` and `GZ_PASSWORD`: a Groundzero user for CI. It signs in
  the way `gz auth login` does, with the password on stdin, never on the
  command line. The user needs to read the project's jobs and lineage, and
  edit permission on the jobs to try them (the code is not saved).
- `GZ_MFA_SECRET` (input `mfa-secret`), only when that user has MFA: the
  authenticator's base32 secret, from which the action makes the current
  code.

API keys (`gz auth login --api-key`) don't reach the lakehouse, so a review
needs a user.

Other inputs: `version` (default: the tag the action is pinned to),
`project` (default: the folder's `gz-sync.toml` `[impact] project`),
`strict`, `keep-branches`, `base-ref` (default: the pull request's base),
`comment` (`false` for no comment) and `github-token` (default
`github.token`). Outputs: `verdict` (pass, warn, fail, or error) and
`exit-code`.

The comment needs `pull-requests: write`. Pull requests from forks get a
read-only token and no secrets, so the review can't run on them. The
action runs on Linux x64, macOS and Windows x64 runners; `action/test.sh`
tests its scripts locally.

## Data quality (`gz quality`)

A job checks a lakehouse table with `runTableDataQualityChecks`. In a
`gz jobs try` run the check reads the branch the run wrote and records a
verdict on that branch: passed or failed, for the snapshot it checked.

```bash
gz quality show Lineage.sales.orders              # each branch's verdict
gz quality require Lineage.sales.orders           # publishing needs a passing verdict
gz quality require Lineage.sales.orders --off     # recorded, not required
gz quality history Lineage.sales.orders --results Lineage.dq.results --failed --limit 50
```

```
┌───────────┬─────────┬──────────┬────────────────────────┬──────────────────────┐
│ branch    ┆ verdict ┆ snapshot ┆ failed checks          ┆ checked              │
╞═══════════╪═════════╪══════════╪════════════════════════╪══════════════════════╡
│ try-bad   ┆ failed  ┆ current  ┆ ranges, missing_values ┆ 2026-11-11 11:05 UTC │
│ try-fresh ┆ none    ┆ —        ┆ —                      ┆ —                    │
│ try-moved ┆ passed  ┆ stale    ┆ —                      ┆ 2026-11-11 09:00 UTC │
└───────────┴─────────┴──────────┴────────────────────────┴──────────────────────┘
```

- `show` lists every branch but main. `snapshot` says whether the verdict
  is for the branch's head (`current`) or an older snapshot (`stale`: the
  branch was written again after the check). `-o json` prints the
  provider's reply, with the snapshot ids and each verdict in full.
- `require` turns the gate on for the table (`--off` turns it off; it
  needs permission to alter the table). With it on, `gz jobs publish` and
  the provider refuse a branch whose head has no passing verdict; gz says
  per table whether the verdict is missing, failed (and which checks) or
  stale, and publishes nothing.
- `history` reads the rows the checks saved (`resultsTable=` on
  `runTableDataQualityChecks`, or `saveDataQualityResults`) for one table,
  newest first: `--limit` rows (default 20), `--failed` for warnings,
  errors and checks that couldn't run. It runs on a `gz sql` session on
  the results table's warehouse, under the same scan limit (`--max-scan`,
  `--compute`, `--keep-session` as for `gz sql`).
- The gate stops mistakes, not a determined admin: anyone who can alter
  the table can also write a verdict by hand.

This needs the lakehouse provider update deployed; until then `show` and
`require` say so, and `gz jobs publish` publishes without the check.

### Scheduled checks (`gz quality watch`)

```bash
gz quality watch Lineage.sales.orders --project 1999 --every 1h --max-age 24h \
  --notify-email ops@acme.io            # --rules [path] runs quality.toml's rules
gz quality watch list
gz quality watch show Lineage.sales.orders --wait   # latest run's checks; exit 2 when they failed
gz quality watch stop Lineage.sales.orders          # schedule, email and job deleted
```

- A watch is a job in `--project` and a platform schedule, both named
  `gz-quality-watch-<table>-<id>`, so it runs while this machine is off
  and the web app's Scheduler shows it. `--every` takes 5m, 10m, 15m, 20m,
  30m, 1h, 2h, 3h, 4h, 6h, 8h, 12h, 1d or 7d; the first run comes one
  interval after the watch is made (`--run-now` also runs it once now).
- `--max-age` fails the check when the table's last commit is older.
- A failing check fails the run, which sends the schedule's failure email
  to `--notify-email` (repeatable; the first gets it, the rest are copied).
- `stop` refuses while a run is still going, since the run needs the job.

## Agent audit (`gz audit agents`)

For site administrators: every agent on the site, including ones your own
policies hide, with who owns it, its model, tools, MCP connections,
sub-agents and team, which tools ask for approval, its limits and quota,
and its runs, failures and cost over the last `--days` (30).

```bash
gz audit agents                                   # agents table, then findings
gz audit agents --owner alice --severity medium
gz audit agents --finding write-without-approval --finding owner-gone
gz audit agents -o json                           # every record with its findings
gz audit agents --fail-on high                    # exit 2 on a high finding, for CI
```

Findings come from what the platform records: tools that change data
without asking, MCP tools (which can't be put behind approval), MCP
connections that are gone, agents that read site data (or are classified
confidential or restricted) and can send data out unreviewed, agents with no
owner or an owner who left or is disabled, agents unused for `--days`,
failing runs, spend far above the median agent, and sub-agents or team
members that can use more than their parent. `gz audit agents --help`
lists each rule. When the owner lookup or a run's price is unavailable, gz
says so under the table instead of reporting a clean result.

The agent builder decides who is an admin (the site's Admin group); anyone
else gets a clear refusal. In CI, an API key works if its user is an Admin.

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

- Reads and searches inside the open folders never ask, in any mode; every
  edit shows its diff first; every command asks (answer `a` to allow it for
  the chat), except the read-only ones below.
- What you allow "for this chat" stays with that conversation: continuing
  it (`gz code --continue`, `-c <id>`, `/resume`) keeps those answers, and a
  new conversation starts with none. They are kept owner-only in
  `~/.config/gz/chat-grants/` and dropped after 30 days unused.
- Everything stays inside the folders you opened (see **Folders** below);
  secrets (`.env*`, keys, `.ssh/`, `.aws/`, git credentials) are always
  blocked.
- Each tool call is logged (tool, target, decision — never contents) to
  `~/.config/gz/audit.log`.
- `Esc` (or `Ctrl+C`) stops the turn at once: the answer stops streaming,
  a running command or script is stopped with its whole process group
  (SIGTERM, then SIGKILL 1.5 s later), an open permission prompt or
  question closes (answered No), and the rest of the model's calls are
  skipped. Each call that did not finish is answered "Interrupted by the
  user" (a stopped command or script returns what it printed before the
  stop, then that note, capped and saved to a file like any long result) so the
  conversation stays intact, and the input is yours again: tell gz what to do instead. An `Esc` right after
  `Enter` counts too. Background commands (`/shells`) and background
  subagents keep running. To answer No and tell gz what to do differently
  without stopping the turn, press `n` at the prompt; to skip one question,
  press `Tab`. A background subagent's prompt is only answered No by `Esc`.
- While a permission prompt or a question of gz's is open, the spinner
  reads `Waiting for you…` and the turn's clock stands still; `Worked for`
  and `/usage` leave that waiting time out.
- When the model asks for several reads at once (`read_file`,
  `list_directory`, `find_files`, `search_files`, `web_fetch`), gz checks
  each in order as usual (one prompt at a time), then runs them together,
  up to 8 at once, and answers in the order asked. Edits, commands,
  scripts and questions always run one at a time, in order.
- Network blips don't end a turn: a dropped answer stream, or one silent
  for 45 s, reconnects where it left off (the spinner reads
  `Reconnecting…` with the reason and the next try, up to 6 tries in a row;
  commands that follow a run print the same note, on stderr with
  `-o json`), and every gz command retries
  reads (GET) after a dropped connection, a 5xx or a 429 (honouring
  `Retry-After` up to 20 s), up to 4 tries. A request that timed out, or a
  504 gateway timeout, is not retried: the server may still be working on
  it. Other requests are retried only
  when the server cannot have seen them (the connection never opened) or
  when it ignores a repeat: chat messages and tool results carry an
  `X-Request-ID`, reused on the retry, so a message is never sent twice.
- `/new` starts over; `/exit` quits.
- `/export` saves the conversation as Markdown (`gz-<date>-<first words>.md` in the current folder, never replacing a file); `/export <path>` picks the file inside an open folder, `/export clipboard` copies it, and `--thinking` (before or after either) keeps the model's reasoning as folded `<details>` blocks. A turn that failed keeps its error, and a resumed conversation exports every earlier message, not just the ones `/resume` showed.

**How output reads.** Code is coloured by language: fenced blocks in
answers (by the fence tag, e.g. ` ```rust `), edit diffs and file reads in
the transcript (by file extension or name, e.g. `Dockerfile`), and the
command or script in an approval prompt. Over 200 languages are known,
TypeScript/TSX, TOML, Kotlin, Swift and Terraform among them; others stay
plain. Colours are the terminal's own 16, so they suit light and
dark themes and Terminal.app. Very large blocks are coloured up to the
first 32 KB and shown plain after that. Inside an edited line, the words
that changed sit on a stronger bar than the rest of the line.

When the model shares its reasoning (Claude and OpenAI models send a
summary), a dim `✻ Thinking… 4s` line counts up while it arrives and
becomes `✻ Thought for 4s`; Ctrl+O shows the full text. Reasoning is only
shown: it is never sent back, and `/export` leaves it out unless you ask
with `/export --thinking`. Tools the chat
server runs itself (a lakehouse query, a document search) show what they
were asked, e.g. `Query lakehouse(select count(*) from orders)`, stay live
until their result arrives, and then print a one-line result (`3 rows ·
1.2 KB · …` or the error); Ctrl+O shows the whole result, secrets already
redacted by the server. The server announces such a tool as soon as the
model starts writing its input, so the input shown can be cut short, or
missing when nothing had been written yet.

A command's result line names the test summary when its output has one
(pytest, cargo test, jest, vitest, go test, mocha), after the exit code
when it failed: `exit code 1 · 1 failed, 11 passed in 0.10s`. Searches
show everything that shaped them: `Search(pattern: "calc_total", path:
"src", glob: "*.py", type: "py")` (the path is left out for the folder gz
started in), and the audit log records the glob and type too. When several
reads and searches fold into one line, it still names the patterns:
`Read 2 files, Searched 2 patterns ("calc_total", "*.py")`.

**Safe edits.** An edit or overwrite only lands on content the assistant
has seen:

- `edit_file`, `notebook_edit` and `write_file` over an existing file are
  refused until the assistant reads that file with `read_file` (a page of it
  counts), and again whenever the file changed on disk after that read (you,
  a formatter, a command); the refusal tells it to read the file again.
  Files it created or changed itself, and files you attached with
  `@path` (their text went into your message), count as read. New files
  need no read.
- The record lives in memory: `/new` and `--continue`/`/resume` start with
  none, so files are read again before they change. Each subagent keeps its
  own.
- The diff you approve is the diff written: if the file changed while the
  prompt was open (or a hook changed it), gz writes nothing and tells the
  assistant to read it again.
- Edits keep a file's encoding: in a file with Windows (CRLF) line
  endings, text the assistant writes with plain newlines still matches and
  is written with CRLF; a UTF-8 byte-order mark and a missing or present
  final newline stay as they were; an overwrite keeps CRLF endings and the
  mark too, and its result says so (to change them, the assistant rewrites
  the file with a script). In a file that mixes CRLF and LF endings, which
  `read_file` doesn't show, `old_text` matches whichever endings are there;
  new text gets CRLF where every line break it replaces was CRLF. A file that is not valid UTF-8 reads with a note, is never
  changed by `edit_file` (the assistant is told to use a script working on
  the bytes), and `write_file` over it is refused when the new content
  holds U+FFFD (what reading showed in place of the undecodable bytes); new
  content without it is a deliberate replacement and is written.
- Every tool result fits what the chat server passes to the model whole:
  56 KiB of UTF-8 JSON (quotes, newlines and control characters count as
  escaped). Longer results keep their start and end, cut on character
  boundaries, with a note saying how many characters and bytes were cut;
  `read_file` stops at a whole line and says which offset to read next.
- Long output from `run_command`, `command_output` and `run_script` is not
  lost: the result's first line names a private file (owner-only, in a
  folder of its own under the system temp folder) holding the output, and
  the rest keeps its start and end. The assistant pages through the file
  with `read_file` (`offset`, `limit`) or searches it with `search_files`,
  without a permission prompt, in every mode; those exact files are the
  only ones outside the open folders it may open, and never for changes.
  A session keeps at most 64 MiB of them (the oldest go first) and deletes
  them when it ends; folders left by a gz that crashed are swept by the
  next one. gz holds up to 1 MiB per stream: a command keeps its latest
  output, a script its first and last 512 KiB. Bytes dropped that way are
  marked in the output, and the result then says the file holds what gz
  kept rather than the whole output.
- `search_files` and `find_files` walk the folder and search its files on
  up to 8 threads; results come back in the same order every time (files
  in path order, `find_files` newest first).

**Typing.** The input box keeps what you send per project folder.

- `↑`/`↓` recall earlier prompts, from this session and earlier ones in the
  same folder; `↓` past the newest brings back what you were typing.
  `Ctrl+R` searches them: type to narrow, `Ctrl+R` again for an older match,
  `Enter` keeps the match in the input (unsent), `Esc` puts your text back.
  The history is in `~/.config/gz/code_history/` (owner-only, one file per
  folder, the newest 1,000 prompts; honors `XDG_CONFIG_HOME`). Pasted images
  are never stored, and prompts over 10,000 characters are recalled in the
  session only.
- New lines: `\` then `Enter`, `Ctrl+J`, or `Shift+Enter`/`Alt+Enter` where
  the terminal reports them.
- `/terminal-setup` makes Shift+Enter add a newline in the terminal you run
  gz in. kitty and Ghostty need nothing. For VS Code, Cursor, Windsurf and
  VS Code Insiders terminals (`keybindings.json`, comments kept) and
  Alacritty (`alacritty.toml`) gz shows the exact change, asks, copies the
  file to `<file>.gz-backup-<UTC date-time>` first, and refuses if the file
  changed in between. For iTerm2, WezTerm, Apple Terminal and tmux, whose
  settings gz leaves alone, it prints the steps.
- `Tab` completes the path before the cursor (relative to the current
  folder, `~/…`, or absolute; also after `/cd` or `!`): one match is filled
  in, folders end in `/`, names with spaces are quoted. Several matches fill
  in what they share and open a small menu; `Tab` again steps through them,
  `Enter` keeps one, `Esc` puts back what you typed. Hidden names show only
  after you type `.`, and secrets are never offered.
- `@` attaches a file (`@src/main.rs`, `@"name with spaces.md"`) or asks for
  a skill (`@skill-name`), with a fuzzy popup. The first `@` starts indexing
  the folder in the background, honoring `.gitignore` and never listing
  secrets, so typing never waits: the popup fills as files are found
  (`indexing files…` until the first match) and the index is rescanned in
  the background when it is more than 10 seconds old and you type `@` again.
  Up to 100,000 entries are indexed.
- `Ctrl+G` opens the message in `$VISUAL` or `$EDITOR` (`vi` when neither is
  set; `code -w` style values work); when you save and quit, the text comes
  back into the input, unsent. If the editor fails, the input is unchanged.

**Sessions.** `gz code --continue` (the last conversation started in this
folder), `gz code -c <id>`, `gz code --resume` and `/resume` reprint the
last 20 messages the way the live session showed them: `@` mentions as their
`⎿ Read a.py (12 lines)` notes, your `!commands` as `! command` items with
their output, rewinds as `⎿ Told gz you rewound 2 files`, notes you sent
while gz was working where they arrived, the tools an answer called on one
line, and an answer that was stopped closed off with a notice (an open code
block is closed first). Subagent reports and finished background commands
show as `⎿ gz reads the update from …`, what hooks added as
`⎿ Hooks added 3 lines of context for the model`, a stop hook's carry-on as
`↻ a stop hook asked the agent to carry on: …`, and plan mode's note to the
model is left out. What gz added for the model is never shown as if you typed it.

- `/resume` (and `gz code --resume`) lists the conversations started from
  `gz code` on this computer, newest first, each with its folder, age and
  message count. Type to search titles and folders, `esc` clears the search,
  and `tab` adds your other chats (web chats included). Each part shows 12
  at a time and says how many more match; search to find older ones. The chat server has
  no marker for gz code chats, so the list comes from
  `~/.config/gz/code_sessions.json` (ids, folders, titles and times only).
  A chat opened with `gz code -c <id>`, `--continue`, `--resume` or the tab
  view joins the list (at the top) as soon as it loads, and so does a fork.
- `/rename <name>` renames the conversation, on the server too (once the
  answer in progress has finished).
- `/fork [name]` copies the whole conversation into a new one and carries on
  there; the original stays as it was and `/resume` opens it. Without a
  name the copy is called "<title> (branch)". A message typed while the
  fork is being made goes to the fork, and gz's file history comes along,
  so `/rewind` in the copy can still put back earlier edits. What you
  allowed "for this chat" and attached documents carry over too; `/resume`
  of another conversation leaves both behind. Commands that
  leave or change the conversation (`/new`, `/clear`, `/resume`, `/rewind`,
  `/fork`, `/rename`, `/compact`) wait until the fork is made.
- `gz code --continue --fork-session` (or `-c <id> --fork-session`, or
  `--resume --fork-session` for the one you pick) starts in a fork instead
  of adding to the original.

**Screen.** The input box sits right under the conversation, and once the
conversation fills the screen it stays at the bottom, also after a turn ends
or a menu closes. A menu or picker covers the conversation's last lines
while it is open and gives them back when it closes. After a resize (once the size settles), gz clears the
screen and its scrollback and prints the conversation again at the new
width, so nothing is left cut or doubled; output from before `gz code`
started is cleared with it, because the terminal has already reflowed it.
`/clear` clears the visible screen only. Text wraps
between words; the status line under the input drops its least important
parts first (the shortcuts hint, then the folder, context and cost) and ends
in `…` when it still doesn't fit. Menus show ten rows at a time and scroll.
Everything works from 40 columns. `/help` lists every command by group,
then the key bindings, on a card as wide as the terminal.

**Notifications.** When a turn finishes, or gz needs you (an approval or a
question) while the terminal is in the background, gz code lets you know.
Terminals that report focus are notified only while unfocused; elsewhere
the notice goes out after about six seconds without a key press. Choose how
with `/notifications`: `auto` (a terminal notification, OSC 9 or OSC 777, in
iTerm2, WezTerm, Ghostty, kitty, foot and rxvt-unicode; the bell elsewhere),
`bell`, `osc`, `desktop` (macOS Notification Center, `notify-send` on Linux,
a Windows toast) or `off`. One-shot and headless runs never notify.

**Themes.** `/theme` lists the color themes and `/theme <name>` switches:
`dark` (the default), `light`, `dark-daltonized` and `light-daltonized`
(blue and orange where red and green carry meaning, as in diffs) and `ansi`
(only the terminal's 16 colors, so its own scheme decides). `gz tui` uses
the same theme.

**Custom status line.** `/statusline <command>` runs your command (through
your shell, in the current folder) and shows the first line it prints under
the input box; `/statusline` shows the current one and `/statusline off`
removes it. The command gets the session as JSON on stdin:

```json
{"model": "Sonnet 5.5 · high", "cwd": "/work/app", "conversationId": "…",
 "costUsd": 0.19, "contextPercent": 12, "gitBranch": "main",
 "mode": "ask", "version": "0.13.2"}
```

`mode` is `ask`, `accept-edits`, `plan`, `auto` or `dont-ask`; `model`,
`contextPercent` and `gitBranch` are `null` when unknown. It reruns when
any of these change (at most once a second) and every 10 seconds, and is
stopped after 3 seconds. Escape sequences in its output are dropped; a
failure or a timeout shows dim in its place. Example:
`/statusline jq -r '"\(.gitBranch // "-") · $\(.costUsd) · \(.contextPercent // 0)% context"'`.

Theme, notifications and the status line are kept for every profile in
`~/.config/gz/terminal.toml`.

**Granting access.** `/permissions` lists every rule (numbered `R1`, `R2`,
… with where it comes from), every saved answer (`L1`) and the chat's
approvals (`C1`, `A1`); `/permissions revoke <id>` (or `remove <id>`) takes
one back. `/permissions allow|ask|deny [user|project|local] <rule>` adds a
rule (see **Permission rules** below; `user` when no scope is given) and
`/permissions trust` trusts the project's rules files as they are now;
`/allow <rule>` is short for `/permissions allow <rule>`. Every change
counts at once, in a running turn too. The older `/permissions allow
<what>` forms save an answer for the folder you are in:

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

**What runs without asking.** After your rules, grants and the mode have
had their say, gz runs these without a prompt (in plan mode only the
reads, since plan mode refuses every command and script):

- reads and searches in the open folders (secret files stay blocked);
- read-only commands: `ls`, `cat`, `head`, `tail`, `wc`, `grep`, `rg`,
  `find`, `pwd`, `echo`, `which`, `file`, `stat`, `du`, `tree`, `sort`,
  `uniq`, `cut`, `diff`, `cmp`, `nl`, `basename`, `dirname`, `realpath`,
  and `git status`/`log`/`diff`/`show`/`rev-parse`/`blame`/`ls-files` and
  listing `git branch`. Only one program with plain arguments counts: a line
  that chains, pipes, redirects or substitutes (`;` `&` `|` `>` `<` `` ` ``
  `$`), uses a glob or `~`, sets a variable, passes a flag that writes a
  file or runs another program (`find -exec`/`-delete`, `sort -o`,
  `tree -o`, `rg --pre`, `git -c`, `git diff --output`/`--ext-diff`),
  follows symlinks met inside a folder (`grep -R`/`-S`, `rg -L`,
  `find -L`, `ls -L`, `du -L`, `tree -l`, `diff -r`: such a link may lead
  out of the open folders), or
  names a path outside the open folders or a secret file asks as usual
  (`..` counts where it really leads, through symlinked folders too). Git
  commands also ask unless the whole repository (the folder holding
  `.git`) is inside the open folders, since git reads all of it, and
  `git show`, `git diff` and `git log` ask in a repository that tracks a
  secret file (git prints those from its own objects, where the sandbox
  can't hide them by name). Anything that could print file contents from
  a commit other than the working tree and the index asks too, since a
  secret committed once and deleted later is still in history: `git log`
  with `-p`/`--patch`/`-u`/`-U`, `-c`/`--cc`/`-m`, `-L`, `-G`/`-S`,
  `--word-diff` or `--check`; `git show` unless it is `HEAD` (or
  `HEAD~n`) with the patch off (`-s`, `--stat`, `--name-only`) and no flag
  that picks refs (`--all`, `--tags`, `--glob=`, `-g`); `git diff`
  against a commit (`--cached`, a revision, `rev:path`) unless it prints
  names or counts only; `git blame` unless it names exactly one file in the
  working tree (git can't tell a tag from a folder of the same name);
  `git status -v`; and
  `git cat-file`, `git archive`, `git grep`, `git format-patch` and
  `git reflog` (never on the list);
- scripts (`run_script`) that call no write helper.

These run under the OS sandbox's read-only profile: nothing can be written
(but gz's own temp folder), the network is off, local servers included (a
script reaches only its own bridge to gz), and on macOS secret files'
contents can't be read even by `grep -r` or `rg`, no other process can be
signalled, no app's settings written, and no system service reached but
user and group lookups (so the clipboard, disk mounting, the keychain and
app launching are closed), and a script can't read files under your home
folder outside the open folders, nor git's object store (`.git/objects`,
where a secret committed once and deleted later still lives), so a script
that runs git or reads history is refused. When the sandbox refuses a script something (a write, a file,
the network), the model is told, and the same script asks the next time in
that chat. Without an OS sandbox (Windows, `/sandbox off`, or no
bubblewrap on Linux) commands and scripts ask as before; on Linux, scripts,
`grep -r` and `rg` always ask, since bubblewrap can't hide secret
files by name or keep a script's loopback bridge to gz.

In **accept-edits mode**, `mkdir`, `touch`, `cp` and `mv` inside the open
folders run without asking too, under the usual OS sandbox, when they
overwrite nothing (no existing destination, no `-f`, nothing in `.git`
or gz's project settings (`.gz/`, `.mcp.json`),
and no copy of a folder's contents such as `cp -R src/ dst`).
`rm` and `rmdir` always ask: `/rewind` couldn't bring back what they delete.

A deny or ask rule on reads (`read(…)`, `read_file(…)`, any read tool, or
`--disallowedTools read`) turns off the read-only commands and scripts
above: they can open a file without naming it (`grep -r`, `git show`, a
script's `open()`), so each asks instead (refused in don't-ask mode and
with `-p`). A deny or ask rule on reads or edits does the same to
accept-edits' `mkdir`, `touch`, `cp` and `mv`. Reads by the file tools
still run without asking wherever no rule matches them.

**Permission rules.** Beyond the answers saved per folder, `allow`, `ask`
and `deny` lists decide tool calls before gz asks. Each list holds patterns:

| Pattern | Matches |
|---|---|
| `run_command` (any tool name alone) | every use of that tool |
| `run_command(npm test)` | that exact command line |
| `run_command(git status:*)` | `git status` alone or with more arguments (not `git statusx`) |
| `read(src/**)`, `edit(*.md)` | reads (`read_file`, `list_directory`, `find_files`, `search_files`) or edits (`edit_file`, `write_file`, `notebook_edit`) of matching paths; each tool name works on its own too |
| `web_fetch(domain:docs.rs)`, `web_fetch(domain:*.example.com)` | pages from that host, or its subdomains |
| `local_mcp_call(github)`, `mcp(github/create_issue)` | a local MCP server's tools, or one of them |
| `run_script` | every script |

Paths are relative to the folder `gz code` started in, absolute, or
`~/…`; `..` and symlinks are resolved on both sides, `*` stays inside one
folder and `**` crosses folders, and a plain folder (`read(docs)`) covers
everything in it. A deny or ask rule on paths also catches listing a
folder whose every entry it covers (`read(secrets)` or `read(secrets/*)`
stops `list_directory secrets`), and searching (`search_files`) any folder
that could hold a file it names, since a search reads contents:
`read(**/.env)` stops a search anywhere in the project, `read(*.log)` only
one of the project folder itself. Listings and `find_files` elsewhere show
names only, so `read(**/.env)` doesn't stop them.

An allow rule must cover the whole call: a `:*` rule never matches a line
that chains, pipes, redirects or substitutes. A deny or ask rule catches any
command the line runs: `run_command(rm:*)` refuses `ls && rm -rf x`,
`echo $(rm x)`, `sudo -u root rm x`, `nice -n 5 rm x`, `timeout 5 rm x`,
`/bin/rm x`, `"rm" x`, `bash -c 'rm x'`, `eval rm x` and
`find . -exec rm {} \;` too. When gz can't tell which program a line runs
until it runs (`$CMD x`, `eval "$X"`, `curl … | sh`), a command deny or ask
rule asks about it instead (refused in don't-ask mode). Deny rules guard
against the model, not against a determined program: a script file or a
program that runs other programs itself (`make`, `npm run`, `python -c`)
does what it says without gz seeing inside, so pair deny rules with the
OS sandbox and narrow allow rules.

```toml
# ~/.config/gz/permissions.toml (user), .gz/permissions.toml (project, shared),
# .gz/permissions.local.toml (project, just you; gz adds it to .gz/.gitignore)
allow = ["run_command(cargo test:*)", "edit(src/**)"]
ask = ["run_command(git push:*)"]
deny = ["read(secrets/**)", "run_command(rm:*)", "web_fetch(domain:*.internal)"]
```

Sources, highest first: `--allowedTools` / `--disallowedTools`, the local
project file, the shared project file, your user file. Across all of them
deny beats ask beats allow: a deny rule refuses at once (the model is told
which rule and why) and an ask rule always prompts, even when an earlier
"always" answer or the mode would let the call through. A project file
comes with the repository, so its allow rules count only once you trust
that version of it with `/permissions trust` (a changed file waits for
trust again); its deny and ask rules always count. So that the model can't
quietly drop them, any edit in a project's `.gz/` folder (rules, hooks,
MCP servers, instruction rules) or of `.mcp.json` always asks, whatever
the mode, an earlier answer or an allow rule says (refused in don't-ask
mode and with `-p`). A rule or file gz can't
read is reported when the session starts. The older `[workspaces]` answers
in `permissions.toml` keep working unchanged. Editing rules with
`/permissions` (or saving an "always" answer) keeps the rest of the file,
comments included, also inside the `[workspaces]` tables: only the entries
that changed are added or removed, and keys gz doesn't know stay. An
emptied allow/ask/deny list stays as `[]`; in the `[workspaces]` tables
an emptied list's key goes, a new folder's table is added at the end of
the file, and a folder left with no answers loses its table unless you
put keys of your own in it.

**Permission modes.** `gz code --permission-mode <mode>` starts in a mode;
`default_mode = "<mode>"` in `permissions.toml` (or a trusted project
file) sets it for every session, and the flag wins:

| Mode | What gz does with calls no rule or grant settled |
|---|---|
| `ask` (also `default`) | asks you |
| `accept-edits` (`acceptEdits`) | runs edits and writes, asks about the rest |
| `plan` | refuses edits, commands, scripts and actions (read-only) |
| `auto` | auto mode's review decides (same as `--auto`) |
| `dont-ask` (`dontAsk`) | refuses at once, never asks: for unattended runs, with what they may do set by rules |

Shift+Tab cycles ask → auto → accept edits → plan; don't-ask joins the
cycle (after plan) when the session started in it or `dont_ask_in_cycle =
true` is set. Its banner reads `⊘ don't ask mode on`. Rules still come
first in every mode: an allow rule runs a call even in don't-ask mode, and
an ask rule prompts even in accept-edits or auto mode (plan and don't-ask
mode refuse instead). Plan mode stays read-only whatever allow rules and
earlier answers say. An ask rule's prompt offers only yes (once) or no,
since the rule asks every time.

In the plain terminal (no full-screen UI) and in script runs, the model
answers with its plan instead of asking to leave plan mode. In the plain
terminal, `/mode` shows the mode and `/mode ask|accept-edits|plan|dont-ask`
switches it, so `/mode ask` carries the plan out; a script run stays
read-only and ends with the plan.

`--allowedTools` and `--disallowedTools` (also `--allowed-tools`,
`--disallowed-tools`; repeatable, rules separated by commas or spaces) add
allow and deny rules for this session only, above every file:

```bash
gz code --permission-mode dont-ask \
  --allowedTools "read run_command(cargo test:*) edit(src/**)" \
  --disallowedTools "web_fetch" "fix the failing test"
```

**Auto mode.** Shift+Tab turns on auto mode (when the chat server reviews
actions): reads, edits and read-only commands run at once (the commands
only under the OS sandbox; without one they go to the review), and a
review decides each command,
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
  status`), with no permission prompt, even while gz is working. While the
  input starts with `!` its marker turns into a yellow `$` and the line
  under it says `! shell command · not sent to gz`. Its output
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

- `/rewind` (or `Esc` twice on an empty input while gz isn't working)
  lists your messages in this conversation, newest first (those
  that led to file changes say how many files and when). Notes you sent
  while gz was working are not listed: the chat keeps them inside the
  answer, not as messages of their own. A list or preview taller than the
  terminal scrolls with the selection and says how much is above and below. Choosing one shows
  what going back to before it restores, nothing changed yet: each file gz
  changed since, with the lines it undoes (`a.py · undoes +3 −1`, files gz
  created are deleted, files changed since gz wrote them are marked), and
  what happens to the conversation. Then pick:
  - **Restore code and conversation**, **Restore conversation only**, or
    **Restore code only** (code choices appear when gz changed files since).
  - The conversation goes back by copying it up to just before that
    message into a new conversation and carrying on there, with your
    message back in the input to edit and send. The original stays as it
    was; `/resume` opens it. The screen reprints the copy. What you
    allowed "for this chat" and attached documents stay with the original,
    so the copy asks again.
  - If a file changed after gz last wrote it (by you or a command), gz
    lists it and asks: put it back too, or leave it and put back the rest.
    After a code-only rewind your next message tells the assistant which
    files went back (`Told gz you rewound 2 files`), so it re-reads them.
  - Changes made by commands (the assistant's or your own `!commands`) are
    not tracked. A turn stopped before the chat saved it has no saved
    message to go back to; gz says so, and only files can go back to it.
  - Files rewound together with the conversation are not reported to the
    assistant (the copy never saw those changes); an earlier code-only
    rewind still is.
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
- **Reads** everywhere except the credential stores: `~/.ssh`; cloud CLIs
  (`~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.oci`, `~/.kube`,
  `~/.docker`); tokens and passwords (`~/.config/gh`, `~/.config/hub`,
  `~/.config/op`, `~/.netrc`, `~/.git-credentials`,
  `~/.config/git/credentials`, `~/.npmrc`, `~/.pypirc`, `~/.gem/credentials`,
  `~/.cargo/credentials(.toml)`, `~/.terraform.d/credentials.tfrc.json`,
  `~/.vault-token`, `~/.password-store`, `~/.gnupg`); keyrings
  (`~/Library/Keychains`, `~/.local/share/keyrings`); and browser profiles,
  whose cookies are signed-in sessions (Chrome, Brave, Edge, Firefox,
  Safari). `/sandbox` lists them all.
- **Keychain:** on macOS a command can't reach the keychain service either,
  since hiding the files alone wouldn't stop `security` or git's
  `osxkeychain` helper from handing out stored passwords and tokens.
- **Unix sockets** are closed, because a connection acts with the powers of
  the program behind it: the ssh-agent signs with your keys, Docker's
  socket is root on the machine, tmux's types into your shells. Commands may
  connect only to sockets in the open folders, to the system's name lookup
  (and its log on macOS), and to sockets in their own temp folder: gz sets
  `TMPDIR` to an owner-only `/tmp/gz-sandbox-<uid>`, so test servers and
  Python's `multiprocessing` keep working. A symlink or hard link to a
  closed socket stays closed. On Linux, abstract sockets (no file, e.g. some
  X11 and D-Bus ones) are closed with Landlock.
- **Network** is on, because installs and fetches need it.
  `/sandbox network off` blocks it except localhost.
- **Other apps and processes:** on macOS a command can't send Apple events
  or open apps and documents through LaunchServices (`open`), so it can't
  ask Terminal, Finder or System Events to act for it, or start an app it
  wrote. On Linux it runs in its own PID namespace, so it can't signal your
  other processes.
- When the sandbox stops a command, the assistant is told which rule did and
  the command that lifts it (for example `/sandbox allow ssh` after `Host key
  verification failed`, or `/sandbox allow socket /private/tmp/tmux-501/default`
  after tmux couldn't connect), and asks you rather than trying another way.
  Commands that write elsewhere fail until you widen it: `go install`
  (`~/go/bin`), `cargo install` and `rustup` toolchain installs
  (`/sandbox allow-write ~/.cargo/bin` or `/sandbox allow-write ~/.rustup`),
  global `npm -g` installs, `brew`, and editing your dotfiles or `~/.config`.

`/sandbox` shows the status: on or off, how it is applied, the writable
folders, the hidden stores, the reachable sockets, the network and what you
allowed. Two kinds of change:

- **Saved on the profile:** `/sandbox allow-write <path>` adds a folder,
  `/sandbox allow-read <path>` opens a path in a credential store, and
  `/sandbox on|off` and `/sandbox network on|off` switch it.
- **This session only**, since each hands commands a power over your
  accounts or machine: `/sandbox allow ssh` opens `~/.ssh` and the ssh-agent
  (`$SSH_AUTH_SOCK` and any `IdentityAgent` in `~/.ssh/config`), so `git
  push` over SSH works; `/sandbox allow keychain` opens the macOS keychain
  (git over https with the keychain helper needs it); `/sandbox allow socket
  <path>` opens one socket or a folder of them (Docker Desktop needs
  `/sandbox allow-read ~/.docker` and `/sandbox allow socket
  ~/.docker/run/docker.sock`). On Linux the keyring answers over the session
  D-Bus, which can also start services outside the sandbox, so it is opened
  as that socket, knowingly: `/sandbox allow socket $XDG_RUNTIME_DIR/bus`.

`/sandbox forget <n>` removes anything numbered. Every change applies from
the next command and is written to the audit log. On macOS the sandbox is
`sandbox-exec`; on Linux, bubblewrap (`bwrap`) when it is installed, plus
Landlock on Linux 6.12 and later.

What it does not stop:

- **The project's own files**, `Makefile` and `package.json` scripts
  included, are writable by design, and so are sockets in the open folders,
  which belong to the project. Read what the assistant changed before you
  run it.
- **With the network on**, a command can send out what it can read: the
  project, and anything you opened with `/sandbox allow…`. Turn the network
  off for work that doesn't need it.
- **On Linux before 6.12** (or where bubblewrap won't start under Landlock),
  abstract sockets stay reachable: they have no file for bubblewrap to hide,
  and only a separate network namespace would cut them off, which takes the
  network with it. gz says so once per session and in `/sandbox`.
- **On Linux, sockets made after a command starts** by programs outside the
  sandbox stay reachable to that command: bubblewrap covers each socket
  that exists when the command starts (every one in `/proc/net/unix`, and
  those in `/tmp`, `/run`, `/var/run` and `$XDG_RUNTIME_DIR`), and its
  mounts can't change while the command runs. macOS checks every
  connection, so this is Linux only.
- **On Linux without bubblewrap and on Windows** commands run without
  limits, and gz says so once per session (in a one-shot `gz code
  "<prompt>"`, on stderr unless `-q`).

**Environment snapshot.** When a session starts, gz records a few facts and
sends them with every message as the first instructions entry (named
`gz session environment`): today's date and weekday, the OS and CPU
architecture, your shell, the folder gz started in, and, inside a git
repository, the branch, the main branch (the remote's default, else
`main`/`master`/`trunk`), the last five commit subjects and whether the
working tree is clean. Never the full `git status`. The snapshot stays the
same for the whole session so the chat server's prompt cache keeps working;
the model is told to run git itself for the current state. Outside a git
repository gz doesn't run git at all. Inside one, each git call takes no
lock, asks for nothing, is abandoned after 3 seconds (the calls run in the
background from the moment gz code starts, so your first message rarely
waits for them), and runs with the
repository's `core.fsmonitor` switched off; when the repository's own
config (or a file it includes) sets a `filter.*.clean` or
`filter.*.process` program, gz skips `git status` and reports the working
tree as unknown, so opening a cloned repository never starts a program it
names.

**Instructions.** gz sends instruction files with every message (read
again each time, so edits apply to the next one), in this order:

- your personal `~/.config/gz/instructions.md` (honors `XDG_CONFIG_HOME`);
  `/instructions edit` opens it in `$EDITOR`, creating it if needed;
- `GZ.md`, `GZ.local.md`, `AGENTS.md` and `CLAUDE.md` from the current
  folder up to your home folder (or the top open folder outside home),
  nearest first. `GZ.local.md` is your personal file for one checkout: it
  goes to the model as your own instructions (which may import any
  non-secret file), and `/instructions` suggests a `.gitignore` entry while
  git would still commit it. It counts as yours only while git doesn't
  track it and its folder isn't one git ignores (a dependency or build
  output); outside a git repository, only one in the folder gz works in or
  above counts. Any other `GZ.local.md` is sent as project instructions,
  with project import limits, and `/instructions` says why;
- rule files in `.gz/rules/` (any `*.md`, subfolders too) of those folders;
- files picked up while gz works: once the model reads, lists or edits
  something in a subfolder (with a file tool or a `run_script` helper), that subfolder's `GZ.md`, `GZ.local.md`,
  `AGENTS.md`, `CLAUDE.md` and `.gz/rules` apply from the next message.
  They are added after the others, so earlier entries keep their place.
  `/clear` forgets them.

A rule file can limit itself to some files with `paths:` front matter
(globs relative to the folder holding `.gz`; a glob without `/` matches the
file name anywhere). It applies once the model reads or edits a matching
file. The front matter itself is not sent.

```markdown
---
paths: ["src/api/**/*.rs", "*.sql"]
---
API handlers return typed errors; migrations are append-only.
```

**Imports.** `@path` in an instruction file pulls another file in:
`@docs/style.md` (relative to the importing file), `@~/notes/rust.md`, or an
absolute path. Imported text is added to the importing file's entry under
an `[gz: imported from …]` line. Imports nest up to 5 deep, a file that
imports itself is skipped, and `@…` inside code spans or fenced code blocks
is ignored. Files in a project may only import files inside the open
folders or below their own folder; your personal files may import any
non-secret file. Secret files are never imported, and neither is a file a
deny or ask read rule covers (`read(secrets/**)`): it would reach the model
without the prompt the rule calls for. A project instruction file such a
rule covers isn't sent either; `/instructions` names the rule.

At most 8 entries go out (the environment snapshot takes one), each at most
20,000 characters including its imports: a file's own text is cut with a
note, and an import that doesn't fit is left out whole and named in a
closing note. Identical files go once, and secret files never.
`/instructions` lists every entry with its size and why it applies, the
files it imports, every import that was skipped and why, and the files not
sent (past the 8-entry limit, or a rule whose front matter can't be read).

**Memory.** The assistant remembers short facts about how you work ("Uses
snake_case for Python") and gets them with every message. It saves, updates
and deletes them itself, without a prompt, and each change shows in the
transcript (`Saved memory: …`) and the audit log (by id). Memories live in
`~/.config/gz/memory.json` (project ones in `memory-projects.json` next to
it, which older gz versions leave alone), per profile, in two scopes:

- **everywhere** (ids `m_…`): facts about you that hold in any folder;
- **this project** (ids `p_…`): facts about one project, kept per project
  folder (the git repository holding the folder gz started in, else that
  folder) and sent only in sessions started there. The model files a fact
  here by starting it with `[project]` (the environment snapshot tells it
  how); gz drops the tag.

Project memories go out first, then the ones for everywhere: at most 200
in all, each up to 500 characters, 20,000 in all. Text that looks like a
key, token or password, or holds control or invisible characters, is
refused.

- `/memory` lists both scopes with their ids.
- `/memory add <text>` saves one for everywhere; `/memory add --project
  <text>` for this project; `/memory edit <id> <text>` rewrites one;
  `/memory move <id>` moves it to the other scope (it gets a new id).
- `/memory forget <id>` deletes one; `/memory clear` deletes yours for
  everywhere and this project, `/memory clear project` only this
  project's (each after confirming with `yes`, e.g. `/memory clear yes`).
- `/memory off` stops saving and sending memories (remembered per profile);
  `/memory on` turns it back on.

Folders, instructions and memory need a chat server that advertises
`features.localContext`; older servers get neither the extra tools nor the
context.

**Context and compaction.** The chat summarizes older
messages once the conversation reaches a size you choose, so long sessions
don't resend a huge prompt on every call. The footer counts down
(`120k until auto-compact`).

`/usage` shows how full the conversation is against the model's budget (the
tokens Groundzero lets a conversation use on that model, which can be less
than the model's advertised window), the session's tokens and cost, and its
time: working time, time spent waiting for your answers to prompts, and how
long the session has been open. Waiting time is never counted as work, in
`/usage` or in the `Worked for` line after a turn. An interrupted turn keeps
the last known fill instead of dropping the countdown. Without a budget
limit, the account's Budget section shows only that no limit is set.

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
- `/context` shows what fills the window: a bar of the total the server
  reported after the last reply, then system prompt and tools, instructions,
  memories, the skills index, MCP tools, messages, free space and the
  auto-compact buffer, each with its share of the window. The server reports
  only the total, so gz estimates the parts it sent itself (instructions,
  memories, skills) and the conversation it showed at 4 characters per
  token, marked `~`; system prompt and tools are what the total leaves.
  Local MCP tools are listed on demand, so the MCP tools row is the tool
  lists and results the model asked for so far.
  Before the first reply only the estimated parts are shown.

**Writing GZ.md.** `/init` asks the model to study the folder (manifests,
build and test commands, CI config, layout, conventions) and write `GZ.md`,
or improve the one there. It runs as a normal turn, so the edit asks for
permission like any other; `/init <focus>` adds what to pay attention to.

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

While a turn runs, token counts update as each step finishes (the spinner
shows `↑ 12.4k ↓ 380 tokens`). The server reports the turn's cost only when
it ends, so in the meantime gz estimates it from the dollars per token it
saw on your earlier turns with the same model (kept in `code_spend.json`)
and marks it with `~`: `~$1.31 this session`, `· ~$0.0310` on the spinner.
The real figure replaces the estimate when the turn ends. Before the first
finished turn on a model there is nothing to estimate from, so only the
tokens show live. The spinner drops the cost, then the counts, when the
terminal is too narrow for them.

A turn that runs local tools takes several steps, and the server bills each
step as a run of its own but reports only the last one's cost. gz prices
the earlier steps at the rate of that last step and adds them, marked with
`~`: the turn line reads `~$0.0500` and `/last` and `/usage` show both parts
(`$0.0300 reported · ~$0.0200 estimated for earlier steps`). These
estimates count toward your caps and the local ledger, so a cap is not
reached late; the session total then shows with `~` too. When no rate is
known yet, the turn line says how many earlier steps were not priced.

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
calls, and every edit still shows its diff unless edits are allowed: "Yes,
for this chat" on any edit's prompt, from a script or not, allows the
script's later edits and every other edit in that chat. Scripts can't run commands through `gz`, stop after 120 s by
default, and are limited to 500 helper calls.

Needs a chat backend with client tools enabled (`CHAT_CLIENT_TOOLS_ENABLED`).

**Worktrees (`--worktree`).** `gz code --worktree fix-login` works in a
git worktree at `.gz/worktrees/fix-login` on a new branch `gz/fix-login`
made from HEAD, so the session's edits stay off your checkout (run it again
with the same name to reuse it). Without a name one is made up
(`session-…`); give the name, or put `--worktree` after the prompt, so a
prompt isn't taken for a name. It works for interactive and `-p` runs,
starts in the same subfolder you were in, keeps `.gz/worktrees/` out of
`git status` (through `.git/info/exclude`), and refuses outside a git
repository. Instruction files (`GZ.md`, `AGENTS.md`, `CLAUDE.md`) come
from the worktree, never from the main checkout it sits in, and a relative
`--add-dir` is read from the folder you started in. Your personal,
uncommitted `GZ.local.md` and `.gz/permissions.local.toml` come from the
main checkout until the worktree has its own, and what you trusted there
(project hooks, project permissions, `.mcp.json` servers) counts in the
worktree too, as long as the file is the same as the one you trusted. When gz code ends it
says (unless `-q`) what changed and how to commit, merge (`git -C <repo>
merge gz/fix-login`) or remove it (`git -C <repo> worktree remove
<repo>/.gz/worktrees/fix-login && git -C <repo> branch -D gz/fix-login`),
with full paths so the commands work from any folder.

**Scripted runs (`gz code -p`).** `-p`/`--print` runs once and never
prompts: actions nothing allowed are refused and reported. The prompt is
the argument or, without one, everything piped in.

```bash
gz code -p "why does the build fail?"                     # prints the final answer only
git diff | gz code -p "review this change"
gz code -p @task.md --output-format json                  # one result object
gz code -p "fix the test" --output-format stream-json     # one JSON event per line
gz code -p "fix the test" --max-turns 10 --max-budget-usd 0.50 --tools read_file,edit_file,run_command
gz code -p "triage" --bare --append-system-prompt "Answer in one paragraph."
```

`--output-format text|json|stream-json`, `--input-format stream-json`
(several messages, one JSON line each), `--include-partial-messages` (text
as it streams), `--max-turns`, `--max-budget-usd`,
`--tools`, `--append-system-prompt[-file]`, `--add-dir`, `--bare` and
`--verbose`, with every schema and exit code, are in
[docs/headless.md](docs/headless.md). Exit codes: `0` done, `1` failed,
`3` a `/budget` cap is spent, `4` an action was refused (it needed
approval, or a mode or deny rule blocked it), `5`
`--max-turns` reached, `6` `--max-budget-usd` reached, `130` interrupted.

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
  "usage": {"inputTokens": 100, "outputTokens": 20, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0, "costUsd": 0.0042,
            "estimatedCostUsd": 0, "unpricedLegs": 0},
  "durationMs": 5210,
  "status": "completed"
}
```

- `answer` is what the model wrote after its last tool call; `model` the
  model it ran on (`null` when the model list couldn't be loaded).
- `toolCalls[].decision` is the audit log's word: `preapproved` (ran
  without asking: a saved rule, `--yes-reads`, a read in the open folders,
  a read-only command or script or a safe file command (the audit log says
  `read-only` or `accept-edits` for those), or a tool that needs no
  permission), `auto` (auto mode's review allowed it), `blocked` (refused,
  with a `reason`), or `server` (a tool the chat server ran itself).
- `usage.costUsd` covers every leg of each turn. The server prices only a
  turn's last leg (each client-tools resume is its own run), so gz prices
  the earlier legs at the rate the last one showed: `estimatedCostUsd` is
  that estimated part, and `unpricedLegs` counts earlier legs left out of
  `costUsd` because no rate was known yet.
- `subagents` (`[{name, status}]`) appears when subagents ran; `error` when
  `status` is `failed`. `status` is `completed`, `failed` or `interrupted`.
- Exit codes: `0` completed; `1` failed (the report's `error` says why);
  `3` a `/budget` cap is spent, nothing sent; `4` completed, but an action
  was refused because it needed approval (no one to ask, or auto mode's
  review blocked it, or a deny rule or don't-ask mode refused it); `5` stopped by `--max-turns`; `6` stopped by
  `--max-budget-usd`; `130` interrupted (Ctrl+C).
- `-o json|yaml` and `--output-format` are two ways to ask for machine
  output; use one (`--output-format json` gives the snake_case result
  object in [docs/headless.md](docs/headless.md)).

An action nothing allowed ahead of time is refused at once, never waited
on: the model is told no one was there to approve it and carries on. Allow
what a CI job needs before it runs:

- `--auto`: auto mode's review decides each command, script, protected
  edit, web fetch and Groundzero action from your prompt; reads and edits in
  the folder run at once. Needs a chat server with `features.actionReview`;
  anything the review would ask a person about is refused.
- `--yes-reads`: every read and search in the folder.
- `--allowedTools "<rules>"` and `--disallowedTools "<rules>"`, or the
  `allow` / `deny` lists in `permissions.toml` (see **Permission rules**);
  `--permission-mode dont-ask` makes it explicit that nothing else is
  asked about.
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

**Hooks.** Your own commands that run at points in a session: when it
starts and ends, when you send a message, around each tool call, when the
agent or a subagent stops, before compaction and when gz needs you. Put
them in `~/.config/gz/hooks.toml` (honours `XDG_CONFIG_HOME`) or, for a
project, in `.gz/hooks.toml` in the folder you open:

```toml
# Format Rust files after the assistant edits them.
[[post_tool_use]]
matcher = "edit_file|write_file"    # a tool name, a|b, or a regular expression
path = "*.rs"                       # a glob; with a "/" it is a path from the current folder
command = 'rustfmt "$GZ_HOOK_PATH"'

# Refuse commands that push, before any permission prompt.
[[pre_tool_use]]
matcher = "run_command"
command = '''
if jq -r .input.command | grep -Eq 'git push'; then
  echo "pushing is for humans" >&2; exit 2
fi
'''
timeout = 5                         # seconds; 30 by default

# Tell the model which branch it is on, once per conversation.
[[session_start]]
command = "echo \"on branch $(git branch --show-current)\""

# Don't let the agent stop while the build is broken.
[[stop]]
command = '''
jq -e .stopHookActive >/dev/null && exit 0
out=$(cargo check -q 2>&1) || { echo "$out" | tail -5 >&2; exit 2; }
'''
```

The events (`/hooks` lists them all):

| Event | Runs | Matcher is matched against | Exit 2 / a block answer |
|---|---|---|---|
| `session_start` | before a conversation's first message | `startup`, `resume` or `clear` | — (stdout goes to the model) |
| `user_prompt_submit` | when you send a message, before the model sees it | — | the message is not sent; you see why |
| `pre_tool_use` | before the permission check | the tool name | the call is denied without a prompt; the reason goes to the model |
| `before_tool` | after `pre_tool_use`, still before the permission check | the tool name | the call is blocked without a prompt; the reason goes to the model |
| `post_tool_use` | after a call that worked | the tool name | stderr is added to the result the model gets |
| `post_tool_use_failure` | after a call that failed | the tool name | stderr is added to the result the model gets |
| `after_tool` | after every call | the tool name | stderr is added to the result the model gets |
| `notification` | when gz shows you a permission prompt or a question (the prompt does not wait) | `permission` or `question` | — (what it prints shows as a dim line) |
| `subagent_stop` | when a subagent finishes | the subagent's name | a background subagent carries on with the reason |
| `stop` | when the agent finished its answer | — | the agent carries on with the reason as its next message (at most 8 times a turn) |
| `turn_end` | when a turn completed | — | — |
| `pre_compact` | before `/compact` (`manual`), or at your next message once the conversation passed the auto-compact size (`auto`: report only, and the timing is approximate, since the chat compacts by itself inside its runs and may already have) | `manual` or `auto` | a `/compact` is not run (the chat's own compaction cannot be stopped) |
| `session_end` | when gz code exits, or for a conversation `/clear` or `/resume` left (run as the next message starts, or at exit if none follows) | `exit`, `clear` or `resume` | — |

- Exit 0 is fine: its output shows dimmed; for `session_start` and
  `user_prompt_submit` stdout is also added for the model (after your
  message, in `<hook_context>`). Exit 2 blocks as in the table. Any other
  exit, a timeout or a crash warns dimly and carries on.
- Instead of an exit code a hook can print one JSON object on stdout (and
  exit 0): `{"decision": "allow"|"deny"|"ask"|"block", "reason": "…",
  "additionalContext": "…", "continue": false, "stopReason": "…",
  "suppressOutput": true}`. For `pre_tool_use`, `allow` runs the call
  without a prompt (plan mode still refuses edits and commands, and
  opening another folder still asks), `deny` refuses it and `ask` shows
  the prompt (with the reason) even when a rule, accept-edits or auto mode
  would allow it; when hooks disagree, deny wins
  over ask over allow. A hook's `allow` never overrides a deny or ask
  rule. The order for every call (in the terminal, with `-p`, in script
  helper calls and in subagents) is: `pre_tool_use` then `before_tool`
  hooks, deny rules, ask rules, allow rules and grants, the mode, gz's
  automatic read-only approvals, then the prompt. `additionalContext` reaches the model (after a tool
  result, or with your message). `"continue": false` stops the turn (the
  stop reason shows). `suppressOutput` hides the hook's stdout.
- Each hook gets JSON on stdin: `event`, `conversationId` and `cwd`, plus
  for tool events `tool`, `summary`, `input` and (after a call)
  `result: {isError, output}`; `source` (`session_start`), `reason`
  (`session_end`), `prompt` (`user_prompt_submit`), `stopHookActive`
  (`stop`, `subagent_stop`, true when the agent is already carrying on
  because of a hook), `subagent`, `status` and `summary`
  (`subagent_stop`), `trigger` and `instructions` (`pre_compact`), `kind`
  and `message` (`notification`). The environment has `GZ_HOOK_EVENT`,
  `GZ_HOOK_CONVERSATION`, and for tool events `GZ_HOOK_TOOL` and
  `GZ_HOOK_PATH` (the file the call targets).
- `matcher` must match the whole subject; `*` or leaving it out matches
  everything. A plain `a|b` list for a tool event must name real tools, so
  a typo is reported instead of silently never matching. Tool events also
  take `tools = [...]` and a `path` glob.
- `pre_tool_use`, `post_tool_use` and `post_tool_use_failure` also run
  for `change_directory`, `memory`, `load_skill` and the session's own
  tools (`ask_user_question`, `todo_write`, `command_output`,
  `stop_command`, `enter_plan_mode`, `exit_plan_mode`). For these a
  `deny` refuses the call; `allow` and `ask` change nothing, since opening
  a folder outside the open ones always asks. `before_tool` and
  `after_tool` cover the other tools only, as before.
- All hooks that match an event run at the same time, except
  `before_tool`, `after_tool` and `turn_end` hooks, which run one by one in
  order (yours, then the project's), so a formatter can run before a
  linter; a `before_tool` block stops that chain. The same command listed
  twice for an event runs once.
- Hooks run as you, unsandboxed, through `sh -c` in the session's current
  folder, around background subagents' calls and script helper calls too.
  One that runs past its timeout is stopped. Every run is in the audit log
  (decision `hook`, the outcome only).
- A project `hooks.toml` comes from the repository, so its hooks run only
  after you trust that exact file: gz lists every hook in full and asks
  once, and asks again when the file changes. Trust is kept by content hash in
  `~/.config/gz/hooks-trust.json`.
- `/hooks` lists every event and each hook with its source, trust and last
  result; `/hooks off|on` switches them for the session; `/hooks trust`
  reviews the project file again. A broken `hooks.toml` is reported by
  name and gz starts without it.

**Reviews.** `/review` asks the assistant to review the uncommitted
changes (staged, unstaged and new files); `/review <branch>` reviews a
branch against the branch it came from; `/review 123`, `/review #123` or
`/review <PR URL>` reviews a pull request through the GitHub CLI (`gh`,
signed in; without it gz says so). `/security-review` takes the same
targets and looks only for security problems. The request goes out as a
normal message: the assistant reads with git, `gh` and its read tools,
changes nothing, and reports findings with file, line, severity and a
suggested fix. Typed while gz is answering, a review waits for that
answer to end.

**Your own slash commands.** A Markdown file in `.gz/commands/` (in the
folder you open, for the project) or in `~/.config/gz/commands/` (yours,
honours `XDG_CONFIG_HOME`) is a slash command named after the file:
`review-api.md` is `/review-api`, and a subfolder is a namespace, so
`frontend/component.md` is `/frontend:component`.

```markdown
---
description: Make a React component      # shown in the / menu and /help
argument-hint: <name> [props]            # shown after the description
model: opus                              # optional; this message only
---
Create the component $1 with the props $2 ($ARGUMENTS in full).
Follow @docs/components.md.
```

- The body is sent as your message. `$ARGUMENTS` is everything after the
  name; `$1` to `$9` are its words (quotes keep words together). When the
  body uses neither, what you typed after the name is added at the end.
- `@path` in the body attaches that file, like an `@` mention you type.
- Without a `description` the first line of the body is used. The front
  matter is optional.
- `model` runs just that message on the named model (`/models` names
  them), then goes back to yours; an unknown name is an error that names
  the file.
- Built-in commands win a name clash, and a project command wins over
  yours; `/help` says which files are not used and why, and lists files it
  could not read. New or changed files are picked up the next time you
  type a command. Typed while gz is answering, a command goes to that
  answer like any message, or, with its own `model`, waits and runs as
  its own turn.
- They work the same in a one-shot `gz code "/review"` and in the plain
  line mode, as do `/review`, `/security-review` and `/<skill>`. There a
  command file's `model` needs the model list: pass `--model` (or use the
  full-screen view).

**Getting to the first word fast.** While you type, gz keeps the chat
server ready: it renews your sign-in when it has less than 10 minutes left
and opens a connection with the server's public `/health` route (again at
most every 20 s while you type, since load balancers close idle
connections), so Enter sends at once instead of first waiting on a token
refresh and a TLS handshake. To see where the time goes, set
`GZ_TRACE_TIMING=1` (writes `timing.log` in the gz config folder) or
`GZ_TRACE_TIMING=/path/to/file`; each turn appends when each step happened,
counted from Enter:

```text
turn 1 +0.0 ms enter
turn 1 +0.2 ms session took the turn
turn 1 +0.5 ms request sent: POST /chat/runs
turn 1 +0.9 ms response headers: POST /chat/runs
turn 1 +1.5 ms request sent: GET /chat/runs/r1/events
turn 1 +1.9 ms response headers: GET /chat/runs/r1/events
turn 1 +1.9 ms first event
turn 1 +2.0 ms first text
turn 1 +3.2 ms first text painted
```

Everything after `request sent` and before `first event` is the network
and the server; the rest is gz.

**Diagnostics after edits.** When the assistant edits a file
(`edit_file`, `write_file`, `notebook_edit`, or a script's helper edits),
gz runs a fast checker on that file and adds the problems the edit
introduced to the tool's result, so the model fixes them in the same turn.
You see a notice such as `ruff found 1 new problem in app.py`.

- Built in, used only when installed on your `PATH`: `ruff` (Python and
  notebooks, run with `--no-fix` so it never edits), `shellcheck` (shell
  scripts) and `go vet` (the file's package). Slower whole-project checkers
  (tsc, eslint, cargo check, mypy) run only when you configure them.
- When the file existed, the checker also runs just before the edit, and
  only problems that are new are reported (matched by message and line
  text, so problems that merely moved are not repeated). At most 20
  problems are added per check. A script's edits are checked once per
  file when the script ends.
- Every run is time-boxed (10 s by default) and stopped at its timeout, so
  a hung checker never holds up the turn; you get a notice instead. After a
  timeout gz no longer runs that checker before edits (it then reports all
  problems in the file, not just new ones), and after three timeouts in a
  row it stops running it for the rest of the session.
- Notebook problems name their cell: `n.ipynb cell 2 line 1 col 8: …`.

Settings live in `~/.config/gz/diagnostics.toml` (honours
`XDG_CONFIG_HOME`). Every key is optional:

```toml
enabled = true        # false turns diagnostics off
auto = true           # false: only the checkers below, no built-in ones
timeout_secs = 10     # per checker run; 1 to 600

[[checkers]]
name = "tsc"
folder = "~/code/web"                 # the project it applies to; it runs there
extensions = ["ts", "tsx"]            # empty or missing: every file
command = "npx tsc --noEmit -p . --pretty false"
timeout_secs = 60
compare_before = true                 # false: skip the run before the edit

[[checkers]]
name = "ruff"                         # replaces the built-in ruff
extensions = ["py"]
command = "ruff check --output-format concise --no-fix --select E,F {files}"
```

- `{files}` becomes the edited file, quoted, relative to the folder the
  checker runs in. Without it the command runs as written (a whole-project
  checker) and only lines about the edited file are kept, however much
  else it prints.
- A checker must print problems as `path:line[:col]: message` (ruff
  concise, shellcheck `-f gcc`, go vet, eslint `-f unix` (on ESLint 9 and
  later, install `eslint-formatter-unix` first), mypy,
  `cargo check --message-format short`) or `path(line,col): message`
  (tsc with `--pretty false`), on stdout or stderr.
- Checkers run as you, unsandboxed, through `sh -c`, like hooks. That is
  why they are set in your own config folder, never by a repository, and
  why built-in checkers are looked for only in absolute `PATH` folders
  (never `.` or `bin`, which would point into the project).
  A broken `diagnostics.toml` is reported at the first edit and gz keeps
  the built-in checkers.

**Your own MCP servers.** The interactive `gz code` can use MCP servers that
run on your machine (stdio) or remotely (Streamable HTTP, or the older
HTTP+SSE transport), with OAuth sign-in where a server asks for it.

Add them with `gz mcp add` (options go before the command or URL):

```sh
gz mcp add files --scope user -- npx -y @scope/server /Users/me/notes
gz mcp add tracker -e TRACKER_TOKEN='${TRACKER_TOKEN}' -- tracker-mcp stdio
gz mcp add docs https://docs.example.com/mcp            # remote, Streamable HTTP
gz mcp add legacy -t sse https://old.example.com/sse    # remote, HTTP+SSE
gz mcp add api -H 'X-Api-Key: ${API_KEY}' https://api.example.com/mcp
gz mcp list                 # every scope, with status: connected, failed, needs login…
gz mcp get docs             # settings (values masked), status and tools
gz mcp login docs           # OAuth: opens the browser and prints the URL
gz mcp logout docs
gz mcp approve team-tools   # a project server (or --decline)
gz mcp remove files
```

Three scopes; the same name in several means local, then project, then user:

- **user** (`--scope user`): `~/.config/gz/mcp.toml` (honors
  `XDG_CONFIG_HOME`), for every project.
- **project** (`--scope project`): `.mcp.json` (or `.gz/mcp.json`) in the
  project, checked in for the team, under `"mcpServers"`. A project server
  runs only after you approve that exact entry (`gz mcp approve <name>` or
  `/mcp approve <name>`); a changed entry asks again. Until then gz never
  contacts it, `gz mcp login` included, and doesn't offer the model the MCP
  tools when no other server can run. Approvals are kept by
  content hash in `~/.config/gz/mcp-trust.json`. `gz code` lists the servers
  waiting for approval when it starts.
- **local** (`--scope local`, the default): `~/.config/gz/mcp-local.json`,
  for this project only and private to you.

The files take the same fields; `mcp.toml` looks like this:

```toml
[servers.files]
command = "npx"
args = ["-y", "@scope/server", "/Users/me/notes"]
env = { TRACKER_TOKEN = "${TRACKER_TOKEN}" }
timeout_seconds = 120          # per request; default 60, at most 600
startup_timeout_seconds = 300  # starting; default 120 (or timeout_seconds if longer)
max_output_chars = 10000       # per tool result; default: gz's cap on any tool result

[servers.docs]
url = "https://docs.example.com/mcp"   # type = "http" is implied; "sse" for HTTP+SSE
headers = { "X-Api-Key" = "${DOCS_KEY}" }
oauth = { client_id = "...", client_secret = "${DOCS_SECRET}", scopes = "read", callback_port = 8765 }
```

and `.mcp.json`:

```json
{"mcpServers": {"team-tools": {"type": "http", "url": "https://tools.example.com/mcp"}}}
```

- `${NAME}` and `${NAME:-default}` in `command`, `args`, `env`, `url`,
  `headers` and the `oauth` settings are filled from gz's own environment
  when the server starts, so the files need no secrets. Server names are
  letters, digits, `_` and `-`. A broken file or entry is reported with the
  file and the problem; the other servers still load.
- **OAuth.** When a remote server answers 401 it shows as *needs login*.
  `gz mcp login <name>` finds the server's authorization server
  (protected-resource and authorization-server metadata), registers gz as a
  client when the server allows it (else set `oauth.client_id`, and
  `oauth.callback_port` if the client was registered with a fixed redirect),
  then signs in with authorization code and PKCE through
  `http://127.0.0.1:<port>/callback`. The browser opens only for this
  command (`--no-browser` just prints the URL). Tokens are kept owner-only in
  `~/.config/gz/mcp-auth.json`, refreshed before they expire and after a
  401, and sent only over https or to this machine. The server's metadata
  must name the server itself as its `resource`, and the authorization
  server's metadata its own `issuer`, or gz refuses to sign in through
  them. In a running `gz code`,
  `/mcp reconnect <name>` picks up the new sign-in (the next use does too).
- A server starts or connects the first time the model uses it and stops
  when gz exits. A local server's stderr goes to
  `~/.config/gz/mcp-logs/<name>.log` (capped at 1 MB), never the screen.
- The model lists the servers' tools without asking (`local_mcp_tools`).
  Listing them all starts every server at once and waits up to 20 seconds;
  one still starting is reported as busy and the model can ask for it by
  name. When a server announces `notifications/tools/list_changed`, gz lists
  its tools again.
  Each call (`local_mcp_call`) asks first, like a command, with the tool's
  arguments: "for this chat" allows that server's tool for the chat, and
  "always" saves a rule for that tool in this folder, listed and revocable
  in `/permissions`. Auto mode reviews every call; plan mode refuses them.
  Hooks run around calls too: name `local_mcp_call` in a hook's `tools`.
- Text results go back to the model, cut to fit one tool result (see
  **Safe edits**), or earlier at the server's `max_output_chars` when it
  sets one, with a note saying so; images and
  other content are described in one line.
- If a local server crashes (or a remote one drops), gz starts it again
  once on the next use; after a second failure it stays failed until
  `/mcp reconnect <name>`. A remote server that forgets gz's session is
  reconnected at once, also when that happens while gz relists its tools.
- A remote server that stops answering is given up on after its
  `timeout_seconds`; messages that need no answer wait at most 5 seconds.
  gz follows a remote server's redirects only within its own origin, since
  every request carries the server's headers; a redirect elsewhere is
  reported, so you can set the new URL. One message (or event) over 8 MiB
  ends the connection.
- `/mcp` lists the servers with their scope, transport, masked command or
  URL and state (not started, connected with its tools' names, busy, needs
  login, needs approval, declined, crashed or failed with the reason); it
  never waits for a busy server. `/mcp reconnect <name>`, `/mcp approve
  <name>` and `/mcp decline <name>` act on one.

Needs a chat server that lists `local_mcp_tools` and `local_mcp_call` in its
client tools; with an older one gz doesn't offer them.

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

- Skills live in `~/.config/gz/skills/<name>/`, and a project's own in
  `.gz/skills/<name>/SKILL.md` in the folder you open (`gz skills list`
  shows them when run there). A project skill comes with whatever
  repository you open, so it is scanned at every load like a team skill:
  HIGH findings block it, and one with MEDIUM findings stays off until you
  review and trust that exact version (`/skills trust <name>`, Enter on it
  in `/skills`, or `gz skills trust <name>`); any change asks again. Any
  action a skill leads to still asks. Extra folders are additive; each
  must hold skill folders and can't be a skill itself. A personal skill
  wins a name clash, then team skills, then a project skill, then folders
  in the order you added them; the losers show as `shadowed`.
- In `gz code`, `/skill-name [text]` loads that skill and sends the text
  with it (a command file of the same name wins); skills show in the `/`
  menu too.
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

## Why someone can or can't use a table (`gz access why`)

```sh
gz access why me Main.sales.customers                    # may I select it?
gz access why bob Main.sales.customers --action delete   # site admins may ask about anyone
gz access why bob Main.sales.customers -o json
```

The lakehouse provider runs its own query authorizer on the statement the
user would run (so `delete` is checked as the Update permission it needs) and
returns the decision, the check that settled it, and every check it made:
the database and warehouse permissions that name the user or their groups,
and the table's own rules, each marked as applying or not. For `select` it
adds the row filters and column masks the user's reads get, and what AI
models get under the model read policy. Groups come from your sign-in, or
for someone else from the user directory; you can't name groups yourself.
Actions: select, insert, update, delete, truncate, merge, alter, drop.

## Find personal data in a table (`gz govern scan`)

```sh
gz govern scan Main.sales.customers                  # read-only; first 2,000 rows
gz govern scan Main.sales.customers --sample 10000
gz govern scan Main.sales.customers --apply          # tag what it found; asks first (--yes to skip)
```

Reads a bounded sample through a lakehouse SQL session (under your scan limit,
`--max-scan` or `sql_max_scan`) and flags columns that look like email,
phone numbers (Indian, North American, UK, international), payment cards
(Luhn), IP addresses, PAN, Aadhaar (Verhoeff), US SSN or IBAN, plus dates of
birth, names and addresses when the column's name agrees (names and
addresses never above medium confidence). Each column shows its kind, how
many sampled values matched, a confidence and a masked example; raw values
are never printed. `--apply` tags medium and high confidence columns
`pii=<kind>`, keeping their other tags and any `pii` tag someone already
set; tagging needs the admin tier.

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
(`gz sql --help`).

### Tab completion

`gz completion <shell>` prints a script that completes subcommands, flags
and real values: project, job, dataset and other names; warehouses and
`WAREHOUSE.DATABASE.TABLE` one part at a time (`DATABASE.TABLE` where a
command takes `--warehouse`); a table's branches and tags for `--at`,
`--from` and `--to`; profiles; compute sizes and runtimes. One line in your
shell's startup file:

| Shell | Line | File |
|---|---|---|
| bash | `eval "$(gz completion bash)"` | `~/.bashrc` |
| zsh | `source <(gz completion zsh)` (after `compinit`) | `~/.zshrc` |
| fish | `gz completion fish \| source` | `~/.config/fish/config.fish` |
| PowerShell | `gz completion powershell \| Out-String \| Invoke-Expression` | `$PROFILE` |

Completion never asks anything and never waits long. Names come from a small
cache per profile, `completion/<profile>.json` in the gz config folder
(names and ids only, never a token): a list older than five minutes is
shown while a background `gz` reads it again, and a list never read is
fetched with a 1.5 s limit. Not signed in, it completes nothing.
`gz completion elvish` still prints a script for subcommands and flags only.

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

### Reporting a bug

```bash
gz bug-report                 # writes gz-bug-report-<time>.md here and prints its path
gz bug-report --out report.md # somewhere else
gz bug-report --stdout        # print it instead
gz bug-report --errors 50     # more failures (default 20, at most 50)
```

The file holds gz's version, OS and architecture, where gz is installed and
how (Homebrew, Scoop, winget, cargo, install script), the active profile's
settings, the host each service resolves to, and the last request failures:
time, gz command, method, URL, HTTP status and the server's request id.

Before anything is written, tokens, keys, passwords and secrets are replaced
with `[redacted]`. That covers any setting whose name looks secret (nested or
unknown ones too) and any value shaped like a token. Your username and email
become `[redacted: personal]`, and your home folder is shown as `~`. Read the
file before you share it.

gz records failures in `errors.jsonl` next to `config.toml`, keeping the last
50. Each entry holds the method, the URL without its query string, the
status, the request id and the subcommand name. It never holds request or
response bodies, headers, tokens or argument values.

## Development

```bash
make build | make test | make check   # check = fmt + clippy -D warnings
make install-local
```

Releases: push a `v*` tag and CI builds Linux, macOS (ARM + Intel), and
Windows, attaches them with checksums, and generates notes.

After publishing, CI renders the Homebrew formula and the Scoop and winget
manifests from `packaging/templates` with the new checksums, and keeps them as
the run's `packaging` artifact. It then:
- pushes `Formula/gz.rb` to `Sarvesh-GanesanW/homebrew-gz` and `bucket/gz.json`
  to `Sarvesh-GanesanW/scoop-gz`, using the `PACKAGE_REPOS_TOKEN` secret;
- opens a `microsoft/winget-pkgs` pull request with `wingetcreate`, using the
  `WINGET_TOKEN` secret.

Without its secret, a step logs that it was skipped and does nothing. To
render the files by hand for a published release:

```bash
make packaging VERSION=0.10.0   # writes to target/packaging/
```

## License

Proprietary. Copyright (c) 2026 Groundzero. All rights reserved. The source
is visible for transparency only; see [LICENSE](LICENSE) for the limited
right to install and run the official releases with a Groundzero account.
