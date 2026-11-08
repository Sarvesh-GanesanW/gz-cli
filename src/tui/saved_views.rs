//! Saved views: a named screen with its drill trail, filter and sort
//! ("my failed jobs"), kept in `tui_views.json` next to the gz config,
//! readable by the owner only.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::assistant::history::write_private;
use crate::tui::app::{Place, SortBy, Step};
use crate::tui::nav;

const FILE_NAME: &str = "tui_views.json";

/// A view as stored: the resource by its sidebar titles, so the file
/// survives sidebar reordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    pub name: String,
    pub module: String,
    pub resource: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub sort: Option<SortBy>,
}

impl SavedView {
    pub fn new(name: &str, place: &Place) -> Self {
        Self {
            name: name.to_string(),
            module: place.resource.module().title.to_string(),
            resource: place.resource.resource().label.to_string(),
            steps: place.steps.clone(),
            filter: place.filter.clone(),
            sort: place.sort.clone(),
        }
    }

    /// The place to open, while its screen still exists.
    pub fn place(&self) -> Option<Place> {
        let resource = nav::find_resource(&self.module, &self.resource)?;
        Some(Place {
            resource,
            steps: self.steps.clone(),
            filter: self.filter.clone(),
            sort: self.sort.clone(),
        })
    }
}

/// The saved views and where they are stored (nowhere in tests).
#[derive(Debug, Clone, Default)]
pub struct SavedViews {
    path: Option<PathBuf>,
    views: Vec<SavedView>,
}

impl SavedViews {
    /// The views in `tui_views.json` next to the gz config.
    pub fn open() -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Self::load(dir.join(FILE_NAME))
    }

    /// A missing file is no views yet; an unreadable one is an error, so a
    /// later save can't silently replace the user's views.
    pub fn load(path: PathBuf) -> Result<Self> {
        let views = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| {
                format!(
                    "{} is not valid saved views; fix or delete it",
                    path.display()
                )
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        Ok(Self {
            path: Some(path),
            views,
        })
    }

    pub fn all(&self) -> &[SavedView] {
        &self.views
    }

    /// Adds `view`, replacing one with the same name, and writes the file.
    pub fn save(&mut self, view: SavedView) -> Result<()> {
        self.views.retain(|v| v.name != view.name);
        self.views.push(view);
        self.views.sort_by_key(|v| v.name.to_lowercase());
        self.write()
    }

    /// Removes the view called `name` and writes the file.
    pub fn delete(&mut self, name: &str) -> Result<()> {
        self.views.retain(|v| v.name != name);
        self.write()
    }

    fn write(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let dir = path.parent().context("saved views path has no folder")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let text = serde_json::to_string_pretty(&self.views)?;
        let temp = path.with_extension("json.tmp");
        write_private(&temp, &text)?;
        std::fs::rename(&temp, path).with_context(|| format!("cannot write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gz-saved-views-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(FILE_NAME)
    }

    fn failed_jobs() -> Place {
        Place {
            steps: vec![Step::drill(&["Id"], "5")],
            filter: "failed".into(),
            sort: Some(SortBy {
                column: "Started".into(),
                descending: true,
            }),
            ..Place::at(nav::find_resource("DE/ML", "Projects").unwrap())
        }
    }

    #[test]
    fn views_round_trip_through_an_owner_only_file() {
        let path = temp_file("round-trip");
        let mut views = SavedViews::load(path.clone()).unwrap();
        assert!(views.all().is_empty(), "no file yet");
        views
            .save(SavedView::new("my failed jobs", &failed_jobs()))
            .unwrap();
        let reloaded = SavedViews::load(path.clone()).unwrap();
        assert_eq!(reloaded.all()[0].place(), Some(failed_jobs()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn saving_a_name_again_replaces_it_and_delete_removes_it() {
        let path = temp_file("replace");
        let mut views = SavedViews::load(path.clone()).unwrap();
        views.save(SavedView::new("b", &failed_jobs())).unwrap();
        views.save(SavedView::new("A", &failed_jobs())).unwrap();
        let datasets = Place::at(nav::find_resource("Designer", "Datasets").unwrap());
        views.save(SavedView::new("b", &datasets)).unwrap();
        let names: Vec<&str> = views.all().iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["A", "b"]);
        assert_eq!(views.all()[1].place(), Some(datasets));
        views.delete("A").unwrap();
        assert_eq!(SavedViews::load(path.clone()).unwrap().all().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_an_empty_list() {
        let path = temp_file("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        let err = SavedViews::load(path.clone()).unwrap_err();
        assert!(format!("{err:#}").contains("fix or delete it"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_view_of_a_removed_screen_has_no_place() {
        let mut view = SavedView::new("x", &failed_jobs());
        view.resource = "Gone".into();
        assert_eq!(view.place(), None);
    }
}
