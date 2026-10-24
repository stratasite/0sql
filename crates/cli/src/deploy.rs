//! `zsql deploy`: the checked-out branch to a server, once or on every
//! change.

use crate::side::Side;
use crate::{project, remote, show, Target, Where};
use anyhow::{bail, Result};

pub fn deploy(at: Where, dry_run: bool, watch: bool, skip_check: bool) -> Result<()> {
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
        let archive = project::archive(&p.dir)?;
        let note = if t.branch == p.production_branch { " (the production branch)" } else { "" };
        println!("{action} {} ({} bytes) as {}/{}{note} to {}", p.name, archive.len(), t.uid, t.branch, t.client.server);
        let v = t.client.post_bytes(&format!("{}/{action}", remote::branch_path(&t.uid, &t.branch)), "application/gzip", &archive)?;
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
