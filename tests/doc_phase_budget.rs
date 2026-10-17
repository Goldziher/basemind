//! The document phase must not waste its budget: structured data skips the quadratic markdown
//! splitter at any size, and a spent phase budget is checked before a document is announced as
//! being chunked.

#![cfg(feature = "documents")]

use std::fs;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use basemind::extract::ExtractError;
use basemind::extract::doc::{DocConfig, extract_doc};
use basemind::extract::doc_guard::DocPhase;

fn offline_doc_config() -> DocConfig {
    DocConfig {
        embed: false,
        embedding_preset: None,
        ..DocConfig::default()
    }
}

#[test]
fn json_yaml_and_svg_under_the_cutover_use_the_linear_chunker() {
    let dir = tempfile::tempdir().unwrap();
    for (name, mime, body) in [
        (
            "cfg.json",
            "application/json",
            r#"{"name": "demo", "items": [1, 2, 3]}"#,
        ),
        (
            "cfg.yaml",
            "application/x-yaml",
            "name: demo\nitems:\n  - alpha\n  - beta\n",
        ),
        (
            "logo.svg",
            "image/svg+xml",
            r#"<svg xmlns="http://www.w3.org/2000/svg"><text>hello</text></svg>"#,
        ),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, body).unwrap();
        let doc = extract_doc(&path, Some(mime), &offline_doc_config()).expect("extract");
        assert!(
            doc.linear_chunked,
            "{name} is structured data and must be chunked linearly"
        );
    }
}

#[test]
fn markdown_under_the_cutover_keeps_the_markdown_splitter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guide.md");
    fs::write(
        &path,
        "# Title\n\nSome prose about the project.\n\n## Part\n\nMore prose.\n",
    )
    .unwrap();
    let doc = extract_doc(&path, Some("text/markdown"), &offline_doc_config()).expect("extract");
    assert!(!doc.linear_chunked);
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_spent_phase_budget_defers_before_any_chunking_is_announced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cfg.json");
    fs::write(&path, r#"{"name": "demo"}"#).unwrap();
    let config = DocConfig {
        phase: Some(Arc::new(DocPhase::with_budget(Duration::ZERO))),
        ..offline_doc_config()
    };

    let captured = Captured::default();
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    let error = tracing::subscriber::with_default(subscriber, || {
        extract_doc(&path, Some("application/json"), &config).expect_err("budget is spent")
    });

    assert!(matches!(error, ExtractError::DocDeferred(_)), "got {error:?}");
    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        !logs.contains("linear chunking"),
        "a deferred document must not be logged as chunked:\n{logs}"
    );
}
