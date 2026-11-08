//! DE/ML: ETL projects and jobs, notebooks, models and experiments.

use crate::tui::nav::{drill, grid, GridSpec, Module, Resource, Source, View};

const PROJECT_IDS: &[&str] = &["Id", "id", "projectId"];

pub const MODULE: Module = Module {
    title: "DE/ML",
    icon: "◈",
    resources: &[
        Resource {
            label: "Projects",
            view: View::Grid(GridSpec {
                source: Source::get("etlprojects", "/etl/etlProjects"),
                drills: &[drill(
                    "project",
                    PROJECT_IDS,
                    Source::get("etlprojects", "/etl/etlJobs/{0}/jobs"),
                )],
                detail: None,
                screen: None,
            }),
        },
        grid(
            "Published jobs",
            Source::get("etlprojects", "/etl/etlJobs/publish"),
        ),
        Resource {
            label: "Notebooks",
            view: View::Grid(GridSpec {
                source: Source::get("etlprojects", "/etl/etlProjects"),
                drills: &[drill(
                    "project",
                    PROJECT_IDS,
                    Source::get("mlops", "/jupyter/{0}/notebook"),
                )],
                detail: None,
                screen: None,
            }),
        },
        grid("Models", Source::get("mlops", "/mlflow/listAllModels")),
        grid(
            "Experiments",
            Source::get("mlops", "/mlflow/listAllExperiments"),
        ),
    ],
};
