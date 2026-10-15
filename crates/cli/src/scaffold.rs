//! `zsql init` and `zsql new`: a project directory and its model files
//! from the templates.

use crate::{config, templates};
use anyhow::{bail, Result};
use clap::Subcommand;
use config::Project;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum NewCommand {
    /// `models/<domain>/tbl.<name>.yml`
    Table {
        name: String,
        #[arg(long)]
        datasource: String,
        #[arg(long)]
        physical_name: Option<String>,
        #[arg(long, default_value = "core")]
        domain: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// `models/<domain>/rel.<domain>.yml`
    Relation {
        #[arg(long)]
        datasource: String,
        #[arg(long, default_value = "core")]
        domain: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// `tests/<name>.yml`
    Test {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

pub fn init(dir: Option<PathBuf>, name: Option<String>, uid: Option<String>) -> Result<()> {
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
    if !dir.join(".git").exists() {
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
    println!("\nnext: describe your warehouses in datasources.yml, `zsql new table ...`, `zsql sql --expr ...`; then `git commit`, `zsql auth --api-key ...`, `zsql deploy`");
    Ok(())
}

pub fn new_file(what: NewCommand) -> Result<()> {
    let (project_dir, rel, text) = match what {
        NewCommand::Table { name, datasource, physical_name, domain, project } => {
            let p = project_of(project)?;
            let file = zsql_project::parameterize(&name).replace('-', "_");
            (
                p.dir,
                format!("models/{domain}/tbl.{file}.yml"),
                templates::table_yml(&name, physical_name.as_deref().unwrap_or(&file), &datasource),
            )
        }
        NewCommand::Relation { datasource, domain, project } => {
            let p = project_of(project)?;
            (p.dir, format!("models/{domain}/rel.{domain}.yml"), templates::relation_yml(&datasource))
        }
        NewCommand::Test { name, project } => {
            let p = project_of(project)?;
            let file = zsql_project::parameterize(&name).replace('-', "_");
            (p.dir, format!("tests/{file}.yml"), templates::test_yml(&name))
        }
    };
    let path = project_dir.join(&rel);
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

fn project_of(dir: Option<PathBuf>) -> Result<Project> {
    match dir {
        Some(d) => config::read_project(&d),
        None => config::find_project(Path::new(".")),
    }
}
