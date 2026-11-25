//! The credential a datasource is deployed with, and what a later
//! deploy does to it.
//!
//! A deploy carries its secrets in the datasource's entry, and they
//! become the credential of that datasource on that branch of that
//! project in that org: what a run connects with when its request sends
//! none. They are kept apart from the compiled model, in a file of
//! their own that only the server's user reads, and are never served.
//!
//! A deploy that carries no secret for a datasource keeps the one the
//! branch had, so a pipeline can deploy a model without holding the
//! secret; but only while the datasource still points at the same
//! place, so a credential never follows a datasource to another host.
//! A branch deployed for the first time has nothing to keep and
//! borrows from no other branch. A deploy asked to clear is the whole
//! truth: what it does not carry is not kept.

use serde_json::{json, Map, Value};
use zsql_core::Model;
use zsql_semantic::Datasource;

/// The settings that say how a datasource is used and not where it is
/// or who is asking: changing one keeps the credential. Every other
/// setting, known or not, is part of the place.
const NOT_THE_PLACE: [&str; 14] = [
    "schema",
    "dataset",
    "warehouse",
    "role",
    "workgroup",
    "output_location",
    "s3_output_location",
    "cache_ttl",
    "query_wait_time",
    // How the connection is secured, not where it goes.
    "sslmode",
    "ssl_mode",
    "ca_file",
    "sslrootcert",
    "ssl_ca",
];

/// Whether two deploys of a datasource point at the same place as the
/// same user: the adapter and every setting but the ones above.
fn same_place(old: &Datasource, new: &Datasource) -> bool {
    let place = |d: &Datasource| -> Map<String, Value> {
        d.connection.iter().filter(|(key, _)| !NOT_THE_PLACE.contains(&key.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    old.adapter.eq_ignore_ascii_case(&new.adapter) && place(old) == place(new)
}

/// Settles the credential of each datasource of a model about to be
/// deployed over `previous`, the model the branch has now: kept where
/// the deploy carried none and the rules above allow. What became of
/// each, by datasource: `set`, `kept`, `dropped` or `none`.
pub(crate) fn settle(deployed: &mut Model, previous: Option<&Model>, clear: bool) -> Value {
    let mut out = Map::new();
    let datasources: Vec<Datasource> = deployed.semantic().snapshot().datasources.clone();
    for new in &datasources {
        let uid = new.uid.to_lowercase();
        let had = previous.and_then(|p| Some((p.datasource(&uid)?, p.secrets_of(&uid)?)));
        let became = if deployed.secrets.contains_key(&uid) {
            "set"
        } else {
            match had {
                Some((old, held)) if !clear && same_place(old, new) => {
                    deployed.secrets.insert(uid.clone(), held.clone());
                    "kept"
                }
                Some(_) => "dropped",
                None => "none",
            }
        };
        out.insert(new.uid.clone(), json!(became));
    }
    Value::Object(out)
}
