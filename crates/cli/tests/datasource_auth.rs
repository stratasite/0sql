//! `zsql datasource auth` and `list`, on the credentials a datasource
//! keeps in `.zsql`: what is saved is what was given, whole; only what
//! the adapter takes is saved; and `list` says `yes` only where a
//! connection would find its credential.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn zsql(home: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zsql"));
    // No Google credential of this machine's: not the environment's file,
    // and not gcloud's own, which is looked for under CLOUDSDK_CONFIG.
    cmd.env("ZSQL_HOME", home)
        .env("CLOUDSDK_CONFIG", home.join("no-gcloud"))
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .env_remove("ZSQL_SERVER")
        .current_dir(project);
    cmd
}

fn run(cmd: &mut Command, stdin: Option<&str>) -> (bool, String) {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.unwrap_or_default().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    eprintln!("$ {:?}\n{text}", cmd.get_args().collect::<Vec<_>>());
    (out.status.success(), text)
}

fn project(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("zsql-ds-auth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("project"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(
        project.join("datasources.yml"),
        "bigquery:\n  name: Sales DWH\n  adapter: bigquery\n  tier: warm\n  project: strata-enterprise\n  dataset: sales\n\
         wh:\n  name: Warehouse\n  adapter: postgres\n  tier: hot\n  host: db\n  username: analyst\n",
    )
    .unwrap();
    (home, project)
}

fn dot_zsql(project: &Path) -> serde_yaml::Value {
    serde_yaml::from_str(&std::fs::read_to_string(project.join(".zsql")).unwrap()).unwrap()
}

fn secret_of(home: &Path, project: &Path, key: &str) -> String {
    let (ok, out) = run(zsql(home, project).args(["datasource", "list", "--json"]), None);
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let row = v["datasources"].as_array().unwrap().iter().find(|d| d["datasource"] == key).unwrap().clone();
    row["secret"].as_str().unwrap().to_string()
}

const PEM: &str =
    "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nBKcwggSjAgEAAoIBAQC7: #not a comment\n-----END PRIVATE KEY-----";

#[test]
fn a_bigquery_key_is_saved_whole_and_list_agrees_with_the_connection() {
    let (home, project) = project("bq");
    assert!(secret_of(&home, &project, "bigquery").starts_with("missing"), "nothing held yet");

    // Only what the adapter takes.
    let (ok, out) =
        run(zsql(&home, &project).args(["datasource", "auth", "-d", "bigquery", "--field", "password", "--secret-stdin"]), Some("x\n"));
    assert!(!ok && out.contains("bigquery does not take password") && out.contains("private_key, client_email"), "{out}");
    let (ok, out) =
        run(zsql(&home, &project).args(["datasource", "auth", "-d", "wh", "--field", "private_key", "--secret-stdin"]), Some(PEM));
    assert!(!ok && out.contains("postgres does not take private_key"), "{out}");
    assert!(!project.join(".zsql").exists(), "a refusal saves nothing");

    // The PEM from stdin, every line of it; it is BigQuery's usual field.
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "auth", "-d", "bigquery", "--secret-stdin"]), Some(&format!("{PEM}\n")));
    assert!(ok && out.contains("saved   private_key for bigquery"), "{out}");
    assert!(out.contains("next: zsql datasource auth -d bigquery --field client_email"), "half a service account: {out}");
    assert_eq!(dot_zsql(&project)["bigquery_private_key"].as_str(), Some(PEM), "the whole key, not its first line");
    assert!(secret_of(&home, &project, "bigquery").starts_with("missing"), "a key with nobody to sign for is not a credential");

    let (ok, out) = run(
        zsql(&home, &project).args(["datasource", "auth", "-d", "bigquery", "--field", "client_email", "--secret-stdin"]),
        Some("zsql@strata-enterprise.iam.gserviceaccount.com\n"),
    );
    assert!(ok && out.contains("next: zsql datasource test -d bigquery"), "{out}");
    assert_eq!(dot_zsql(&project)["bigquery_private_key"].as_str(), Some(PEM), "one key written does not disturb another");
    assert_eq!(secret_of(&home, &project, "bigquery"), "yes");

    // A key named by its file is saved as the key, not as the path.
    std::fs::write(project.join("key.pem"), format!("{PEM}\n")).unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "auth", "-d", "bigquery", "--secret-stdin"]), Some("key.pem\n"));
    assert!(ok, "{out}");
    assert_eq!(dot_zsql(&project)["bigquery_private_key"].as_str(), Some(PEM));
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "auth", "-d", "bigquery", "--secret-stdin"]), Some("absent.pem\n"));
    assert!(!ok && out.contains("neither a PEM nor a file"), "{out}");
    // One written by hand over several lines is replaced whole.
    std::fs::write(
        project.join(".zsql"),
        "bigquery_private_key: |\n  -----BEGIN PRIVATE KEY-----\n  old\nbigquery_client_email: sa@p.iam.gserviceaccount.com\n",
    )
    .unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "auth", "-d", "bigquery", "--secret-stdin"]), Some(PEM));
    assert!(ok && !out.contains("warning"), "{out}");
    assert_eq!(dot_zsql(&project)["bigquery_private_key"].as_str(), Some(PEM));
    assert_eq!(secret_of(&home, &project, "bigquery"), "yes");

    // A BigQuery datasource is added without a key file, and told where its key goes.
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "add", "bq2", "--adapter", "bigquery", "--project-id", "p"]), None);
    assert!(ok && out.contains("no key_file") && out.contains("zsql datasource auth -d bq2"), "{out}");

    // Nested, as .strata had them, is the same credential.
    std::fs::write(
        project.join(".zsql"),
        format!(
            "bigquery:\n  client_email: zsql@strata-enterprise.iam.gserviceaccount.com\n  private_key: {}\n",
            serde_json::to_string(PEM).unwrap()
        ),
    )
    .unwrap();
    assert_eq!(secret_of(&home, &project, "bigquery"), "yes");
    // And a key saved flat beside a nested one is not the one read, which is said.
    let (ok, out) = run(
        zsql(&home, &project).args(["datasource", "auth", "-d", "bigquery", "--secret-stdin"]),
        Some("-----BEGIN PRIVATE KEY-----\nother\n"),
    );
    assert!(ok && out.contains("warning") && out.contains("nested under `bigquery:`"), "{out}");
}

