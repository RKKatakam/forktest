//! Batches for workers: module groups, failed first, longest processing time. Unit U1.
//!
//! A module costing more than an even share of the run is split by class, then into consecutive runs, so
//! module- and class-scoped fixtures are rebuilt as rarely as the share allows.
//!
//! Failed-first and cost order decide which worker gets which batch. Each worker then runs its tests in collection
//! order: suites that leak state between tests (a test mutating a shared module global) pass under pytest's order,
//! and running a later file first on the same worker would expose the leak to an earlier one.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

/// `items` are `(nodeid, cost in seconds)` in collection order. Returns per-worker lists of indices into `items`,
/// each ascending.
pub fn schedule(items: &[(&str, f64)], failed: &HashSet<&str>, workers: usize) -> Vec<Vec<usize>> {
    let workers = workers.max(1);
    let cost = |batch: &[usize]| sum(batch.iter().map(|&i| items[i].1));
    let all: Vec<usize> = (0..items.len()).collect();
    let share = cost(&all) / workers as f64;
    let mut batches: Vec<Vec<usize>> = Vec::new();
    for module in group(items, &all, |nodeid| nodeid.split("::").next().unwrap_or(nodeid)) {
        if cost(&module) <= share {
            batches.push(module);
            continue;
        }
        for class in group(items, &module, |nodeid| nodeid.rsplit_once("::").map_or(nodeid, |(head, _)| head)) {
            batches.extend(chunks(items, &class, share));
        }
    }
    let mut keyed: Vec<(bool, f64, Vec<usize>)> =
        batches.into_iter().map(|b| (!b.iter().any(|&i| failed.contains(items[i].0)), cost(&b), b)).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)));

    let n = workers.min(keyed.len());
    let mut assigned: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut loads: BinaryHeap<Reverse<(Load, usize)>> = (0..n).map(|k| Reverse((Load(0.0), k))).collect();
    for (_, c, batch) in keyed {
        let Reverse((Load(load), k)) = loads.pop().expect("one load per worker");
        assigned[k].extend(batch);
        loads.push(Reverse((Load(load + c), k)));
    }
    assigned.retain(|a| !a.is_empty());
    assigned.iter_mut().for_each(|a| a.sort_unstable());
    assigned
}

/// Neumaier's compensated sum, as Python's `sum` of floats since 3.12, so share boundaries split identically.
fn sum(values: impl Iterator<Item = f64>) -> f64 {
    let (mut total, mut compensation) = (0.0f64, 0.0f64);
    for x in values {
        let t = total + x;
        compensation += if total.abs() >= x.abs() { (total - t) + x } else { (x - t) + total };
        total = t;
    }
    total + compensation
}

#[derive(PartialEq)]
struct Load(f64);

impl Eq for Load {}

impl PartialOrd for Load {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Load {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Groups in order of first appearance, each in item order.
fn group<'a>(items: &[(&'a str, f64)], members: &[usize], key: impl Fn(&'a str) -> &'a str) -> Vec<Vec<usize>> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut out: Vec<Vec<usize>> = Vec::new();
    for &i in members {
        let k = *index.entry(key(items[i].0)).or_insert_with(|| {
            out.push(Vec::new());
            out.len() - 1
        });
        out[k].push(i);
    }
    out
}

