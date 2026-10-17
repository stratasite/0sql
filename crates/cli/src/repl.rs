//! The interactive loop, over either side: a line of the shorthand, or
//! a dot command, until `.quit`.

use crate::failure::Refusal;
use crate::side::{bold, dim, Query, Side};
use crate::{config, show};
use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Write};
use std::time::Instant;

pub fn run(mut side: impl Side, context: Option<Value>) -> Result<()> {
    let mut field_names = side.announce()?;
    println!("{}", dim("type a line of projections and filters; .help for the forms, .quit to leave"));

    let mut editor: Option<rustyline::DefaultEditor> =
        if std::io::stdin().is_terminal() { rustyline::DefaultEditor::new().ok() } else { None };
    let history = config::home().map(|h| h.join("history"));
    if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
        if let Some(parent) = h.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = ed.load_history(h);
    }
    let stdin = std::io::stdin();
    loop {
        let line = match editor.as_mut() {
            Some(ed) => match ed.readline("zsql> ") {
                Ok(l) => {
                    let _ = ed.add_history_entry(l.as_str());
                    l
                }
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(_) => break,
            },
            None => {
                print!("zsql> ");
                std::io::stdout().flush()?;
                let mut l = String::new();
                if stdin.lock().read_line(&mut l)? == 0 {
                    break;
                }
                l
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if side.refresh()? {
            field_names = side.announce()?;
        }
        let (word, rest) = line.split_once(' ').map(|(c, r)| (c, r.trim())).unwrap_or((line, ""));
        let names_a_field = |w: &str| field_names.iter().any(|n| n == &w.to_lowercase());
        let cmd = match word.strip_prefix('.') {
            Some(c) => c,
            None if matches!(word, "help" | "?" | "quit" | "exit" | "q") && rest.is_empty() && !names_a_field(word) => word,
            None => "",
        };
        if matches!(cmd, "quit" | "exit" | "q") {
            break;
        }
        let outcome: Result<()> = (|| {
            match cmd {
                "help" | "?" => println!("{}", zsql_shorthand::HELP),
                "tables" => show::print_tables(&side.tables()?),
                "fields" => show::print_fields(&side.fields(rest, false)?),
                "values" => {
                    if rest.is_empty() {
                        println!("usage: .values <dimension>");
                    } else {
                        show::print_values(&side.values(rest)?);
                    }
                }
                "branch" => {
                    if side.branch(if rest.is_empty() { None } else { Some(rest) })? {
                        field_names = side.announce()?;
                    }
                }
                "reload" => {
                    side.reload()?;
                    field_names = side.announce()?;
                }
                "spec" => match zsql_service::spec_of(None, Some(rest), &zsql_service::unlimited).map_err(Refusal::from) {
                    Ok((spec, _, items)) => {
                        for d in items {
                            println!("{}", dim(&d));
                        }
                        println!("{}", serde_json::to_string_pretty(&spec)?);
                    }
                    Err(e) => println!("error: {e}"),
                },
                "" | "json" | "explain" | "explore" => {
                    // A query: the shorthand, or a JSON spec after .json or
                    // starting with a brace. .explain and .explore take
                    // either; .explore narrows by a term after ` ? `.
                    let text = if cmd.is_empty() { line } else { rest };
                    let (text, term) = match cmd {
                        "explore" => text.rsplit_once(" ? ").map(|(l, q)| (l.trim(), Some(q.trim()))).unwrap_or((text, None)),
                        _ => (text, None),
                    };
                    let query = if cmd == "json" || text.starts_with('{') {
                        match serde_json::from_str(text) {
                            Ok(v) => Query::Spec(v),
                            Err(e) => {
                                println!("error: {e}");
                                return Ok(());
                            }
                        }
                    } else {
                        Query::Expr(text.to_string())
                    };
                    let origin = side.origin();
                    if cmd == "explore" {
                        match side.explore(&query, context.as_ref(), term) {
                            Ok(v) => show::print_explore(&v, &dim, origin),
                            Err(e) => println!("error: {e}"),
                        }
                        return Ok(());
                    }
                    let t0 = Instant::now();
                    match side.sql(&query, context.as_ref(), cmd == "explain") {
                        Ok(v) if cmd == "explain" => show::print_explain(&v, &dim, origin),
                        Ok(v) => {
                            show::print_sql(&v, &dim, &bold);
                            let took = t0.elapsed().as_secs_f64() * 1e3;
                            println!(
                                "{}",
                                dim(&if origin == "server" {
                                    format!("-- round trip {took:.2} ms")
                                } else {
                                    format!("-- planned here in {took:.2} ms")
                                })
                            );
                        }
                        Err(e) => println!("error: {e}"),
                    }
                }
                other => println!("unknown command .{other}; .help lists them"),
            }
            Ok(())
        })();
        if let Err(e) = outcome {
            println!("error: {e}");
        }
    }
    if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
        let _ = ed.save_history(h);
    }
    Ok(())
}
