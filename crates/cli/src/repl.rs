//! The interactive loop, over either side: a line of the expression
//! language, or a dot command, until `.quit`.

use crate::assist::Assist;
use crate::failure::Refusal;
use crate::side::{bold, dim, Query, Side};
use crate::{config, show};
use anyhow::Result;
use serde_json::Value;
use std::cell::RefCell;
use std::io::{BufRead, IsTerminal, Write};
use std::rc::Rc;
use std::time::Instant;

type Editor<S> = rustyline::Editor<Assist<Rc<RefCell<S>>>, rustyline::history::DefaultHistory>;

pub fn run<S: Side>(side: S, context: Option<Value>) -> Result<()> {
    let side = Rc::new(RefCell::new(side));
    let mut field_names = side.borrow_mut().announce()?;
    println!("{}", dim("type a line: projections, filters, calculations, segments; tab completes, .help for the forms, .quit to leave"));

    // The line editor, with the grammar's completion, hints and color,
    // when stdin is a terminal; a plain read otherwise.
    let mut editor: Option<Editor<S>> = if std::io::stdin().is_terminal() {
        let config = rustyline::Config::builder().completion_type(rustyline::CompletionType::List).build();
        rustyline::Editor::with_config(config).ok().map(|mut ed: Editor<S>| {
            ed.set_helper(Some(Assist::new(side.clone())));
            ed
        })
    } else {
        None
    };
    let history = config::home().map(|h| h.join("history"));
    if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
        if let Some(parent) = h.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = ed.load_history(h);
    }
    // After the model changes: the field names here, the helper's catalog.
    let reread = |editor: &mut Option<Editor<S>>, side: &Rc<RefCell<S>>| -> Result<Vec<String>> {
        let names = side.borrow_mut().announce()?;
        if let Some(h) = editor.as_ref().and_then(|ed| ed.helper()) {
            h.refresh();
        }
        Ok(names)
    };
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
        if side.borrow_mut().refresh()? {
            field_names = reread(&mut editor, &side)?;
        }
        let (word, rest) = line.split_once(' ').map(|(c, r)| (c, r.trim())).unwrap_or((line, ""));
        let names_a_field = |w: &str| field_names.iter().any(|n| n == &w.to_lowercase());
        let cmd = match word.strip_prefix('.') {
            Some("") => {
                println!("a dot alone is no command; .help lists them");
                continue;
            }
            Some(c) => c,
            None if matches!(word, "help" | "?" | "quit" | "exit" | "q") && rest.is_empty() && !names_a_field(word) => word,
            None => "",
        };
        if matches!(cmd, "quit" | "exit" | "q") {
            break;
        }
        let outcome: Result<()> = (|| {
            match cmd {
                "help" | "?" => println!("{}", zsql_shorthand::help()),
                "tables" => show::print_tables(&side.borrow_mut().tables()?),
                "fields" => show::print_fields(&side.borrow_mut().fields(rest, false)?),
                "values" => {
                    if rest.is_empty() {
                        println!("usage: .values <dimension>");
                    } else {
                        show::print_values(&side.borrow_mut().values(rest)?);
                    }
                }
                "branch" => {
                    if side.borrow_mut().branch(if rest.is_empty() { None } else { Some(rest) })? {
                        field_names = reread(&mut editor, &side)?;
                    }
                }
                "reload" => {
                    side.borrow_mut().reload()?;
                    field_names = reread(&mut editor, &side)?;
                }
                "run" | "run!" => {
                    // The line as a query, run and kept; `.run!` runs it fresh.
                    if rest.is_empty() {
                        println!("usage: .run <line or {{...spec...}}>   (.run! to bypass the cache)");
                        return Ok(());
                    }
                    let query = if rest.starts_with('{') {
                        match serde_json::from_str(rest) {
                            Ok(v) => Query::Spec(v),
                            Err(e) => {
                                println!("error: {e}");
                                return Ok(());
                            }
                        }
                    } else {
                        Query::Expr(rest.to_string())
                    };
                    if let Err(e) = side.borrow_mut().run(&query, context.as_ref(), cmd == "run!") {
                        println!("error: {e}");
                    }
                }
                "results" => {
                    if let Err(e) = side.borrow_mut().results() {
                        println!("error: {e}");
                    }
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
                    // A query: an expression line, or a JSON spec after .json or
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
                    let origin = side.borrow_mut().origin();
                    if cmd == "explore" {
                        match side.borrow_mut().explore(&query, context.as_ref(), term) {
                            Ok(v) => show::print_explore(&v, &dim, origin),
                            Err(e) => println!("error: {e}"),
                        }
                        return Ok(());
                    }
                    let t0 = Instant::now();
                    match side.borrow_mut().sql(&query, context.as_ref(), cmd == "explain") {
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
                "catalog" | "generate" | "sample" | "check" | "test" | "deploy" => {
                    println!("`zsql {cmd}` is a command-line step; the repl plans and runs queries. .help lists what it does.")
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
