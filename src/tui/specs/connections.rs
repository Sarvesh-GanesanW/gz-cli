//! Connections: the data sources, AI providers and stores a site can use.
//! The reference conversion to screen specs; see `src/tui/README.md`.
//!
//! Verified contracts (see the TUI research notes, "Connections"):
//! - list `GET connections /connection` → bare array of
//!   `{Id, Name, type, modified_by, modified_on}`
//! - detail `GET connections /connection/{Id}` → `[{name, type, description, config}]`
//!   with secrets stripped; an unknown id answers `[]`
//! - test (lakehouse only) `POST lakehouse /lakehouse/testconnection` loads the
//!   saved connection by name server-side
//!   (groundzero-iceberg-provider-api app_routes/schemas.py:56-95)
//!
//! - test (ontology) `POST data /testConnection {ConnectionName, ConnectionConfig:{type, config}}`
//!   fills the secrets the detail stripped from the stored connection
//!   (groundzero-data-provider-api app.py:112-141, ontology_sources/secrets.py)
//! - browse tables `POST data /getSchema {ConnectionName}` (+ `?siteName=` for
//!   lakehouse; app.py:61-87, web etlUtlis.js:41-54), in the schema browser view
//! - delete `DELETE connections /connection/{Id}`: no in-use check exists
//!   (connection.controller.mjs:163-179, fn_d_connection)
//!
//! Edit is left out on purpose: PUT replaces the whole config, and the
//! detail endpoint strips secrets, so saving would erase them. Other types
//! can't be tested either: the generic test needs the secrets re-entered.

use serde_json::Value;

use crate::tui::format::{text_of, Format};
use crate::tui::nav::{screen, Module, Source, Verb};
use crate::tui::screen::{
    ActionKind, ActionSpec, After, Column, Cond, CopySpec, DetailField, DetailSpec, Goto, Identity,
    LinkSpec, RelatedSpec, RequestSpec, ScreenSpec, Section, SectionBody, Select, Sort, Target,
};
use crate::tui::views::schema_browser::SchemaBrowser;
use crate::tui::views::Ctx;

