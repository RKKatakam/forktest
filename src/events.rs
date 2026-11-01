//! What a run reports. Reporters consume these, and the daemon ships them to clients as JSON lines.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Passed,
    Failed,
    Skipped,
    Error,
    Xfailed,
    Xpassed,
}

impl Outcome {
    pub fn event_name(self) -> &'static str {
        match self {
            Outcome::Passed => "pass",
            Outcome::Failed => "fail",
            Outcome::Skipped => "skip",
            Outcome::Error => "error",
            Outcome::Xfailed => "xfail",
            Outcome::Xpassed => "xpass",
        }
    }

    pub fn from_event_name(name: &str) -> Option<Outcome> {
        Some(match name {
            "pass" => Outcome::Passed,
            "fail" => Outcome::Failed,
            "skip" => Outcome::Skipped,
            "error" => Outcome::Error,
            "xfail" => Outcome::Xfailed,
            "xpass" => Outcome::Xpassed,
            _ => return None,
        })
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Outcome::Failed | Outcome::Error)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub file: String,
    pub line: u32,
    #[serde(rename = "fn")]
    pub func: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestResult {
    pub nodeid: String,
    pub outcome: Outcome,
    /// Seconds, the sum of pytest's report durations.
    pub duration: f64,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub message: Option<String>,
    pub trace: Vec<Frame>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Done {
    pub passed: u64,
    pub failed: u64,
    pub errors: u64,
    pub skipped: u64,
    pub xfailed: u64,
    pub xpassed: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Selection { changed: Vec<String>, selected: u64, total: u64, reasons: BTreeMap<String, u64>, duration_ms: u64 },
    Result(TestResult),
    CollectError { nodeid: String, message: String },
    Miss { nodeid: String, message: String },
    Verified { checked: u64, misses: u64, duration_ms: u64 },
    /// pytest's own reporter printed the run, headline first, and its session exited with `exit`.
    Reported { exit: i32 },
    /// pytest's reporter could not report the run; forktest's own output stands in.
    ReporterLost { reason: String, headline_shown: bool },
    Done(Done),
}

impl Event {
    /// The Python version's JSON shape, key order included.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{Map, Value, json};
        match self {
            Event::Selection { changed, selected, total, reasons, duration_ms } => json!({
                "event": "selection", "changed": changed, "selected": selected, "total": total,
                "reasons": reasons, "duration_ms": duration_ms,
            }),
            Event::Result(r) => {
                let mut m = Map::new();
                m.insert("event".into(), Value::from(r.outcome.event_name()));
                m.insert("test".into(), Value::from(r.nodeid.clone()));
                m.insert("duration_ms".into(), json!((r.duration * 10_000.0).round() / 10.0));
                if let Some(message) = &r.message {
                    m.insert("file".into(), json!(r.file));
                    m.insert("line".into(), json!(r.line));
                    m.insert("message".into(), Value::from(message.clone()));
                    m.insert("trace".into(), serde_json::to_value(&r.trace).unwrap());
                }
                Value::Object(m)
            }
            Event::CollectError { nodeid, message } => {
                json!({"event": "collect_error", "nodeid": nodeid, "message": message})
            }
            Event::Miss { nodeid, message } => json!({"event": "miss", "test": nodeid, "message": message}),
            Event::Verified { checked, misses, duration_ms } => {
                json!({"event": "verified", "checked": checked, "misses": misses, "duration_ms": duration_ms})
            }
            Event::Reported { exit } => json!({"event": "reported", "exit": exit}),
            Event::ReporterLost { reason, headline_shown } => {
                json!({"event": "reporter_lost", "reason": reason, "headline_shown": headline_shown})
            }
            Event::Done(d) => json!({
                "event": "done", "passed": d.passed, "failed": d.failed, "errors": d.errors,
                "skipped": d.skipped, "xfailed": d.xfailed, "xpassed": d.xpassed, "duration_ms": d.duration_ms,
            }),
        }
    }

    pub fn to_line(&self) -> String {
        self.to_json().to_string()
    }

    pub fn parse(line: &str) -> anyhow::Result<Event> {
        let v: serde_json::Value = serde_json::from_str(line)?;
        let kind = v["event"].as_str().unwrap_or_default();
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        if let Some(outcome) = Outcome::from_event_name(kind) {
            return Ok(Event::Result(TestResult {
                nodeid: s("test"),
                outcome,
                duration: v["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0,
                file: v["file"].as_str().map(str::to_string),
                line: v["line"].as_u64().map(|l| l as u32),
                message: v["message"].as_str().map(str::to_string),
                trace: serde_json::from_value(v["trace"].clone()).unwrap_or_default(),
            }));
        }
        Ok(match kind {
            "selection" => Event::Selection {
                changed: serde_json::from_value(v["changed"].clone())?,
                selected: n("selected"),
                total: n("total"),
                reasons: serde_json::from_value(v["reasons"].clone())?,
                duration_ms: n("duration_ms"),
            },
            "collect_error" => Event::CollectError { nodeid: s("nodeid"), message: s("message") },
            "miss" => Event::Miss { nodeid: s("test"), message: s("message") },
            "verified" => Event::Verified { checked: n("checked"), misses: n("misses"), duration_ms: n("duration_ms") },
            "reported" => Event::Reported { exit: v["exit"].as_i64().unwrap_or(3) as i32 },
            "reporter_lost" => {
                Event::ReporterLost { reason: s("reason"), headline_shown: v["headline_shown"].as_bool().unwrap_or(false) }
            }
            "done" => Event::Done(Done {
                passed: n("passed"),
                failed: n("failed"),
                errors: n("errors"),
                skipped: n("skipped"),
                xfailed: n("xfailed"),
                xpassed: n("xpassed"),
                duration_ms: n("duration_ms"),
            }),
            other => anyhow::bail!("unknown event {other:?}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_round_trips_through_its_json_line() {
        let events = vec![
            Event::Selection {
                changed: vec!["billing/tax.py".into()],
                selected: 7,
                total: 16,
                reasons: BTreeMap::from([("coverage".into(), 7)]),
                duration_ms: 5,
            },
            Event::Result(TestResult {
                nodeid: "tests/test_tax.py::test_round".into(),
                outcome: Outcome::Failed,
                duration: 0.0004,
                file: Some("tests/test_tax.py".into()),
                line: Some(14),
                message: Some("assert 1 == 2".into()),
                trace: vec![Frame { file: "tests/test_tax.py".into(), line: 14, func: "test_round".into() }],
            }),
            Event::CollectError { nodeid: "tests/test_x.py".into(), message: "boom".into() },
            Event::Miss { nodeid: "t::a".into(), message: "m".into() },
            Event::Verified { checked: 9, misses: 0, duration_ms: 3 },
            Event::Reported { exit: 1 },
            Event::ReporterLost { reason: "gone".into(), headline_shown: true },
            Event::Done(Done { passed: 6, failed: 1, duration_ms: 52, ..Done::default() }),
        ];
        for e in events {
            assert_eq!(Event::parse(&e.to_line()).unwrap(), e);
        }
    }
}
