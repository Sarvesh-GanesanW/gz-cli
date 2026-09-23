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

## Setup

```bash
gz configure            # prompts: client, site, domain
gz auth login           # browser sign-in
gz doctor               # sanity check
```

That's it. Service URLs derive from your client and domain, the same way the
web UI builds them. If your workspace uses gateway URLs for data or MLflow
services, your admin will give them to you:

```bash
gz configure --service data=https://<gateway-url-from-your-admin>
```

No browser on the machine? `gz auth login --manual` signs in with
username, password, and MFA code. Tokens live in `~/.config/gz/config.toml`
with the file locked down, expire on their own, and `gz auth logout`
revokes them.

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

## TUI

`gz tui` opens a full-screen console: dashboard, warehouse browser, SQL
runner, job monitor, file browser, agents, and logs. `1-7` switch screens,
`?` shows all keys, `q` quits.

## Reference

`gz --help` lists every command; each group has its own help
(`gz sql --help`). Shell completions: `gz completion bash|zsh|fish`.

| Service key | What it talks to              | URL source          |
|-------------|-------------------------------|---------------------|
| lakehouse   | warehouses, tables, backups   | derived             |
| etl         | jobs, runs, lineage           | derived             |
| files       | file manager, site settings   | derived             |
| agents      | agent builder API             | derived             |
| chat, auth  | chat + sign-in                | derived             |
| logs        | log exports                   | derived             |
| data        | data connections              | `--service` one-off |
| mlops       | notebooks, MLflow             | `--service` one-off |
| mlopsadmin  | MLflow users, deploy          | `--service` one-off |
| iceberg     | engine internals              | `--service` one-off |

Config layers: flags beat env (`GZ_PROFILE`, `GZ_TOKEN`, `GZ_CLIENT`,
`GZ_SITE`, `GZ_DOMAIN`) beat the config file.

## Troubleshooting

| Symptom                   | Fix                                              |
|---------------------------|--------------------------------------------------|
| `no client configured`    | run `gz configure`                               |
| `401/403` from a provider | `gz auth login`; check `gz auth status`          |
| install script 404s       | releases page directly, or check your network    |
| loopback bind fails       | `gz auth login --port 0` picks a free port       |

## Development

```bash
make build | make test | make check   # check = fmt + clippy -D warnings
make install-local
```

Releases: push a `v*` tag and CI builds Linux, macOS (ARM + Intel), and
Windows, attaches them with checksums, and generates notes.
