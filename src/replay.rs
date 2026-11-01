//! What pytest's reporter is sent: the run's tests in collection order, whatever order the workers finish them in.

use crate::zygote::Shown;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Holds what each test reports until every test before it in collection order has been released, and stops the run
/// where pytest would: after the `maxfail`-th failure in that order.
pub struct InOrder<T> {
    position: HashMap<String, usize>,
    held: Vec<Option<(bool, T)>>,
    next: usize,
    failures: u32,
    pub maxfail: u32,
    stopped: bool,
}

impl<T> InOrder<T> {
    pub fn new(order: &[&str], maxfail: u32) -> InOrder<T> {
        InOrder {
            position: order.iter().enumerate().map(|(i, n)| (n.to_string(), i)).collect(),
            held: order.iter().map(|_| None).collect(),
            next: 0,
            failures: 0,
            maxfail,
            stopped: false,
        }
    }

    /// Takes one test's outcome and what it reported, and returns what is now due, in order. Nothing is due after the
    /// test that reached `maxfail`.
    pub fn arrived(&mut self, nodeid: &str, failed: bool, report: T) -> Vec<T> {
        let mut due = Vec::new();
        let Some(&i) = self.position.get(nodeid) else { return due };
        self.held[i].get_or_insert((failed, report));
        while !self.stopped
            && let Some((failed, report)) = self.held.get_mut(self.next).and_then(Option::take)
        {
            self.next += 1;
            due.push(report);
            if failed {
                self.failures += 1;
                self.stopped = self.maxfail > 0 && self.failures >= self.maxfail;
            }
        }
        due
    }

    /// Whether a test in order reached `maxfail`, so the rest of the run is not wanted.
    pub fn stopped(&self) -> bool {
        self.stopped
    }
}

/// The message that ends the reporter's collection: forktest's headline, the tests it will replay in order, how many
/// it deselected, what collection itself reported, and when the run started, which pytest counts its time from.
pub fn begin(
    headline: Option<String>,
    items: &[&str],
    deselected: u64,
    shown: &Shown,
    with_skips: bool,
    started: std::time::SystemTime,
) -> Value {
    let reports: Vec<&Value> =
        shown.collect_reports.iter().filter(|r| with_skips || r["outcome"] != "skipped").collect();
    json!({
        "type": "begin",
        "headline": headline,
        "items": items,
        "deselected": deselected,
        "collect_reports": reports,
        "warnings": shown.warnings,
        "started": started.duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()),
    })
}

/// A test's reports and warnings as a worker serialized them.
pub fn test(mut pytest: Value) -> Value {
    pytest["type"] = json!("test");
    pytest
}

/// A test whose worker died or never collected it, which the reporter shows as an error in setup.
pub fn lost(nodeid: &str, message: &str) -> Value {
    json!({"type": "lost", "nodeid": nodeid, "message": message})
}

pub fn collect(report: Value) -> Value {
    json!({"type": "collect", "report": report})
}

pub fn warnings(warnings: Vec<Value>) -> Value {
    json!({"type": "warnings", "warnings": warnings})
}

pub fn end() -> Value {
    json!({"type": "end"})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(due: Vec<&str>) -> Vec<String> {
        due.into_iter().map(str::to_string).collect()
    }

    fn msg(n: &str) -> &str {
        n
    }

    #[test]
    fn tests_are_released_in_collection_order_as_the_prefix_completes() {
        let mut o = InOrder::new(&["a", "b", "c", "d"], 0);
        assert!(o.arrived("c", false, msg("c")).is_empty());
        assert!(o.arrived("b", false, msg("b")).is_empty());
        assert_eq!(ids(o.arrived("a", false, msg("a"))), ["a", "b", "c"]);
        assert_eq!(ids(o.arrived("d", false, msg("d"))), ["d"]);
        assert!(!o.stopped());
    }

    #[test]
    fn maxfail_counts_failures_in_collection_order_not_arrival_order() {
        let mut o = InOrder::new(&["a", "b", "c", "d"], 1);
        assert!(o.arrived("d", true, msg("d")).is_empty());
        assert!(!o.stopped(), "d failed first, but a, b and c come before it");
        assert_eq!(ids(o.arrived("a", false, msg("a"))), ["a"]);
        assert_eq!(ids(o.arrived("c", false, msg("c"))), Vec::<String>::new());
        assert_eq!(ids(o.arrived("b", true, msg("b"))), ["b"]);
        assert!(o.stopped(), "b is the first failure in order; c and d are past the stop");
        assert!(o.arrived("x", true, msg("x")).is_empty());
    }

    #[test]
    fn what_is_held_is_up_to_the_caller() {
        let mut o = InOrder::new(&["a", "b"], 1);
        assert!(o.arrived("b", true, ()).is_empty() && !o.stopped());
        assert_eq!(o.arrived("a", true, ()).len(), 1);
        assert!(o.stopped());
    }

    #[test]
    fn begin_leaves_out_module_skips_unless_asked() {
        let shown = Shown {
            collect_reports: vec![json!({"outcome": "failed"}), json!({"outcome": "skipped"})],
            warnings: vec![],
            deselected: 2,
        };
        let now = std::time::SystemTime::now();
        assert_eq!(begin(None, &["t::a"], 3, &shown, false, now)["collect_reports"].as_array().unwrap().len(), 1);
        assert_eq!(begin(None, &["t::a"], 3, &shown, true, now)["collect_reports"].as_array().unwrap().len(), 2);
    }
}
