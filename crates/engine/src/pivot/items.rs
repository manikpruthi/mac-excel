//! PivotTable report trees: the row and column items as they nest, in report order. The items
//! themselves (and their labels) are interned in the model's aggregation
//! (`gridcraft_model::features::pivot::agg`); these trees lay them out.

use std::cmp::Ordering;
use std::collections::HashMap;

use gridcraft_model::{PivotField, PivotSort};

use super::{FieldItems, cmp_items};

#[derive(Clone, Debug)]
pub(super) struct Node {
    pub(super) item: u32,
    pub(super) children: Vec<Node>,
}

pub(super) fn build_tree(paths: &[&[u32]], depth: usize, fields: &[&PivotField], items: &[FieldItems]) -> Vec<Node> {
    let (Some(field), Some(fi)) = (fields.get(depth), items.get(depth)) else { return vec![] };
    let mut order: Vec<u32> = Vec::new();
    let mut groups: HashMap<u32, Vec<&[u32]>> = HashMap::new();
    for p in paths {
        if let Some(&it) = p.get(depth) {
            let g = groups.entry(it).or_default();
            if g.is_empty() {
                order.push(it);
            }
            g.push(p);
        }
    }
    let mut nodes: Vec<Node> = order
        .into_iter()
        .map(|it| {
            let ps = groups.remove(&it).unwrap_or_default();
            Node { item: it, children: build_tree(&ps, depth + 1, fields, items) }
        })
        .collect();
    let by = |a: &Node, b: &Node| match (fi.get(a.item), fi.get(b.item)) {
        (Some(x), Some(y)) => match field.sort {
            PivotSort::Asc => cmp_items(x, y),
            PivotSort::Desc => cmp_items(y, x),
            PivotSort::None => x.first.cmp(&y.first),
        },
        _ => Ordering::Equal,
    };
    nodes.sort_by(by);
    nodes
}

pub(super) fn collect_siblings(nodes: &[Node], prefix: &mut Vec<u32>, out: &mut HashMap<Vec<u32>, Vec<u32>>) {
    out.insert(prefix.clone(), nodes.iter().map(|n| n.item).collect());
    for n in nodes {
        prefix.push(n.item);
        collect_siblings(&n.children, prefix, out);
        prefix.pop();
    }
}
