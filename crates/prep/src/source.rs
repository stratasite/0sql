//! Where a dataset's rows come from. Each kind of source ends in a
//! snapshot of the same shape, so the recipe never knows which kind
//! fed it; this module is the CSV contract of an upload.
//!
//! A CSV is parsed here, in Rust, and never by the engine: the engine
//! has no access to files at all. Every choice the parse makes, said by
//! the request or detected, is kept in the snapshot's manifest, so the
//! same file parses the same way again and a reader can see why a
//! column came out as it did.

use crate::error::{Error, Result};
use crate::schema::{ColumnType, DECIMAL_PRECISION, DECIMAL_SCALE_MAX};
use serde::{Deserialize, Serialize};

/// The kinds of source.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A file the application uploaded.
    Upload,
    /// A spec run on a branch of the model, fresh, under the context the
    /// refresh was sent with.
    Semantic,
}

/// What a semantic source runs: a spec, or the shorthand for one, on a
/// branch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
}

/// What a semantic snapshot was made by: where and when the spec ran,
/// as what SQL, and for whom. The context itself is never kept, only a
/// fingerprint of it; nor is the run's result id, which expires with the
/// cache.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SemanticRead {
    pub branch: String,
    /// When the deployment that planned it was made.
    pub deployed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    pub sql_sha256: String,
    /// The hash of the security context it ran under.
    pub context: String,
    /// Each column's field in the model, by uid, where it is one.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub fields: std::collections::BTreeMap<String, String>,
}

/// What a request may say about how its CSV is laid out. Anything it
/// leaves out is detected.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CsvOptions {
    pub delimiter: Option<char>,
    pub quote: Option<char>,
    pub header: Option<bool>,
    /// How many rows may fail to parse before the import fails. By
    /// default one in a hundred.
    pub max_bad_rows: Option<usize>,
}

/// How a file was parsed, as kept in its snapshot's manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Parse {
    pub encoding: String,
    pub bom: bool,
    pub delimiter: char,
    pub delimiter_detected: bool,
    pub quote: char,
    pub header: bool,
    pub header_detected: bool,
    pub rows: usize,
    pub bad_rows: usize,
    /// The first few rows that did not parse, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bad_examples: Vec<BadRow>,
    /// Names that were made unique or given to an empty header.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renamed: Vec<Renamed>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BadRow {
    /// The line of the file it starts on, from 1.
    pub line: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Renamed {
    pub from: String,
    pub to: String,
}

/// A parsed file: each column's name, inferred type and values, by
/// column. An empty field is a null.
pub struct Parsed {
    pub columns: Vec<(String, ColumnType)>,
    pub values: Vec<Vec<Option<String>>>,
    pub parse: Parse,
}

/// How many bad rows are shown.
const BAD_EXAMPLES: usize = 5;

/// How many lines the delimiter is guessed from.
const SNIFF_LINES: usize = 50;

const DELIMITERS: [char; 4] = [',', ';', '\t', '|'];

