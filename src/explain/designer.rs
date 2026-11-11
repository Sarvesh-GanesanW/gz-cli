//! What a Designer visualization plots, how its dataset is defined, and
//! which filters narrow it, read from the Designer's own records:
//!
//! - `GET /visualization/{id}` → `{Id, Name, config}` (ground-zero
//!   amplify/backend/function/Visualization, controllers/visualization.controller.mjs:4;
//!   web pages/Utils/visualizationUtil.js:69);
//! - `GET /dataset/{id}` → `{Id, Name, DataSourceQuery, TransformationQuery,
//!   ConnectionNames, Config, filters}` (DataSetFunction
//!   controllers/dataset.controller.mjs:7; web pages/Utils/datasetUtils.js:49);
//! - `GET /dashboard/{id}` → `{config, description}`, with no id or name
//!   (Dashboard controllers/dashboard.controller.mjs:7; web
//!   pages/Utils/dashboardUtils.js:29).
//!
//! The chart builder (web pages/Utils/chartUtils.js:39 `createNewChartObject`)
//! copies dataset columns into the chart as they are, so a figure's
//! aggregation lives in the dataset's SQL; only pivot tables aggregate.

use serde::Serialize;
use serde_json::Value;

use crate::output::scalar_text;

/// The `TransformationQuery` a SQL dataset has when it changes nothing
/// (groundzero-data-provider-api utils/transformation_rules.py:6).
const NO_PREP: &str = "outputDF = queryData";

/// A visualization: what it plots and from which dataset.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Figure {
    pub id: String,
    pub name: String,
    pub chart_type: String,
    /// `chartConfig.datasetName`, which holds the dataset's id.
    pub dataset_id: Option<String>,
    /// The columns the figure is drawn along (x axis, slices, rows).
    pub dimensions: Vec<String>,
    pub measures: Vec<Measure>,
    /// What the chart itself does to the rows, when it does anything.
    pub chart_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Measure {
    pub column: String,
    /// The dataset SQL that computes the column (`ROUND(SUM(x), 0)`), when
    /// gz could find it.
    pub expression: Option<String>,
    /// How a pivot table aggregates the column.
    pub chart_aggregation: Option<String>,
}

impl Measure {
    fn plain(column: String) -> Self {
        Self {
            column,
            expression: None,
            chart_aggregation: None,
        }
    }
}

fn text_at(value: &Value, pointer: &str) -> Option<String> {
    let text = scalar_text(value.pointer(pointer)?);
    (!text.is_empty()).then_some(text)
}

