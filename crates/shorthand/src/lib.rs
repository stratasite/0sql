//! The expression language: one line (or a few) of comma-separated items,
//! each read by its leading structure, turned into the JSON spec the
//! server plans. The grammar is `docs/expression-language.md`; `parse`
//! is that PEG read top down, `lower` is the AST to the spec.
//!
//! ```text
//! item category, month(order date), store net paid, yoy_pct(store net paid) as Growth desc,
//! order date between 2024-01-01 and 2024-12-31, item category top 5 by store net paid,
//! include (customer id, catalog net paid > 0), limit 100
//! ```
//!
//! A field is bare words, `[bracketed]` or `"quoted"`, with `@d` or `@m`
//! when a name is both. A value is bare text or `'single quoted'`. A
//! projection is a field inside decorator functions, with `as Alias`
//! and `asc`/`desc`. A filter is a field, a predicate and a value, joined
//! into trees with `and`, `or` and parentheses. A calculation is
//! `calc(sql) as Alias`. A segment is `include (...)`, `exclude (...)` or
//! `expand (...) with (...)`. `limit N` and `via table` are the settings,
//! and `{...}` merges spec JSON into the query or the item before it.

mod lower;
mod parse;

pub use parse::{
    Expr, Field, FilterNode, Item, ItemKind, Kind, Leaf, ParseError, Pred, Projection, Segment, SegmentItem, SegmentKind, Val,
};
use serde_json::Value;

/// The line as a spec, with what each item was read as. An error names
/// the mistake and where it is.
pub fn parse_line(line: &str) -> Result<(Value, Vec<Item>), String> {
    let items = parse::parse(line).map_err(|e| located(line, e))?;
    let spec = lower::to_spec(&items)?;
    Ok((spec, items))
}

/// One line per item, for echoing what was understood.
pub fn describe(items: &[Item]) -> Vec<String> {
    lower::describe(items)
}

/// The message with its place: a column on a one-line input, a line and
/// a column otherwise. Columns count characters, not bytes.
fn located(line: &str, e: ParseError) -> String {
    let at = e.at.min(line.len());
    let before = &line[..at];
    let row = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    if line.contains('\n') {
        format!("{} (line {row}, column {col})", e.msg)
    } else {
        format!("{} (column {col})", e.msg)
    }
}

