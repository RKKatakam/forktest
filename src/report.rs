//! Human, JSONL and JUnit reporters, the exit status, and the fan-out that survives a reader closing early.

use crate::events::{Done, Event, Outcome, TestResult};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;

pub trait Reporter {
    fn handle(&mut self, event: &Event) -> io::Result<()>;

    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One JSON object per line, ASCII only, as Python's `json.dumps`. Passing tests are summarized in `done`.
pub struct JsonlReporter<W: Write> {
    out: W,
}

impl<W: Write> JsonlReporter<W> {
    pub fn new(out: W) -> Self {
        JsonlReporter { out }
    }
}

impl<W: Write> Reporter for JsonlReporter<W> {
    fn handle(&mut self, event: &Event) -> io::Result<()> {
        if let Event::Result(r) = event
            && r.outcome == Outcome::Passed
        {
            return Ok(());
        }
        let mut line = ascii_json(&event.to_line());
        line.push('\n');
        self.out.write_all(line.as_bytes())?;
        self.out.flush()
    }
}

/// Python's `ensure_ascii`: everything outside space to tilde is escaped, as UTF-16 code units.
fn ascii_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() && c != '\x7f' {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

#[derive(Default)]
struct Counts {
    passed: u64,
    failed: u64,
    skipped: u64,
    error: u64,
    xfailed: u64,
    xpassed: u64,
}

impl Counts {
    fn add(&mut self, outcome: Outcome) {
        *match outcome {
            Outcome::Passed => &mut self.passed,
            Outcome::Failed => &mut self.failed,
            Outcome::Skipped => &mut self.skipped,
            Outcome::Error => &mut self.error,
            Outcome::Xfailed => &mut self.xfailed,
            Outcome::Xpassed => &mut self.xpassed,
        } += 1;
    }
}

/// Compact terminal output: failures stream as they arrive, files summarize at the end.
pub struct HumanReporter<W: Write> {
    out: W,
    expect_verify: bool,
    color: bool,
    files: Vec<(String, Counts)>,
    index: HashMap<String, usize>,
}

impl<W: Write> HumanReporter<W> {
    pub fn new(out: W, color: bool, expect_verify: bool) -> Self {
        HumanReporter { out, expect_verify, color, files: Vec::new(), index: HashMap::new() }
    }

    fn mark(&self, ok: bool) -> &'static str {
        match (self.color, ok) {
            (false, true) => "✓",
            (false, false) => "✗",
            (true, true) => "\x1b[32m✓\x1b[0m",
            (true, false) => "\x1b[31m✗\x1b[0m",
        }
    }

    fn emit(&mut self, text: &str) -> io::Result<()> {
        self.out.write_all(text.as_bytes())?;
        self.out.write_all(b"\n")?;
        self.out.flush()
    }

    fn selection(&mut self, changed: usize, selected: u64, total: u64, reasons: &std::collections::BTreeMap<String, u64>, ms: u64) -> io::Result<()> {
        let head = if reasons.get("all") == Some(&selected) && selected > 0 {
            format!("all {} tests selected", thousands(total))
        } else if reasons.get("new") == Some(&total) && total > 0 {
            format!("no history · {} of {} tests selected", thousands(selected), thousands(total))
        } else {
            let s = if changed != 1 { "s" } else { "" };
            format!("{changed} file{s} changed · {} of {} tests selected", thousands(selected), thousands(total))
        };
        self.emit(&format!("forktest · {head} ({})", seconds(ms, 2)))
    }

    fn result(&mut self, r: &TestResult) -> io::Result<()> {
        let file = r.nodeid.split("::").next().unwrap_or_default().to_string();
        let i = match self.index.get(&file) {
            Some(&i) => i,
            None => {
                self.index.insert(file.clone(), self.files.len());
                self.files.push((file, Counts::default()));
                self.files.len() - 1
            }
        };
        self.files[i].1.add(r.outcome);
        if !r.outcome.is_failure() {
            return Ok(());
        }
        self.emit(&format!("  {} {}", self.mark(false), r.nodeid))?;
        let place = match &r.file {
            Some(f) => format!("{f}:{}  ", r.line.map_or("None".to_string(), |l| l.to_string())),
            None => String::new(),
        };
        let mut lines = splitlines(r.message.as_deref().unwrap_or_default());
        if lines.is_empty() {
            lines.push("");
        }
        self.emit(&format!("      {place}{}", lines[0]))?;
        for line in lines.iter().skip(1).take(5) {
            self.emit(&format!("      {}", strip(line)))?;
        }
        Ok(())
    }

    fn done(&mut self, d: &Done) -> io::Result<()> {
        let width = self.files.iter().map(|(f, _)| f.chars().count()).max().unwrap_or(0);
        let mut summaries = Vec::new();
        for (file, c) in &self.files {
            if c.failed > 0 || c.error > 0 {
                continue;
            }
            let parts: Vec<String> = [(c.passed, "passed"), (c.skipped, "skipped"), (c.xfailed, "xfailed"), (c.xpassed, "xpassed")]
                .into_iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, k)| format!("{n} {k}"))
                .collect();
            let pad = " ".repeat(width - file.chars().count());
            summaries.push(format!("  {} {file}{pad}   {}", self.mark(true), parts.join(", ")));
        }
        for line in summaries {
            self.emit(&line)?;
        }
        let s = if d.errors != 1 { "s" } else { "" };
        let parts = [
            (d.failed > 0).then(|| format!("{} failed", d.failed)),
            (d.errors > 0).then(|| format!("{} error{s}", d.errors)),
            Some(format!("{} passed", d.passed)),
            (d.skipped > 0).then(|| format!("{} skipped", d.skipped)),
            (d.xfailed > 0).then(|| format!("{} xfailed", d.xfailed)),
            (d.xpassed > 0).then(|| format!("{} xpassed", d.xpassed)),
        ];
        let mut summary = parts.into_iter().flatten().collect::<Vec<_>>().join(", ");
        if d.passed + d.failed + d.errors + d.skipped + d.xfailed + d.xpassed == 0 {
            summary = "nothing to run".into();
        }
        let tail = if self.expect_verify { " · safety net: running unselected tests" } else { "" };
        self.emit(&format!("{summary} in {}{tail}", seconds(d.duration_ms, 2)))
    }
}

