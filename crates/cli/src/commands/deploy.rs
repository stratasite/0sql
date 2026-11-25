//! `zsql deploy`: the checked-out branch to a server, once or on every
//! change.

use crate::cli::{Target, Where};
use crate::side::Side;
use crate::{project, remote, show};
use anyhow::{bail, Result};

/// How a deploy is asked for.
pub struct How {
    pub dry_run: bool,
    pub watch: bool,
    pub skip_check: bool,
    pub clear_secrets: bool,
}

pub fn deploy(at: Where, how: How) -> Result<()> {
    let How { dry_run, watch, skip_check, clear_secrets } = how;
    let t = at.target(true)?;
    let Some(p) = &t.project else { bail!("deploy needs a project directory") };
    // The model loaded and tested here first: a round trip to the server
    // to be told the file does not load is a round trip wasted.
    if !skip_check {
        let local = crate::local::Local { project: p.clone() };
        let mut side = crate::side::Session::quiet(local)?;
        let v = side.check()?;
        if !show::print_tests(&v["test_results"]) {
            bail!("tests fail here; fix them, or --skip-check to send anyway");
        }
    }
    let action = if dry_run { "validate" } else { "deploy" };
    let once = |t: &Target| -> Result<bool> {
        let archive = project::archive(&p.dir, if dry_run { project::For::Validate } else { project::For::Deploy })?;
        let note = if t.branch == p.production_branch { " (the production branch)" } else { "" };
        println!("{action} {} ({} bytes) as {}{note} to {}", p.name, archive.len(), t.name(), t.client.server);
        // Said before it goes: a deploy carries the secrets this machine
        // holds, and they become the datasource's credential there.
        let carried = project::carries_secrets_for(&p.dir);
        if !carried.is_empty() && !dry_run {
            println!("with the secrets held here for: {}", carried.join(", "));
        }
        let clear = if clear_secrets && !dry_run { "?clear_secrets=true" } else { "" };
        let v = t.client.post_bytes(
            &format!("{}/{action}{clear}", remote::branch_path(t.org.as_deref(), &t.uid, &t.branch)),
            "application/gzip",
            &archive,
        )?;
        Ok(show::print_deploy(&v))
    };
    if !watch {
        if !once(&t)? {
            bail!("tests failed");
        }
        return Ok(());
    }
    if let Err(e) = once(&t) {
        eprintln!("error: {e}");
    }
    println!("watching {} for changes (ctrl-c to stop)", p.dir.display());
    project::watch(
        &p.dir,
        || false,
        || {
            println!();
            if let Err(e) = once(&t) {
                eprintln!("error: {e}");
            }
        },
    );
    Ok(())
}