/// Column names from a list of strings or of `{name}` objects.
fn names(list: Option<&Value>) -> Vec<String> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| match entry {
            Value::String(name) => Some(name.clone()),
            other => ["name", "field", "column"]
                .iter()
                .find_map(|key| other[*key].as_str().map(String::from)),
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn dedupe(mut columns: Vec<String>) -> Vec<String> {
    let mut seen = Vec::new();
    columns.retain(|c| {
        let new = !seen.contains(c);
        seen.push(c.clone());
        new
    });
    columns
}

/// Reads a visualization record.
pub fn figure(record: &Value) -> Figure {
    let config = &record["config"];
    let chart = &config["chartConfig"];
    let chart_type = text_at(config, "/chartType").unwrap_or_else(|| "unknown".to_string());
    let (dimensions, measures, chart_note) = columns_of(&chart_type, chart);
    Figure {
        id: text_at(record, "/Id").unwrap_or_default(),
        name: text_at(record, "/Name").unwrap_or_default(),
        chart_type,
        dataset_id: text_at(chart, "/datasetName"),
        dimensions: dedupe(dimensions),
        measures,
        chart_note,
    }
}

type Columns = (Vec<String>, Vec<Measure>, Option<String>);

/// Which `chartConfig` fields name the dimension and the measures, per
/// chart type, as `createNewChartObject` and `createData` read them.
fn columns_of(chart_type: &str, chart: &Value) -> Columns {
    let h_axis = || {
        text_at(chart, "/hAxis/title")
            .into_iter()
            .collect::<Vec<_>>()
    };
    let v_axis = || {
        text_at(chart, "/vAxis/title")
            .into_iter()
            .collect::<Vec<_>>()
    };
    let series = || names(chart.get("selectedSeriesArr"));
    let plain = |columns: Vec<String>| -> Vec<Measure> {
        dedupe(columns).into_iter().map(Measure::plain).collect()
    };
    match chart_type {
        "PieChart" | "DonutChart" | "FunnelChart" => (h_axis(), plain(v_axis()), None),
        "HalfDonutChart" => (
            h_axis(),
            plain(v_axis()),
            Some("adds a total of the series as one more slice".to_string()),
        ),
        "LineChart" => {
            let lines = names(chart.get("seriesArr"));
            let measures = if lines.is_empty() { series() } else { lines };
            (h_axis(), plain(measures), None)
        }
        // A horizontal bar chart is drawn along its vertical axis.
        "BarChart" => (v_axis(), plain(series()), None),
        "ScorecardChart" => {
            let mut values: Vec<String> = text_at(chart, "/valueColumn").into_iter().collect();
            values.extend(
                chart["components"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|component| component["type"] == "value")
                    .filter_map(|component| component["column"].as_str().map(String::from)),
            );
            (
                Vec::new(),
                plain(values),
                Some("shows the first row of the dataset only".to_string()),
            )
        }
        "PivotChart" => {
            let mut dimensions = series();
            dimensions.extend(names(chart.get("selectedPivotValuesArr")));
            let measures = names(chart.get("selectedValuesArr"))
                .into_iter()
                .map(|column| {
                    let aggregation =
                        text_at(chart, &format!("/aggFunc/{}", pointer_escape(&column)))
                            .unwrap_or_else(|| "sum".to_string());
                    Measure {
                        column,
                        expression: None,
                        chart_aggregation: Some(aggregation),
                    }
                })
                .collect();
            (
                dimensions,
                measures,
                Some("the pivot table aggregates the dataset's rows itself".to_string()),
            )
        }
        "FlatChart" => {
            let mut shown = names(chart.get("selectedColumnArr"));
            shown.extend(names(chart.get("selectedSparklineColumnArr")));
            (series(), plain(shown), None)
        }
        _ => (h_axis(), plain(series()), None),
    }
}

/// A JSON pointer segment (`~` and `/` escaped).
fn pointer_escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// How a dataset gets its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DatasetKind {
    /// `DataSourceQuery` runs on its one connection (DuckDB for a
    /// lakehouse connection), then the optional pandas prep.
    Sql,
    /// An ETL-job dataset (`Config.isETLJob`): only its code runs, in the
    /// runtime the dataset names; any SQL is inside that code.
    Code,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetDef {
    pub id: String,
    pub name: String,
    pub kind: DatasetKind,
    pub connections: Vec<String>,
    /// `Config.rteName` for code datasets.
    pub runtime: Option<String>,
    pub sql: Option<String>,
    /// The code: the whole dataset for `code`, the pandas prep for `sql`.
    pub code: Option<String>,
    pub filters: Vec<FilterUse>,
}

/// A filter attached to a dataset, and whether its query uses it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterUse {
    pub id: String,
    pub name: String,
    pub filter_type: Option<String>,
    /// The column a list filter offers values of.
    pub column: Option<String>,
    /// The values a viewer starts with; a viewer may pick others.
    pub defaults: Vec<String>,
    /// The `{{Name.prop}}` placeholders in the query that take its value.
    pub placeholders: Vec<String>,
}

/// Reads a dataset record.
pub fn dataset(record: &Value) -> DatasetDef {
    let config = &record["Config"];
    let kind = if config["isETLJob"].as_bool() == Some(true) {
        DatasetKind::Code
    } else {
        DatasetKind::Sql
    };
    let sql = text_at(record, "/DataSourceQuery").filter(|q| !q.trim().is_empty());
    let code = text_at(record, "/TransformationQuery")
        .filter(|code| !code.trim().is_empty() && code.trim() != NO_PREP);
    let query_text = format!(
        "{}\n{}",
        sql.as_deref().unwrap_or_default(),
        code.as_deref().unwrap_or_default()
    );
    DatasetDef {
        id: text_at(record, "/Id").unwrap_or_default(),
        name: text_at(record, "/Name").unwrap_or_default(),
        kind,
        connections: names(record.get("ConnectionNames")),
        runtime: (kind == DatasetKind::Code)
            .then(|| text_at(config, "/rteName"))
            .flatten(),
        sql: (kind == DatasetKind::Sql).then_some(sql).flatten(),
        code,
        filters: filters(record, &query_text),
    }
}

