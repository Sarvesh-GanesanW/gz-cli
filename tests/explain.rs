//! `gz explain dashboard` against a stub of every service it reads: the
//! Designer records, the connection, the table's snapshots and lineage,
//! the run list and the job's lineage. Checks the trace in text and JSON,
//! and that it only reads.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Never sent anywhere but the stub; the output must not show it.
const CONNECTION_SECRET: &str = "stub-password-do-not-print";
const RUN: &str = "6e36a1953ce3491fa4b4afcb78e9545f";

fn snapshot(id: u64, parent: Option<u64>, at: &str, total: u64, app: &str) -> Value {
    json!({
        "snapshotId": id.to_string(),
        "parentSnapshotId": parent.map(|p| p.to_string()),
        "committedAt": at,
        "operation": "overwrite",
        "summary": {
            "added-records": total.to_string(),
            "total-records": total.to_string(),
            "app-name": "InvoiceBulkExtract",
            "spark.app.id": app,
            "engine-name": "spark",
        },
    })
}

fn versions() -> Value {
    json!({
        "currentSnapshotId": "104",
        "snapshots": [
            snapshot(104, Some(103), "2026-11-06T05:36:20.094000+00:00", 100, "app-20261106053549-0000"),
            snapshot(103, Some(102), "2026-11-05T11:33:15.685000+00:00", 99, "app-20261105111827-0000"),
            snapshot(102, Some(101), "2026-11-05T07:46:49.239000+00:00", 98, "app-20261105073019-0000"),
            snapshot(101, None, "2026-11-05T06:53:23.204000+00:00", 90, "app-20261105064217-0000"),
        ],
        "branches": [{"name": "main", "snapshotId": "104"}],
        "tags": [],
    })
}

fn dataset() -> Value {
    json!({
        "Id": 1873, "Name": "WC-YeartoQuarter",
        "ConnectionNames": ["GZLakehouseProd"],
        "DataSourceQuery": "WITH yearly AS (\r\n  SELECT \"Year\", revenue27 FROM bi_working_capital\r\n  WHERE supplier_region IN {{WC-Region.value}}\r\n)\r\nSELECT \"Year\" AS Year, ROUND(SUM(revenue27), 0) AS Revenue\r\nFROM yearly GROUP BY \"Year\"",
        "TransformationQuery": "outputDF = queryData",
        "Config": {"ConnectionType": "GZLAKEHOUSE", "isETLJob": false},
        "filters": {"1273": {
            "filterName": "WC-Region", "filterType": "multiSelect",
            "filterConfig": {"columnName": "supplier_region",
                             "defaultItems": [{"value": "APAC"}, {"value": "Europe"}]},
        }},
    })
}

fn runs() -> Value {
    json!({"items": [
        {"executionId": RUN, "jobId": 1825, "jobName": "InvoicePDFToLakehouse",
         "projectId": 1405, "projectName": "Invoice-processing", "provider": "spark",
         "status": "succeeded", "workloadType": "job",
         "startedAt": "2026-11-05T07:30:12.487896+00:00",
         "completedAt": "2026-11-05T07:46:52.277779+00:00"},
        // Listed as running since August: its end was never recorded.
        {"executionId": "stale", "datasetId": "1567", "datasetName": "Odata",
         "provider": "spark", "status": "running", "workloadType": "dataset",
         "startedAt": "2026-08-28T04:55:25.354609+00:00", "completedAt": null},
    ], "pagination": {"limit": 100, "offset": 0, "total": 2}})
}

fn job_lineage() -> Value {
    json!({
        "versions": [{"job_version_id": 1399, "version_number": 6,
                      "compiled_on": "2026-11-05T07:21:17.618466+00:00"}],
        "graph": {
            "jobVersion": {"jobVersionId": 1399, "versionNumber": 6,
                           "codeHash": "3cf7e274fa207514a440b9ad6e2ba160",
                           "compiledOn": "2026-11-05T07:21:17.618466+00:00",
                           "sourceUri": "s3://bucket/preview/etlJob/1405/1825/resolved/3cf7e274.py"},
            "runs": [{"runId": 2542, "jobExecutionId": RUN}],
            "observations": [],
        },
    })
}

