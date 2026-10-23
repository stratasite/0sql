//! `zsql init` and `zsql new`: a project directory and its model files
//! from the templates.

use crate::{config, templates};
use anyhow::{bail, Result};
use config::Project;
use std::path::{Path, PathBuf};

pub fn init(dir: Option<PathBuf>, name: Option<String>, uid: Option<String>, no_git: bool) -> Result<()> {
    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    if dir.join("project.yml").exists() {
        bail!("{} already holds a project", dir.display());
    }
    let name = name.unwrap_or_else(|| {
        dir.canonicalize().ok().and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string())).unwrap_or_else(|| "project".into())
    });
    let uid = uid.map(|u| u.to_lowercase()).unwrap_or_else(|| zsql_project::parameterize(&name));
    let write = |rel: &str, text: &str| -> Result<()> {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            println!("keep   {rel}");
            return Ok(());
        }
        std::fs::write(&path, text)?;
        println!("create {rel}");
        Ok(())
    };
    write("project.yml", &templates::project_yml(&name, &uid))?;
    write("datasources.yml", templates::DATASOURCES_YML)?;
    write("security.yml", templates::SECURITY_YML)?;
    write("AGENTS.md", templates::AGENTS_MD)?;
    write("models/.keep", "")?;
    write("tests/.keep", "")?;
    let gi = dir.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if !existing.contains(".zsql") {
        std::fs::write(
            &gi,
            format!("{existing}{}{}", if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" }, templates::GITIGNORE),
        )?;
        println!("update .gitignore");
    }
    if !no_git && !dir.join(".git").exists() {
        // Deploying reads the checked-out branch, so an uncommitted
        // project cannot be deployed. Say so here, where it is a step on
        // a list, rather than at the deploy that fails; and say when git
        // itself could not be run, since the hint then does not apply.
        match std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&dir).status() {
            Ok(s) if s.success() => println!("git init (branch main) — commit before deploying; zsql deploys the checked-out branch"),
            Ok(_) => println!("git init failed (an old git without `init -b`?); run it yourself before deploying"),
            Err(_) => println!("git is not installed; a deploy needs the project in a git repository"),
        }
    }
    eprintln!("\nnext: zsql datasource add, or edit datasources.yml; then zsql catalog read --schema <name>, zsql generate, zsql check");
    Ok(())
}

fn write_new(project_dir: &Path, rel: &str, text: &str) -> Result<()> {
    let path = project_dir.join(rel);
    if path.exists() {
        bail!("{} exists", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, text)?;
    println!("create {rel}");
    Ok(())
}

/// `zsql generate NAME --empty table`: a table model from the template,
/// with no catalog behind it.
pub fn empty_table(
    project_dir: Option<&Path>,
    name: &str,
    datasource: &str,
    physical_name: Option<&str>,
    domain: Option<&str>,
) -> Result<()> {
    let p = project_of(project_dir.map(Path::to_path_buf))?;
    let domain = domain.unwrap_or("core");
    let file = zsql_project::parameterize(name).replace('-', "_");
    let physical = physical_name.unwrap_or(name);
    write_new(&p.dir, &format!("models/{domain}/tbl.{file}.yml"), &templates::table_yml(name, physical, datasource))?;
    eprintln!("next: name its fields, then zsql check");
    Ok(())
}

/// `zsql test --new NAME`: a test from the template, projecting the
/// model's own first dimension and measure when it loads, so the test
/// runs as written; placeholders otherwise.
pub fn new_test(project_dir: Option<&Path>, name: &str) -> Result<()> {
    let p = project_of(project_dir.map(Path::to_path_buf))?;
    let file = zsql_project::parameterize(name).replace('-', "_");
    let fields = zsql_service::Model::from_dir(&p.dir, zsql_service::Trust::Owned, crate::local::dialects()).ok().map(|m| {
        let s = m.universe().snapshot();
        let first = |kind: zsql_semantic::FieldKind| s.fields.iter().find(|f| f.kind == kind && !f.hidden).map(|f| f.name.clone());
        (first(zsql_semantic::FieldKind::Dimension), first(zsql_semantic::FieldKind::Measure))
    });
    write_new(&p.dir, &format!("tests/{file}.yml"), &templates::test_yml(name, fields))?;
    eprintln!("next: zsql test");
    Ok(())
}

fn project_of(dir: Option<PathBuf>) -> Result<Project> {
    match dir {
        Some(d) => config::read_project(&d),
        None => config::find_project(Path::new(".")),
    }
}
