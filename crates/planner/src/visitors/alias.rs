//! Rails `ProjectionAliasVisitor`: root projections keep their names, made
//! unique by suffix; intermediate ones get `dim`/`msr` plus a hash so parent
//! nodes can reference them safely.

use crate::dag::Graph;
use crate::Result;

pub const SKIP_ALIAS_PREFIX: &str = "__msr";

pub fn run(graph: &mut Graph) -> Result<()> {
    for id in graph.post_order() {
        let root = graph.nodes[id].is_root();
        let node = &mut graph.nodes[id];
        let mut used: Vec<String> = Vec::new();
        for p in node.projections.iter_mut() {
            if root {
                let mut a = p.alias.clone();
                let mut i = 0;
                while used.contains(&a) {
                    a = format!("{}_{}", p.alias, i);
                    i += 1;
                }
                used.push(a.clone());
                p.alias = a;
            } else {
                if p.alias.starts_with(SKIP_ALIAS_PREFIX) {
                    used.push(p.alias.clone());
                    continue;
                }
                let prefix = if p.is_dimension() { "dim" } else { "msr" };
                let key = format!("{}{}", p.alias, p.proj.def.ordinal.map(|o| o.to_string()).unwrap_or_default());
                let mut a = format!("{}{}", prefix, &format!("{:x}", md5::compute(key.as_bytes()))[..7]);
                let mut i = 0;
                while used.contains(&a) {
                    let k = format!("{}_{}", key, i);
                    a = format!("{:x}", md5::compute(k.as_bytes()))[..7].to_string();
                    i += 1;
                }
                used.push(a.clone());
                p.alias = a;
            }
        }
    }
    Ok(())
}
