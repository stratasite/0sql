//! Near matches for a field reference that resolves to nothing: character
//! trigram similarity between the reference and every field's name, uid
//! and synonyms. Typos, plurals, partial names and swapped words all keep
//! most of their trigrams; abbreviations do not, and belong in synonyms.

use std::collections::HashSet;
use zsql_semantic::{Field, FieldKind, Universe};

/// A reference shorter than this only resolves exactly; trigram overlap
/// on three letters is noise.
pub const MIN_LEN: usize = 4;
/// The best match must score at least this...
pub const FLOOR: f64 = 0.5;
/// ...and lead the runner-up (on another field) by at least this to be taken.
pub const MARGIN: f64 = 0.2;
/// How many near misses an error or a listing names.
pub const SUGGESTIONS: usize = 3;

#[derive(Debug, Clone)]
pub struct Suggestion<'u> {
    pub field: &'u Field,
    /// The name, uid or synonym that matched best.
    pub form: String,
    pub score: f64,
}

/// A reference the resolver corrected, so callers can echo it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Correction {
    pub term: String,
    pub field_uid: String,
    pub field_name: String,
    pub score: f64,
}

pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim_end().to_string()
}

fn trigrams(s: &str) -> HashSet<[char; 3]> {
    let chars: Vec<char> = format!("  {s} ").chars().collect();
    chars.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
}

/// The overlap coefficient of the two strings' trigrams, 0 to 1: shared
/// trigrams over the smaller set, so a partial name scores by how much of
/// it is found rather than by how much of the field it leaves out.
pub fn similarity(a: &str, b: &str) -> f64 {
    overlap(&trigrams(a), &trigrams(b))
}

fn overlap(ta: &HashSet<[char; 3]>, tb: &HashSet<[char; 3]>) -> f64 {
    let smaller = ta.len().min(tb.len());
    if smaller == 0 {
        return 0.0;
    }
    ta.intersection(tb).count() as f64 / smaller as f64
}

/// Every field of the wanted kinds ranked by its best form's similarity to
/// `term`, best first, those scoring nothing left out.
pub fn suggest<'u>(universe: &'u Universe, term: &str, kinds: &[FieldKind]) -> Vec<Suggestion<'u>> {
    let wanted = normalize(term);
    if wanted.chars().count() < MIN_LEN {
        return vec![];
    }
    let tw = trigrams(&wanted);
    let mut out: Vec<Suggestion> = universe
        .fields()
        .iter()
        .filter(|f| kinds.contains(&f.kind))
        .filter_map(|f| {
            let forms = std::iter::once(&f.name).chain(std::iter::once(&f.uid)).chain(f.synonyms.iter());
            forms
                .map(|form| (form, overlap(&tw, &trigrams(&normalize(form)))))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .filter(|(_, score)| *score > 0.0)
                .map(|(form, score)| Suggestion { field: f, form: form.clone(), score })
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.field.name.cmp(&b.field.name)));
    out
}

/// The suggestion to take without asking: clears the floor and leads the
/// next field by the margin.
pub fn pick<'a, 'u>(ranked: &'a [Suggestion<'u>]) -> Option<&'a Suggestion<'u>> {
    let best = ranked.first()?;
    if best.score < FLOOR {
        return None;
    }
    let next = ranked.get(1).map(|s| s.score).unwrap_or(0.0);
    (best.score - next >= MARGIN).then_some(best)
}

/// "Did you mean" text for an error, or empty.
pub fn did_you_mean(ranked: &[Suggestion]) -> String {
    let names: Vec<String> = ranked.iter().take(SUGGESTIONS).filter(|s| s.score >= FLOOR / 2.0).map(|s| s.field.name.clone()).collect();
    if names.is_empty() {
        String::new()
    } else {
        format!(" Did you mean: {}?", names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typos_plurals_and_partials_stay_close() {
        assert!(similarity(&normalize("departmnt"), &normalize("Department")) > 0.7);
        assert!(similarity(&normalize("sale"), &normalize("Sales")) > 0.7);
        assert!(similarity(&normalize("depart"), &normalize("Department")) > 0.8);
        assert!(similarity(&normalize("net paid store"), &normalize("Store Net Paid")) > 0.5);
        assert!(similarity(&normalize("revenue"), &normalize("Sales Value")) < 0.2);
    }

    #[test]
    fn normalize_folds_case_and_punctuation() {
        assert_eq!(normalize("  Store_Net-Paid! "), "store net paid");
    }
}