/// Parses a CSV that may hold at most `max_rows` rows.
pub fn parse_csv(bytes: &[u8], options: &CsvOptions, max_rows: usize) -> Result<Parsed> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(Error::Unprocessable("this file is UTF-16; a CSV is read as UTF-8, so save it as UTF-8 and upload it again".into()));
    }
    let bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    let body = if bom { &bytes[3..] } else { bytes };
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(Error::Unprocessable("this file is empty".into()));
    }
    let quote = options.quote.unwrap_or('"');
    let delimiter = match options.delimiter {
        Some(d) => d,
        None => sniff_delimiter(body, quote),
    };
    for (what, c) in [("delimiter", delimiter), ("quote", quote)] {
        if !c.is_ascii() || c == '\n' || c == '\r' {
            return Err(Error::Invalid(format!("the {what} must be one ASCII character other than a line break; it is {c:?}")));
        }
    }
    if delimiter == quote {
        return Err(Error::Invalid(format!("the delimiter and the quote are both {delimiter:?}")));
    }

    let mut reader =
        csv::ReaderBuilder::new().has_headers(false).flexible(true).delimiter(delimiter as u8).quote(quote as u8).from_reader(body);

    // Every record, as text, with the line it started on; a record that
    // is not UTF-8 is a bad row.
    let mut records: Vec<(u64, Vec<String>)> = Vec::new();
    let mut bad: Vec<BadRow> = Vec::new();
    let mut bad_rows = 0usize;
    let mut note_bad = |line: u64, reason: String, bad: &mut Vec<BadRow>| {
        bad_rows += 1;
        if bad.len() < BAD_EXAMPLES {
            bad.push(BadRow { line, reason });
        }
    };
    let mut cells = 0usize;
    for record in reader.byte_records() {
        match record {
            Ok(r) => {
                let line = r.position().map(|p| p.line()).unwrap_or(0);
                if r.len() > COLUMN_LIMIT {
                    return Err(Error::Unprocessable(format!(
                        "line {line} has {} fields, more than the {COLUMN_LIMIT} columns a dataset has",
                        r.len()
                    )));
                }
                if let Some(long) = r.iter().find(|f| f.len() > FIELD_LIMIT) {
                    return Err(Error::Unprocessable(format!(
                        "line {line} has a field of {} bytes, more than the {FIELD_LIMIT} a value is",
                        long.len()
                    )));
                }
                cells += r.len();
                if cells > CELL_LIMIT {
                    return Err(Error::Unprocessable(format!("this file has more than {CELL_LIMIT} values, the most a dataset takes")));
                }
                // A blank line between rows is nothing, not a row of one
                // empty field.
                if r.len() == 1 && r[0].iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                let fields: std::result::Result<Vec<String>, _> = r.iter().map(|f| std::str::from_utf8(f).map(str::to_string)).collect();
                match fields {
                    Ok(f) => records.push((line, f)),
                    Err(_) => note_bad(line, "is not UTF-8".into(), &mut bad),
                }
                if records.len() > max_rows + 1 {
                    return Err(too_many(max_rows));
                }
            }
            Err(e) => {
                let line = e.position().map(|p| p.line()).unwrap_or(0);
                note_bad(line, e.to_string(), &mut bad);
            }
        }
    }
    if records.is_empty() {
        return Err(Error::Unprocessable("no row of this file could be read as UTF-8 text".into()));
    }

    let width = records[0].1.len();
    let mut rows: Vec<(u64, Vec<String>)> = Vec::with_capacity(records.len());
    for (i, (line, fields)) in records.into_iter().enumerate() {
        if i > 0 && fields.len() != width {
            note_bad(line, format!("has {} fields where the first row has {width}", fields.len()), &mut bad);
        } else {
            rows.push((line, fields));
        }
    }

    // The types of the columns as the rows below the first say, which is
    // what decides whether the first is a header.
    let types_below: Vec<ColumnType> = (0..width).map(|c| infer(rows.iter().skip(1).map(|(_, r)| r[c].as_str()))).collect();
    let (header, header_detected) = match options.header {
        Some(h) => (h, false),
        None => (looks_like_a_header(&rows[0].1, &types_below, rows.len()), true),
    };
    let names_row = if header { Some(rows.remove(0).1) } else { None };
    if rows.len() > max_rows {
        return Err(too_many(max_rows));
    }

    let total = rows.len() + bad_rows;
    let allowed = options.max_bad_rows.unwrap_or(total / 100);
    if bad_rows > allowed {
        let shown: Vec<String> = bad.iter().map(|b| format!("line {} {}", b.line, b.reason)).collect();
        return Err(Error::Unprocessable(format!(
            "{bad_rows} of {total} rows do not parse, more than the {allowed} allowed (max_bad_rows): {}",
            shown.join("; ")
        )));
    }

    let (names, renamed) = column_names(names_row.as_deref(), width);
    let mut values: Vec<Vec<Option<String>>> = vec![Vec::with_capacity(rows.len()); width];
    let row_count = rows.len();
    for (_, fields) in rows {
        for (c, field) in fields.into_iter().enumerate() {
            values[c].push(if field.trim().is_empty() { None } else { Some(field) });
        }
    }
    let columns: Vec<(String, ColumnType)> = names
        .into_iter()
        .enumerate()
        .map(|(c, name)| {
            let ty = if header { types_below[c] } else { infer(values[c].iter().flatten().map(String::as_str)) };
            (name, ty)
        })
        .collect();
    // A typed value is kept trimmed, so the engine's cast reads what the
    // inference read; text is kept as it came.
    for (c, (_, ty)) in columns.iter().enumerate() {
        if *ty != ColumnType::Text {
            for v in values[c].iter_mut().flatten() {
                *v = v.trim().to_string();
            }
        }
    }
    Ok(Parsed {
        columns,
        values,
        parse: Parse {
            encoding: "utf-8".into(),
            bom,
            delimiter,
            delimiter_detected: options.delimiter.is_none(),
            quote,
            header,
            header_detected,
            rows: row_count,
            bad_rows,
            bad_examples: bad,
            renamed,
        },
    })
}