fn reply_for(method: &str, path: &str) -> Option<Value> {
    let reply = match (method, path) {
        ("GET", "/dashboard") => json!([{"Id": 1114, "Name": "Working-Capital"}]),
        ("GET", "/dashboard/1114") => json!({"config": {
            "tabs": [{"tabId": "1"}],
            "visualizations": {"1": ["1717"]},
            "visualizationDetails": {"1717": {"chartName": "WC-YearChart"}},
            "dashboardFilters": {},
        }}),
        ("GET", "/visualization/1717") => json!({
            "Id": 1717, "Name": "WC-YearChart",
            "config": {"chartType": "ColumnChart", "chartConfig": {
                "datasetName": 1873, "hAxis": {"title": "Year"},
                "selectedSeriesArr": [{"name": "Revenue"}],
            }},
        }),
        ("GET", "/dataset/1873") => dataset(),
        ("GET", "/connection") => {
            json!([{"Id": 1321, "Name": "GZLakehouseProd", "type": "GZLAKEHOUSE"}])
        }
        ("GET", "/connection/1321") => json!([{
            "name": "GZLakehouseProd", "type": "GZLAKEHOUSE",
            "config": {"warehouseName": "GZWarehousePROD", "databaseName": "DemoDB",
                       "password": CONNECTION_SECRET},
        }]),
        ("GET", "/lakehouse/tables/GZWarehousePROD/DemoDB/bi_working_capital/versions") => {
            json!({"status": 200, "response": versions()})
        }
        ("GET", "/lakehouse/governance/lineage/table") => json!({
            "nodes": [],
            "edges": [{
                "source": {"namespace": "DemoDB", "tableName": "invoices_raw"},
                "target": {"namespace": "DemoDB", "tableName": "bi_working_capital"},
                "operation": "INSERT OVERWRITE", "eventTime": "2026-11-05T07:46:50+00:00",
                "correlationId": "2542",
            }],
        }),
        ("GET", "/etl/consumption-metrics") => runs(),
        ("GET", "/etl/lineage/jobs/1825") => job_lineage(),
        _ => return None,
    };
    Some(reply)
}

fn stub() -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            seen.lock().unwrap().push(request.clone());
            let path = request.target.split('?').next().unwrap_or_default();
            match reply_for(&request.method, path) {
                Some(reply) => respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    &reply.to_string(),
                ),
                None => respond(&mut stream, "404 Not Found", "application/json", "{}"),
            }
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-explain-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    cmd.env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "explain-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme");
    cmd
}

/// Every request is a GET, and none of them can settle a live run.
fn assert_only_reads(log: &Arc<Mutex<Vec<Recorded>>>) {
    for request in log.lock().unwrap().iter() {
        assert_eq!(
            request.method, "GET",
            "{} {}",
            request.method, request.target
        );
        assert!(
            request.body.is_null(),
            "a read sends no body: {}",
            request.target
        );
        assert!(
            !request.target.contains("current-run") && !request.target.starts_with("/etl/logs"),
            "{}",
            request.target
        );
    }
}

fn query_of(log: &Arc<Mutex<Vec<Recorded>>>, path: &str) -> String {
    log.lock()
        .unwrap()
        .iter()
        .find(|r| r.target.starts_with(&format!("{path}?")))
        .map(|r| r.target.clone())
        .unwrap_or_else(|| panic!("no request to {path}"))
}

#[test]
fn explain_dashboard_traces_a_figure_to_the_run_that_wrote_its_table() {
    let dir = temp_dir("text");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args([
            "explain",
            "dashboard",
            "working-capital",
            "--at",
            "2026-11-05T08:00",
        ])
        .output()
        .unwrap();
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    for part in [
        "Explaining dashboard Working-Capital (1114), as of 2026-11-05 08:00 UTC",
        "  WC-YearChart (visualization 1717, ColumnChart) · dataset WC-YeartoQuarter (1873)",
        "    measure  Revenue = ROUND(SUM(revenue27), 0)",
        "    by       Year",
        "    filters  WC-Region (supplier_region): APAC, Europe",
        "Dataset WC-YeartoQuarter (1873) · SQL on GZLakehouseProd → GZWarehousePROD (default database DemoDB)",
        "  reads    GZWarehousePROD.DemoDB.bi_working_capital",
        "Table GZWarehousePROD.DemoDB.bi_working_capital · read by 1873",
        "  snapshot  102 · 2026-11-05 07:46 UTC · overwrite · 98 records (+98)",
        "  written   by InvoiceBulkExtract (Spark application app-20261105073019-0000)",
        "  run       6e36a1953ce3491fa4b4afcb78e9545f of job InvoicePDFToLakehouse (1825), project Invoice-processing · succeeded",
        "  match     strong: the only run going when the snapshot was committed; Spark application app-20261105073019-0000 started during this run",
        "  code      version 6 · 3cf7e274fa20 · saved 2026-11-05 07:21 UTC",
        "  upstream  DemoDB.invoices_raw → DemoDB.bi_working_capital (INSERT OVERWRITE, 2026-11-05 07:46 UTC)",
        "  since     2 commits after it; records 98 → 100 (+2)",
        "What gz cannot tell",
        "The platform does not record the job run id on table commits",
        "Dataset and visualization definitions are shown as they are now",
        "Filter values are the defaults a viewer starts with",
    ] {
        assert!(stdout.contains(part), "missing {part:?} in\n{stdout}");
    }
    assert!(!stdout.contains(CONNECTION_SECRET));
    assert_only_reads(&log);

    let runs = query_of(&log, "/etl/consumption-metrics");
    assert!(
        runs.contains("from=2026-11-05T07%3A44%3A49")
            && runs.contains("to=2026-11-05T07%3A48%3A49"),
        "runs are asked for around the commit: {runs}"
    );
    let lineage = query_of(&log, "/lakehouse/governance/lineage/table");
    for part in [
        "warehouseName=GZWarehousePROD",
        "databaseName=DemoDB",
        "tableName=bi_working_capital",
        "direction=upstream",
        "depth=1",
    ] {
        assert!(lineage.contains(part), "{lineage}");
    }
}

