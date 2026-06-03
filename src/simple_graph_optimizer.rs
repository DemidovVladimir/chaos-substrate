use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    hash::Hash,
};

#[derive(Debug, Clone)]
pub struct WeightedMultiGraph<Id>
where
    Id: Clone + Eq + Hash,
{
    outgoing: HashMap<Id, Vec<(Id, f64)>>,
}

impl<Id> WeightedMultiGraph<Id>
where
    Id: Clone + Eq + Hash,
{
    pub fn new() -> Self {
        Self {
            outgoing: HashMap::new(),
        }
    }

    pub fn add_edge(&mut self, source: Id, target: Id, cost: f64) {
        self.outgoing
            .entry(source)
            .or_default()
            .push((target, cost));
    }

    pub fn shortest_path(&self, source: Id, target: Id) -> Option<OptimalPath<Id>> {
        let mut dist: HashMap<Id, f64> = HashMap::new();
        let mut prev: HashMap<Id, Id> = HashMap::new();
        let mut heap = BinaryHeap::new();
        dist.insert(source.clone(), 0.0);
        heap.push(State {
            node: source.clone(),
            cost: 0.0,
        });

        while let Some(State { node, cost }) = heap.pop() {
            if node == target {
                let mut nodes = vec![target.clone()];
                let mut current = target;
                while let Some(parent) = prev.get(&current) {
                    nodes.push(parent.clone());
                    current = parent.clone();
                }
                nodes.reverse();
                return Some(OptimalPath {
                    nodes,
                    total_cost: cost,
                });
            }
            if cost > *dist.get(&node).unwrap_or(&f64::INFINITY) {
                continue;
            }
            for (next, edge_cost) in self.outgoing.get(&node).into_iter().flatten() {
                let next_cost = cost + edge_cost;
                if next_cost < *dist.get(next).unwrap_or(&f64::INFINITY) {
                    dist.insert(next.clone(), next_cost);
                    prev.insert(next.clone(), node.clone());
                    heap.push(State {
                        node: next.clone(),
                        cost: next_cost,
                    });
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct OptimalPath<Id> {
    pub nodes: Vec<Id>,
    pub total_cost: f64,
}

#[derive(Clone)]
struct State<Id> {
    node: Id,
    cost: f64,
}

impl<Id> PartialEq for State<Id> {
    fn eq(&self, other: &Self) -> bool {
        self.cost == other.cost
    }
}

impl<Id> Eq for State<Id> {}

impl<Id> Ord for State<Id> {
    fn cmp(&self, other: &Self) -> Ordering {
        // `total_cmp` is a total order over all f64 (NaN sorts consistently),
        // so the BinaryHeap invariant holds even if a cost is ever non-finite.
        // Reversed (other vs self) to make the heap pop the lowest cost first.
        other.cost.total_cmp(&self.cost)
    }
}

impl<Id> PartialOrd for State<Id> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
