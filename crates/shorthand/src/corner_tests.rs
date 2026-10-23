//! The corners of the grammar: whitespace, quoting, keywords in odd
//! places, every error path, and the raw JSON merge. Each case is an
//! input and either the spec it must make or the words its error must
//! hold.

use crate::{describe, parse_line};
use serde_json::{json, Value};

fn spec(line: &str) -> Value {
    parse_line(line).unwrap_or_else(|e| panic!("{line}: {e}")).0
}
fn err(line: &str) -> String {
    match parse_line(line) {
        Ok((spec, _)) => panic!("{line}: parsed to {spec}"),
        Err(e) => e,
    }
}
fn errs(cases: &[(&str, &str)]) {
    for (line, expected) in cases {
        let e = err(line);
        assert!(e.contains(expected), "{line}: expected `{expected}` in `{e}`");
    }
}
fn fields(s: &Value) -> Vec<String> {
    s["projections"].as_array().unwrap().iter().map(|p| p["field"].as_str().unwrap().to_string()).collect()
}

#[test]
fn whitespace_in_every_place() {
    assert_eq!(fields(&spec("  a ,b,  c  ")), ["a", "b", "c"]);
    assert_eq!(fields(&spec("a,,b,")), ["a", "b"]);
    assert_eq!(fields(&spec("a\tb")), ["a b"]);
    assert_eq!(fields(&spec("a   b")), ["a b"], "runs of spaces inside a name are one space");
    assert_eq!(fields(&spec("a\r\nb\r\n")), ["a", "b"], "CRLF separates like LF");
    assert_eq!(fields(&spec("\n\na\n\n\nb\n")), ["a", "b"]);
    let s = spec("moving_avg(\n  x,\n  7\n  by a\n  order by b desc\n)");
    assert_eq!(s["projections"][0]["decorators"][0]["order_by"], json!({"b": "desc"}));
    let s = spec("calc(\n  sum([A]@m)\n  / 2\n) as X");
    assert_eq!(s["calculations"][0]["sql"], "sum([A]@m)\n  / 2");
    let s = spec("include (\n  customer id\n  x > 1\n)");
    assert_eq!(s["segments"][0]["keys"], json!(["customer id"]));
    assert_eq!(s["segments"][0]["filters"].as_array().unwrap().len(), 1);
    let s = spec("x in (\n a ,\n b\n)");
    assert_eq!(s["filters"][0]["value"], "a, b");
    assert_eq!(spec("x = a\n, y = b")["filters"].as_array().unwrap().len(), 2);
}

#[test]
fn keywords_are_case_insensitive_and_names_keep_their_case() {
    let s = spec("Country IN (A), Paid DESC, MONTH(Order Date) AS Mo, Include (Customer Id, Paid > 1) As Buyers, LIMIT 5");
    assert_eq!(s["filters"][0]["field"], "Country");
    assert_eq!(s["projections"][0], json!({"field": "Paid", "order_by": "desc"}));
    assert_eq!(s["projections"][1]["decorators"][0]["grain"], "month");
    assert_eq!(s["projections"][1]["alias"], "Mo");
    assert_eq!(s["segments"][0]["name"], "Buyers");
    assert_eq!(s["limit"], 5);
    assert_eq!(spec("[Net Paid]@M, \"Net Paid\"@D")["projections"][0]["field_type"], "measure");
}

#[test]
fn item_leading_and_function_words_fall_through_to_fields() {
    let s = spec("limit, hints, include, expand tax, exclude, calc, extract, custom, month, year to date, rank, share");
    assert_eq!(
        fields(&s),
        ["limit", "hints", "include", "expand tax", "exclude", "calc", "extract", "custom", "month", "year to date", "rank", "share"]
    );
    assert_eq!(spec("[top], [in], \"as\", [and], [order by status]")["projections"].as_array().unwrap().len(), 5);
    assert_eq!(fields(&spec("created by, order date, null count")), ["created by", "order date", "null count"]);
}

