//! Turn timing trace: with `GZ_TRACE_TIMING` set, gz writes when each step
//! between pressing Enter and the first answer text on screen happened,
//! counted from Enter, so slow steps on the client side can be told from
//! slow servers.
//!
//! `GZ_TRACE_TIMING=1` writes `timing.log` in the gz config folder; any
//! other value is the file to write. Each turn appends lines such as
//! `turn 2 +41.3 ms request sent: POST /chat/runs`. Only the first time a
//! step happens in a turn is written, so marking inside loops is cheap.
//! Off (the default), every call returns at once.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const ENV: &str = "GZ_TRACE_TIMING";
const DEFAULT_FILE: &str = "timing.log";

struct Tracer {
    file: Mutex<File>,
    turn: Mutex<Turn>,
}

struct Turn {
    number: u64,
    started: Instant,
    seen: HashSet<String>,
}

fn tracer() -> Option<&'static Tracer> {
    static TRACER: OnceLock<Option<Tracer>> = OnceLock::new();
    TRACER
        .get_or_init(|| {
            let value = std::env::var_os(ENV).filter(|v| !v.is_empty())?;
            let path = trace_path(&value.to_string_lossy())?;
            open(path)
        })
        .as_ref()
}

/// Where the trace goes for the variable's `value`.
fn trace_path(value: &str) -> Option<PathBuf> {
    match value {
        "0" | "false" | "off" => None,
        "1" | "true" | "on" => crate::config::config_path()
            .ok()?
            .parent()
            .map(|dir| dir.join(DEFAULT_FILE)),
        path => Some(PathBuf::from(path)),
    }
}

fn open(path: PathBuf) -> Option<Tracer> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    Some(Tracer {
        file: Mutex::new(file),
        turn: Mutex::new(Turn {
            number: 0,
            started: Instant::now(),
            seen: HashSet::new(),
        }),
    })
}

/// Enter was pressed: a new turn's clock starts.
pub fn start_turn() {
    let Some(tracer) = tracer() else {
        return;
    };
    if let Ok(mut turn) = tracer.turn.lock() {
        turn.number += 1;
        turn.started = Instant::now();
        turn.seen.clear();
    }
    write(tracer, "enter");
}

/// Records `step` the first time it happens this turn.
pub fn mark(step: &str) {
    if let Some(tracer) = tracer() {
        write(tracer, step);
    }
}

/// Like [`mark`], for a label that costs something to build.
pub fn mark_with(step: impl FnOnce() -> String) {
    if let Some(tracer) = tracer() {
        write(tracer, &step());
    }
}

fn write(tracer: &Tracer, step: &str) {
    let Ok(mut turn) = tracer.turn.lock() else {
        return;
    };
    if !turn.seen.insert(step.to_string()) {
        return;
    }
    let line = line(turn.number, turn.started.elapsed().as_secs_f64(), step);
    drop(turn);
    if let Ok(mut file) = tracer.file.lock() {
        // A trace that cannot be written must never disturb the turn.
        let _ = file.write_all(line.as_bytes());
    }
}

fn line(turn: u64, seconds: f64, step: &str) -> String {
    format!("turn {turn} +{:.1} ms {step}\n", seconds * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variable_names_the_file_or_turns_it_off() {
        assert_eq!(trace_path("0"), None);
        assert_eq!(trace_path("off"), None);
        assert_eq!(
            trace_path("/var/log/gz.log"),
            Some(PathBuf::from("/var/log/gz.log"))
        );
        let default = trace_path("1").unwrap();
        assert!(default.ends_with("gz/timing.log"), "{}", default.display());
    }

    #[test]
    fn lines_show_the_turn_and_milliseconds_since_enter() {
        assert_eq!(
            line(3, 0.0413, "request sent"),
            "turn 3 +41.3 ms request sent\n"
        );
    }

    #[test]
    fn each_step_is_written_once_per_turn() {
        let dir = crate::assistant::sandbox::tests::temp_dir("timing");
        let path = dir.join("t.log");
        let tracer = open(path.clone()).unwrap();
        write(&tracer, "first text");
        write(&tracer, "first text");
        {
            let mut turn = tracer.turn.lock().unwrap();
            turn.number = 1;
            turn.seen.clear();
        }
        write(&tracer, "first text");
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.matches("first text").count(), 2, "{text}");
        assert!(text.contains("turn 1 +"), "{text}");
    }
}