/// The most columns a file may have.
pub const COLUMN_LIMIT: usize = 4096;

/// The longest field, in bytes.
pub const FIELD_LIMIT: usize = 1 << 20;

/// The most values a file may have, all its rows' fields together. Each
/// is held a few times over while it is read, so this, not the bytes,
/// is what bounds a file of empty fields.
pub const CELL_LIMIT: usize = 32 << 20;

fn too_many(max_rows: usize) -> Error {
    Error::Unprocessable(format!("this file has more than {max_rows} rows, the most a dataset holds"))
}

/// The delimiter that splits the first lines into the same number of
/// fields, more than one, the most often; a comma when none does.
fn sniff_delimiter(body: &[u8], quote: char) -> char {
    let mut best = (',', 0usize, 0usize);
    for d in DELIMITERS {
        let mut reader =
            csv::ReaderBuilder::new().has_headers(false).flexible(true).delimiter(d as u8).quote(quote as u8).from_reader(body);
        let widths: Vec<usize> = reader.byte_records().take(SNIFF_LINES).flatten().map(|r| r.len()).collect();
        let Some(&first) = widths.first() else { continue };
        if first < 2 {
            continue;
        }
        let agreeing = widths.iter().filter(|w| **w == first).count();
        // Agreement first, then the wider split.
        if (agreeing, first) > (best.1, best.2) {
            best = (d, agreeing, first);
        }
    }
    best.0
}

/// Whether a first row is names rather than data: some column below it
/// has a type other than text that its value in the first row is not.
/// When every column is text there is no telling, and a file nearly
/// always has a header, so it is one.
fn looks_like_a_header(first: &[String], types_below: &[ColumnType], rows: usize) -> bool {
    if rows < 2 || types_below.iter().all(|t| *t == ColumnType::Text) {
        return true;
    }
    first
        .iter()
        .zip(types_below)
        .any(|(v, t)| *t != ColumnType::Text && !v.trim().is_empty() && !infer(std::iter::once(v.as_str())).widens_to(*t))
}

/// The names of the columns: the header's, trimmed, with an empty one
/// given `column_<n>` and a repeat (case aside: the engine does not
/// tell `Region` from `region`) given a suffix.
fn column_names(header: Option<&[String]>, width: usize) -> (Vec<String>, Vec<Renamed>) {
    let mut names: Vec<String> = Vec::with_capacity(width);
    let mut renamed = Vec::new();
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    for c in 0..width {
        let given = header.map(|h| h[c].trim().to_string()).unwrap_or_default();
        let base = if given.is_empty() { format!("column_{}", c + 1) } else { given.clone() };
        let mut name = base.clone();
        let mut n = 2;
        while taken.contains(&name.to_lowercase()) {
            name = format!("{base}_{n}");
            n += 1;
        }
        taken.insert(name.to_lowercase());
        if header.is_some() && name != given {
            renamed.push(Renamed { from: given, to: name.clone() });
        }
        names.push(name);
    }
    (names, renamed)
}

/// What one value could be read as.
#[derive(Clone, Copy, Default)]
struct Could {
    boolean: bool,
    integer: bool,
    /// The scale, when it reads as a decimal of at most
    /// [`DECIMAL_SCALE_MAX`] places that fits the precision.
    decimal: Option<u8>,
    double: bool,
    date: bool,
    timestamp: bool,
    /// Digits with a leading zero: a code, not a number.
    identifier: bool,
}

