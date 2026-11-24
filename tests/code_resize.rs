//! `gz code` on a real pseudo-terminal: keys typed right after a window
//! resize reach the session at once instead of waiting for the next key.

#![cfg(unix)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;

use common::{read_request, respond, Recorded};

/// What the terminal is asked for the cursor position, and its answer.
const CURSOR_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_REPLY: &[u8] = b"\x1b[1;1R";
/// The primary device attributes query, sent when probing key reporting.
const ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";
const ATTRIBUTES_REPLY: &[u8] = b"\x1b[?62c";

/// The size the session starts at, and the size it is dragged to.
const WIDE: (u16, u16) = (130, 45);
const NARROW: (u16, u16) = (80, 24);
/// How many resize-then-type rounds the test runs.
const ROUNDS: usize = 8;
/// Far longer than a sent message takes; a stranded one waits for ever.
const SEND_LIMIT: Duration = Duration::from_secs(5);

/// Records every request; answers the few a session start and one turn need.
struct Stub {
    base: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl Stub {
    fn serve() -> Self {
        let capabilities = r#"{"groundzeroModes":[{"value":"sonnet55","label":"Claude Sonnet 5.5","reasoningEfforts":[]}],"defaultGroundzeroMode":"sonnet55","features":{"localContext":false}}"#;
        let events = "id: 1\ndata: {\"type\":\"llm_chunk\",\"text\":\"Hi.\"}\n\nid: 2\ndata: {\"type\":\"run_status\",\"status\":\"completed\"}\n\ndata: [DONE]\n\n";
        let routes: HashMap<&str, &str> = HashMap::from([
            ("GET /chat/capabilities", capabilities),
            ("POST /chat/runs", r#"{"runId":"r1"}"#),
            ("GET /chat/runs/r1/events", events),
            ("GET /connection", "[]"),
        ]);
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
                let key = format!("{} {path}", request.method);
                match routes.get(key.as_str()) {
                    Some(body) if path.ends_with("/events") => {
                        respond(&mut stream, "200 OK", "text/event-stream", body)
                    }
                    Some(body) => respond(&mut stream, "200 OK", "application/json", body),
                    None => respond(&mut stream, "404 Not Found", "application/json", "{}"),
                }
            }
        });
        Self { base, log }
    }

    /// What each `POST /chat/runs` asked, in order.
    fn messages_sent(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST" && r.target.split('?').next() == Some("/chat/runs"))
            .map(|r| r.body["userInput"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

fn winsize((cols, rows): (u16, u16)) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// A pseudo-terminal pair: the master gz's screen is read from and keys are
/// typed into, and the slave gz runs on.
fn open_pty(size: (u16, u16)) -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let mut size = winsize(size);
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // Linux declares the size as *const and macOS as *mut; a raw
            // pointer fits both (a &mut trips clippy on Linux).
            std::ptr::addr_of_mut!(size),
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// A folder for this test's config and workspace, outside the system temp
/// folder (the sandbox treats that one specially).
fn test_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("gz-code-resize-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    dir
}

fn spawn_gz_code(slave: &OwnedFd, stub: &Stub, dir: &std::path::Path) -> Child {
    let stdio = || Stdio::from(slave.try_clone().unwrap());
    let mut command = Command::new(env!("CARGO_BIN_EXE_gz"));
    command
        .current_dir(dir.join("ws"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("HOME", dir.join("home"))
        .env("TERM", "xterm-256color")
        .env("GZ_PROFILE", "resize-test")
        .env("GZ_HOST", &stub.base)
        .env("GZ_TOKEN", "stub-token")
        .env("GZ_SITE", "acme")
        .arg("code")
        .stdin(stdio())
        .stdout(stdio())
        .stderr(stdio());
    // The slave becomes the child's controlling terminal, so a size change
    // on the master reaches it as SIGWINCH.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().unwrap()
}

/// Reads the screen and answers the terminal queries gz sends, the way a
/// terminal would. Returns everything read so far.
fn answer_terminal(master: &OwnedFd) -> Arc<Mutex<Vec<u8>>> {
    let screen = Arc::new(Mutex::new(Vec::new()));
    let seen = screen.clone();
    let mut reader = File::from(master.try_clone().unwrap());
    let mut writer = File::from(master.try_clone().unwrap());
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            for _ in 0..count(chunk, CURSOR_QUERY) {
                writer.write_all(CURSOR_REPLY).ok();
            }
            for _ in 0..count(chunk, ATTRIBUTES_QUERY) {
                writer.write_all(ATTRIBUTES_REPLY).ok();
            }
            seen.lock().unwrap().extend_from_slice(chunk);
        }
    });
    screen
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

fn stop(child: &mut Child) {
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
    child.wait().ok();
}

#[test]
fn a_message_typed_right_after_a_resize_is_sent_at_once() {
    let dir = test_dir();
    let stub = Stub::serve();
    let (master, slave) = open_pty(WIDE);
    let mut child = spawn_gz_code(&slave, &stub, &dir);
    drop(slave);
    let screen = answer_terminal(&master);
    let ready = wait_for(Duration::from_secs(20), || {
        common::plain(&screen.lock().unwrap()).contains("sonnet")
            || stub.log.lock().unwrap().len() > 1
    });
    // Let the first frames settle so the keys below meet an idle reader.
    std::thread::sleep(Duration::from_millis(1500));
    let mut keys = File::from(master.try_clone().unwrap());
    // Whether keys get stranded depends on the order the size change and the
    // keys are noticed in, so several drags make the check dependable.
    let mut sent = true;
    for round in 1..=ROUNDS {
        let size = if round % 2 == 1 { NARROW } else { WIDE };
        let resized = unsafe { libc::ioctl(keys_fd(&master), libc::TIOCSWINSZ, &winsize(size)) };
        assert_eq!(resized, 0, "{}", std::io::Error::last_os_error());
        // The size change lands first, the keys a moment later: both are
        // waiting when gz next looks at the terminal.
        std::thread::sleep(Duration::from_millis(3));
        keys.write_all(format!("Say hi {round}\r").as_bytes())
            .unwrap();
        sent = wait_for(SEND_LIMIT, || stub.messages_sent().len() == round);
        if !sent {
            break;
        }
        // Let the answer finish so the next message starts a run of its own.
        std::thread::sleep(Duration::from_millis(400));
    }
    stop(&mut child);
    std::fs::remove_dir_all(&dir).ok();
    assert!(ready, "gz code did not start");
    let messages = stub.messages_sent();
    assert!(sent, "a message waited for another key; sent: {messages:?}");
    assert_eq!(
        messages.last().map(String::as_str),
        Some(format!("Say hi {ROUNDS}").as_str())
    );
}

fn keys_fd(master: &OwnedFd) -> libc::c_int {
    master.as_raw_fd()
}