#[test]
fn a_password_is_saved_as_it_was_typed_whatever_yaml_makes_of_it() {
    let (home, project) = project("pw");
    assert!(secret_of(&home, &project, "wh").starts_with("missing"));
    let auth = |secret: &str| run(zsql(&home, &project).args(["datasource", "auth", "-d", "wh", "--secret-stdin"]), Some(secret));
    let (ok, out) = auth("hunter2\n");
    assert!(ok && out.contains("saved   password for wh"), "{out}");
    assert!(std::fs::read_to_string(project.join(".zsql")).unwrap().contains("wh_password: hunter2\n"), "plain where plain reads back");
    // Digits, a comment sign, a colon, a quote: each reads back as text.
    for secret in ["123456", "a #b", "k: v", "it's \"quoted\"", "*star", "true"] {
        let (ok, out) = auth(&format!("{secret}\r\n"));
        assert!(ok, "{out}");
        assert_eq!(dot_zsql(&project)["wh_password"].as_str(), Some(secret));
        assert_eq!(secret_of(&home, &project, "wh"), "yes");
    }
    let (ok, out) = auth("\n");
    assert!(!ok && out.contains("nothing entered"), "{out}");
}

/// `.zsql` holds more than secrets: any key of the entry may be kept
/// there, and where both files say one, `.zsql` is the one read.
#[test]
fn a_setting_in_dot_zsql_goes_over_the_one_in_datasources_yml() {
    let (home, project) = project("overlay");
    let place = |key: &str| {
        let (ok, out) = run(zsql(&home, &project).args(["datasource", "list", "--json"]), None);
        assert!(ok, "{out}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let row = v["datasources"].as_array().unwrap().iter().find(|d| d["datasource"] == key).unwrap().clone();
        row["where"].as_str().unwrap().to_string()
    };
    assert_eq!((place("wh"), place("bigquery")), ("db".to_string(), "strata-enterprise".to_string()));
    std::fs::write(project.join(".zsql"), "wh:\n  host: localhost\n  adapter: sqlite\nbigquery_project_id: my-own\n").unwrap();
    assert_eq!((place("wh"), place("bigquery")), ("localhost".to_string(), "my-own".to_string()), "nested or flat, under any spelling");
    let (_, out) = run(zsql(&home, &project).args(["datasource", "list"]), None);
    assert!(out.contains("postgres") && !out.contains("sqlite"), "the adapter is the model's, not this machine's: {out}");
}
