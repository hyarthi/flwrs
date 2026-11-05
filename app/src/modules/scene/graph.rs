//! Graph helpers for scenes. Shared with the director later.

use std::collections::{HashMap, HashSet};

/// Ids of nodes that lie on a cycle (including self-loops), using Tarjan's
/// strongly connected components. `edges` are `(source, target)` pairs.
pub(crate) fn nodes_in_cycles<'a>(
    nodes: &[&'a str],
    edges: &[(&'a str, &'a str)],
) -> HashSet<&'a str> {
    let mut adjacency: HashMap<&str, Vec<&str>> = nodes.iter().map(|n| (*n, vec![])).collect();
    for (from, to) in edges {
        adjacency.entry(from).or_default().push(to);
        adjacency.entry(to).or_default();
    }

    struct State<'a> {
        index: usize,
        indices: HashMap<&'a str, usize>,
        low: HashMap<&'a str, usize>,
        stack: Vec<&'a str>,
        on_stack: HashSet<&'a str>,
        in_cycle: HashSet<&'a str>,
    }

    fn visit<'a>(node: &'a str, adjacency: &HashMap<&'a str, Vec<&'a str>>, s: &mut State<'a>) {
        s.indices.insert(node, s.index);
        s.low.insert(node, s.index);
        s.index += 1;
        s.stack.push(node);
        s.on_stack.insert(node);
        for &next in &adjacency[node] {
            if !s.indices.contains_key(next) {
                visit(next, adjacency, s);
                let low = s.low[node].min(s.low[next]);
                s.low.insert(node, low);
            } else if s.on_stack.contains(next) {
                let low = s.low[node].min(s.indices[next]);
                s.low.insert(node, low);
            }
        }
        if s.low[node] == s.indices[node] {
            let mut component = vec![];
            while let Some(member) = s.stack.pop() {
                s.on_stack.remove(member);
                component.push(member);
                if member == node {
                    break;
                }
            }
            let self_loop = adjacency[node].contains(&node);
            if component.len() > 1 || self_loop {
                s.in_cycle.extend(component);
            }
        }
    }

    let mut state = State {
        index: 0,
        indices: HashMap::new(),
        low: HashMap::new(),
        stack: vec![],
        on_stack: HashSet::new(),
        in_cycle: HashSet::new(),
    };
    let mut order: Vec<&str> = adjacency.keys().copied().collect();
    order.sort_unstable(); // deterministic traversal
    for node in order {
        if !state.indices.contains_key(node) {
            visit(node, &adjacency, &mut state);
        }
    }
    state.in_cycle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(set: HashSet<&str>) -> Vec<&str> {
        let mut v: Vec<&str> = set.into_iter().collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn finds_cycles() {
        // a -> b -> c -> a is a cycle; c -> d leaves it; e loops on itself; f is alone
        let nodes = ["a", "b", "c", "d", "e", "f"];
        let edges = [("a", "b"), ("b", "c"), ("c", "a"), ("c", "d"), ("e", "e")];
        assert_eq!(
            sorted(nodes_in_cycles(&nodes, &edges)),
            ["a", "b", "c", "e"]
        );
    }

    #[test]
    fn acyclic_graphs_have_none() {
        let nodes = ["s", "t1", "t2", "k"];
        let edges = [("s", "t1"), ("s", "t2"), ("t1", "k"), ("t2", "k")];
        assert!(nodes_in_cycles(&nodes, &edges).is_empty());
        assert!(nodes_in_cycles(&[], &[]).is_empty());
    }

    #[test]
    fn two_separate_cycles() {
        let nodes = ["a", "b", "x", "y", "z"];
        let edges = [("a", "b"), ("b", "a"), ("x", "y"), ("y", "z"), ("z", "y")];
        assert_eq!(
            sorted(nodes_in_cycles(&nodes, &edges)),
            ["a", "b", "y", "z"]
        );
    }
}