/// The dataset's filters (`filters`, else `Config.filters`), as the
/// dashboard collects them (web pages/Utils/dashboardUtils.js:348).
fn filters(record: &Value, query_text: &str) -> Vec<FilterUse> {
    let listed = record["filters"]
        .as_object()
        .filter(|map| !map.is_empty())
        .or_else(|| record["Config"]["filters"].as_object());
    let placeholders = placeholders(query_text);
    listed
        .into_iter()
        .flatten()
        .map(|(id, filter)| {
            let name = ["filterName", "Name", "name"]
                .iter()
                .find_map(|key| filter[*key].as_str())
                .unwrap_or(id)
                .to_string();
            let settings = &filter["filterConfig"];
            FilterUse {
                id: id.clone(),
                filter_type: ["filterType", "FilterType"]
                    .iter()
                    .find_map(|key| filter[*key].as_str().map(String::from)),
                column: text_at(settings, "/columnName"),
                defaults: defaults(settings),
                placeholders: placeholders
                    .iter()
                    .filter(|(filter_name, _)| filter_name.eq_ignore_ascii_case(&name))
                    .map(|(_, written)| written.clone())
                    .collect(),
                name,
            }
        })
        .collect()
}

/// The starting values of a filter, by type (web
/// components/dashboardWIdgets/visualizationWidget.js:1226).
fn defaults(settings: &Value) -> Vec<String> {
    let items: Vec<String> = settings["defaultItems"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| scalar_text(&item["value"]))
        .filter(|value| !value.is_empty())
        .collect();
    if !items.is_empty() {
        return items;
    }
    let range = |from: &str, to: &str| -> Option<String> {
        Some(format!(
            "{} to {}",
            text_at(settings, from)?,
            text_at(settings, to)?
        ))
    };
    range("/from", "/to")
        .or_else(|| range("/start", "/end"))
        .or_else(|| text_at(settings, "/value"))
        .or_else(|| text_at(settings, "/checked"))
        .into_iter()
        .collect()
}

/// `{{Name.prop}}` placeholders as `(Name, written)`, in order, once each.
fn placeholders(text: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        let inner = after[..close].trim();
        if let Some((name, _)) = inner.split_once('.') {
            let entry = (name.trim().to_string(), format!("{{{{{inner}}}}}"));
            if !found.contains(&entry) {
                found.push(entry);
            }
        }
        rest = &after[close + 2..];
    }
    found
}

