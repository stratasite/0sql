//! The server inside the CLI, driven as a process: it comes up, it
//! survives a client that hangs up mid-answer, it refuses to share a
//! data directory, it stops cleanly on SIGTERM, and inside a project
//! it serves that project.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects/grocery-store")
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("zsql-serve-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

struct Server {
    child: Child,
    stderr: BufReader<std::process::ChildStderr>,
    addr: String,
}

/// `zsql serve` on a free port, with the rest of the arguments given,
/// read until it says where it listens.
fn start(cwd: &Path, home: &Path, args: &[&str]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zsql"))
        .arg("serve")
        .args(["--bind", "127.0.0.1:0"])
        .args(args)
        .current_dir(cwd)
        .env("ZSQL_HOME", home)
        .env_remove("ZSQL_SERVER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("zsql starts");
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let addr = loop {
        let mut line = String::new();
        if stderr.read_line(&mut line).unwrap() == 0 {
            let out = child.wait_with_output().unwrap();
            panic!("the server ended before listening; stdout:\n{}", String::from_utf8_lossy(&out.stdout));
        }
        if let Some(rest) = line.trim().strip_prefix("zsqld: listening on http://") {
            break rest.to_string();
        }
    };
    Server { child, stderr, addr }
}

fn get(addr: &str, path: &str) -> (u16, String) {
    let r = ureq::Agent::new_with_config(ureq::config::Config::builder().http_status_as_error(false).build())
        .get(&format!("http://{addr}{path}"))
        .call()
        .unwrap();
    let status = r.status().as_u16();
    (status, r.into_body().read_to_string().unwrap())
}

#[cfg(unix)]
fn terminate(child: &Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(unix)]
#[test]
fn the_server_comes_up_survives_a_hang_up_and_stops_on_sigterm() {
    let root = scratch("basic");
    let mut s = start(&root, &root.join("home"), &["--no-project", "--data-dir", root.join("data").to_str().unwrap()]);
    assert_eq!(get(&s.addr, "/healthz").0, 200);

    // A client that asks for an answer and hangs up before reading it.
    for _ in 0..5 {
        let mut c = TcpStream::connect(&s.addr).unwrap();
        c.write_all(b"GET /projects HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        drop(c);
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(get(&s.addr, "/healthz").0, 200, "the server died when a client hung up");

    // Another server on the same data directory is refused, and says who has it.
    let second = Command::new(env!("CARGO_BIN_EXE_zsql"))
        .args(["serve", "--bind", "127.0.0.1:0", "--no-project", "--data-dir", root.join("data").to_str().unwrap()])
        .env("ZSQL_HOME", root.join("home"))
        .output()
        .unwrap();
    assert!(!second.status.success());
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains(&format!("already served by process {}", s.child.id())), "{err}");

    terminate(&s.child);
    let status = s.child.wait().unwrap();
    assert!(status.success(), "{status}");
    let mut rest = String::new();
    s.stderr.read_to_string(&mut rest).unwrap();
    assert!(rest.contains("zsqld: stopping") && rest.contains("zsqld: stopped"), "{rest}");
    // The lock is released with the process: a new server takes the directory.
    let mut again = start(&root, &root.join("home"), &["--no-project", "--data-dir", root.join("data").to_str().unwrap()]);
    assert_eq!(get(&again.addr, "/healthz").0, 200);
    terminate(&again.child);
    again.child.wait().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn inside_a_project_it_serves_that_project() {
    let root = scratch("project");
    let project = root.join("grocery-store");
    copy_dir(&fixture(), &project);
    let mut s = start(&project, &root.join("home"), &["--data-dir", root.join("data").to_str().unwrap()]);
    // The deploy happens after the listen line; wait for the model to be there.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (_, body) = get(&s.addr, "/projects");
        if body.contains("\"grocery-store\"") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the project was not deployed: {body}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let (status, body) = get(&s.addr, "/projects/grocery-store/branches/main/fields?q=sales");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Sales Value"), "{body}");
    terminate(&s.child);
    assert!(s.child.wait().unwrap().success());
    // The branch is on disk whole: the snapshot and the tests, nothing staged or previous.
    let branch = root.join("data/grocery-store/main");
    assert!(branch.join("snapshot.json.gz").exists() && branch.join("tests.json").exists());
    let names: Vec<String> = std::fs::read_dir(root.join("data/grocery-store"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["main"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_open_bind_with_accounts_off_is_refused() {
    let root = scratch("open");
    let out = Command::new(env!("CARGO_BIN_EXE_zsql"))
        .args(["serve", "--bind", "0.0.0.0:0", "--no-project", "--data-dir", root.join("data").to_str().unwrap()])
        .env("ZSQL_HOME", root.join("home"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--open"));
    let _ = std::fs::remove_dir_all(&root);
}

fn alive_self() -> bool {
    true
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// The background form: start writes a pidfile and a log, status finds
/// it answering, stop takes it down and says so.
#[cfg(unix)]
#[test]
fn the_background_server_starts_reports_and_stops() {
    let root = scratch("background");
    let home = root.join("home");
    // A free port, so status can ask the default server address of this home.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let bind = format!("127.0.0.1:{port}");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_zsql"))
            .args(args)
            .current_dir(&root)
            .env("ZSQL_HOME", &home)
            .env_remove("ZSQL_SERVER")
            .output()
            .unwrap()
    };
    let out = run(&["server", "start", "--bind", &bind, "--no-project", "--data-dir", root.join("data").to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("started process"), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(home.join("server.pid").exists());

    let out = run(&["server", "status"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("running as process") && text.contains(&bind) && text.contains("it answers"), "{text}");

    let out = run(&["server", "start", "--bind", &bind, "--no-project"]);
    assert!(!out.status.success(), "a second start must refuse");
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));

    let out = run(&["server", "logs", "--lines", "5"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("listening on"), "{}", String::from_utf8_lossy(&out.stdout));

    let out = run(&["server", "stop"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped process"), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(!home.join("server.pid").exists());
    let out = run(&["server", "status"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("not running"));

    // A start that fails is reported as a failure, with the log's tail.
    let out = run(&["server", "start", "--bind", "127.0.0.1:1", "--no-project", "--data-dir", root.join("data").to_str().unwrap()]);
    assert!(!out.status.success(), "a bind to port 1 must fail for a user");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("stopped at once"), "{err}");
    assert!(!home.join("server.pid").exists());

    // A stale pidfile naming a process that is not the server is not believed.
    std::fs::write(
        home.join("server.pid"),
        format!(r#"{{"pid":{},"bind":"{bind}","data_dir":{:?}}}"#, std::process::id(), root.join("data")),
    )
    .unwrap();
    let out = run(&["server", "status"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("not running"), "{}", String::from_utf8_lossy(&out.stdout));
    let out = run(&["server", "stop"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("no server is running"));
    assert!(alive_self(), "stop must not signal this process");
    let _ = std::fs::remove_dir_all(&root);
}