#[test]
fn reserved_words_end_names_wherever_they_stand() {
    errs(&[
        ("is active", "`is` is a reserved word and cannot start a name; write [is active]"),
        ("top seller, x", "`top` is a reserved word and cannot start a name"),
        ("sign in date", "a name holding `in` is bracketed: [sign in date]"),
        ("x in a", "`in` takes a parenthesised list"),
        ("top up, x", "`top` is a reserved word and cannot start a name"),
        ("x top up", "a name holding `top` is bracketed: [x top up]"),
        ("is active", "`is` is a reserved word and cannot start a name"),
        ("x is active", "a name holding `is` is bracketed: [x is active]"),
        ("x as top seller", "`as` needs an alias after it"),
        ("x and", "`and` is a reserved word and ends the name `x`"),
        ("x or y", "`or` is a reserved word and ends the name `x`"),
        ("x is", "`is` is followed by `null` or `not null`"),
        ("x is nul", "`is` is followed by `null` or `not null`"),
        ("x not null", "`not` goes before in, contains, starts with or ends with"),
        ("x not is null", "`not` goes before in"),
        ("x contains", "expected a space and then the value"),
        ("x starts foo", "`starts` is followed by `with`"),
        ("x between 1 2", "between needs `and`"),
        ("x between 1 and", "then the upper bound"),
        ("x top 0", "at least 1"),
        ("x top 5 by", "needs a measure"),
        ("x in", "`in` takes a parenthesised list"),
        ("x in ()", "at least one value"),
        ("x in (,a)", "expected a value in the list"),
        ("x in (a", "never closed"),
        ("x keyword a", "expected a parenthesised list"),
    ]);
}

#[test]
fn values_at_their_edges() {
    let s = spec("a = 0, b = -1.5, c = 1e5, d = .5, e = 1., f = null, g = '', h = ' x ', i = 'it''s', j = -");
    let v: Vec<Value> = s["filters"].as_array().unwrap().iter().map(|f| f["value"].clone()).collect();
    assert_eq!(
        v,
        vec![
            json!(0),
            json!(-1.5),
            json!("1e5"),
            json!(".5"),
            json!("1."),
            json!("null"),
            json!(""),
            json!(" x "),
            json!("it's"),
            json!("-")
        ]
    );
    let s = spec("x in (a,), z in (Rock and Roll, 'x (y)'), w in ( 'q' )");
    assert_eq!(s["filters"][0]["value"], "a", "a trailing comma in a list is tolerated");
    assert_eq!(s["filters"][1]["value"], "Rock and Roll, x (y)");
    assert_eq!(s["filters"][2]["value"], "q");
    errs(&[
        ("y in ('a, b')", "cannot hold a comma"),
        ("x = 'open", "never closed"),
        ("x = \"v\"", "single-quoted"),
        ("x in (\"v\")", "single-quoted"),
    ]);
    let s = spec("x between 'a and b' and c, y >= -5, z <> 'a', w != b, v>=3, u<=4, t=5, s>6, r<7");
    assert_eq!(s["filters"][0]["value"], "a and b");
    assert_eq!(s["filters"][0]["value_end"], "c");
    let preds: Vec<&str> = s["filters"].as_array().unwrap().iter().map(|f| f["predicate"].as_str().unwrap()).collect();
    assert_eq!(
        preds,
        [
            "between",
            "greater_than_or_equal_to",
            "does_not_equal",
            "does_not_equal",
            "greater_than_or_equal_to",
            "less_than_or_equal_to",
            "equals",
            "greater_than",
            "less_than"
        ]
    );
    errs(&[("x => 5", "not an operator"), ("x =< 5", "not an operator")]);
    assert_eq!(spec("x <= =5")["filters"][0]["value"], "=5", "a value may begin with any character but a quote");
    assert_eq!(spec("rank(x order a)")["projections"][0]["field"], "x order a", "`order` is a word unless `by` follows");
    // A value runs to the item's end, keywords and all; and/or end it.
    let s = spec("x = made in china top desc as, y = a {\"z\": 1}");
    assert_eq!(s["filters"][0]["value"], "made in china top desc as");
    assert_eq!(s["filters"][1], json!({"field": "y", "predicate": "equals", "value": "a", "z": 1}));
}

#[test]
fn negations_and_the_like_family() {
    let s = spec("a not in (x), b not contains x, c not starts with x, d not ends with x, e is not null, f contains x, g starts with x, h ends with x, i is null");
    let preds: Vec<&str> = s["filters"].as_array().unwrap().iter().map(|f| f["predicate"].as_str().unwrap()).collect();
    assert_eq!(
        preds,
        [
            "exclude_list",
            "does_not_contain",
            "does_not_start_with",
            "does_not_end_with",
            "is_not_null",
            "contains",
            "starts_with",
            "ends_with",
            "is_null"
        ]
    );
    errs(&[("x not = 1", "write != instead of not ="), ("x not between 1 and 2", "no `not between`"), ("x not (a = 1)", "no `not (...)`")]);
}

