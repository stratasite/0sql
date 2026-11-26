//! A recipe, as the one statement the engine runs to make a
//! materialization. Each source is a table named by its id.

use crate::error::{Error, Result};
use crate::recipe::Recipe;
use crate::schema::ident;
use crate::store::SourceState;

pub fn recipe_sql(recipe: &Recipe, sources: &[SourceState]) -> Result<String> {
    let [source] = sources else {
        return Err(Error::Internal(format!("a dataset has {} sources, and reading more than one comes with combining", sources.len())));
    };
    if !recipe.steps.is_empty() {
        return Err(Error::Internal("steps are not compiled yet".into()));
    }
    Ok(format!("SELECT * FROM {}", ident(&source.id)))
}