/// Connector codes → display names (web `constants/connectionCatalog.js`).
pub(crate) const TYPE_LABELS: &[(&str, &str)] = &[
    ("ATHENA", "Athena"),
    ("AWSS3", "AWS S3"),
    ("AICHATSTORAGE", "AI Chat Storage"),
    ("SQLSERVER", "MS SQL Server"),
    ("NEO4J", "Neo4j"),
    ("FIREBOLT", "Firebolt"),
    ("TIMESCALEDB", "Timescale"),
    ("INFLUXDB", "InfluxDB"),
    ("PINECONE", "Pinecone"),
    ("AWSBEDROCK", "AWS Bedrock"),
    ("ANTHROPIC", "Anthropic"),
    ("AZUREOPENAI", "Azure OpenAI"),
    ("COHERE", "Cohere"),
    ("CUSTOMLLM", "Custom LLM"),
    ("FIREWORKSAI", "Fireworks AI"),
    ("GEMINI", "Google Gemini"),
    ("HUGGINGFACE", "Hugging Face"),
    ("OPENAI", "OpenAI"),
    ("TOGETHERAI", "Together AI"),
    ("XAI", "xAI"),
    ("MCP", "MCP Server"),
    ("GZAGENT", "Groundzero Agents"),
    ("ONTOLOGY", "Ontology Import"),
    ("EXASOL", "Exasol"),
    ("ORACLE", "Oracle"),
    ("MYSQL", "MySQL"),
    ("POSTGRESQL", "PostgreSQL"),
    ("GZLAKEHOUSE", "Groundzero Lakehouse"),
    ("REDSHIFT", "RedShift"),
    ("SNOWFLAKE", "Snowflake"),
    ("KINESIS", "AWS Kinesis"),
    ("KAFKA", "Apache Kafka"),
    ("HUDI", "Apache Hudi"),
    ("AZUREBLOB", "Azure Blob Storage"),
    ("CASSANDRA", "Apache Cassandra"),
    ("DUCKDB", "MotherDuck"),
    ("SKYSQL", "SkySQL"),
    ("COSMOSDB", "CosmosDB"),
    ("DB2", "DB2"),
    ("DYNAMODB", "DynamoDB"),
    ("GOOGLEBIGQUERY", "Google BigQuery"),
    ("GCS", "Google Cloud Storage"),
    ("MONGODB", "MongoDB"),
    ("REST", "Rest-API"),
    ("SALESFORCE", "Salesforce CRM"),
    ("SAPHANA", "SAP HANA"),
    ("SFTP", "SFTP"),
    ("TERADATA", "Teradata"),
    ("ELASTICSEARCH", "ElasticSearch"),
    ("AZURESYNAPSE", "Azure Synapse"),
    ("DATABRICKS", "Databricks"),
    ("CLICKHOUSE", "ClickHouse"),
    ("EVENTHUBS", "Azure Event Hubs"),
    ("COCKROACHDB", "CockroachDB"),
    ("COUCHBASE", "Couchbase"),
    ("GOOGLEDRIVE", "Google Drive"),
    ("ONEDRIVE", "OneDrive"),
    ("ONELAKE", "Microsoft OneLake"),
    ("SHAREPOINT", "Sharepoint"),
    ("SINGLESTORE", "SingleStore"),
    ("SHOPIFY", "Shopify"),
    ("SPLUNK", "Splunk"),
    ("SCYLLADB", "ScyllaDB"),
    ("WEAVIATECLOUD", "Weaviate Cloud"),
    ("SERVICENOW", "ServiceNow"),
    ("HUBSPOT", "Hubspot"),
    ("ZOHOCRM", "Zoho CRM"),
    ("MICROSOFTDATAVERSE", "Microsoft Dataverse"),
];

/// Connector codes → category, by the web's keyword rules
/// (`Connections.js` CONNECTION_CATEGORY_RULES, default "Data source").
const CATEGORIES: &[(&str, &str)] = &[
    ("ATHENA", "Data source"),
    ("AWSS3", "Object storage"),
    ("AICHATSTORAGE", "Object storage"),
    ("SQLSERVER", "Database"),
    ("NEO4J", "Data source"),
    ("FIREBOLT", "Data source"),
    ("TIMESCALEDB", "Data source"),
    ("INFLUXDB", "Data source"),
    ("PINECONE", "Data source"),
    ("AWSBEDROCK", "AI provider"),
    ("ANTHROPIC", "AI provider"),
    ("AZUREOPENAI", "AI provider"),
    ("COHERE", "AI provider"),
    ("CUSTOMLLM", "AI provider"),
    ("FIREWORKSAI", "AI provider"),
    ("GEMINI", "AI provider"),
    ("HUGGINGFACE", "AI provider"),
    ("OPENAI", "AI provider"),
    ("TOGETHERAI", "AI provider"),
    ("XAI", "AI provider"),
    ("MCP", "AI provider"),
    ("GZAGENT", "AI provider"),
    ("ONTOLOGY", "Ontology source"),
    ("EXASOL", "Data source"),
    ("ORACLE", "Database"),
    ("MYSQL", "Database"),
    ("POSTGRESQL", "Database"),
    ("GZLAKEHOUSE", "Lakehouse"),
    ("REDSHIFT", "Database"),
    ("SNOWFLAKE", "Database"),
    ("KINESIS", "Streaming"),
    ("KAFKA", "Streaming"),
    ("HUDI", "Data source"),
    ("AZUREBLOB", "Object storage"),
    ("CASSANDRA", "Data source"),
    ("DUCKDB", "Data source"),
    ("SKYSQL", "Database"),
    ("COSMOSDB", "Data source"),
    ("DB2", "Data source"),
    ("DYNAMODB", "Data source"),
    ("GOOGLEBIGQUERY", "Database"),
    ("GCS", "Object storage"),
    ("MONGODB", "Database"),
    ("REST", "Data source"),
    ("SALESFORCE", "Application"),
    ("SAPHANA", "Data source"),
    ("SFTP", "Data source"),
    ("TERADATA", "Data source"),
    ("ELASTICSEARCH", "Data source"),
    ("AZURESYNAPSE", "Data source"),
    ("DATABRICKS", "Database"),
    ("CLICKHOUSE", "Data source"),
    ("EVENTHUBS", "Streaming"),
    ("COCKROACHDB", "Data source"),
    ("COUCHBASE", "Data source"),
    ("GOOGLEDRIVE", "Data source"),
    ("ONEDRIVE", "Data source"),
    ("ONELAKE", "Data source"),
    ("SHAREPOINT", "Data source"),
    ("SINGLESTORE", "Data source"),
    ("SHOPIFY", "Application"),
    ("SPLUNK", "Data source"),
    ("SCYLLADB", "Data source"),
    ("WEAVIATECLOUD", "Data source"),
    ("SERVICENOW", "Application"),
    ("HUBSPOT", "Application"),
    ("ZOHOCRM", "Application"),
    ("MICROSOFTDATAVERSE", "Data source"),
];

