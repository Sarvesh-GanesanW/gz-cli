//! Screen data for each sidebar module, one file per module, so screens can
//! be changed side by side without touching each other. `nav::MODULES`
//! lists them in sidebar order.

pub mod agents;
pub mod catalog;
pub mod chat;
pub mod connections;
pub mod deml;
pub mod designer;
pub mod files;
pub mod lakehouse;
pub mod rules;
pub mod schedules;
pub mod usage;
pub mod workspaces;
