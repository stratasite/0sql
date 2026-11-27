//! Columns: their types, their roles, and how a type evolves when new
//! rows meet it.
//!
//! The types are few on purpose. A dataset is small and made by hand,
//! and each type here is one a user can name in the grid and one every
//! step knows how to treat: a truth value, a whole number, a decimal
//! with a scale, a float, a date, a timestamp, and text.

use crate::error::{Error, Result};
use arrow_schema::{DataType, TimeUnit};
use serde::{Deserialize, Serialize};
use std::fmt;

/// The precision every decimal has. One precision means two decimals
/// differ only in scale, so widening one to the other is a question of
/// scale alone.
pub const DECIMAL_PRECISION: u8 = 38;

/// The largest scale a decimal is given. A value with more digits after
/// the point is a float.
pub const DECIMAL_SCALE_MAX: u8 = 18;

/// A column's type.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum ColumnType {
    Boolean,
    /// A 64-bit integer.
    Integer,
    /// A decimal of [`DECIMAL_PRECISION`] digits with this scale.
    Decimal(u8),
    /// A 64-bit float.
    Double,
    Date,
    /// A timestamp with no time zone, to the microsecond.
    Timestamp,
    Text,
}

impl ColumnType {
    /// The type as DuckDB names it.
    pub fn sql(self) -> String {
        match self {
            ColumnType::Boolean => "BOOLEAN".into(),
            ColumnType::Integer => "BIGINT".into(),
            ColumnType::Decimal(s) => format!("DECIMAL({DECIMAL_PRECISION},{s})"),
            ColumnType::Double => "DOUBLE".into(),
            ColumnType::Date => "DATE".into(),
            ColumnType::Timestamp => "TIMESTAMP".into(),
            ColumnType::Text => "VARCHAR".into(),
        }
    }

    /// The Arrow type a column of this type is stored as.
    pub fn arrow(self) -> DataType {
        match self {
            ColumnType::Boolean => DataType::Boolean,
            ColumnType::Integer => DataType::Int64,
            ColumnType::Decimal(s) => DataType::Decimal128(DECIMAL_PRECISION, s as i8),
            ColumnType::Double => DataType::Float64,
            ColumnType::Date => DataType::Date32,
            ColumnType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            ColumnType::Text => DataType::Utf8,
        }
    }

    /// The type of a column the engine handed back. Only the types a
    /// step can make are known: anything else is a step that should
    /// have cast its result, and is an error on this side.
    pub fn from_arrow(t: &DataType) -> Result<ColumnType> {
        Ok(match t {
            DataType::Boolean => ColumnType::Boolean,
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => ColumnType::Integer,
            DataType::UInt8 | DataType::UInt16 | DataType::UInt32 => ColumnType::Integer,
            DataType::Float16 | DataType::Float32 | DataType::Float64 => ColumnType::Double,
            DataType::Decimal128(_, s) | DataType::Decimal256(_, s) if *s >= 0 && *s as u8 <= DECIMAL_SCALE_MAX => {
                ColumnType::Decimal(*s as u8)
            }
            // More places than a decimal is given: a float, as a file's
            // value with that many is.
            DataType::Decimal128(..) | DataType::Decimal256(..) => ColumnType::Double,
            DataType::Date32 | DataType::Date64 => ColumnType::Date,
            DataType::Timestamp(_, None) => ColumnType::Timestamp,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => ColumnType::Text,
            other => return Err(Error::Internal(format!("the engine made a column of type {other}, which a dataset has no name for"))),
        })
    }

    pub fn is_number(self) -> bool {
        matches!(self, ColumnType::Integer | ColumnType::Decimal(_) | ColumnType::Double)
    }

    /// Whether every value of `self` is a value of `to`, unchanged.
    pub fn widens_to(self, to: ColumnType) -> bool {
        use ColumnType::*;
        match (self, to) {
            (a, b) if a == b => true,
            // A BIGINT has 19 digits, which leave room for a scale of
            // 19 in 38; the scale is never more than 18.
            (Integer, Decimal(_) | Double) => true,
            (Decimal(a), Decimal(b)) => b >= a,
            (Decimal(_), Double) => true,
            (Date, Timestamp) => true,
            _ => false,
        }
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnType::Boolean => f.write_str("boolean"),
            ColumnType::Integer => f.write_str("integer"),
            ColumnType::Decimal(s) => write!(f, "decimal({s})"),
            ColumnType::Double => f.write_str("double"),
            ColumnType::Date => f.write_str("date"),
            ColumnType::Timestamp => f.write_str("timestamp"),
            ColumnType::Text => f.write_str("text"),
        }
    }
}

impl From<ColumnType> for String {
    fn from(t: ColumnType) -> String {
        t.to_string()
    }
}

impl TryFrom<String> for ColumnType {
    type Error = String;

    fn try_from(s: String) -> std::result::Result<ColumnType, String> {
        let t = s.trim().to_ascii_lowercase();
        Ok(match t.as_str() {
            "boolean" | "bool" => ColumnType::Boolean,
            "integer" | "int" | "bigint" => ColumnType::Integer,
            "double" | "float" => ColumnType::Double,
            "date" => ColumnType::Date,
            "timestamp" => ColumnType::Timestamp,
            "text" | "string" | "varchar" => ColumnType::Text,
            "decimal" => ColumnType::Decimal(2),
            _ => {
                let scale = t.strip_prefix("decimal(").and_then(|r| r.strip_suffix(')')).and_then(|n| n.trim().parse::<u8>().ok());
                match scale {
                    Some(s) if s <= DECIMAL_SCALE_MAX => ColumnType::Decimal(s),
                    _ => {
                        return Err(format!(
                            "'{s}' is not a type: boolean, integer, decimal(<scale up to {DECIMAL_SCALE_MAX}>), double, date, timestamp or text"
                        ))
                    }
                }
            }
        })
    }
}

