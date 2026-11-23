/// What a statement returned. Every cell is text or null: a catalog is
/// strings and a sampled value is the string a consumer will match
/// against, so nothing here needs a type system yet.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Rows {
    /// The cell in `row` under the column of this name, if the statement
    /// returned one and it is not null.
    pub fn get<'a>(&'a self, row: &'a [Option<String>], column: &str) -> Option<&'a str> {
        let at = self.columns.iter().position(|c| c == column)?;
        row.get(at)?.as_deref()
    }
}
