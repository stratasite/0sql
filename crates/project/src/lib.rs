//! A project directory, loaded into a [`Snapshot`](zsql_semantic::Snapshot) the planner can use,
//! without a server. The layout is what `zsql init` produces and `zsql
//! deploy` uploads:
//!
//! ```text
//! project.yml           name, uid, production_branch
//! datasources.yml       one warehouse per key: name, adapter, tier, ...
//! models/**/tbl.*.yml   a table with its dimensions and measures
//! models/**/rel.*.yml   joins between a datasource's tables
//! security.yml          row-level security policies
//! tests/*.yml           planner assertions (see [`ProjectTest`])
//! ```
//!
//! Loading follows the deployer's order, which is also the formation
//! cycle: tables and fields, then joins, then paths, then what needs
//! paths (snapshot dates, partitions, exclusion and inclusion rules,
//! expression integrity), then policies. Ids are sequential; uids are
//! slugs of names, made unique the way the server makes them, so a
//! project deployed to a server and loaded here name the same things.

mod builder;
mod error;
mod load;
mod uid;
pub mod values;
mod yaml;

#[cfg(test)]
mod tests;

pub use builder::parse_format_syntax;
pub use error::Error;
pub use load::{load, load_confined, Loaded, ProjectTest};
pub use uid::parameterize;
