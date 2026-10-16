//! The server inside the CLI, driven as a process: it comes up, it
//! survives a client that hangs up mid-answer, it refuses to share a
//! data directory, it stops cleanly on SIGTERM, inside a project it
//! serves that project, and the background form keeps its promises.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// A scratch directory that goes when the test does, passed or failed.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("zsql-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Scratch(root)
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The environment a deployment loads must not reach the tests: a
/// shell with `/etc/zsqld.env` sourced would change what they exercise.
fn zsql() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zsql"));
    for (k, _) in std::env::vars() {
        if k.starts_with("ZSQL_") {
            cmd.env_remove(k);
        }
    }
    cmd
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects/grocery-store")
}

/// A running `zsql serve`, with its stderr drained on a thread so a
/// chatty server can never fill the pipe and stall.
struct Server {
    child: Child,
    addr: String,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    drain: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// `zsql serve` on a free port with the arguments given, read until
    /// it says where it listens.
    fn start(cwd: &Path, home: &Path, args: &[&str]) -> Server {
        let mut child = zsql()
            .arg("serve")
            .args(["--bind", "127.0.0.1:0"])
            .args(args)
            .current_dir(cwd)
            .env("ZSQL_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("zsql starts");
        let mut reader = BufReader::new(child.stderr.take().unwrap());
        let mut seen = String::new();
        let addr = loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                panic!("the server ended before listening; it said:\n{seen}");
            }
            seen.push_str(&line);
            if let Some(rest) = line.trim().strip_prefix("zsqld: listening on http://") {
                break rest.to_string();
            }
        };
        let stderr = std::sync::Arc::new(std::sync::Mutex::new(seen));
        let sink = stderr.clone();
        let drain = std::thread::spawn(move || {
            let mut rest = String::new();
            let _ = reader.read_to_string(&mut rest);
            sink.lock().unwrap().push_str(&rest);
        });
        Server { child, addr, stderr, drain: Some(drain) }
    }

    /// SIGTERM, then everything it said.
    fn stop(mut self) -> (std::process::ExitStatus, String) {
        terminate(&self.child);
        let status = self.child.wait().unwrap();
        if let Some(d) = self.drain.take() {
            d.join().unwrap();
        }
        let text = self.stderr.lock().unwrap().clone();
        (status, text)
    }
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
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
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

#[cfg(unix)]
#[test]
fn the_server_comes_up_survives_a_hang_up_and_stops_on_sigterm() {
    let root = Scratch::new("basic");
    let data = root.path("data");
    let s = Server::start(&root.0, &root.path("home"), &["--no-project", "--data-dir", data.to_str().unwrap()]);
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
    let second = zsql()
        .args(["serve", "--bind", "127.0.0.1:0", "--no-project", "--data-dir", data.to_str().unwrap()])
        .env("ZSQL_HOME", root.path("home"))
        .output()
        .unwrap();
    assert!(!second.status.success());
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains(&format!("already served by process {}", s.child.id())), "{err}");

    let (status, said) = s.stop();
    assert!(status.success(), "{status}");
    assert!(said.contains("zsqld: stopping") && said.contains("zsqld: stopped"), "{said}");
    // The lock is released with the process: a new server takes the directory.
    let again = Server::start(&root.0, &root.path("home"), &["--no-project", "--data-dir", data.to_str().unwrap()]);
    assert_eq!(get(&again.addr, "/healthz").0, 200);
    again.stop();
}