pub const HELP: &str = "\
Type comma-separated items; a newline separates items too. Each item is read from its shape:
  projection   item category | month(order date) | yoy(store net paid) | store net paid as Paid desc
               truncate: hour day week month quarter year (also raw minute second millisecond)
               extract(part, x): minute hour day_of_month day_of_week day_of_year week_of_year month quarter year
                                 day_name month_name year_month
               yoy qoq mom wow dod (prior period), yoy_pct ... (relative change)
               share(x), share(x by region), share_filtered(x by region) (keeps the query's filters in the total)
               running_sum/avg/min/max/count(x by a order by b), moving_avg(x, 7 by a) (7 rows in the frame)
               rank dense_rank row_number percent_rank cume_dist first_value last_value, ntile(x, 4), lag(x, 1), lead(x, 1)
               custom('upper(@expression)', x)
  calculation  calc(sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m)) as Share | calc(count(*), integer) as Rows
               SQL over [Name]@m, [Name]@d and [Alias]; a decorator wraps it like a field: yoy_pct(calc(...)) as Growth
  filter       country = Canada | store net paid > 100 | order date between 2024-01-01 and 2024-12-31 | order date >= 28d
               category in (Books, Music) | category not in (Toys) | name contains foo | name not starts with x
               date is null | category top 10 by store net paid | description keyword (sale, clearance)
               (country = Canada or region = West) and store net paid > 10
  segment      include (customer id, catalog net paid > 0) as Buyers | exclude (customer id, net paid > 1000) for net paid
               expand (customer id, item = Books) with (item as Also bought) left dedupe none
  settings     limit 100 | via store-sales | {\"name\": \"...\"} merges spec JSON; after an item it merges into that item
Fields are names or uids, matched without regard to case; add @d or @m when a name is both.
[Name] or \"Name\" is always a field, 'text' is always a value. Reserved words, which end a bare name:
  and or not in is between top contains starts ends keyword as asc desc
Commands start with a dot, as in sqlite: .explain <line or {...spec...}>, .explore <line> [? term] (what the query can add),
.fields [text], .tables, .values <dim>,
.json {...spec...}, .spec <line> (the spec a line makes), .branch [name], .help, .quit";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(line: &str) -> Value {
        parse_line(line).unwrap_or_else(|e| panic!("{line}: {e}")).0
    }
    fn err(line: &str) -> String {
        match parse_line(line) {
            Ok((spec, _)) => panic!("{line}: parsed to {spec}"),
            Err(e) => e,
        }
    }

    /// The examples of docs/expression-language.md, with the specs it gives.
    #[test]
    fn documented_examples_lower_to_their_specs() {
        let cases = [
            (
                "item category, store net paid desc",
                json!({"projections": [{"field": "item category"}, {"field": "store net paid", "order_by": "desc"}]}),
            ),
            (
                "month(order date), store net paid, yoy_pct(store net paid) as Growth",
                json!({"projections": [
                    {"field": "order date", "decorators": [{"type": "truncate", "grain": "month"}]},
                    {"field": "store net paid"},
                    {"field": "store net paid", "alias": "Growth",
                     "decorators": [{"type": "temporalize", "transform": "year_over_year", "percent_change": true}]}]}),
            ),
            (
                "region, store net paid, order date between 2024-01-01 and 2024-12-31, item category in (Books, Music)",
                json!({"projections": [{"field": "region"}, {"field": "store net paid"}],
                       "filters": [
                        {"field": "order date", "predicate": "between", "value": "2024-01-01", "value_end": "2024-12-31"},
                        {"field": "item category", "predicate": "in_list", "value": "Books, Music"}]}),
            ),
            (
                "item category, orders, item category top 5 by store net paid",
                json!({"projections": [{"field": "item category"}, {"field": "orders"}],
                       "filters": [{"field": "item category", "predicate": "top_n", "value": 5, "top_n_measure": "store net paid"}]}),
            ),
            (
                "region, item category, store net paid, share(store net paid by region) as Share of region",
                json!({"projections": [
                    {"field": "region"}, {"field": "item category"}, {"field": "store net paid"},
                    {"field": "store net paid", "alias": "Share of region",
                     "decorators": [{"type": "contribute", "partition_refs": ["region"]}]}]}),
            ),
            (
                "day(order date), region, store net paid, moving_avg(store net paid, 7 by region order by order date) as 7 day avg",
                json!({"projections": [
                    {"field": "order date", "decorators": [{"type": "truncate", "grain": "day"}]},
                    {"field": "region"}, {"field": "store net paid"},
                    {"field": "store net paid", "alias": "7 day avg",
                     "decorators": [{"type": "window", "mode": "moving", "function": "avg", "size": 6,
                                     "partition_refs": ["region"], "order_by": {"order date": "asc"}}]}]}),
            ),
            (
                "country, store net paid, (country = Canada or region = West) and store net paid > 1000",
                json!({"projections": [{"field": "country"}, {"field": "store net paid"}],
                       "filters": {"and": [
                        {"or": [{"field": "country", "predicate": "equals", "value": "Canada"},
                                {"field": "region", "predicate": "equals", "value": "West"}]},
                        {"field": "store net paid", "predicate": "greater_than", "value": 1000}]}}),
            ),
            (
                "country, calc(sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m)) as Store to catalog, calc(count(*), integer) as Rows",
                json!({"projections": [{"field": "country"}],
                       "calculations": [
                        {"calculation": true, "alias": "Store to catalog", "sql": "sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m)"},
                        {"calculation": true, "alias": "Rows", "sql": "count(*)", "data_type": "integer"}]}),
            ),
            (
                "customer id, store net paid, include (customer id, catalog net paid > 0) as Catalog buyers",
                json!({"projections": [{"field": "customer id"}, {"field": "store net paid"}],
                       "segments": [{"name": "Catalog buyers", "mode": "include", "keys": ["customer id"],
                                     "filters": [{"field": "catalog net paid", "predicate": "greater_than", "value": 0}]}]}),
            ),
            (
                "item category, orders, expand (customer id, item category = Books) with (item category as Also bought) left dedupe none as Book pairs",
                json!({"projections": [{"field": "item category"}, {"field": "orders"}],
                       "segments": [{"name": "Book pairs", "keys": ["customer id"],
                                     "filters": [{"field": "item category", "predicate": "equals", "value": "Books"}],
                                     "expanding": [{"field": "item category", "alias": "Also bought"}],
                                     "join": "left", "pair_dedupe": "none"}]}),
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(spec(line), expected, "{line}");
        }
    }

    /// The remaining documented lines parse, and read back as themselves.
    #[test]
    fn remaining_examples_parse() {
        let lines = [
            "region, store net paid, catalog net paid, exclude (customer id, store net paid > 1000) for store net paid",
            "include (customer id, store net paid@m), customer id, catalog net paid",
            "month(order date), running_sum(orders) as Orders to date",
            "region, item category, store net paid, row_number(store net paid by region) as \"Row in region\"",
            "extract(day_of_week, order date) as Weekday, orders",
            "custom('upper(@expression)', country) as Country, orders",
            "item category, orders, item category keyword (book, music)",
            "month(order date), yoy_pct(calc(sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m))) as Mix growth desc",
            "item category, store net paid, item category not in (Toys, Games), item category not starts with X",
            "\"Is Active\" = true, customer id, store net paid@m > 100",
            "store net paid, order date >= 28d, limit 10, via store-sales",
            "country, store net paid {\"format\": {\"type\": \"currency\"}}, {\"name\": \"Paid by country\"}",
        ];
        for line in lines {
            let (first, items) = parse_line(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            let again = describe(&items).iter().map(|d| d[13..].to_string()).collect::<Vec<_>>().join(", ");
            let (second, _) = parse_line(&again).unwrap_or_else(|e| panic!("{again}: {e}"));
            assert_eq!(first, second, "{line} read back as {again}");
        }
        let s = spec("month(order date), yoy_pct(calc(sum([A]@m) / sum([B]@m))) as Mix growth desc");
        assert_eq!(s["calculations"][0]["decorators"][0]["type"], "temporalize");
        assert_eq!(s["calculations"][0]["order_by"], "desc");
        let s = spec("store net paid, order date >= 28d, limit 10, via store-sales");
        assert_eq!(s["limit"], 10);
        assert_eq!(s["hints"], json!(["store-sales"]));
        assert_eq!(s["filters"][0]["value"], "28d");
        let s = spec("country, store net paid {\"format\": {\"type\": \"currency\"}}, {\"name\": \"Paid by country\"}");
        assert_eq!(s["projections"][1]["format"]["type"], "currency");
        assert_eq!(s["name"], "Paid by country");
        let s = spec("\"Is Active\" = true, customer id, store net paid@m > 100");
        assert_eq!(s["filters"][0], json!({"field": "Is Active", "predicate": "equals", "value": true}));
        assert_eq!(s["filters"][1]["field_type"], "measure");
        let s = spec("include (customer id, store net paid@m), customer id");
        assert_eq!(s["segments"][0]["measures"], json!(["store net paid"]));
        let s = spec("custom('upper(@expression)', country) as Country");
        assert_eq!(s["projections"][0]["decorators"][0], json!({"type": "customize", "custom_sql": "upper(@expression)"}));
    }

    #[test]
    fn the_multi_line_form() {
        let line = "month(order date), region\nstore net paid, share_filtered(store net paid by region) as Share\norder date >= 1y, region in (West, East)\ninclude (customer id, catalog net paid > 0) as Catalog buyers\nlimit 500";
        let s = spec(line);
        assert_eq!(s["projections"].as_array().unwrap().len(), 4);
        assert_eq!(s["projections"][3]["decorators"][0]["ignore_partition_filters"], false);
        assert_eq!(s["filters"].as_array().unwrap().len(), 2);
        assert_eq!(s["segments"][0]["name"], "Catalog buyers");
        assert_eq!(s["limit"], 500);
        // A continuation line may begin with and/or, and a list may wrap.
        let s = spec("country\ncountry = Canada\nor region = West\nregion in (\n  West,\n  East\n)");
        assert_eq!(s["filters"]["and"][0]["or"].as_array().unwrap().len(), 2);
        assert_eq!(s["filters"]["and"][1]["value"], "West, East");
    }

    /// The hazards section: each tricky input reads the way the document says.
    #[test]
    fn hazards_read_as_documented() {
        let s = spec("running_sum(x by order date), rank(x by region order by order date desc)");
        assert_eq!(s["projections"][0]["decorators"][0]["partition_refs"], json!(["order date"]));
        assert_eq!(s["projections"][1]["decorators"][0]["order_by"], json!({"order date": "desc"}));
        let s = spec("date between 1y and 0d and country = Canada");
        assert_eq!(s["filters"].as_array().unwrap().len(), 2);
        let s = spec("name contains made in china, description keyword (a, b)");
        assert_eq!(s["filters"][0]["value"], "made in china");
        let s = spec("created by, order date, null count, x in (Rock and Roll, Jazz)");
        assert_eq!(s["projections"].as_array().unwrap().len(), 3);
        assert_eq!(s["filters"][0]["value"], "Rock and Roll, Jazz");
        let s = spec("name = 'Rock and Roll', title = 'Toys, games (new)', who = O'Brien, mail = a@b.c");
        assert_eq!(s["filters"][0]["value"], "Rock and Roll");
        assert_eq!(s["filters"][1]["value"], "Toys, games (new)");
        assert_eq!(s["filters"][2]["value"], "O'Brien");
        assert_eq!(s["filters"][3]["value"], "a@b.c");
        let s = spec("calc(coalesce([A]@m, 0) / 'a, b') as X");
        assert_eq!(s["calculations"][0]["sql"], "coalesce([A]@m, 0) / 'a, b'");
        let s = spec("\"Net Paid\"@m, [Net Paid]@d, revenue > -5, limit, include tax, month, P&L, Avg. Price, Größe as Size");
        assert_eq!(s["projections"][0], json!({"field": "Net Paid", "field_type": "measure"}));
        assert_eq!(s["projections"][1]["field_type"], "dimension");
        assert_eq!(s["filters"][0]["value"], -5);
        let names: Vec<&str> = s["projections"].as_array().unwrap()[2..].iter().map(|p| p["field"].as_str().unwrap()).collect();
        assert_eq!(names, ["limit", "include tax", "month", "P&L", "Avg. Price", "Größe"]);
        assert_eq!(s["projections"][7]["alias"], "Size");
        let s = spec("x = 02134, y = 1.10, z = '2024', w = 1.5, v = TRUE");
        assert_eq!(s["filters"][0]["value"], "02134");
        assert_eq!(s["filters"][1]["value"], 1.1);
        assert_eq!(s["filters"][2]["value"], "2024");
        assert_eq!(s["filters"][3]["value"], 1.5);
        assert_eq!(s["filters"][4]["value"], true);
        let s = spec("a = 1 and b = 2, c = 3");
        assert_eq!(s["filters"].as_array().unwrap().len(), 3);
        let s = spec("\"Say \"\"hi\"\"\", country=Canada, x>=3, x<>4, x in(a,b)");
        assert_eq!(s["projections"][0]["field"], "Say \"hi\"");
        assert_eq!(s["filters"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn errors_say_what_is_wrong() {
        let cases = [
            ("Is Active = true", "reserved word"),
            ("x as Row in region", "reserved word and ends the name"),
            ("monthly(order date)", "unknown function `monthly`"),
            ("extract(dow, order date)", "extract needs a part"),
            ("country not = Canada", "write != instead of not ="),
            ("name = Rock and Roll", "after `and`"),
            ("share(x by created by)", "holding `by` is bracketed"),
            ("calc(a, b) as X", "expected a result type"),
            ("calc(a as X", "never closed"),
            ("[Sign In Date >= 7d", "never closed"),
            ("store net paid@x", "expected @d or @m"),
            ("store net paid@month", "expected @d or @m"),
            ("country = 'Canada", "never closed"),
            ("country = \"Canada\"", "single-quoted"),
            ("x in ('Toys, games')", "cannot hold a comma"),
            ("moving_avg(x, 1)", "at least two rows"),
            ("lag(x)", "needs the number of rows"),
            ("yoy(x by region)", "takes no `by`"),
            ("limit 10x", "whole number"),
            ("limit 10 {\"a\": 1}", "cannot follow a setting"),
            ("x == 5", "not an operator"),
            ("month(order date) = 5", "applies to a field, not to a function"),
            ("x asc desc", "direction is given twice"),
            ("x not between 1 and 2", "no `not between`"),
            ("expand (customer id) left", "followed by with"),
            ("include (customer id) left", "apply to expand"),
            ("x = 1 or", "after `or`"),
            ("{\"a\": }", "does not parse"),
            ("", "nothing to plan"),
            ("category top five", "top needs a count"),
        ];
        for (line, expected) in cases {
            let e = err(line);
            assert!(e.contains(expected), "{line}: {e}");
        }
        assert!(err("x\ny = ").contains("line 2"));
    }
}