const ID: &[&str] = &["Id", "id"];
const NAME: &[&str] = &["Name", "name"];
const TYPE: &[&str] = &["type"];

const LAKEHOUSE: Cond = Cond {
    field: TYPE,
    any_of: &["GZLAKEHOUSE"],
};

const OBJECT_STORAGE: Cond = Cond {
    field: TYPE,
    any_of: &["AWSS3", "AZUREBLOB"],
};

const TEST_LAKEHOUSE: ActionSpec = ActionSpec::new(
    't',
    "Test connection",
    ActionKind::Request(RequestSpec {
        source: Source::new(
            "lakehouse",
            Verb::Post,
            "/lakehouse/testconnection",
            Some(r#"{"connectionName":"{Name|name}","connectionConfig":{"config":{"siteName":"{config.siteName|$site}"}}}"#),
        ),
        success: "Test connection successful",
        after: After::Stay,
    }),
)
.when(LAKEHOUSE);

const ONTOLOGY: Cond = Cond {
    field: TYPE,
    any_of: &["ONTOLOGY"],
};

/// Needs the detail's `config` (non-secret fields); the server fills the
/// stripped secrets back in from the saved connection.
const TEST_ONTOLOGY: ActionSpec = ActionSpec::new(
    'T',
    "Test ontology connection",
    ActionKind::Request(RequestSpec {
        source: Source::post(
            "data",
            "/testConnection",
            r#"{"ConnectionName":"{Name|name}","ConnectionConfig":{"type":"ONTOLOGY","config":{config}}}"#,
        ),
        success: "Connection successfully tested",
        after: After::Stay,
    }),
)
.when(ONTOLOGY);

const BROWSE_TABLES: ActionSpec =
    ActionSpec::new('s', "Browse tables", ActionKind::Custom(browse_tables));

/// Lakehouse connections name their site (the web adds `?siteName=`).
fn browse_tables(row: &Value, ctx: &mut Ctx) {
    let name = text_of(row.get("Name").or(row.get("name")).unwrap_or(&Value::Null));
    let site = LAKEHOUSE.holds(row).then(|| {
        row["config"]["siteName"]
            .as_str()
            .map(str::to_string)
            .or_else(|| ctx.site.clone())
            .unwrap_or_default()
    });
    ctx.push(Box::new(SchemaBrowser::new(name, site)));
}

const DELETE: ActionSpec = ActionSpec::new(
    'd',
    "Delete",
    ActionKind::Request(RequestSpec {
        source: Source::new("connections", Verb::Delete, "/connection/{Id}", None),
        success: "Connection deleted successfully",
        after: After::RefreshAndClose,
    }),
)
.destructive(
    "Delete connection {Name|name}? Nothing checks whether datasets, jobs or agents use it: anything that does will stop working.",
);

const LINEAGE_ASSET_COLUMNS: &[Column] = &[
    Column::new("Asset", &["displayName", "qualifiedName"], Format::Text),
    Column::new("Type", &["assetType"], Format::Text),
    Column::new("Columns", &["latestVersion.columnCount"], Format::Number),
    Column::new("Observed", &["latestVersion.observedOn"], Format::RelTime),
];

pub static CONNECTIONS: ScreenSpec = ScreenSpec {
    noun: "connection",
    columns: &[
        Column::new("Name", NAME, Format::Text),
        Column::new("Type", TYPE, Format::Label(TYPE_LABELS)),
        Column::new("Category", TYPE, Format::Label(CATEGORIES)),
        Column::new("Updated by", &["modified_by"], Format::Text),
        Column::new("Updated", &["modified_on"], Format::RelTime).width(10),
    ],
    sort: Some(Sort {
        field: &["modified_on"],
        descending: true,
    }),
    identity: Identity {
        id: ID,
        title: NAME,
    },
    detail: Some(DetailSpec {
        load: Some(Source::get("connections", "/connection/{Id}")),
        missing: "Connection not found. It may have been deleted.",
        subtitle: &[
            (TYPE, Format::Label(TYPE_LABELS)),
            (TYPE, Format::Label(CATEGORIES)),
        ],
        sections: &[
            Section {
                title: "Summary",
                body: SectionBody::Fields(&[
                    DetailField::new("Description", &["description"], Format::Text).optional(),
                    DetailField::new("Updated by", &["modified_by"], Format::Text),
                    DetailField::new("Updated", &["modified_on"], Format::RelTime),
                    DetailField::new("Updated at", &["modified_on"], Format::DateTime),
                    DetailField::new("Id", ID, Format::Text),
                ]),
            },
            Section {
                title: "Configured details",
                body: SectionBody::Object(&["config"]),
            },
        ],
        related: &[RelatedSpec {
            title: "Lineage assets",
            source: Source::get("etl", "/etl/lineage/assets?connectionId={Id}&limit=10"),
            columns: LINEAGE_ASSET_COLUMNS,
            when: None,
            empty: "No lineage recorded for this connection",
            limit: 10,
        }],
        copy: Some(CopySpec {
            label: "name",
            template: "{Name|name}",
        }),
        ..DetailSpec::BLANK
    }),
    actions: &[BROWSE_TABLES, TEST_LAKEHOUSE, TEST_ONTOLOGY, DELETE],
    links: &[
        LinkSpec {
            key: 'L',
            label: "lineage",
            goto: Goto {
                target: Target {
                    module: "Catalog",
                    resource: "Data lineage",
                },
                select: Select::Drill {
                    field: &["connectionId"],
                    from: ID,
                },
            },
            when: None,
        },
        LinkSpec {
            key: 'w',
            label: "warehouse",
            goto: Goto {
                target: Target {
                    module: "Lakehouse",
                    resource: "Warehouses",
                },
                // `config` comes with the details, so this works there.
                select: Select::Drill {
                    field: &["name"],
                    from: &["config.warehouseName"],
                },
            },
            when: Some(LAKEHOUSE),
        },
        LinkSpec {
            key: 'f',
            label: "files",
            goto: Goto {
                target: Target {
                    module: "Files",
                    resource: "Storage",
                },
                select: Select::Drill {
                    field: NAME,
                    from: NAME,
                },
            },
            when: Some(OBJECT_STORAGE),
        },
    ],
    empty: "No connections to show",
    ..ScreenSpec::BLANK
};

pub const MODULE: Module = Module {
    title: "Connections",
    icon: "⇄",
    resources: &[screen(
        "Connections",
        Source::get("connections", "/connection"),
        &CONNECTIONS,
    )],
};

#[cfg(test)]
mod tests;