fn chunks(items: &[(&str, f64)], members: &[usize], share: f64) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new()];
    let mut load = 0.0;
    for &i in members {
        let d = items[i].1;
        if !out.last().expect("never empty").is_empty() && load + d > share {
            out.push(Vec::new());
            load = 0.0;
        }
        out.last_mut().expect("never empty").push(i);
        load += d;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan<'a>(items: &[(&'a str, f64)], failed: &[&str], workers: usize) -> Vec<Vec<&'a str>> {
        let failed: HashSet<&str> = failed.iter().copied().collect();
        schedule(items, &failed, workers).into_iter().map(|b| b.into_iter().map(|i| items[i].0).collect()).collect()
    }

    fn worker_of<'a>(plan: &[Vec<&'a str>]) -> HashMap<&'a str, usize> {
        plan.iter().enumerate().flat_map(|(k, b)| b.iter().map(move |n| (*n, k))).collect()
    }

    #[test]
    fn a_module_heavier_than_an_even_share_is_split_along_class_boundaries() {
        let heavy: Vec<String> =
            "ABCD".chars().flat_map(|c| (0..5).map(move |k| format!("tests/test_heavy.py::Test{c}::test_{k}"))).collect();
        let light: Vec<String> =
            (0..4).flat_map(|m| (0..3).map(move |k| format!("tests/test_light_{m}.py::test_{k}"))).collect();
        let items: Vec<(&str, f64)> =
            heavy.iter().map(|n| (n.as_str(), 0.1)).chain(light.iter().map(|n| (n.as_str(), 0.01))).collect();
        let cost: HashMap<&str, f64> = items.iter().copied().collect();
        let p = plan(&items, &[], 4);
        let w = worker_of(&p);
        let loads: Vec<f64> = p.iter().map(|b| b.iter().map(|n| cost[n]).sum()).collect();
        assert_eq!(p.len(), 4);
        assert!(loads.iter().all(|&l| l <= 0.6), "{loads:?}");
        for c in "ABCD".chars() {
            let ws: HashSet<usize> = (0..5).map(|k| w[format!("tests/test_heavy.py::Test{c}::test_{k}").as_str()]).collect();
            assert_eq!(ws.len(), 1, "{c}");
        }
        for m in 0..4 {
            let ws: HashSet<usize> = (0..3).map(|k| w[format!("tests/test_light_{m}.py::test_{k}").as_str()]).collect();
            assert_eq!(ws.len(), 1, "{m}");
        }
    }

    #[test]
    fn one_oversized_class_is_split_into_consecutive_runs() {
        let names: Vec<String> = (0..8).map(|k| format!("t.py::TestBig::test_{k}")).collect();
        let items: Vec<(&str, f64)> = names.iter().map(|n| (n.as_str(), 0.1)).collect();
        let p = plan(&items, &[], 4);
        let mut lens: Vec<usize> = p.iter().map(Vec::len).collect();
        lens.sort();
        assert_eq!(lens, [2, 2, 2, 2]);
        for batch in &p {
            let ks: Vec<usize> = batch.iter().map(|n| n.rsplit_once('_').unwrap().1.parse().unwrap()).collect();
            assert_eq!(ks, (ks[0]..ks[0] + ks.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn unknown_costs_still_spread_a_single_module_over_the_workers() {
        let names: Vec<String> = (0..6).map(|k| format!("t.py::test_{k}")).collect();
        let items: Vec<(&str, f64)> = names.iter().map(|n| (n.as_str(), 0.01)).collect();
        assert_eq!(plan(&items, &[], 3).iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 2]);
    }

    #[test]
    fn modules_that_fit_stay_whole_and_failed_tests_run_first() {
        let names: Vec<String> =
            ["a", "b"].iter().flat_map(|m| (0..3).map(move |k| format!("{m}.py::test_{k}"))).collect();
        let items: Vec<(&str, f64)> = names.iter().map(|n| (n.as_str(), 0.01)).collect();
        let p = plan(&items, &["b.py::test_2"], 2);
        let mut modules: Vec<Vec<char>> =
            p.iter().map(|b| b.iter().map(|n| n.chars().next().unwrap()).collect()).collect();
        modules.sort();
        assert_eq!(modules, [vec!['a'; 3], vec!['b'; 3]]);
        assert!(p[0][0].starts_with("b.py"));
    }

    #[test]
    fn each_worker_runs_its_tests_in_collection_order() {
        let names: Vec<String> =
            ["a", "b", "c", "d"].iter().flat_map(|m| (0..2).map(move |k| format!("{m}.py::test_{k}"))).collect();
        let items: Vec<(&str, f64)> = names.iter().map(|n| (n.as_str(), 0.01)).collect();
        let failed: HashSet<&str> = HashSet::from(["d.py::test_1"]);
        let plan = schedule(&items, &failed, 2);
        assert_eq!(plan.iter().map(Vec::len).sum::<usize>(), 8);
        for worker in &plan {
            assert!(worker.windows(2).all(|w| w[0] < w[1]), "{worker:?}");
        }
        assert!(plan.iter().any(|w| w.contains(&7)), "the failed module is still scheduled");
    }

    #[test]
    fn nothing_to_run_gives_no_batches() {
        assert!(plan(&[], &[], 4).is_empty());
    }
}