#[test]
fn filter_trees_flatten_when_they_can() {
    assert_eq!(spec("a = 1 and b = 2 and c = 3")["filters"].as_array().unwrap().len(), 3);
    assert_eq!(spec("(a = 1 and b = 2), c = 3")["filters"].as_array().unwrap().len(), 3, "parentheses around an and still flatten");
    let s = spec("a = 1 or b = 2");
    assert_eq!(s["filters"]["or"].as_array().unwrap().len(), 2);
    let s = spec("a = 1 or b = 2, c = 3");
    assert_eq!(s["filters"]["and"][0]["or"].as_array().unwrap().len(), 2);
    assert_eq!(s["filters"]["and"][1]["field"], "c");
    let s = spec("a = 1 and b = 2 or c = 3 and d = 4");
    assert_eq!(s["filters"]["or"].as_array().unwrap().len(), 2, "and binds tighter than or");
    assert_eq!(s["filters"]["or"][0]["and"].as_array().unwrap().len(), 2);
    let s = spec("((a = 1))");
    assert_eq!(s["filters"][0]["field"], "a");
    let s = spec("(a = 1 or (b = 2 and (c = 3 or d = 4)))");
    assert_eq!(s["filters"]["or"][1]["and"][1]["or"].as_array().unwrap().len(), 2);
    errs(&[
        ("(a = 1", "expected ) to close the filter group"),
        ("()", "expected a filter inside"),
        ("(a)", "expected a filter inside"),
        (")", "nothing open"),
    ]);
}

