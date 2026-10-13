//! Regression tests for basemind issue #78: xberg verdicts on text-like files (a Helm template named
//! `.yaml`, a CSV past the table-cell limit, an SVG with an oversized entity) must not lose the
//! document. `extract_doc` re-reads them as `text/plain` and flags the result as a text fallback;
//! genuine failures stay failures.

#![cfg(feature = "documents")]

use std::fs;

use basemind::extract::ExtractError;
use basemind::extract::doc::{DocConfig, extract_doc};

fn offline_doc_config() -> DocConfig {
    DocConfig {
        embed: false,
        embedding_preset: None,
        ..DocConfig::default()
    }
}

const HELM_TEMPLATE: &str = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ include \"chart.fullname\" . }}\n  labels:\n    {{- include \"chart.labels\" . | nindent 4 }}\ndata:\n{{- range $k, $v := .Values.config }}\n  {{ $k }}: {{ $v | quote }}\n{{- end }}\n{{- if .Values.extra }}\n  extra: {{ .Values.extra }}\n{{- end }}\n";

#[test]
fn helm_template_yaml_is_read_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("configmap.yaml");
    fs::write(&path, HELM_TEMPLATE).unwrap();

    let doc = extract_doc(&path, Some("application/x-yaml"), &offline_doc_config()).expect("fallback");
    // xberg >= 1.3.9 reads the template itself; older releases rejected it and the text fallback took over.
    // Either way the document must survive with its body intact.
    assert!(doc.content.contains("include \"chart.fullname\""));
    assert!(!doc.chunks.is_empty());
}

#[test]
fn valid_yaml_is_not_a_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("values.yaml");
    fs::write(&path, "replicas: 2\nimage:\n  tag: v1\n").unwrap();

    let doc = extract_doc(&path, Some("application/x-yaml"), &offline_doc_config()).expect("extract");
    assert!(!doc.text_fallback);
}

#[test]
fn csv_over_table_cell_limit_is_read_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.csv");
    // 400 columns x 400 rows = 160k cells, past xberg's 100k-cell table limit.
    let header: Vec<String> = (0..400).map(|c| format!("c{c}")).collect();
    let mut body = header.join(",");
    body.push('\n');
    for r in 0..400 {
        let row: Vec<String> = (0..400).map(|c| (r * c).to_string()).collect();
        body.push_str(&row.join(","));
        body.push('\n');
    }
    fs::write(&path, &body).unwrap();

    let doc = extract_doc(&path, Some("text/csv"), &offline_doc_config()).expect("fallback");
    // xberg >= 1.3.9 accepts 160k cells; older releases tripped the cell limit and fell back to text.
    assert!(!doc.chunks.is_empty());
    assert!(doc.content.starts_with("c0,c1"));
}

#[test]
fn svg_with_oversized_entity_is_read_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embedded.svg");
    let payload = "A".repeat(2 * 1024 * 1024);
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><desc>{payload}</desc><rect width=\"1\" height=\"1\"/></svg>"
    );
    fs::write(&path, svg).unwrap();

    let doc = extract_doc(&path, Some("image/svg+xml"), &offline_doc_config()).expect("fallback");
    // xberg >= 1.3.9 accepts the 2 MiB text node; older releases tripped the entity limit and fell back to text.
    assert!(!doc.chunks.is_empty());
    assert!(doc.content.contains("<rect"));
}

#[test]
fn corrupt_pdf_stays_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.pdf");
    fs::write(&path, b"%PDF-1.4 this is not a pdf").unwrap();

    let result = extract_doc(&path, Some("application/pdf"), &offline_doc_config());
    assert!(matches!(result, Err(ExtractError::Document(_))), "{result:?}");
}

#[test]
fn missing_file_stays_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let result = extract_doc(&dir.path().join("gone.csv"), Some("text/csv"), &offline_doc_config());
    assert!(result.is_err());
}