/// The narrowest type every value fits, nulls aside. A column with a
/// value like `007` or `02134` is text, so codes and postcodes keep
/// their zeros.
pub fn infer<'a>(values: impl Iterator<Item = &'a str>) -> ColumnType {
    let mut all = Could { boolean: true, integer: true, decimal: Some(0), double: true, date: true, timestamp: true, identifier: false };
    let mut any = false;
    for v in values {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        any = true;
        let c = could(v);
        if c.identifier {
            return ColumnType::Text;
        }
        all.boolean &= c.boolean;
        all.integer &= c.integer;
        all.decimal = match (all.decimal, c.decimal) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        all.double &= c.double;
        all.date &= c.date;
        all.timestamp &= c.timestamp || c.date;
        if !(all.boolean || all.integer || all.decimal.is_some() || all.double || all.date || all.timestamp) {
            return ColumnType::Text;
        }
    }
    if !any {
        ColumnType::Text
    } else if all.boolean {
        ColumnType::Boolean
    } else if all.integer {
        ColumnType::Integer
    } else if let Some(s) = all.decimal {
        ColumnType::Decimal(s)
    } else if all.double {
        ColumnType::Double
    } else if all.date {
        ColumnType::Date
    } else if all.timestamp {
        ColumnType::Timestamp
    } else {
        ColumnType::Text
    }
}

fn could(v: &str) -> Could {
    let mut c = Could { boolean: v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false"), ..Could::default() };
    let unsigned = v.strip_prefix(['-', '+']).unwrap_or(v);
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (unsigned, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if digits(whole) && whole.len() > 1 && whole.starts_with('0') {
        c.identifier = true;
        return c;
    }
    match fraction {
        None if digits(whole) => {
            c.integer = v.parse::<i64>().is_ok();
            c.decimal = (whole.len() <= (DECIMAL_PRECISION - DECIMAL_SCALE_MAX) as usize).then_some(0);
            c.double = true;
        }
        Some(f) if digits(whole) && digits(f) => {
            let fits = f.len() <= DECIMAL_SCALE_MAX as usize && whole.len() <= (DECIMAL_PRECISION - DECIMAL_SCALE_MAX) as usize;
            c.decimal = fits.then_some(f.len() as u8);
            c.double = true;
        }
        _ => {
            // Scientific notation and the like; never `inf` or `nan`,
            // which are words.
            c.double = v.bytes().any(|b| b.is_ascii_digit())
                && v.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'-' | b'+'))
                && v.parse::<f64>().is_ok_and(f64::is_finite);
        }
    }
    c.date = is_date(v);
    c.timestamp = !c.date && is_timestamp(v);
    c
}

/// `YYYY-MM-DD`, a real day.
fn is_date(v: &str) -> bool {
    let b = v.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |s: &str| s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse::<u32>().ok()).flatten();
    let (Some(y), Some(m), Some(d)) = (num(&v[0..4]), num(&v[5..7]), num(&v[8..10])) else { return false };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    (1..=12).contains(&m) && d >= 1 && d <= days[(m - 1) as usize]
}

