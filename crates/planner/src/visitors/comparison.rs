//! Where a filter comparing a measure with another field is applied.
//!
//! It compares at the grain the query answers at, which is the root's.
//! Below a merge a node may aggregate a measure at a rule's grain, a
//! share's denominator at a coarser one, or hold one side only, so none of
//! them applies it. When the root is a merge the comparison leaves the
//! nodes below and is applied there, and each measure it reads becomes a
//! column of an input that aggregates it at the query's grain. A root that
//! is not a merge is the only node and applies it as a HAVING.

use crate::dag::{Filtered, Graph, NodeId};
use crate::resolved::{ResolvedFilter, ResolvedProjection, ResolvedQuery};
use crate::Result;
use std::rc::Rc;

pub fn run<'a>(graph: &mut Graph<'a>, q: &ResolvedQuery<'a>) -> Result<()> {
    let root = graph.root();
    if !graph.nodes[root].is_merge() {
        return Ok(());
    }
    let comparisons: Vec<Rc<ResolvedFilter<'a>>> = q.filters.iter().filter(|f| f.is_measure() && f.is_comparison()).cloned().collect();
    if comparisons.is_empty() {
        return Ok(());
    }
    let fids: Vec<usize> = comparisons.iter().map(|c| c.fid).collect();
    let below: Vec<NodeId> = graph.live().filter(|n| n.id != root).map(|n| n.id).collect();
    let mut held: Vec<NodeId> = Vec::new();
    for id in below {
        if graph.nodes[id].filters.iter().any(|f| fids.contains(&f.fid())) {
            held.push(id);
            graph.nodes[id].filters.retain(|f| !fids.contains(&f.fid()));
        }
    }
    for c in comparisons {
        if !graph.nodes[root].filters.iter().any(|f| f.fid() == c.fid) {
            graph.nodes[root].filters.push(Filtered::new(c.clone()));
        }
        for m in c.fields().filter(|m| m.is_measure()) {
            let inputs = graph.nodes[root].plain_inputs();
            let output = inputs.iter().any(|i| {
                graph.nodes[*i].projections.iter().any(|p| p.field().map(|f| f.id == m.id).unwrap_or(false) && p.decorators().is_empty())
            });
            if output {
                continue;
            }
            // An input that applied the comparison aggregated the measure
            // at the query's grain; failing one, the reference visitor
            // refuses with the column no input outputs.
            let holder = inputs
                .iter()
                .copied()
                .find(|i| held.contains(i) && graph.nodes[*i].universe.as_ref().map(|u| u.has_field(m.id)).unwrap_or(false));
            if let Some(i) = holder {
                graph.nodes[i].add_projection(Rc::new(ResolvedProjection::for_field(m)));
            }
        }
    }
    Ok(())
}
