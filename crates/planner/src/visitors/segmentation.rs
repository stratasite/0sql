//! Rails `Planner::Segmentation`: which segment links apply to a component,
//! resolved through provenance, and the signatures that decide whether two
//! measures may share an aggregation.

use crate::dag::Node;
use crate::model::{RProjection, RQuery};
use std::rc::Rc;
use zsql_spec::LinkMode;
use zsql_semantic::Field;

pub type Signature = Vec<(usize, LinkMode)>;

/// The links on the first persisted, linked projection up the source chain.
pub fn component_links<'a>(p: &Rc<RProjection<'a>>) -> Vec<(usize, LinkMode)> {
    let mut current = Some(p.clone());
    let mut seen: Vec<usize> = Vec::new();
    while let Some(c) = current {
        if seen.contains(&c.pid) {
            break;
        }
        seen.push(c.pid);
        if c.persisted && !c.links.is_empty() {
            return c.links.clone();
        }
        current = c.source();
    }
    vec![]
}

pub fn signature_for(links: &[(usize, LinkMode)]) -> Signature {
    let mut v: Vec<(usize, LinkMode)> = links.to_vec();
    v.sort();
    v.dedup();
    v
}

pub fn signature<'a>(p: &Rc<RProjection<'a>>) -> Signature {
    signature_for(&component_links(p))
}

/// The union over a node's measure components.
pub fn node_signature(node: &Node) -> Signature {
    let links: Vec<(usize, LinkMode)> = node.projections.iter().filter(|p| p.is_measure()).flat_map(|p| component_links(&p.proj)).collect();
    signature_for(&links)
}

/// Every key dimension any of the query's segments demands, measure-level included.
pub fn all_key_dimensions<'a>(q: &RQuery<'a>) -> Vec<&'a Field> {
    let mut out: Vec<&Field> = q.query_level_key_dimensions();
    for p in q.projections.iter().filter(|p| p.is_measure() || p.is_calculation()) {
        for (sid, _) in component_links(p) {
            if let Some(s) = q.segment(sid) {
                for f in s.key_dimensions() {
                    if !out.iter().any(|x| x.id == f.id) {
                        out.push(f);
                    }
                }
            }
        }
    }
    out
}

/// Key dimensions demanded by a projection's own links and the query-level ones.
pub fn keys_for<'a>(p: &Rc<RProjection<'a>>, q: &RQuery<'a>) -> Vec<&'a Field> {
    let mut out: Vec<&Field> = Vec::new();
    let mut sids: Vec<usize> = component_links(p).iter().map(|(s, _)| *s).collect();
    sids.dedup();
    for sid in sids {
        if let Some(s) = q.segment(sid) {
            for f in s.key_dimensions() {
                if !out.iter().any(|x| x.id == f.id) {
                    out.push(f);
                }
            }
        }
    }
    for f in q.query_level_key_dimensions() {
        if !out.iter().any(|x| x.id == f.id) {
            out.push(f);
        }
    }
    out
}
