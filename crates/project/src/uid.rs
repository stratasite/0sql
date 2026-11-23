use std::collections::HashSet;

/// ActiveSupport `parameterize`: ASCII letters, digits, `_` and `-`
/// survive, everything else becomes one `-`, and the ends are trimmed.
pub fn parameterize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
            dash = ch == '-';
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// FriendlyId with candidates: the first free slug wins; when every
/// candidate is taken a counter is appended (Rails appends a UUID).
pub(crate) struct Slugs {
    used: HashSet<String>,
}

impl Slugs {
    pub(crate) fn new() -> Self {
        Slugs { used: HashSet::new() }
    }

    pub(crate) fn take(&mut self, candidates: &[String]) -> String {
        for c in candidates {
            let slug = parameterize(c);
            if !slug.is_empty() && self.used.insert(slug.clone()) {
                return slug;
            }
        }
        let base = candidates.first().map(|c| parameterize(c)).filter(|s| !s.is_empty()).unwrap_or_else(|| "item".into());
        let mut n = 2;
        loop {
            let slug = format!("{base}-{n}");
            if self.used.insert(slug.clone()) {
                return slug;
            }
            n += 1;
        }
    }
}