/// Inside a project, the project is served, and the branch lands on
/// disk whole; then the same model is reached through `-r`, so the
/// remote path is held to the local one end to end.
#[cfg(unix)]
#[test]
fn inside_a_project_it_serves_that_project_and_minus_r_reaches_it() {
    let root = Scratch::new("project");
    let project = root.path("grocery-store");
    copy_dir(&fixture(), &project);
    let data = root.path("data");
    let s = Server::start(&project, &root.path("home"), &["--data-dir", data.to_str().unwrap()]);
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

    // The same answers through the CLI's remote side as through its local one.
    let run = |args: &[&str]| {
        zsql()
            .args(args)
            .current_dir(&project)
            .env("ZSQL_HOME", root.path("home"))
            .env("ZSQL_SERVER", format!("http://{}", s.addr))
            .output()
            .unwrap()
    };
    let local = run(&["sql", "--json", "--expr", "department, sales value"]);
    let remote = run(&["sql", "--json", "--branch", "main", "-r", "--expr", "department, sales value"]);
    assert!(
        local.status.success() && remote.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&local.stderr),
        String::from_utf8_lossy(&remote.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&local.stdout), String::from_utf8_lossy(&remote.stdout));
    let refused = run(&["sql", "--json", "--branch", "main", "-r", "--expr", "no such field"]);
    assert_eq!(refused.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(v["error"]["class"], "FieldNotFound");
    let fields = run(&["fields", "-r", "--branch", "main", "disc"]);
    assert!(String::from_utf8_lossy(&fields.stdout).contains("retail-discount"));
    let list = run(&["list"]);
    assert!(list.status.success(), "{}", String::from_utf8_lossy(&list.stderr));
    assert!(String::from_utf8_lossy(&list.stdout).contains("grocery-store"), "{}", String::from_utf8_lossy(&list.stdout));
    let status = run(&["status", "-r", "--branch", "main"]);
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    let v: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(v["branch"], "main", "{v}");

    let (status, _) = s.stop();
    assert!(status.success());
    let branch = data.join("grocery-store/main");
    assert!(branch.join("snapshot.json.gz").exists() && branch.join("tests.json").exists());
    let names: Vec<String> =
        std::fs::read_dir(data.join("grocery-store")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    assert_eq!(names, vec!["main"]);
}

#[test]
fn an_open_bind_with_accounts_off_is_refused() {
    let root = Scratch::new("open");
    let out = zsql()
        .args(["serve", "--bind", "0.0.0.0:0", "--no-project", "--data-dir", root.path("data").to_str().unwrap()])
        .env("ZSQL_HOME", root.path("home"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--open"));
}

/// The background form: start records where it listens (a bind of
/// port 0 included), status asks that address from any directory, a
/// failed start is reported with the log's tail, a pidfile naming
/// some other process is not believed, and stop takes it down.
#[cfg(unix)]
#[test]
fn the_background_server_starts_reports_and_stops() {
    let root = Scratch::new("background");
    let home = root.path("home");
    let project = root.path("grocery-store");
    copy_dir(&fixture(), &project);
    let run = |cwd: &Path, args: &[&str]| zsql().args(args).current_dir(cwd).env("ZSQL_HOME", &home).output().unwrap();

    // Started inside the project, with a relative data directory.
    let out = run(&project, &["server", "start", "--bind", "127.0.0.1:0", "--data-dir", "data-here"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("started process") && said.contains("127.0.0.1:") && !said.contains(":0;"), "{said}");
    assert!(home.join("server.pid").exists());

    // Status and stop work from elsewhere, since the record is absolute.
    let out = run(&root.0, &["server", "status"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("running as process") && text.contains("it answers"), "{text}");
    // The project at hand was handed over: it is deployed.
    let addr = text.split(" on ").nth(1).and_then(|t| t.split(';').next()).unwrap().trim().to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !get(&addr, "/projects").1.contains("\"grocery-store\"") {
        assert!(std::time::Instant::now() < deadline, "the project was not deployed by the background server");
        std::thread::sleep(Duration::from_millis(100));
    }

    let out = run(&root.0, &["server", "start", "--bind", "127.0.0.1:0", "--no-project"]);
    assert!(!out.status.success(), "a second start must refuse");
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));

    let out = run(&root.0, &["server", "logs", "--lines", "5"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("listening on"), "{}", String::from_utf8_lossy(&out.stdout));

    let out = run(&root.0, &["server", "stop"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped process"), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(!home.join("server.pid").exists());
    let out = run(&root.0, &["server", "status"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("not running"));

    // A start on a port that is taken is reported as a failure, with the log's tail.
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = taken.local_addr().unwrap().to_string();
    let out = run(&root.0, &["server", "start", "--bind", &busy, "--no-project", "--data-dir", root.path("data").to_str().unwrap()]);
    assert!(!out.status.success(), "a bind to a taken port must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("stopped at once"), "{err}");
    assert!(!home.join("server.pid").exists());
    drop(taken);

    // A pidfile naming a process that is not the server is not believed,
    // and that process is left alone.
    let mut decoy = Command::new("sleep").arg("30").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    std::fs::write(home.join("server.pid"), format!(r#"{{"pid":{},"bind":"127.0.0.1:1","data_dir":{:?}}}"#, decoy.id(), root.path("data")))
        .unwrap();
    let out = run(&root.0, &["server", "status"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("not running"), "{}", String::from_utf8_lossy(&out.stdout));
    let out = run(&root.0, &["server", "stop"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("no server is running"));
    std::thread::sleep(Duration::from_millis(100));
    assert!(alive(decoy.id()), "stop signalled a process that is not the server");
    let _ = decoy.kill();
    let _ = decoy.wait();
}

/// The environment file a deployment loads is the base: a `ZSQL_*`
/// variable is honoured when no flag names the setting, and a flag
/// wins over its variable.
#[cfg(unix)]
#[test]
fn the_environment_is_the_base_and_flags_win() {
    let root = Scratch::new("env");
    let listen = |extra: &[&str], accounts: bool| -> (Child, BufReader<std::process::ChildStderr>, String) {
        let mut cmd = zsql();
        cmd.args(["serve", "--no-project"])
            .args(extra)
            .current_dir(&root.0)
            .env("ZSQL_HOME", root.path("home"))
            .env("ZSQL_BIND", "127.0.0.1:0")
            .env("ZSQL_DATA_DIR", root.path("elsewhere"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if accounts {
            cmd.env("ZSQL_ACCOUNTS", "on");
        }
        let mut child = cmd.spawn().unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut banner = String::new();
        loop {
            let mut line = String::new();
            assert!(stderr.read_line(&mut line).unwrap() > 0, "ended early:\n{banner}");
            banner.push_str(&line);
            if line.contains("listening on") {
                break;
            }
        }
        (child, stderr, banner)
    };
    // The reader stays open until the child is done: a server whose
    // stderr has gone away is a different test.
    let finish = |mut child: Child, mut stderr: BufReader<std::process::ChildStderr>, banner: String| {
        terminate(&child);
        let status = child.wait().unwrap();
        let mut rest = String::new();
        let _ = stderr.read_to_string(&mut rest);
        assert!(status.success(), "{status}; it said:\n{banner}{rest}");
    };
    let (child, stderr, banner) = listen(&[], false);
    assert!(root.path("elsewhere/lock").exists(), "ZSQL_DATA_DIR was not the data directory");
    finish(child, stderr, banner);

    let (child, stderr, banner) = listen(&["--data-dir", root.path("here").to_str().unwrap(), "--no-accounts"], true);
    assert!(root.path("here/lock").exists(), "--data-dir did not win over ZSQL_DATA_DIR");
    assert!(banner.contains("accounts off"), "--no-accounts did not win over ZSQL_ACCOUNTS=on:\n{banner}");
    finish(child, stderr, banner);
}
