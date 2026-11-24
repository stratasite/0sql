//! `zsql`: the command line. One rule decides where a command runs:
//! anything about the model runs here, on the files, unless a flag names
//! a server. `-r` says the server, `--server` and `--uid` imply it, and
//! `--branch` alone is a mistake, not a hint. Commands that connect to a
//! warehouse (`catalog`, `generate --live`, `sample`, `run`, `datasource
//! test`) never take those flags: the credentials are on this machine
//! and the server holds none.

mod assist;
mod cli;
mod commands;
mod config;
mod failure;
mod local;
mod project;
mod remote;
mod show;
mod side;
mod templates;
mod tty;

use clap::Parser;
use cli::{Cli, Command, Remote};
use std::path::Path;

fn main() {
    let Cli { command, project, no_input, agent, verbose, log, log_format } = Cli::parse();
    tty::set_no_input(no_input);
    tty::set_agent(agent);
    let command = match command {
        Some(c) => c,
        // Bare `zsql` on a terminal inside a project is the repl; anywhere
        // else, or with no terminal to talk to, the help.
        None if tty::interactive() && (project.is_some() || config::find_project(Path::new(".")).is_ok()) => {
            Command::Repl { context: None, remote: Remote::default() }
        }
        None => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            return;
        }
    };
    // A closed pipe (`zsql ... | head`) ends the command quietly instead
    // of panicking. The server must keep Rust's default of ignoring
    // SIGPIPE instead, or a client hanging up mid-answer would kill it.
    #[cfg(unix)]
    if !command.serves() {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    // The log, before anything that might report to it. A server says
    // what it is doing unless told not to; a command says nothing
    // unless asked, so what it prints stays what it always printed.
    let serves = command.serves();
    let installed = log_format.parse::<zsql_log::Format>().and_then(|format| {
        zsql_log::init(&zsql_log::Config {
            name: if serves { "zsqld" } else { "zsql" },
            filter: log.unwrap_or_else(|| zsql_log::filter_for(verbose, if serves { "info" } else { "off" })),
            format,
        })
    });
    if let Err(e) = installed {
        eprintln!("error: {e}");
        std::process::exit(2);
    }
    let json = command.wants_json();
    if let Err(e) = commands::run(command, project) {
        if e.downcast_ref::<tty::Cancelled>().is_some() {
            eprintln!("{e}");
            std::process::exit(130);
        }
        failure::report(&e, json);
        std::process::exit(1);
    }
}
