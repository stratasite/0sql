//! Printing what a query answered, whether it was planned here or on a
//! server: both produce the same JSON shapes, so one set of printers
//! serves both.

use anyhow::Result;
use serde_json::Value;

pub fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// The line above every statement: which datasource runs it. A SQL
/// comment, so the output stays pasteable into a client.
pub fn datasource_line(v: &Value) -> String {
    // The key, as datasources.yml and -d spell it; the display name after.
    match (v["datasource_uid"].as_str(), v["datasource"].as_str()) {
        (Some(uid), Some(name)) if name != uid => format!("-- datasource: {uid} ({name}, {})", v["adapter"].as_str().unwrap_or("?")),
        (Some(uid), _) => format!("-- datasource: {uid} ({})", v["adapter"].as_str().unwrap_or("?")),
        (None, name) => format!("-- datasource: {}", name.unwrap_or("?")),
    }
}

/// How each item of an expression line was read, when the query came from one.
pub fn items(v: &Value) -> Vec<String> {
    v["items"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

/// One line per reference that resolved by near match.
pub fn corrections(v: &Value) -> Vec<String> {
    v["corrections"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| format!("{} → {} (~{})", c["term"].as_str().unwrap_or(""), c["field_name"].as_str().unwrap_or(""), c["score"]))
        .collect()
}

/// The SQL answer as text: how the items were read, the corrections,
/// the datasource line, the statement.
pub fn print_sql(v: &Value, style: &dyn Fn(&str) -> String, emphasis: &dyn Fn(&str) -> String) {
    for i in items(v) {
        println!("{}", style(&i));
    }
    for c in corrections(v) {
        println!("{}", style(&c));
    }
    println!("{}", style(&datasource_line(v)));
    println!("{}", emphasis(v["sql"].as_str().unwrap_or("")));
}

/// The explain answer as text: the SQL, the node graph from the root down,
/// and what each phase cost. `origin` says who planned it: `server` or
/// `local`.
pub fn print_explain(v: &Value, style: &dyn Fn(&str) -> String, origin: &str) {
    for i in items(v) {
        println!("{}", style(&i));
    }
    for c in corrections(v) {
        println!("{}", style(&c));
    }
    println!("{}", style(&datasource_line(v)));
    println!("{}", v["sql"].as_str().unwrap_or(""));
    let nodes = v["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        return;
    }
    println!();
    println!("{}", style(&format!("-- {} nodes", nodes.len())));
    fn walk(n: &Value, depth: usize, via: &str, nodes: &[Value], style: &dyn Fn(&str) -> String) {
        let by_id = |id: u64| nodes.iter().find(|n| n["id"].as_u64() == Some(id));
        let pad = "  ".repeat(depth);
        let mut head = format!("{pad}{via}{} {}", n["kind"].as_str().unwrap_or("?"), n["alias"].as_str().unwrap_or(""));
        if let Some(t) = n["table"].as_str() {
            head.push_str(&format!(" on {t}"));
            if let Some(d) = n["datasource"].as_str() {
                head.push_str(&format!(" ({d})"));
            }
        }
        if let Some(j) = n["join_type"].as_str() {
            head.push_str(&format!(" {j} join"));
        }
        if n["group_by"].as_bool() == Some(true) {
            head.push_str(" group by");
        }
        if n["segment"].as_bool() == Some(true) {
            head.push_str(" [segment]");
        }
        if let Some(t) = n["transform"].as_str() {
            head.push_str(&format!(" [{t}]"));
        }
        if let Some(p) = n["purpose"].as_str() {
            head.push_str(&format!(" [{p}]"));
        }
        println!("{head}");
        let list = |label: &str, key: &str| {
            let items: Vec<&str> = n[key].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if !items.is_empty() {
                println!("{pad}    {}", style(&format!("{label}: {}", items.join(", "))));
            }
        };
        list("paths", "paths");
        list("projections", "projections");
        list("filters", "filters");
        list("security", "security_filters");
        list("masks", "security_masks");
        for s in n["strategies"].as_array().into_iter().flatten() {
            if let Some(c) = s["node"].as_u64().and_then(by_id) {
                walk(c, depth + 1, &format!("{} <- ", s["kind"].as_str().unwrap_or("strategy")), nodes, style);
            }
        }
        for c in n["inputs"].as_array().into_iter().flatten().filter_map(Value::as_u64).filter_map(by_id) {
            walk(c, depth + 1, "", nodes, style);
        }
    }
    if let Some(root) = nodes.iter().find(|n| n["root"].as_bool() == Some(true)) {
        walk(root, 0, "", &nodes, style);
    }
    let phases: Vec<String> =
        v["phases"].as_array().into_iter().flatten().map(|p| format!("{} {}", p["name"].as_str().unwrap_or(""), p["us"])).collect();
    println!();
    println!(
        "{}",
        style(&format!(
            "-- {origin} {} us: parser {} us, plan {} us",
            v["timings"]["total_us"], v["timings"]["parser_us"], v["timings"]["plan_us"]
        ))
    );
    println!("{}", style(&format!("-- phases (us): {}", phases.join(", "))));
}

pub fn print_explore(v: &Value, style: &dyn Fn(&str) -> String, origin: &str) {
    let mut counts = Vec::new();
    for (key, label) in [("dimensions", "dim"), ("measures", "msr")] {
        let fields = v[key].as_array().cloned().unwrap_or_default();
        for f in &fields {
            let tables: Vec<&str> = f["tables"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let near = match f["score"].as_f64() {
                Some(s) if s < 1.0 => format!(" ~{s:.2}"),
                _ => String::new(),
            };
            println!(
                "{:<40} {} {:<32} {}{near}",
                f["uid"].as_str().unwrap_or(""),
                label,
                f["name"].as_str().unwrap_or(""),
                style(&tables.join(", "))
            );
        }
        counts.push(format!("{} {key}", fields.len()));
    }
    println!("{}", style(&format!("-- can add {} · {origin} {} us", counts.join(", "), v["total_us"])));
}

pub fn print_fields(v: &Value) {
    for f in v["fields"].as_array().into_iter().flatten() {
        let kind = match f["kind"].as_str() {
            Some("dimension") => "dim",
            Some("measure") => "msr",
            _ => "?",
        };
        let near = match f["score"].as_f64() {
            Some(s) if s < 1.0 => format!(" ~{s:.2}"),
            _ => String::new(),
        };
        println!(
            "{:<40} {} {}{}{near}",
            f["uid"].as_str().unwrap_or(""),
            kind,
            f["name"].as_str().unwrap_or(""),
            if f["hidden"].as_bool().unwrap_or(false) { " (hidden)" } else { "" }
        );
    }
}

pub fn print_tables(v: &Value) {
    for t in v["tables"].as_array().into_iter().flatten() {
        println!(
            "{:<32} {:<32} {} cost {}",
            t["uid"].as_str().unwrap_or(""),
            t["physical_name"].as_str().unwrap_or(""),
            t["datasource"].as_str().unwrap_or("?"),
            t["cost"]
        );
    }
}

pub fn print_values(v: &Value) {
    if let Some(r) = v.get("range") {
        println!(
            "{:<40} {} .. {}  (distinct {})",
            v["uid"].as_str().unwrap_or(""),
            r["min"].as_str().unwrap_or(""),
            r["max"].as_str().unwrap_or(""),
            v["distinct"]
        );
        return;
    }
    let values = v["values"].as_array().cloned().unwrap_or_default();
    println!("{:<40} {} of {} values", v["uid"].as_str().unwrap_or(""), values.len(), v["distinct"]);
    for x in values.iter().take(20) {
        println!("    {:<60} {}", x["v"].as_str().unwrap_or(""), x["n"]);
    }
    if values.len() > 20 {
        println!("    ... {} more", values.len() - 20);
    }
}

/// The deploy, validate or check answer: counts, warnings, test results.
pub fn print_deploy(v: &Value) -> bool {
    let validated = v["validated_only"].as_bool().unwrap_or(false);
    println!(
        "{} {}/{}: {} tables, {} fields, {} joins, {} paths, {} policies, {} sampled dimensions",
        if validated { "validated" } else { "deployed" },
        v["project"].as_str().unwrap_or(""),
        v["branch"].as_str().unwrap_or(""),
        v["tables"],
        v["fields"],
        v["joins"],
        v["paths"],
        v["policies"],
        v["values"]
    );
    for w in v["warnings"].as_array().into_iter().flatten() {
        println!("warning: {}", w.as_str().unwrap_or(""));
    }
    let catalog_ok = if v["catalog"].is_object() { crate::catalog_check::print(&v["catalog"]) } else { true };
    print_tests(&v["test_results"]) && catalog_ok
}

/// Whether a check passed, for the callers that print the JSON instead
/// of the lines. The same judgment as [`print_deploy`], without the
/// printing.
pub fn deploy_ok(v: &Value) -> bool {
    tests_passed(&v["test_results"]) && !v["catalog"]["fails"].as_bool().unwrap_or(false)
}

/// The same for test results alone.
pub fn tests_passed(t: &Value) -> bool {
    t["failed"].as_u64().unwrap_or(0) == 0
}

/// Prints test results; true when none failed.
pub fn print_tests(t: &Value) -> bool {
    let results = t["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        if t.is_object() {
            println!("no tests (tests/*.yml)");
        }
        return true;
    }
    for r in &results {
        let status = r["status"].as_str().unwrap_or("?");
        println!("{:<6} {}", status.to_uppercase(), r["name"].as_str().unwrap_or(""));
        if status != "passed" {
            if let Some(m) = r["message"].as_str() {
                println!("       {m}");
            }
            if let Some(sql) = r["generated_sql"].as_str() {
                println!("       --- generated\n{}", indent(sql));
            }
            if let Some(sql) = r["expected_sql"].as_str() {
                println!("       --- expected\n{}", indent(sql));
            }
        }
    }
    println!("tests: {} passed, {} failed", t["passed"], t["failed"]);
    t["failed"].as_u64().unwrap_or(0) == 0
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("       {l}")).collect::<Vec<_>>().join("\n")
}

/// After a local `check`: the one next step, when there is one. A
/// model that marks dimensions for sampling and has no sample file yet
/// is told the command; the rest is quiet.
pub fn next_after_check(v: &Value, project: &crate::config::Project) {
    let sampled = v["values"].as_u64().unwrap_or(0);
    let has_file = project.dir.join("samples").join(zsql_project::values::FILE_NAME).exists();
    if sampled > 0 || has_file {
        return;
    }
    let mut declares = false;
    let mut stack = vec![project.dir.join("models")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "yml" || x == "yaml")
                && std::fs::read_to_string(&p).is_ok_and(|s| s.lines().any(|l| l.trim_start().starts_with("enable_sampling: true")))
            {
                declares = true;
            }
        }
    }
    if declares {
        eprintln!("next: zsql sample (the model marks dimensions for sampling and none is sampled yet)");
    }
}