#[test]
fn decorators_nest_and_check_their_arguments() {
    let s = spec("share(yoy(x)), running_sum(moving_avg(x, 3)), yoy(extract(month, custom('f(@expression)', d)))");
    let types = |i: usize| -> Vec<String> {
        s["projections"][i]["decorators"].as_array().unwrap().iter().map(|d| d["type"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(types(0), ["temporalize", "contribute"], "the innermost decorator applies first");
    assert_eq!(types(1), ["window", "window"]);
    assert_eq!(types(2), ["customize", "extract", "temporalize"]);
    let s = spec("ntile(x, 4), lag(x, 1), lead(x, 2), moving_sum(x, 2), running_count(x), dense_rank(x order by d), first_value(x by a, b order by c asc, d desc)");
    let d = |i: usize| s["projections"][i]["decorators"][0].clone();
    assert_eq!(d(0)["buckets"], 4);
    assert_eq!(d(1)["offset"], 1);
    assert_eq!(d(2)["offset"], 2);
    assert_eq!(d(3)["size"], 1);
    assert_eq!(d(4), json!({"type": "window", "mode": "running", "function": "count"}));
    assert_eq!(d(5)["mode"], "running");
    assert_eq!(d(6)["partition_refs"], json!(["a", "b"]));
    assert_eq!(d(6)["order_by"], json!({"c": "asc", "d": "desc"}));
    let s = spec("rank(x by order date order by order date desc), share(x by [order by status])");
    assert_eq!(s["projections"][0]["decorators"][0]["partition_refs"], json!(["order date"]));
    assert_eq!(s["projections"][1]["decorators"][0]["partition_refs"], json!(["order by status"]));
    for t in ["raw", "millisecond", "second", "minute", "hour", "day", "week", "month", "quarter", "year"] {
        assert_eq!(spec(&format!("{t}(d)"))["projections"][0]["decorators"][0]["grain"], t);
    }
    for t in ["yoy", "qoq", "mom", "wow", "dod"] {
        let s = spec(&format!("{t}(x), {t}_pct(x)"));
        assert_eq!(s["projections"][0]["decorators"][0].get("percent_change"), None);
        assert_eq!(s["projections"][1]["decorators"][0]["percent_change"], true);
    }
    errs(&[
        ("yoy(x, 2)", "takes no number"),
        ("rank(x, 2)", "takes no number"),
        ("running_sum(x, 2)", "takes no number"),
        ("ntile(x)", "needs the number of buckets"),
        ("lag(x, 0)", "at least 1"),
        ("moving_avg(x, 1)", "at least two rows"),
        ("moving_avg(x, a)", "expected a number"),
        ("month(x by a)", "takes no `by`"),
        ("share(x order by a)", "takes no `order by`"),
        ("share(x by)", "`by` needs a projected dimension"),
        ("rank(x order by)", "`order by` needs a projected dimension"),
        ("month()", "month() needs a field"),
        ("month(x", "month( is never closed"),
        ("month(x, y)", "expected a number"),
        ("extract(x)", "extract needs a part"),
        ("extract(month x)", "expected a comma after the part"),
        ("extract(month, )", "extract() needs a field"),
        ("custom(x)", "the SQL comes first, single-quoted"),
        ("custom('a' x)", "expected a comma after the SQL"),
        ("yoy(month(x) desc)", "expected ) to close yoy("),
        ("Count (Orders)", "unknown function `Count`"),
    ]);
}

#[test]
fn calculations_at_their_edges() {
    let s = spec("calc(a) as X, calc(a, integer), calc('a, )b' || ')'), calc((1 + (2))), calc(\"a)\"), calc([a)]@m), calc(a) desc, calc(a) as X {\"data_type\": \"string\"}");
    let c = s["calculations"].as_array().unwrap();
    assert_eq!(c[0]["alias"], "X");
    assert_eq!(c[1], json!({"calculation": true, "alias": "a", "sql": "a", "data_type": "integer"}));
    assert_eq!(c[2]["sql"], "'a, )b' || ')'");
    assert_eq!(c[3]["sql"], "(1 + (2))");
    assert_eq!(c[4]["sql"], "\"a)\"");
    assert_eq!(c[5]["sql"], "[a)]@m");
    assert_eq!(c[6]["order_by"], "desc");
    assert_eq!(c[7]["data_type"], "string");
    for t in ["string", "integer", "decimal", "date", "date_time", "boolean", "bigint"] {
        assert_eq!(spec(&format!("calc(a, {t})"))["calculations"][0]["data_type"], t);
    }
    errs(&[
        ("calc()", "calc() needs SQL"),
        ("calc( )", "calc() needs SQL"),
        ("calc(a", "calc( is never closed"),
        ("calc(a, binary)", "expected a result type"),
        ("calc(a, integer, b)", "expected ) to close calc("),
        ("calc(a) as", "`as` needs an alias"),
        ("calc(a) as desc", "`as` needs an alias"),
        ("calc(a) as X as Y", "alias is given twice"),
        ("calc(a) desc asc", "direction is given twice"),
        ("calc(a) as X@m", "alias carries no @d or @m"),
    ]);
}

#[test]
fn segments_at_their_edges() {
    let s = spec("exclude (a, b@m, c > 1 or d = 2, e) for (m1, m2) as \"Name for it\"");
    let g = &s["segments"][0];
    assert_eq!(g["mode"], "exclude");
    assert_eq!(g["keys"], json!(["a", "e"]));
    assert_eq!(g["measures"], json!(["b"]));
    assert_eq!(g["filters"]["or"].as_array().unwrap().len(), 2);
    assert_eq!(g["apply_to"], json!(["m1", "m2"]));
    assert_eq!(g["name"], "Name for it");
    let s = spec("include (a,) for m as N, include (a) as N for m");
    assert_eq!(s["segments"][0]["apply_to"], json!(["m"]));
    assert_eq!(s["segments"][1]["name"], "N");
    let s = spec("expand (a, b = 1) with (c, d as D) inner dedupe lt as E");
    let g = &s["segments"][0];
    assert_eq!(g.get("mode"), None);
    assert_eq!(g["expanding"], json!([{"field": "c"}, {"field": "d", "as": "D"}]));
    assert_eq!(g["join"], "inner");
    assert_eq!(g["pair_dedupe"], "lt");
    assert_eq!(g["name"], "E");
    let s = spec("include (a@d, b = 1 and c = 2)");
    assert_eq!(s["segments"][0]["keys"], json!(["a"]));
    assert_eq!(s["segments"][0]["filters"].as_array().unwrap().len(), 2);
    errs(&[
        ("include ()", "at least a key"),
        ("include (", "never closed"),
        ("include (a", "never closed"),
        ("include (a) as", "`as` needs the segment's name"),
        ("include (a) as N as M", "name is given twice"),
        ("include (a) for", "`for` needs a measure"),
        ("include (a) left", "apply to expand"),
        ("include (a) dedupe lt", "applies to expand"),
        ("expand (a)", "followed by with"),
        ("expand (a) with", "parenthesised list"),
        ("expand (a) with ()", "expected a dimension to carry"),
        ("expand (a) with (b) left inner", "join is given twice"),
        ("expand (a) with (b) dedupe x", "neq, lt or none"),
        ("expand (a) with (b) for m", "`for` does not apply"),
        ("include (a) {\"x\": 1} as N", "spec JSON comes last on an item"),
        ("include (month(a))", "unknown function `month`"),
    ]);
}

#[test]
fn settings_and_raw_json() {
    let s = spec("limit 0, hints a, hints (b, c), {\"name\": \"n\"}, {\"db_settings\": {\"week_start_day\": 1}}, {\"hints\": [\"d\"]}");
    assert_eq!(s["limit"], 0);
    assert_eq!(s["hints"], json!(["a", "b", "c", "d"]), "arrays append");
    assert_eq!(s["name"], "n");
    assert_eq!(s["db_settings"]["week_start_day"], 1);
    let s = spec("x as A {\"alias\": \"B\", \"decorators\": [{\"type\": \"truncate\", \"grain\": \"day\"}]}, month(y) {\"decorators\": [{\"type\": \"extract\", \"extract\": \"year\"}]}");
    assert_eq!(s["projections"][0]["alias"], "B", "scalars replace");
    assert_eq!(s["projections"][0]["decorators"][0]["grain"], "day");
    assert_eq!(s["projections"][1]["decorators"].as_array().unwrap().len(), 2, "arrays append");
    let s = spec("x = 1 {\"predicate\": \"custom\"}, {\"filters\": [{\"field\": \"y\", \"predicate\": \"is_null\"}]}");
    assert_eq!(s["filters"][0]["predicate"], "custom");
    assert_eq!(s["filters"][1]["field"], "y");
    let s = spec("{\"a\": {\"b\": 1}}, {\"a\": {\"c\": 2}}, {}");
    assert_eq!(s["a"], json!({"b": 1, "c": 2}), "objects merge deep");
    let s = spec("{\"s\": \"a \\\"quoted\\\" } brace\"}");
    assert_eq!(s["s"], "a \"quoted\" } brace");
    errs(&[
        ("limit -1", "whole number"),
        ("limit 1.5", "whole number"),
        ("limit 5, limit 6", "given twice"),
        ("hints (a", "expected , or )"),
        ("{", "never closed"),
        ("{\"a\": \"}", "never closed"),
        ("{\"a\"}", "does not parse"),
        ("x {\"a\": 1} {\"b\": 2}", "spec JSON attaches once per item"),
        ("x {\"a\": 1} desc", "spec JSON comes last on an item"),
        ("a = 1 or b = 2 {\"x\": 1}", "attaches to one filter"),
        ("x = 1 {1}", "does not parse"),
    ]);
    assert_eq!(fields(&spec("limit, hints")), ["limit", "hints"], "a setting word without its operand is a field");
}

#[test]
fn quoting_and_odd_characters_in_names() {
    let s = spec("[a, b], \"c, d\", [e (f)], \"g \"\"h\"\"\", [i \"j\"], \"k [l]\", P&L, Avg. Price, Share %, Q1'24, #Orders, Größe, 日本語 売上, x-y, _u");
    assert_eq!(
        fields(&s),
        [
            "a, b",
            "c, d",
            "e (f)",
            "g \"h\"",
            "i \"j\"",
            "k [l]",
            "P&L",
            "Avg. Price",
            "Share %",
            "Q1'24",
            "#Orders",
            "Größe",
            "日本語 売上",
            "x-y",
            "_u"
        ]
    );
    errs(&[
        ("[]", "empty []"),
        ("\"\"", "empty \"\""),
        ("[a", "never closed"),
        ("\"a", "never closed"),
        ("'a'", "single quote opens a value"),
        ("x@", "expected @d or @m"),
        ("x@x", "expected @d or @m"),
        ("x @m", "no space before it"),
        ("x@dd", "expected @d or @m"),
        ("x\"y\"", "a quote opens a name or a value only at the start of a word"),
    ]);
    assert_eq!(spec("a = 1 b")["filters"][0]["value"], "1 b", "a value runs to the item's end");
    assert_eq!(spec("Men's Wear, x = Men's")["filters"][0]["value"], "Men's");
}

/// A line cannot nest deeper than the parser's limit, so the stack is
/// never the thing that gives out.
#[test]
fn deep_nesting_is_refused_not_overflowed() {
    let deep = format!("{}x = 1{}", "(".repeat(5000), ")".repeat(5000));
    assert!(err(&deep).contains("nested deeper than 64 levels"));
    let deep = format!("{}x{}", "yoy(".repeat(5000), ")".repeat(5000));
    assert!(err(&deep).contains("nested deeper than 64 levels"));
    let deep = format!("include ({}x = 1{})", "(".repeat(5000), ")".repeat(5000));
    assert!(err(&deep).contains("nested deeper than 64 levels"));
    let fine = format!("{}x = 1{}", "(".repeat(60), ")".repeat(60));
    assert_eq!(spec(&fine)["filters"][0]["field"], "x");
    let fine = format!("{}x{}", "yoy(".repeat(60), ")".repeat(60));
    assert_eq!(spec(&fine)["projections"][0]["decorators"].as_array().unwrap().len(), 60);
}

/// The messages an exploratory test found wanting, each now saying
/// what the user meant to do.
#[test]
fn messages_found_by_trying_the_cli() {
    errs(&[
        ("not (a = 1)", "the spec has no `not (...)`"),
        ("monthly(x)", "the help lists the functions"),
        ("x @m", "no space before it"),
        ("limit 99999999999999999999999", "too large a number"),
        ("moving_avg(x, 99999999999999999999999)", "too large a number"),
        ("x top 99999999999999999999999", "too large a number"),
        ("moving_avg(x, 7 order by month(d))", "`order by` takes a projected dimension's name, not an expression"),
        ("share(x by month(d))", "`by` takes a projected dimension's name, not an expression"),
        ("x {\"alias\": \"X\"} as Y", "spec JSON comes last on an item"),
        ("limit 10, {\"limit\": 5}", "limit is given twice"),
        ("{\"limit\": 5}, limit 10", "limit is given twice"),
    ]);
    let s = spec("{\"projections\": [{\"field\": \"x\"}]}, department");
    assert_eq!(fields(&s), ["department", "x"], "raw JSON merges after the written items");
}

#[test]
fn errors_carry_a_position_in_characters() {
    assert!(err("Größe = \"x\"").ends_with("(column 9)"), "{}", err("Größe = \"x\""));
    assert!(err("a\nb\nc = \"x\"").ends_with("(line 3, column 5)"));
    assert!(err("x = 'open").contains("column 5"), "an unclosed quote names where it opened");
    assert!(err("月(x)").contains("unknown function `月`"));
}

#[test]
fn describe_reads_back_to_the_same_spec() {
    let lines = [
        "a, [b c]@m as D desc, \"e\"@d, month(f) as \"Is It\", share(g by h, i) asc",
        "x = 1, y != 'a, b', z between 'p and q' and r, w in (a, 'b c', 'd''e'), v not in (f), u contains made in china",
        "s is null, t is not null, q top 3 by m, p keyword (k, l), (a = 1 or b = 2) and (c = 3 or d = 4), e = 1 or f = 2",
        "calc(sum([A]@m) / 'x, )') as \"Share for\", yoy_pct(calc(count(*), integer)) as G desc",
        "include (k, m@m, f > 1 or g = 2) for (m1, m2) as \"For keeps\", expand (k, f = 1) with (c as \"Also, bought\", d) left dedupe none as E",
        "limit 5, hints (t1, t2), {\"name\": \"n\"}, x {\"format\": {\"type\": \"pct\"}}",
        "rank(x by order date order by order date desc, [created by] asc) as [Row in region], custom('lower(@expression)', y)",
    ];
    for line in lines {
        let (first, items) = parse_line(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        let again = describe(&items).iter().map(|d| d[13..].to_string()).collect::<Vec<_>>().join(", ");
        let (second, _) = parse_line(&again).unwrap_or_else(|e| panic!("{line}\n  read back as {again}\n  {e}"));
        assert_eq!(first, second, "{line}\n  read back as {again}");
    }
    let (_, items) = parse_line("x, y = 1, calc(a) as B, include (k), limit 1, hints t, {\"a\": 1}").unwrap();
    let kinds: Vec<String> = describe(&items).iter().map(|d| d[..12].trim().to_string()).collect();
    assert_eq!(kinds, ["projection", "filter", "calculation", "segment", "setting", "setting", "spec"]);
}
