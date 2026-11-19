//! A call is recorded at its callee name, not at the receiver or path that starts the call
//! expression: the line and column a text search reports, and the byte a resolver records the use
//! at. A multi-line method chain is where the two differ.

use super::Call;
use super::l2::extract_l2;

/// The one call to `callee` in `src`, as `(0-based row, 0-based column)`, with the source text from
/// its start byte to its end byte.
fn position<'s>(lang: &'static str, src: &'s str, callee: &str) -> ((u32, u32), &'s str) {
    let calls = extract_l2(lang, src.as_bytes()).expect("extract l2").calls;
    let found: Vec<&Call> = calls.iter().filter(|c| c.callee == callee).collect();
    let [call] = found[..] else {
        panic!("{lang}: one `{callee}` call expected in {calls:?}");
    };
    let text = &src[call.start_byte as usize..call.end_byte as usize];
    ((call.start_row, call.start_col), text)
}

#[test]
fn a_rust_method_call_in_a_multi_line_chain_is_at_its_method_name() {
    // tokio `runtime/scheduler/multi_thread/worker.rs`, `steal_work`.
    let src = "\
impl Core {
    fn steal_work(&mut self, worker: &Worker) -> Option<Notified> {
        let target = &worker.handle.shared.remotes[i];
        if let Some(task) = target
            .steal
            .steal_into(&mut self.run_queue, &mut self.stats)
        {
            return Some(task);
        }
        None
    }
}
";
    let (at, text) = position("rust", src, "steal_into");
    assert_eq!(at, (5, 13));
    assert_eq!(text, "steal_into(&mut self.run_queue, &mut self.stats)");
}

#[test]
fn rust_calls_through_a_path_turbofish_or_chain_are_at_their_names() {
    let src = "\
fn t() {
    let set = LocalSet::new();
    set
        .run_until(async {})
        .await;
    let n = s
        .parse::<u32>();
    a.b().c();
    helper(1);
}
";
    assert_eq!(position("rust", src, "new"), ((1, 24), "new()"));
    assert_eq!(position("rust", src, "run_until").0, (3, 9));
    assert_eq!(position("rust", src, "parse"), ((6, 9), "parse::<u32>()"));
    assert_eq!(position("rust", src, "helper"), ((8, 4), "helper(1)"));
    let (b, c) = (position("rust", src, "b"), position("rust", src, "c"));
    assert_eq!(
        (b.0, c.0),
        ((7, 6), (7, 10)),
        "one chain's calls start at distinct bytes"
    );
}

#[test]
fn method_chains_in_other_languages_are_at_their_method_names() {
    let js = "client\n  .fetchAll()\n  .then(done);\n";
    for lang in ["javascript", "typescript", "tsx"] {
        assert_eq!(position(lang, js, "fetchAll").0, (1, 3), "{lang}");
        assert_eq!(position(lang, js, "then"), ((2, 3), "then(done)"), "{lang}");
    }
    let py = "rows = (session\n    .query(User)\n    .all())\n";
    assert_eq!(position("python", py, "query"), ((1, 5), "query(User)"));
    let go = "package p\n\nfunc f() {\n\tb.\n\t\tRun(1)\n}\n";
    assert_eq!(position("go", go, "Run"), ((4, 2), "Run(1)"));
    let java = "class A { void f() { builder\n    .add(1)\n    .build(); } }\n";
    assert_eq!(position("java", java, "add"), ((1, 5), "add(1)"));
}