/// `YYYY-MM-DD HH:MM[:SS[.ffffff]]`, with a space or a `T`.
fn is_timestamp(v: &str) -> bool {
    if v.len() < 16 || !v.is_char_boundary(10) || !is_date(&v[..10]) || !matches!(v.as_bytes()[10], b' ' | b'T') {
        return false;
    }
    let time = &v[11..];
    let (hms, fraction) = match time.split_once('.') {
        Some((a, f)) => (a, Some(f)),
        None => (time, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let two = |s: &str, max: u32| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u32>().is_ok_and(|n| n <= max);
    let ok = match parts.as_slice() {
        [h, m] => two(h, 23) && two(m, 59) && fraction.is_none(),
        [h, m, s] => two(h, 23) && two(m, 59) && two(s, 59),
        _ => false,
    };
    ok && fraction.is_none_or(|f| (1..=6).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ColumnType::*;

    fn parse(text: &str) -> Parsed {
        parse_csv(text.as_bytes(), &CsvOptions::default(), 1000).unwrap()
    }

    #[test]
    fn types_are_inferred_narrowest_first() {
        assert_eq!(infer(["1", "2", "-3"].into_iter()), Integer);
        assert_eq!(infer(["1", "2.5", "3.25"].into_iter()), Decimal(2));
        assert_eq!(infer(["1e3", "2"].into_iter()), Double);
        assert_eq!(infer(["true", "FALSE"].into_iter()), Boolean);
        assert_eq!(infer(["2026-01-31", "2024-02-29"].into_iter()), Date);
        assert_eq!(infer(["2026-02-30"].into_iter()), Text, "not a day");
        assert_eq!(infer(["2026-01-31", "2026-01-31 10:00"].into_iter()), Timestamp);
        assert_eq!(infer(["2026-01-31T10:00:01.5"].into_iter()), Timestamp);
        assert_eq!(infer(["a", "1"].into_iter()), Text);
        assert_eq!(infer(["", " "].into_iter()), Text, "nothing to go on");
        assert_eq!(infer(["inf", "nan"].into_iter()), Text);
        assert_eq!(infer(["1,234"].into_iter()), Text);
    }

    #[test]
    fn identifiers_keep_their_zeros() {
        assert_eq!(infer(["02134", "10001"].into_iter()), Text);
        assert_eq!(infer(["007"].into_iter()), Text);
        assert_eq!(infer(["0", "0.5", "10"].into_iter()), Decimal(1), "a lone zero is a number");
    }

    #[test]
    fn a_plain_file_parses_with_its_header() {
        let p = parse("region,month,amount\nEMEA,2026-01-01,10.5\nAPAC,2026-02-01,7\n");
        assert_eq!(p.columns, vec![("region".to_string(), Text), ("month".to_string(), Date), ("amount".to_string(), Decimal(1))]);
        assert_eq!(p.values[2], vec![Some("10.5".to_string()), Some("7".to_string())]);
        assert_eq!(p.parse.rows, 2);
        assert!(p.parse.header && p.parse.header_detected && p.parse.delimiter_detected);
        assert_eq!(p.parse.delimiter, ',');
    }

    #[test]
    fn the_delimiter_and_the_bom_are_detected() {
        let p = parse_csv("\u{feff}a;b\n1;x\n2;y\n".as_bytes(), &CsvOptions::default(), 10).unwrap();
        assert!(p.parse.bom);
        assert_eq!(p.parse.delimiter, ';');
        assert_eq!(p.columns[0], ("a".to_string(), Integer));
        let p = parse("a\tb\n1\t2\n");
        assert_eq!(p.parse.delimiter, '\t');
    }

    #[test]
    fn a_file_with_no_header_is_told_by_its_types() {
        let p = parse("1,x\n2,y\n");
        assert!(!p.parse.header);
        assert_eq!(p.columns[0], ("column_1".to_string(), Integer));
        assert_eq!(p.parse.rows, 2);
    }

    #[test]
    fn names_are_made_unique_and_given_when_empty() {
        let p = parse("Region,region,,amount\nx,y,z,1\n");
        let names: Vec<&str> = p.columns.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Region", "region_2", "column_3", "amount"]);
        assert_eq!(p.parse.renamed.len(), 2);
    }

    #[test]
    fn bad_rows_are_counted_and_past_the_threshold_fail() {
        let text = "a,b\n1,2\n3\n4,5\n";
        let e = parse_csv(text.as_bytes(), &CsvOptions::default(), 10).err().unwrap();
        assert!(e.to_string().contains("1 of 3 rows do not parse") && e.to_string().contains("line 3"), "{e}");
        let p = parse_csv(text.as_bytes(), &CsvOptions { max_bad_rows: Some(1), ..CsvOptions::default() }, 10).unwrap();
        assert_eq!(p.parse.bad_rows, 1);
        assert_eq!(p.parse.bad_examples[0].line, 3);
        assert_eq!(p.parse.rows, 2);
    }

    #[test]
    fn quoted_fields_hold_delimiters_and_newlines() {
        let p = parse("name,note\n\"Smith, J\",\"two\nlines\"\n");
        assert_eq!(p.values[0][0].as_deref(), Some("Smith, J"));
        assert_eq!(p.values[1][0].as_deref(), Some("two\nlines"));
    }

    #[test]
    fn the_caps_and_the_encodings_are_refused_plainly() {
        assert!(parse_csv(b"a\n1\n2\n3\n", &CsvOptions::default(), 2).err().unwrap().to_string().contains("more than 2 rows"));
        assert!(parse_csv(&[0xFF, 0xFE, b'a'], &CsvOptions::default(), 2).err().unwrap().to_string().contains("UTF-16"));
        assert!(parse_csv(b"  \n", &CsvOptions::default(), 2).is_err());
        let o = CsvOptions { delimiter: Some('"'), ..CsvOptions::default() };
        assert!(matches!(parse_csv(b"a\n", &o, 2), Err(Error::Invalid(_))));
    }

    #[test]
    fn empty_fields_are_nulls() {
        let p = parse("a,b\n1,\n,x\n");
        assert_eq!(p.values[0], vec![Some("1".to_string()), None]);
        assert_eq!(p.values[1], vec![None, Some("x".to_string())]);
    }
}
