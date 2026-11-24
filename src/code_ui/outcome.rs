//! The decisive line of a command's output: the pass/fail summary a test
//! runner prints (pytest, cargo test, jest, vitest, go test, mocha), so a
//! folded command result says how the run went without opening it.

use std::sync::OnceLock;

use regex::Regex;

struct Patterns {
    ansi: Regex,
    cargo: Regex,
    pytest: Regex,
    jest: Regex,
    vitest: Regex,
    go_package: Regex,
    go_failed_test: Regex,
    mocha_passing: Regex,
    mocha_failing: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let compile = |pattern: &str| Regex::new(pattern).expect("pattern is valid");
        Patterns {
            ansi: compile(r"\x1b\[[0-9;?]*[A-Za-z]"),
            cargo: compile(
                r"(?m)^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored",
            ),
            pytest: compile(
                r"(?m)^=*\s*((?:\d+ (?:passed|failed|errors?|skipped|xfailed|xpassed|warnings?|deselected|rerun)(?:, )?)+ in [\d.]+s(?: \([^)]*\))?|no tests ran in [\d.]+s)\s*=*\s*$",
            ),
            jest: compile(r"(?m)^Tests:\s+(.*\d+ total)\s*$"),
            vitest: compile(r"(?m)^\s*Tests\s+(.*\(\d+\))\s*$"),
            go_package: compile(r"(?m)^(ok|FAIL)\s+\S+\s+(?:[\d.]+s|\(cached\))"),
            go_failed_test: compile(r"(?m)^\s*--- FAIL: (\S+)"),
            mocha_passing: compile(r"(?m)^\s+(\d+) passing \(([^)]+)\)\s*$"),
            mocha_failing: compile(r"(?m)^\s+(\d+) failing\s*$"),
        }
    })
}

/// The summary line of a test run found in `output`, if any.
pub fn decisive_line(output: &str) -> Option<String> {
    let p = patterns();
    let output = p.ansi.replace_all(output, "");
    cargo(p, &output)
        .or_else(|| last_capture(&p.pytest, &output))
        .or_else(|| last_capture(&p.jest, &output).map(|tests| format!("Tests: {tests}")))
        .or_else(|| last_capture(&p.vitest, &output).map(|tests| format!("Tests {tests}")))
        .or_else(|| go(p, &output))
        .or_else(|| mocha(p, &output))
}

fn last_capture(pattern: &Regex, output: &str) -> Option<String> {
    pattern
        .captures_iter(output)
        .last()
        .and_then(|captures| captures.get(1))
        .map(|summary| summary.as_str().trim().to_string())
}

/// Every `test result:` line summed (a workspace prints one per target).
fn cargo(p: &Patterns, output: &str) -> Option<String> {
    let mut seen = false;
    let (mut failed_any, mut passed, mut failed, mut ignored) = (false, 0u64, 0u64, 0u64);
    for captures in p.cargo.captures_iter(output) {
        seen = true;
        let number = |i: usize| captures[i].parse::<u64>().unwrap_or(0);
        failed_any |= &captures[1] == "FAILED";
        passed += number(2);
        failed += number(3);
        ignored += number(4);
    }
    seen.then(|| {
        let verdict = if failed_any { "FAILED" } else { "ok" };
        format!("test result: {verdict}. {passed} passed; {failed} failed; {ignored} ignored")
    })
}

/// `ok`/`FAIL` package lines counted, with the failed tests' names.
fn go(p: &Patterns, output: &str) -> Option<String> {
    let (mut ok, mut failed) = (0, 0);
    for captures in p.go_package.captures_iter(output) {
        if &captures[1] == "ok" {
            ok += 1;
        } else {
            failed += 1;
        }
    }
    if ok + failed == 0 {
        return None;
    }
    let mut line = format!("go test: {ok} ok, {failed} FAIL");
    let names: Vec<&str> = p
        .go_failed_test
        .captures_iter(output)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();
    if !names.is_empty() {
        line.push_str(&format!(" ({})", names.join(", ")));
    }
    Some(line)
}

fn mocha(p: &Patterns, output: &str) -> Option<String> {
    let passing = p.mocha_passing.captures_iter(output).last()?;
    let mut line = format!("{} passing ({})", &passing[1], &passing[2]);
    if let Some(failing) = p.mocha_failing.captures_iter(output).last() {
        line.push_str(&format!(", {} failing", &failing[1]));
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pytest_summaries() {
        let quiet = "..F.\n=== FAILURES ===\n...\n1 failed, 11 passed in 0.10s\n";
        assert_eq!(
            decisive_line(quiet).as_deref(),
            Some("1 failed, 11 passed in 0.10s")
        );
        let verbose =
            "collected 12 items\n\n============== 12 passed, 1 warning in 0.03s ==============\n";
        assert_eq!(
            decisive_line(verbose).as_deref(),
            Some("12 passed, 1 warning in 0.03s")
        );
        let none = "===== no tests ran in 0.01s =====\n";
        assert_eq!(
            decisive_line(none).as_deref(),
            Some("no tests ran in 0.01s")
        );
    }

    #[test]
    fn cargo_results_are_summed_across_targets() {
        let output = "running 3 tests\ntest result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\nrunning 2 tests\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert_eq!(
            decisive_line(output).as_deref(),
            Some("test result: FAILED. 4 passed; 1 failed; 1 ignored")
        );
    }

    #[test]
    fn jest_and_vitest_summaries_survive_colour_codes() {
        let jest = "PASS src/a.test.js\nTest Suites: 1 failed, 2 passed, 3 total\n\x1b[1mTests:\x1b[22m       \x1b[31m1 failed\x1b[39m, 11 passed, 12 total\nTime: 1.2 s\n";
        assert_eq!(
            decisive_line(jest).as_deref(),
            Some("Tests: 1 failed, 11 passed, 12 total")
        );
        let vitest = " Test Files  1 passed (1)\n      Tests  1 failed | 11 passed (12)\n   Start at  10:00:00\n";
        assert_eq!(
            decisive_line(vitest).as_deref(),
            Some("Tests 1 failed | 11 passed (12)")
        );
    }

    #[test]
    fn go_test_counts_packages_and_names_failures() {
        let output = "--- FAIL: TestTotal (0.00s)\n    calc_test.go:9: got 3\nFAIL\nFAIL\texample.com/calc\t0.004s\nok  \texample.com/util\t(cached)\nok  \texample.com/io\t0.010s\n";
        assert_eq!(
            decisive_line(output).as_deref(),
            Some("go test: 2 ok, 1 FAIL (TestTotal)")
        );
    }

    #[test]
    fn mocha_summary() {
        let output = "  calc\n    ✓ adds\n\n  12 passing (34ms)\n  1 failing\n";
        assert_eq!(
            decisive_line(output).as_deref(),
            Some("12 passing (34ms), 1 failing")
        );
    }

    #[test]
    fn other_output_has_no_decisive_line() {
        assert_eq!(decisive_line("exit code 0\nfile1\nfile2\n"), None);
        assert_eq!(decisive_line("tests passed\n"), None);
    }
}