/// The visualization ids on a dashboard, tab by tab in tab order.
pub fn dashboard_visualizations(record: &Value) -> Vec<String> {
    let config = &record["config"];
    let by_tab = &config["visualizations"];
    let mut tab_ids: Vec<String> = config["tabs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tab| text_at(tab, "/tabId"))
        .collect();
    // Tabs the `tabs` list leaves out still hold visualizations.
    for key in by_tab.as_object().into_iter().flatten().map(|(k, _)| k) {
        if !tab_ids.contains(key) {
            tab_ids.push(key.clone());
        }
    }
    let ids = tab_ids
        .iter()
        .flat_map(|tab| names(by_tab.get(tab.as_str())))
        .collect();
    dedupe(ids)
}

/// The chart name the dashboard shows for a visualization.
pub fn dashboard_chart_name(record: &Value, viz_id: &str) -> Option<String> {
    text_at(
        &record["config"]["visualizationDetails"][viz_id],
        "/chartName",
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_column_chart_plots_its_series_along_the_horizontal_axis() {
        let figure = figure(&json!({
            "Id": 1717, "Name": "WC-YearChart",
            "config": {"chartType": "ColumnChart", "chartConfig": {
                "datasetName": 1873,
                "hAxis": {"title": "Year"}, "vAxis": {"title": "Revenue"},
                "selectedSeriesArr": [{"id": "x", "name": "Revenue"}],
            }},
        }));
        assert_eq!(figure.id, "1717");
        assert_eq!(figure.dataset_id.as_deref(), Some("1873"));
        assert_eq!(figure.dimensions, ["Year"]);
        assert_eq!(figure.measures, [Measure::plain("Revenue".to_string())]);
        assert_eq!(figure.chart_note, None);
    }

    #[test]
    fn each_chart_type_names_its_columns_its_own_way() {
        let chart = json!({
            "hAxis": {"title": "Series label"}, "vAxis": {"title": "Region"},
            "selectedSeriesArr": [{"name": "Sales"}, {"name": "Cost"}],
            "seriesArr": ["Sales"],
            "valueColumn": "VALUE_1",
            "components": [{"type": "label", "column": "Revenue"}, {"type": "value", "column": "VALUE_2"}],
            "selectedValuesArr": [{"name": "amount"}, {"name": "qty"}],
            "aggFunc": {"qty": "avg"},
        });
        let of = |chart_type: &str| {
            let (dimensions, measures, note) = columns_of(chart_type, &chart);
            let measures: Vec<String> = measures.into_iter().map(|m| m.column).collect();
            (dimensions, measures, note.is_some())
        };
        assert_eq!(
            of("BarChart"),
            (
                vec!["Region".into()],
                vec!["Sales".into(), "Cost".into()],
                false
            ),
            "a horizontal bar chart runs along its vertical axis"
        );
        assert_eq!(
            of("PieChart"),
            (vec!["Series label".into()], vec!["Region".into()], false)
        );
        assert_eq!(of("LineChart").1, ["Sales"], "lines come from seriesArr");
        assert_eq!(
            of("ScorecardChart"),
            (vec![], vec!["VALUE_1".into(), "VALUE_2".into()], true)
        );
        let (_, pivot, _) = columns_of("PivotChart", &chart);
        let aggregations: Vec<_> = pivot
            .iter()
            .map(|m| m.chart_aggregation.clone().unwrap())
            .collect();
        assert_eq!(aggregations, ["sum", "avg"]);
    }

    fn working_capital() -> Value {
        json!({
            "Id": 1873, "Name": "WC-YeartoQuarter",
            "ConnectionNames": ["GZLakehouseProd"],
            "DataSourceQuery": "SELECT \"Year\", ROUND(SUM(revenue27), 0) AS Revenue\r\nFROM DemoDB.bi_working_capital\r\nWHERE supplier_region IN {{WC-Region.value}}\r\n  AND \"Year\" IN {{ WC-YEAR.value }}",
            "TransformationQuery": "outputDF = queryData\r\n",
            "Config": {"ConnectionType": "GZLAKEHOUSE", "isETLJob": false, "filters": {}},
            "filters": {
                "1213": {"filterName": "WC-YEAR", "filterType": "multiSelect",
                         "filterConfig": {"columnName": "year", "defaultItems": [{"value": 2025}, {"value": 2024}]}},
                "1273": {"filterName": "WC-Region", "filterType": "multiSelect",
                         "filterConfig": {"columnName": "supplier_region", "defaultItems": [{"value": "APAC"}]}},
                "1300": {"filterName": "Unused", "filterType": "range",
                         "filterConfig": {"from": "2026-01-01", "to": "2026-02-01"}},
            },
        })
    }

    #[test]
    fn a_sql_dataset_keeps_its_query_and_drops_a_no_op_prep() {
        let dataset = dataset(&working_capital());
        assert_eq!(dataset.kind, DatasetKind::Sql);
        assert_eq!(dataset.connections, ["GZLakehouseProd"]);
        assert!(dataset.sql.unwrap().contains("bi_working_capital"));
        assert_eq!(dataset.code, None, "`outputDF = queryData` changes nothing");
    }

    #[test]
    fn filters_say_which_placeholders_take_them_and_their_defaults() {
        let filters = dataset(&working_capital()).filters;
        let by_name = |name: &str| filters.iter().find(|f| f.name == name).unwrap();
        assert_eq!(by_name("WC-YEAR").defaults, ["2025", "2024"]);
        assert_eq!(
            by_name("WC-YEAR").placeholders,
            ["{{WC-YEAR.value}}"],
            "spaces inside the braces are allowed"
        );
        assert_eq!(
            by_name("WC-Region").column.as_deref(),
            Some("supplier_region")
        );
        assert!(by_name("Unused").placeholders.is_empty());
        assert_eq!(by_name("Unused").defaults, ["2026-01-01 to 2026-02-01"]);
    }

    #[test]
    fn an_etl_job_dataset_is_code_with_its_runtime() {
        let dataset = dataset(&json!({
            "Id": 1864, "Name": "ordersDailyRevenue",
            "ConnectionNames": ["LineageWarehouseConnection"],
            "DataSourceQuery": "",
            "TransformationQuery": "duck.readGzLakehouse(connName=\"x\", query=\"SELECT 1 FROM demo_sales.orders\")",
            "Config": {"isETLJob": true, "rteName": "gz-rte-duckdb-sklearn:1.0.0"},
        }));
        assert_eq!(dataset.kind, DatasetKind::Code);
        assert_eq!(
            dataset.runtime.as_deref(),
            Some("gz-rte-duckdb-sklearn:1.0.0")
        );
        assert_eq!(dataset.sql, None);
        assert!(dataset.code.unwrap().contains("readGzLakehouse"));
    }

    #[test]
    fn lists_a_dashboards_visualizations_in_tab_order() {
        let dashboard = json!({"config": {
            "tabs": [{"tabId": "2"}, {"tabId": "1"}],
            "visualizations": {"1": ["1750", "1756"], "2": ["1759"], "9": ["1750", "1800"]},
            "visualizationDetails": {"1750": {"chartName": "RiskZoneMixByShift"}},
        }});
        assert_eq!(
            dashboard_visualizations(&dashboard),
            ["1759", "1750", "1756", "1800"]
        );
        assert_eq!(
            dashboard_chart_name(&dashboard, "1750").as_deref(),
            Some("RiskZoneMixByShift")
        );
    }
}
