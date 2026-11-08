//! Catalog: data catalog, usage, lineage, streams, ontology and log exports.
//! The data screens live in `catalog/`, one file each.

mod data_catalog;
mod lineage;
mod log_exports;
mod streams;

use crate::tui::nav::Module;
use crate::tui::specs::usage;

pub const MODULE: Module = Module {
    title: "Catalog",
    icon: "▦",
    resources: &[
        data_catalog::RESOURCE,
        usage::AI_USAGE_RESOURCE,
        usage::AGENT_USAGE_RESOURCE,
        usage::JOB_USAGE_RESOURCE,
        usage::PROJECT_USAGE_RESOURCE,
        lineage::RESOURCE,
        streams::STREAMS_RESOURCE,
        streams::MODELS_RESOURCE,
        log_exports::RUNS_RESOURCE,
        log_exports::SCHEDULES_RESOURCE,
    ],
};
