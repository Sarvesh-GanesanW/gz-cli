//! `gz access why` and `gz govern scan` against a stub provider: the
//! explain route's query, how its answer is shown, the sample statement the
//! scan runs, that raw values never reach the output, and the tag writes
//! `--apply` makes. Each gz run has its own config folder; nothing leaves
//! 127.0.0.1.

mod common;

use common::{plain, read_request, respond, Recorded};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const EXPLAIN: &str = "/lakehouse/governance/access/explain";
const EMAIL_TAGS: &str = "/lakehouse/tags/wh/tables/crm/customers/columns/2/tags";
const PHONE_TAGS: &str = "/lakehouse/tags/wh/tables/crm/customers/columns/3/tags";
const RAW_VALUES: [&str; 4] = [
    "asha@example.in",
    "ravi@example.com",
    "+91 98765 43210",
    "+91 91234 56780",
];

/// What the stub answers that differs between tests.
#[derive(Clone)]
struct Answers {
    explain: Value,
    /// The tags each column holds now, by its tag route.
    column_tags: Vec<(&'static str, Value)>,
    explain_route_missing: bool,
}

impl Default for Answers {
    fn default() -> Self {
        Self {
            explain: refused_update(),
            column_tags: vec![
                (EMAIL_TAGS, json!({"version": 3, "tags": {"owner": "crm"}})),
                (PHONE_TAGS, json!({"version": 1, "tags": {"pii": "false"}})),
            ],
            explain_route_missing: false,
        }
    }
}

fn refused_update() -> Value {
    json!({
        "warehouseName": "wh", "databaseName": "crm", "tableName": "customers",
        "action": "delete", "statement": "DELETE FROM `wh`.`crm`.`customers`",
        "decision": "deny", "reason": "You do not have All or Update permission",
        "subject": {"userName": "alice", "groups": ["analyst"], "groupsFrom": "sign-in",
            "admin": false},
        "decidedBy": {"check": "table", "effect": "deny", "database": "crm",
            "table": "customers", "tableId": 10, "action": "Update", "hasRules": true,
            "requiredExplicitAllow": false},
        "checks": [
            {"check": "database", "effect": "allow", "database": "crm", "action": "Update",
             "resourceType": "GZLakehouse Database", "resourceId": 1,
             "rules": [{"member": "analyst", "memberType": "group", "permission": "All",
                 "effect": "allow", "scope": "this"}]},
            {"check": "table", "effect": "deny", "database": "crm", "table": "customers",
             "tableId": 10, "action": "Update", "hasRules": true,
             "requiredExplicitAllow": false,
             "rules": [{"userName": "alice", "groupName": null, "operator": "or",
                 "action": "Update", "effect": "deny", "applies": true}]},
        ],
        "dataPolicy": null, "modelRead": null,
    })
}

fn allowed_read() -> Value {
    let mut answer = refused_update();
    answer["action"] = json!("select");
    answer["decision"] = json!("allow");
    answer["reason"] = json!("Allowed");
    answer["subject"] = json!({"userName": "bob", "groups": ["finance"],
        "groupsFrom": "directory", "admin": false});
    answer["dataPolicy"] = json!({"protected": false, "rowFilters": ["region = 'north'"],
        "columnMasks": {"email": {"type": "partial"}}, "unmaskedColumns": [],
        "policyVersion": "v3"});
    answer["modelRead"] = json!({"classification": "confidential", "tableTags": {},
        "columnTags": {}, "maskedColumns": ["email"]});
    answer
}

fn schema() -> Value {
    json!([
        {"columnId": 1, "columnName": "customer_id", "dataType": "bigint"},
        {"columnId": 2, "columnName": "contact", "dataType": "string"},
        {"columnId": 3, "columnName": "mobile", "dataType": "string"},
        {"columnId": 4, "columnName": "score", "dataType": "double"},
        {"columnId": 5, "columnName": "city", "dataType": "string"},
    ])
}

/// The sample row: each scanned column's values gathered into a list.
fn sample_row() -> Value {
    json!({
        "gz_rows": 2,
        "c0": ["1001", "1002"],
        "c1": [RAW_VALUES[0], RAW_VALUES[1]],
        "c2": [RAW_VALUES[2], RAW_VALUES[3]],
        "c3": ["Pune", "Chennai"],
    })
}

fn stub(answers: Answers) -> (String, Arc<Mutex<Vec<Recorded>>>) {
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
            let tags = answers.column_tags.iter().find(|(route, _)| *route == path);
            let reply = match (request.method.as_str(), path, tags) {
                ("GET", EXPLAIN, _) if answers.explain_route_missing => {
                    respond(
                        &mut stream,
                        "404 NOT FOUND",
                        "text/html",
                        "<!doctype html><title>404 Not Found</title><p>The requested URL was not found on the server.</p>",
                    );
                    continue;
                }
                ("GET", EXPLAIN, _) => json!({"status": 200, "response": answers.explain}),
                ("GET", "/admin/model-read-policy", _) => json!({"site": "acme",
                    "isDefault": true, "policy": {
                        "groundzero": {"confidential": "allow"},
                        "customer": {"confidential": "mask"}}}),
                ("GET", "/lakehouse/schema/wh/crm/customers", _) => {
                    json!({"status": 200, "response": schema()})
                }
                ("GET", "/lakehouse/warehouse", _) => {
                    json!({"status": 200, "response": [{"id": 7, "name": "wh"}]})
                }
                ("GET", "/rte/compute", _) => json!([{"Id": 1000, "Name": "mini"}]),
                ("POST", "/lakehouse/startsession", _) => {
                    json!({"status": 200, "response": {"sessionId": "s1"}})
                }
                ("POST", "/lakehouse/submitquery", _) => json!({"status": 200,
                    "queryExecutionId": "q1", "response": {
                        "schema": [{"name": "gz_rows", "type": "long"}],
                        "data": [sample_row()],
                        "pageNumber": 1, "hasMore": false, "totalRecords": 1}}),
                ("POST", "/lakehouse/stopsession", _) => json!({"status": 200, "response": {}}),
                ("GET", _, Some((_, current))) => current.clone(),
                ("PUT", _, Some((_, current))) => {
                    json!({"version": current["version"].as_u64().unwrap() + 1,
                        "tags": request.body["tags"]})
                }
                _ => {
                    respond(&mut stream, "404 Not Found", "application/json", "{}");
                    continue;
                }
            };
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                &reply.to_string(),
            );
        }
    });
    (base, log)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-governance-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn gz(dir: &Path, base: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gz"));
    for var in ["GZ_DOMAIN", "GZ_AUTH_HOST", "GZ_API_KEY"] {
        cmd.env_remove(var);
    }
    cmd.args(args)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GZ_PROFILE", "governance-test")
        .env("GZ_HOST", base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .env("GZ_CLIENT", "dev")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn requests(log: &Arc<Mutex<Vec<Recorded>>>) -> Vec<Recorded> {
    log.lock().unwrap().clone()
}

fn query_of(request: &Recorded) -> Vec<(String, String)> {
    let query = request
        .target
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or_default();
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[test]
fn access_why_me_asks_the_authorizer_and_shows_its_reasoning() {
    let dir = temp_dir("why-me");
    let (base, log) = stub(Answers::default());
    let out = gz(
        &dir,
        &base,
        &[
            "access",
            "why",
            "me",
            "wh.crm.customers",
            "--action",
            "delete",
        ],
    );
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    for part in [
        "alice may not delete wh.crm.customers: You do not have All or Update permission",
        "Decided by: table crm.customers rules for Update: refuses",
        "Groups counted: analyst (from the sign-in)",
        "  1. database crm permission Update: allows",
        "       group analyst: All allow (on this database)",
        "  2. table crm.customers rules for Update: refuses",
        "       user alice: Update deny (applies)",
    ] {
        assert!(stdout.contains(part), "missing {part:?} in\n{stdout}");
    }
    let seen = requests(&log);
    assert_eq!(seen.len(), 1, "a refusal needs no model read policy");
    let query = query_of(&seen[0]);
    assert!(query.contains(&("action".into(), "delete".into())));
    assert!(query.contains(&("warehouseName".into(), "wh".into())));
    assert!(
        query
            .iter()
            .all(|(key, _)| key != "userName" && key != "groups"),
        "`me` names nobody and no groups are ever sent: {query:?}"
    );
}

#[test]
fn access_why_as_json_adds_what_models_get_for_a_read() {
    let dir = temp_dir("why-json");
    let answers = Answers {
        explain: allowed_read(),
        ..Answers::default()
    };
    let (base, log) = stub(answers);
    let out = gz(
        &dir,
        &base,
        &["-o", "json", "access", "why", "bob", "wh.crm.customers"],
    );
    assert!(out.status.success(), "{}", plain(&out.stderr));
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply["decision"], "allow");
    assert_eq!(reply["dataPolicy"]["rowFilters"][0], "region = 'north'");
    assert_eq!(
        reply["modelReadActions"],
        json!({"level": "confidential", "groundzero": "allow", "customer": "mask"})
    );
    let seen = requests(&log);
    assert!(query_of(&seen[0]).contains(&("userName".into(), "bob".into())));
    assert!(query_of(&seen[0]).contains(&("action".into(), "select".into())));
}

#[test]
fn access_why_says_when_the_provider_lacks_the_route() {
    let dir = temp_dir("why-missing");
    let answers = Answers {
        explain_route_missing: true,
        ..Answers::default()
    };
    let (base, _log) = stub(answers);
    let out = gz(&dir, &base, &["access", "why", "me", "wh.crm.customers"]);
    assert!(!out.status.success());
    let stderr = plain(&out.stderr);
    assert!(stderr.contains("cannot explain access yet"), "{stderr}");
}

#[test]
fn govern_scan_flags_columns_from_one_bounded_sample_and_never_prints_raw_values() {
    let dir = temp_dir("scan");
    let (base, log) = stub(Answers::default());
    for format in ["table", "json"] {
        let out = gz(
            &dir,
            &base,
            &[
                "-o",
                format,
                "govern",
                "scan",
                "wh.crm.customers",
                "--sample",
                "500",
                "--compute",
                "mini",
            ],
        );
        let stdout = plain(&out.stdout);
        let stderr = plain(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        for raw in RAW_VALUES {
            assert!(
                !stdout.contains(raw) && !stderr.contains(raw),
                "{raw} leaked"
            );
        }
        if format == "table" {
            for part in [
                "Sampled 2 rows of wh.crm.customers; 2 of 4 columns look like personal data.",
                "a***@e***.in",
                "+** ***** ***10",
            ] {
                assert!(stdout.contains(part), "missing {part:?} in\n{stdout}");
            }
        } else {
            let reply: Value = serde_json::from_str(&stdout).unwrap();
            let kinds: Vec<&str> = reply["findings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|finding| finding["kind"].as_str().unwrap())
                .collect();
            assert_eq!(kinds, ["email", "phone"]);
            assert_eq!(reply["findings"][0]["columnId"], 2);
            assert_eq!(reply["findings"][0]["confidence"], "high");
        }
    }
    let seen = requests(&log);
    let sample = seen
        .iter()
        .find(|r| r.target == "/lakehouse/submitquery")
        .unwrap();
    assert_eq!(
        sample.body["query"],
        "SELECT count(*) AS gz_rows, collect_list(CAST(`customer_id` AS STRING)) AS c0, \
         collect_list(CAST(`contact` AS STRING)) AS c1, \
         collect_list(CAST(`mobile` AS STRING)) AS c2, \
         collect_list(CAST(`city` AS STRING)) AS c3 \
         FROM (SELECT * FROM `crm`.`customers` LIMIT 500) AS gz_sample"
    );
    assert_eq!(sample.body["catalog"], "wh");
    assert!(
        seen.iter().all(|r| r.method != "PUT"),
        "a scan without --apply writes nothing"
    );
    assert!(seen.iter().any(|r| r.target == "/lakehouse/stopsession"));
}

#[test]
fn govern_scan_apply_tags_flagged_columns_and_keeps_other_tags() {
    let dir = temp_dir("apply");
    let (base, log) = stub(Answers::default());
    let out = gz(
        &dir,
        &base,
        &[
            "govern",
            "scan",
            "wh.crm.customers",
            "--compute",
            "mini",
            "--apply",
            "--yes",
        ],
    );
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", plain(&out.stderr));
    assert!(stdout.contains("contact: tagged (pii=email)"), "{stdout}");
    assert!(
        stdout.contains("mobile: kept its tag pii=false (pii=phone)"),
        "{stdout}"
    );

    let puts: Vec<Recorded> = requests(&log)
        .into_iter()
        .filter(|r| r.method == "PUT")
        .collect();
    assert_eq!(
        puts.len(),
        1,
        "the column someone marked pii=false is left alone"
    );
    assert_eq!(puts[0].target, EMAIL_TAGS);
    assert_eq!(
        puts[0].body,
        json!({"tags": {"owner": "crm", "pii": "email"}})
    );
}

#[test]
fn govern_scan_apply_without_a_terminal_needs_yes() {
    let dir = temp_dir("apply-refused");
    let (base, log) = stub(Answers::default());
    let out = gz(
        &dir,
        &base,
        &[
            "govern",
            "scan",
            "wh.crm.customers",
            "--compute",
            "mini",
            "--apply",
        ],
    );
    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("pass --yes"),
        "{}",
        plain(&out.stderr)
    );
    assert!(requests(&log).iter().all(|r| r.method != "PUT"));
}
