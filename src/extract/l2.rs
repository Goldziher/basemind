use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryCursor, QueryMatch};

use super::{Call, DocComment, ExtractError, FileMapL2, SCHEMA_VER};
use crate::lang::{LangId, QueryKind, try_get_query, with_parser};

pub fn extract_l2(lang: LangId, source: &[u8]) -> Result<FileMapL2, ExtractError> {
    let tree = with_parser(lang, |p| p.parse(source, None))?.ok_or(ExtractError::ParseFailure)?;
    let root = tree.root_node();

    let calls = run_calls(lang, root, source)?;
    let docs = run_docs(lang, root, source)?;

    Ok(FileMapL2 {
        schema_ver: SCHEMA_VER,
        language: lang.to_string(),
        calls,
        docs,
    })
}

fn run_calls(
    lang: LangId,
    root: tree_sitter::Node,
    source: &[u8],
) -> Result<Vec<Call>, ExtractError> {
    let Some(q) = try_get_query(lang, QueryKind::Calls)? else {
        return Ok(Vec::new());
    };
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&q, root, source);
    let mut out = Vec::new();
    while let Some(m) = iter.next() {
        if let Some(call) = build_call(&q, m, source) {
            out.push(call);
        }
    }
    Ok(out)
}

fn run_docs(
    lang: LangId,
    root: tree_sitter::Node,
    source: &[u8],
) -> Result<Vec<DocComment>, ExtractError> {
    let Some(q) = try_get_query(lang, QueryKind::Docs)? else {
        return Ok(Vec::new());
    };
    let mut cursor = QueryCursor::new();
    let mut iter = cursor.matches(&q, root, source);
    let mut out = Vec::new();
    while let Some(m) = iter.next() {
        if let Some(doc) = build_doc(&q, m, source) {
            out.push(doc);
        }
    }
    Ok(out)
}

fn capture_name(q: &Query, index: u32) -> &str {
    q.capture_names()[index as usize]
}

fn build_call(q: &Query, m: &QueryMatch, source: &[u8]) -> Option<Call> {
    let mut callee: Option<String> = None;
    let mut range_node = None;
    for cap in m.captures {
        let cname = capture_name(q, cap.index);
        match cname {
            "call.callee" => {
                callee = cap.node.utf8_text(source).ok().map(|s| s.to_string());
            }
            "call.range" => range_node = Some(cap.node),
            _ => {}
        }
    }
    let node = range_node?;
    let pos = node.start_position();
    Some(Call {
        callee: callee?,
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
        start_row: pos.row as u32,
        start_col: pos.column as u32,
    })
}

fn build_doc(q: &Query, m: &QueryMatch, source: &[u8]) -> Option<DocComment> {
    for cap in m.captures {
        if capture_name(q, cap.index) == "doc.text" {
            let node = cap.node;
            let text = node.utf8_text(source).ok()?.to_string();
            return Some(DocComment {
                text,
                start_byte: node.start_byte() as u32,
                end_byte: node.end_byte() as u32,
            });
        }
    }
    None
}