#[test]
fn explain_dashboard_as_json_carries_the_whole_trace() {
    let dir = temp_dir("json");
    let (base, log) = stub();
    let out = gz(&dir, &base)
        .args([
            "-o",
            "json",
            "explain",
            "dashboard",
            "1114",
            "--viz",
            "WC-YearChart",
        ])
        .args(["--at", "2026-11-05T08:00", "--depth", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains(CONNECTION_SECRET));
    let trace: Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(
        trace["subject"],
        json!({"kind": "dashboard", "id": "1114", "name": "Working-Capital"})
    );
    assert_eq!(trace["at"], "2026-11-05T08:00:00");
    let figure = &trace["figures"][0];
    assert_eq!(figure["datasetId"], "1873");
    assert_eq!(
        figure["measures"][0],
        json!({"column": "Revenue", "expression": "ROUND(SUM(revenue27), 0)", "chartAggregation": null})
    );

    let dataset = &trace["datasets"][0];
    assert_eq!(dataset["kind"], "sql");
    assert_eq!(
        dataset["tables"],
        json!(["GZWarehousePROD.DemoDB.bi_working_capital"])
    );
    assert_eq!(
        dataset["connectionDetails"],
        json!([{"name": "GZLakehouseProd", "type": "GZLAKEHOUSE",
                "warehouse": "GZWarehousePROD", "database": "DemoDB"}])
    );
    assert_eq!(
        dataset["filters"][0]["placeholders"],
        json!(["{{WC-Region.value}}"])
    );
    assert!(
        dataset["sql"].as_str().unwrap().contains("GROUP BY"),
        "SQL is whole in JSON"
    );

    let table = &trace["tables"][0];
    assert_eq!(table["warehouse"], "GZWarehousePROD");
    assert_eq!(table["readBy"], json!(["1873"]));
    assert_eq!(table["snapshot"]["id"], "102");
    assert_eq!(table["snapshot"]["sparkAppId"], "app-20261105073019-0000");
    assert_eq!(table["run"]["certainty"], "strong");
    assert_eq!(table["run"]["run"]["executionId"], RUN);
    assert_eq!(table["run"]["alternatives"], json!([]));
    assert_eq!(table["codeVersion"]["number"], 6);
    assert_eq!(table["upstream"][0]["correlationId"], "2542");
    let commits: Vec<&str> = table["changesSince"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(commits, ["104", "103"]);
    assert_eq!(table["changesSince"]["recordsNow"], 100);
    assert_eq!(table["problems"], json!([]));
    assert!(trace["gaps"].as_array().unwrap().len() >= 2);

    assert!(query_of(&log, "/lakehouse/governance/lineage/table").contains("depth=2"));
    assert_only_reads(&log);
}

#[test]
fn explain_dashboard_names_what_it_cannot_find() {
    let dir = temp_dir("missing");
    let (base, _log) = stub();
    let out = gz(&dir, &base)
        .args(["explain", "dashboard", "Sales"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(
        stderr
            .contains("no dashboard 'Sales' that you can see; `gz designer dashboards` lists them"),
        "{stderr}"
    );

    let out = gz(&dir, &base)
        .args(["explain", "dashboard", "1114", "--viz", "Nope"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("has no visualization 'Nope'"),
        "{}",
        plain(&out.stderr)
    );

    let out = gz(&dir, &base)
        .args(["explain", "dashboard", "1114", "--at", "last week"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(plain(&out.stderr).contains("is not a UTC date or time"));
}
