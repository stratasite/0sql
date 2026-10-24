//! Rails `Planner::Sql::Builder`: collects clauses and prints them in the
//! exact layout the goldens expect (tabs and newlines included).

#[derive(Debug, Default, Clone)]
pub struct Builder {
    pub selections: Vec<String>,
    pub groupings: Vec<String>,
    pub where_clauses: Vec<String>,
    pub having_clauses: Vec<String>,
    pub order_by: Vec<(String, String)>,
    pub from: Vec<String>,
}

impl Builder {
    pub fn new() -> Self {
        Builder::default()
    }

    pub fn from(&mut self, clause: String) -> &mut Self {
        self.from.push(clause);
        self
    }

    pub fn select(&mut self, column: String) -> &mut Self {
        self.selections.push(column);
        self
    }

    pub fn group_by(&mut self, column: String) -> &mut Self {
        self.groupings.push(column);
        self
    }

    pub fn r#where(&mut self, condition: String) -> &mut Self {
        self.where_clauses.push(condition);
        self
    }

    pub fn having(&mut self, condition: String) -> &mut Self {
        self.having_clauses.push(condition);
        self
    }

    pub fn order(&mut self, column: String, direction: &str) -> &mut Self {
        self.order_by.push((column, direction.to_string()));
        self
    }

    pub fn to_sql(&self) -> String {
        let selections = if self.selections.is_empty() { vec!["*".to_string()] } else { self.selections.clone() };
        let mut q = format!("SELECT\n\t{}", selections.join(",\n\t"));
        q.push_str(&format!("\nFROM{}", self.from.concat()));
        if !self.where_clauses.is_empty() {
            q.push_str(&format!("\nWHERE\n\t{}", self.where_clauses.join("\n\tAND ")));
        }
        if !self.groupings.is_empty() {
            q.push_str(&format!("\nGROUP BY\n\t{}", self.groupings.join(",\n\t")));
        }
        if !self.having_clauses.is_empty() {
            q.push_str(&format!("\nHAVING\n\t{}", self.having_clauses.join("\n\tAND ")));
        }
        if !self.order_by.is_empty() {
            q.push_str("\nORDER BY");
            let parts: Vec<String> = self.order_by.iter().map(|(c, d)| format!("\n\t{c} {d}")).collect();
            q.push_str(&parts.join(", "));
        }
        q
    }
}
