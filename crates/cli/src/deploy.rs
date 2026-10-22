//! `zsql deploy`: the checked-out branch to a server, once or on every
//! change.

use crate::{project, remote, show, Target, Where};
use anyhow::{bail, Result};

pub fn deploy(at: Where, dry_run: bool, watch: bool) -> Result<()> {
    let t = at.target(true)?;
    let Some(p) = &t.project else { bail!("deploy needs a project directory") };
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
