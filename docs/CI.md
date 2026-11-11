# gz in CI

Automation can't answer an MFA prompt, so CI uses a Groundzero **API key**
instead of `gz auth login`.

## What an API key can do

Keys are accepted only by these backends:

| Works with a key | Commands |
|---|---|
| ETL provider (all routes) | `gz jobs submit`, `jobs status`, `jobs logs`, `jobs abort`, `jobs metrics`, lineage, ontology, `gz impact` (see the README's Impact section) |
| Data provider: schema, connection tests, storage, OData | `gz data schema`, `data iceberg-schema`, `data test`, `data o-data`, `data o-data-meta`, `gz fs ls/lsd/list-all/mkdir/rm/rmdir/connections` |
| Agent builder reporting | `gz agents metrics`, `agents timeseries`, `agents audit` |

Everything else (lakehouse/SQL, chat, files upload/download, projects,
designer, schedules, `gz data read/execution/fingerprint`) needs a signed-in user; gz says so
instead of sending a request that would fail.

## Set up

1. Create a key once, signed in as the user CI should act as:
   `gz permissions api-key create <username> --use-case ci` (shown once; store it as a
   CI secret, e.g. `GZ_API_KEY`).
2. Export the job you want CI to run. `gz jobs run` reads the saved job from
   the projects service, which needs a user, so save its submit spec once:

   ```bash
   gz jobs run <project-id> <job-id> --compute <id> --rte gz-rte-spark-pytorch:1.0.0 \
     --spec-only > ci/nightly-job.json
   ```

   Commit `ci/nightly-job.json`.
3. In CI, `gz jobs submit --data @ci/nightly-job.json --follow` starts the run,
   streams its logs, and exits non-zero unless the run ends `Success`
   (`Failed`, `Aborted`, or a run that stops reporting fail the build).

`GZ_API_KEY` wins over any signed-in session for the routes that accept keys,
so the run is attributed to the key's user. Keys are only sent over https
(or to localhost).

## GitHub Actions

```yaml
name: nightly-etl
on:
  schedule: [{cron: "0 2 * * *"}]
  workflow_dispatch:

jobs:
  run:
    runs-on: ubuntu-latest
    env:
      GZ_CLIENT: stage
      GZ_SITE: preview
      GZ_DOMAIN: groundzerodev.cloud
      GZ_API_KEY: ${{ secrets.GZ_API_KEY }}
    steps:
      - uses: actions/checkout@v4
      - name: Install gz
        run: |
          curl -fsSL https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Run the nightly job
        run: gz jobs submit --data @ci/nightly-job.json --follow
```

Data-provider commands (`gz fs`, `gz data schema`) also need the site's
gateway URLs; run `gz auth login --api-key` once in the job (reading the key
from stdin: `echo "$GZ_API_KEY" | gz auth login --api-key`) to load them, or
set `gz configure --service data=https://<gateway>`.