/// What a column is for. A dimension is something rows are told apart
/// and grouped by; a measure is something added up. The role decides
/// what an edit to a cell means and how a dataset blends.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Dimension,
    Measure,
}

impl Role {
    /// The role a new column is given: a number is a measure, unless
    /// its name says it is an identifier or a part of a date; anything
    /// else is a dimension. The user changes it when this is wrong.
    pub fn inferred(name: &str, ty: ColumnType) -> Role {
        if !ty.is_number() || names_an_identifier(name) {
            Role::Dimension
        } else {
            Role::Measure
        }
    }
}

/// Whether a column's name says its numbers name things rather than
/// count them: `id`, `store_id`, `StoreId`, `zip code`, `year`.
fn names_an_identifier(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    const WHOLE: [&str; 9] = ["id", "key", "code", "year", "month", "quarter", "week", "day", "zip"];
    const ENDS: [&str; 6] = ["id", "key", "code", "no", "number", "zip"];
    if WHOLE.contains(&lower.as_str()) {
        return true;
    }
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if words.len() > 1 && words.last().is_some_and(|w| ENDS.contains(w)) {
        return true;
    }
    // camelCase: `storeId`, `StoreID`.
    let trimmed = name.trim();
    trimmed.len() > 2
        && (trimmed.ends_with("Id") || trimmed.ends_with("ID"))
        && trimmed[..trimmed.len() - 2].chars().any(|c| c.is_ascii_lowercase())
}

/// One column of a snapshot or a materialization.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: ColumnType,
    pub role: Role,
}

/// How a column's established type meets the type of the rows that
/// arrive for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Meet {
    /// The same type.
    Same,
    /// The new rows fit the established type as they are, and are cast
    /// to it.
    Fits,
    /// The established type widens to the new one, losing nothing.
    Widens(ColumnType),
    /// Neither widens to the other: the new rows are kept at the
    /// established type only if every one of their values converts.
    Converts,
}

/// The rule of "Types evolve, never truncate".
pub fn meet(established: ColumnType, incoming: ColumnType) -> Meet {
    if established == incoming {
        Meet::Same
    } else if incoming.widens_to(established) {
        Meet::Fits
    } else if established.widens_to(incoming) {
        Meet::Widens(incoming)
    } else {
        Meet::Converts
    }
}

/// The type two sides meet at, as a join key or an append: the wider
/// of the two, when one widens to the other. Two types that do not are
/// refused; a join never converts.
pub fn common(a: ColumnType, b: ColumnType) -> Option<ColumnType> {
    if a.widens_to(b) {
        Some(b)
    } else if b.widens_to(a) {
        Some(a)
    } else {
        None
    }
}

/// A name as the engine is given it: quoted, so any name a user can
/// type is one identifier, and none of them is a keyword.
pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Text as a SQL literal.
pub fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ColumnType::*;

    #[test]
    fn types_widen_only_where_nothing_is_lost() {
        assert!(Integer.widens_to(Decimal(2)));
        assert!(Integer.widens_to(Double));
        assert!(Decimal(2).widens_to(Decimal(4)));
        assert!(!Decimal(4).widens_to(Decimal(2)));
        assert!(Decimal(2).widens_to(Double));
        assert!(Date.widens_to(Timestamp));
        assert!(!Timestamp.widens_to(Date));
        assert!(!Double.widens_to(Integer));
        assert!(!Integer.widens_to(Text), "a number never quietly becomes text");
        assert!(!Text.widens_to(Date));
    }

    #[test]
    fn meeting_a_new_type_widens_fits_or_must_convert() {
        assert_eq!(meet(Integer, Integer), Meet::Same);
        assert_eq!(meet(Double, Integer), Meet::Fits);
        assert_eq!(meet(Integer, Decimal(2)), Meet::Widens(Decimal(2)));
        assert_eq!(meet(Date, Timestamp), Meet::Widens(Timestamp));
        assert_eq!(meet(Date, Text), Meet::Converts);
        assert_eq!(meet(Integer, Text), Meet::Converts);
        assert_eq!(common(Integer, Decimal(2)), Some(Decimal(2)));
        assert_eq!(common(Text, Integer), None);
    }

    #[test]
    fn types_read_and_print_as_a_user_writes_them() {
        for t in [Boolean, Integer, Decimal(3), Double, Date, Timestamp, Text] {
            assert_eq!(ColumnType::try_from(t.to_string()).unwrap(), t);
            assert_eq!(ColumnType::from_arrow(&t.arrow()).unwrap(), t);
        }
        assert_eq!(ColumnType::try_from("DECIMAL(4)".to_string()).unwrap(), Decimal(4));
        assert!(ColumnType::try_from("decimal(40)".to_string()).is_err());
        assert!(ColumnType::try_from("blob".to_string()).is_err());
    }

    #[test]
    fn numbers_are_measures_unless_named_as_identifiers() {
        assert_eq!(Role::inferred("amount", Decimal(2)), Role::Measure);
        assert_eq!(Role::inferred("region", Text), Role::Dimension);
        assert_eq!(Role::inferred("order date", Date), Role::Dimension);
        for name in ["id", "store_id", "StoreId", "storeID", "zip code", "Year", "order no", "account-number"] {
            assert_eq!(Role::inferred(name, Integer), Role::Dimension, "{name}");
        }
        for name in ["paid", "units", "valid"] {
            assert_eq!(Role::inferred(name, Integer), Role::Measure, "{name}");
        }
    }

    #[test]
    fn names_are_quoted_whole() {
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(literal("it's"), "'it''s'");
    }
}