impl<W: Write> Reporter for HumanReporter<W> {
    fn handle(&mut self, event: &Event) -> io::Result<()> {
        match event {
            Event::Selection { changed, selected, total, reasons, duration_ms } => {
                self.selection(changed.len(), *selected, *total, reasons, *duration_ms)
            }
            Event::Result(r) => self.result(r),
            Event::CollectError { nodeid, message } => {
                self.emit(&format!("  {} collection error in {nodeid}", self.mark(false)))?;
                self.emit(&indent(message, 6, Some(12)))
            }
            Event::Miss { nodeid, message } => {
                self.emit(&format!("  {} MISS {nodeid} (not selected, failed in safety net)", self.mark(false)))?;
                self.emit(&indent(message, 6, None))
            }
            Event::Verified { checked, misses, duration_ms } => {
                let tail = if *misses == 0 { "0 misses".to_string() } else { format!("{misses} MISSED") };
                self.emit(&format!("safety net: {checked} unselected tests ran in {} · {tail}", seconds(*duration_ms, 2)))
            }
            Event::Done(d) => self.done(d),
        }
    }
}

fn seconds(ms: u64, places: usize) -> String {
    format!("{:.places$}s", ms as f64 / 1000.0)
}

/// Python's `f"{n:,}"`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Python's `str.splitlines()`.
fn splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.push(&text[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|(_, n)| *n == '\n') {
                chars.next();
                start += 1;
            }
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Python's `str.strip()`, whose whitespace also counts the ASCII separators 0x1c to 0x1f.
fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

fn indent(text: &str, n: usize, limit: Option<usize>) -> String {
    let mut lines: Vec<String> = splitlines(text).into_iter().map(str::to_string).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    if let Some(limit) = limit
        && lines.len() > limit
    {
        let more = lines.len() - limit + 1;
        lines.truncate(limit - 1);
        lines.push(format!("... {more} more lines"));
    }
    let pad = " ".repeat(n);
    lines.iter().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn quoteattr(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub struct JunitReporter {
    path: PathBuf,
    results: Vec<TestResult>,
    collect_errors: Vec<(String, String)>,
    done: Option<Done>,
}

impl JunitReporter {
    pub fn new(path: PathBuf) -> Self {
        JunitReporter { path, results: Vec::new(), collect_errors: Vec::new(), done: None }
    }

    fn render(&self) -> String {
        let d = self.done.clone().unwrap_or_default();
        let errors = d.errors + self.collect_errors.len() as u64;
        let mut lines = vec![
            r#"<?xml version="1.0" encoding="utf-8"?>"#.to_string(),
            format!(
                r#"<testsuites><testsuite name="forktest" tests="{}" failures="{}" errors="{errors}" skipped="{}" time="{}">"#,
                self.results.len(),
                d.failed,
                d.skipped + d.xfailed,
                format!("{:.3}", d.duration_ms as f64 / 1000.0),
            ),
        ];
        for r in &self.results {
            let (classname, name) = r.nodeid.rsplit_once("::").unwrap_or(("", &r.nodeid));
            let classname = classname.replace('/', ".");
            let classname = classname.strip_suffix(".py").unwrap_or(&classname).replace("::", ".");
            let head = format!(
                r#"<testcase classname={} name={} time="{:.3}""#,
                quoteattr(&classname),
                quoteattr(name),
                r.duration
            );
            let tag = match r.outcome {
                Outcome::Passed | Outcome::Xpassed => {
                    lines.push(head + " />");
                    continue;
                }
                Outcome::Failed => "failure",
                Outcome::Error => "error",
                Outcome::Skipped | Outcome::Xfailed => "skipped",
            };
            let kind = if r.outcome == Outcome::Xfailed { r#" type="pytest.xfail""# } else { "" };
            let message = r.message.as_deref().unwrap_or_default();
            let first = splitlines(message).first().copied().unwrap_or_default();
            lines.push(format!(
                "{head}><{tag}{kind} message={}>{}</{tag}></testcase>",
                quoteattr(first),
                escape(message)
            ));
        }
        for (nodeid, message) in &self.collect_errors {
            lines.push(format!(
                r#"<testcase classname="collection" name={} time="0"><error message="collection error">{}</error></testcase>"#,
                quoteattr(nodeid),
                escape(message)
            ));
        }
        lines.push("</testsuite></testsuites>".into());
        lines.join("\n") + "\n"
    }
}

impl Reporter for JunitReporter {
    fn handle(&mut self, event: &Event) -> io::Result<()> {
        match event {
            Event::Result(r) => self.results.push(r.clone()),
            Event::CollectError { nodeid, message } => self.collect_errors.push((nodeid.clone(), message.clone())),
            Event::Done(d) => self.done = Some(d.clone()),
            _ => {}
        }
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        std::fs::write(&self.path, self.render())
    }
}

/// Remembers only what the exit code should be.
#[derive(Default)]
pub struct ExitStatus {
    done: Option<Done>,
    total: Option<u64>,
    misses: u64,
    collect_errors: u64,
}

impl ExitStatus {
    pub fn handle(&mut self, event: &Event) {
        match event {
            Event::Done(d) => self.done = Some(d.clone()),
            Event::Selection { total, .. } => self.total = Some(*total),
            Event::Miss { .. } => self.misses += 1,
            Event::CollectError { .. } => self.collect_errors += 1,
            _ => {}
        }
    }

    /// `fallback` is pytest's own exit code, for a run that stopped before reporting anything.
    pub fn code(&self, fallback: i32) -> i32 {
        let Some(done) = &self.done else { return if fallback != 0 { fallback } else { 3 } };
        if self.collect_errors > 0 && self.total.is_none() {
            return 2;
        }
        if done.failed > 0 || done.errors > 0 || self.misses > 0 {
            return 1;
        }
        if self.total == Some(0) {
            return 5;
        }
        0
    }
}

/// Every reporter plus the exit status. A reporter whose reader left (`| head`) is dropped and the run goes on, so
/// its results still get recorded.
pub struct Reporters {
    live: Vec<Box<dyn Reporter>>,
    pub status: ExitStatus,
}

impl Reporters {
    pub fn new(live: Vec<Box<dyn Reporter>>) -> Self {
        Reporters { live, status: ExitStatus::default() }
    }

    pub fn emit(&mut self, event: &Event) {
        self.status.handle(event);
        self.live.retain_mut(|r| r.handle(event).is_ok());
    }

    pub fn close(&mut self) {
        for mut r in self.live.drain(..) {
            if let Err(e) = r.close() {
                eprintln!("forktest: writing a report: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Frame;
    use std::collections::BTreeMap;

    fn result(nodeid: &str, outcome: Outcome, message: Option<&str>) -> Event {
        Event::Result(TestResult {
            nodeid: nodeid.into(),
            outcome,
            duration: 0.0123,
            file: message.map(|_| "tests/test_a.py".into()),
            line: message.map(|_| 7),
            message: message.map(str::to_string),
            trace: message.map(|_| vec![Frame { file: "tests/test_a.py".into(), line: 7, func: "test_b".into() }]).unwrap_or_default(),
        })
    }

    fn human(events: &[Event], verify: bool) -> String {
        let mut out = Vec::new();
        let mut r = HumanReporter::new(&mut out, false, verify);
        for e in events {
            r.handle(e).unwrap();
        }
        drop(r);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn human_output_matches_the_python_reporter() {
        let events = [
            Event::Selection { changed: vec!["a.py".into()], selected: 3, total: 1234, reasons: BTreeMap::from([("coverage".into(), 3)]), duration_ms: 15 },
            result("tests/test_a.py::test_a", Outcome::Passed, None),
            result("tests/test_a.py::test_b", Outcome::Failed, Some("assert 1 == 2\n  where 1 = f()\n\n x\n5\n6\n7")),
            result("tests/test_long_name.py::test_c", Outcome::Xfailed, Some("known")),
            result("tests/test_long_name.py::test_d", Outcome::Passed, None),
            Event::CollectError { nodeid: "tests/test_x.py".into(), message: (1..=14).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n") },
            Event::Done(Done { passed: 2, failed: 1, xfailed: 1, errors: 1, duration_ms: 1234, ..Done::default() }),
        ];
        let expected = "forktest · 1 file changed · 3 of 1,234 tests selected (0.01s)\n\
            \x20 ✗ tests/test_a.py::test_b\n\
            \x20     tests/test_a.py:7  assert 1 == 2\n\
            \x20     where 1 = f()\n\
            \x20     \n\
            \x20     x\n\
            \x20     5\n\
            \x20     6\n\
            \x20 ✗ collection error in tests/test_x.py\n\
            \x20     l1\n      l2\n      l3\n      l4\n      l5\n      l6\n      l7\n      l8\n      l9\n      l10\n      l11\n\
            \x20     ... 3 more lines\n\
            \x20 ✓ tests/test_long_name.py   1 passed, 1 xfailed\n\
            1 failed, 1 error, 2 passed, 1 xfailed in 1.23s\n";
        assert_eq!(human(&events, false), expected);
    }

    #[test]
    fn human_headlines_for_all_new_and_nothing() {
        let sel = |selected, total, reason: &str| Event::Selection {
            changed: vec![],
            selected,
            total,
            reasons: BTreeMap::from([(reason.to_string(), selected)]),
            duration_ms: 0,
        };
        assert_eq!(human(&[sel(16, 16, "all")], false), "forktest · all 16 tests selected (0.00s)\n");
        assert_eq!(human(&[sel(16, 16, "new")], false), "forktest · no history · 16 of 16 tests selected (0.00s)\n");
        assert_eq!(human(&[sel(0, 16, "new")], false), "forktest · 0 files changed · 0 of 16 tests selected (0.00s)\n");
        assert_eq!(
            human(&[Event::Done(Done::default())], true),
            "nothing to run in 0.00s · safety net: running unselected tests\n"
        );
        let verified = Event::Verified { checked: 9, misses: 2, duration_ms: 250 };
        assert_eq!(human(&[verified], false), "safety net: 9 unselected tests ran in 0.25s · 2 MISSED\n");
    }

    #[test]
    fn jsonl_skips_passes_and_escapes_like_python() {
        let mut out = Vec::new();
        let mut r = JsonlReporter::new(&mut out);
        r.handle(&result("t.py::a", Outcome::Passed, None)).unwrap();
        r.handle(&result("t.py::é", Outcome::Failed, Some("😀\x7f"))).unwrap();
        drop(r);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"event\":\"fail\",\"test\":\"t.py::\\u00e9\",\"duration_ms\":12.3,\"file\":\"tests/test_a.py\",\"line\":7,\
             \"message\":\"\\ud83d\\ude00\\u007f\",\"trace\":[{\"file\":\"tests/test_a.py\",\"line\":7,\"fn\":\"test_b\"}]}\n"
        );
    }

    #[test]
    fn junit_matches_the_python_reporter() {
        let dir = crate::fingerprint::tests::tmp_dir("junit");
        let path = dir.join("out.xml");
        let mut r = JunitReporter::new(path.clone());
        for e in [
            result("tests/test_a.py::TestX::test_a", Outcome::Passed, None),
            result("tests/test_a.py::test_b", Outcome::Failed, Some("a < b & \"c\"\nmore")),
            result("tests/test_a.py::test_c", Outcome::Xfailed, Some("known")),
            Event::CollectError { nodeid: "tests/test_x.py".into(), message: "boom".into() },
            Event::Done(Done { passed: 1, failed: 1, xfailed: 1, duration_ms: 52, ..Done::default() }),
        ] {
            r.handle(&e).unwrap();
        }
        r.close().unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <testsuites><testsuite name=\"forktest\" tests=\"3\" failures=\"1\" errors=\"1\" skipped=\"1\" time=\"0.052\">\n\
             <testcase classname=\"tests.test_a.py.TestX\" name=\"test_a\" time=\"0.012\" />\n\
             <testcase classname=\"tests.test_a\" name=\"test_b\" time=\"0.012\"><failure message=\"a &lt; b &amp; &quot;c&quot;\">a &lt; b &amp; \"c\"\nmore</failure></testcase>\n\
             <testcase classname=\"tests.test_a\" name=\"test_c\" time=\"0.012\"><skipped type=\"pytest.xfail\" message=\"known\">known</skipped></testcase>\n\
             <testcase classname=\"collection\" name=\"tests/test_x.py\" time=\"0\"><error message=\"collection error\">boom</error></testcase>\n\
             </testsuite></testsuites>\n"
        );
    }

    #[test]
    fn exit_codes_follow_pytest() {
        let code = |events: &[Event], fallback| {
            let mut s = ExitStatus::default();
            events.iter().for_each(|e| s.handle(e));
            s.code(fallback)
        };
        let sel = |total| Event::Selection { changed: vec![], selected: 0, total, reasons: BTreeMap::new(), duration_ms: 0 };
        let done = |failed| Event::Done(Done { failed, ..Done::default() });
        assert_eq!(code(&[], 4), 4);
        assert_eq!(code(&[], 0), 3);
        assert_eq!(code(&[Event::CollectError { nodeid: "x".into(), message: "m".into() }, done(0)], 0), 2);
        assert_eq!(code(&[sel(3), done(1)], 0), 1);
        assert_eq!(code(&[sel(3), Event::Miss { nodeid: "x".into(), message: "m".into() }, done(0)], 0), 1);
        assert_eq!(code(&[sel(0), done(0)], 0), 5);
        assert_eq!(code(&[sel(3), done(0)], 0), 0);
    }

    struct Closed;

    impl Reporter for Closed {
        fn handle(&mut self, _: &Event) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn a_reader_that_left_is_dropped_and_the_status_still_counts() {
        let mut r = Reporters::new(vec![Box::new(Closed)]);
        r.emit(&Event::Selection { changed: vec![], selected: 1, total: 1, reasons: BTreeMap::new(), duration_ms: 0 });
        r.emit(&Event::Done(Done { failed: 1, ..Done::default() }));
        assert!(r.live.is_empty());
        assert_eq!(r.status.code(0), 1);
    }
}
