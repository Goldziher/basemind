use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryCursor, QueryMatch};

use super::{Call, DocComment, ExtractError, FileMapL2, SCHEMA_VER};
use crate::lang::{
    LangId, ParseOutcome, QueryKind, parse_with_default_timeout, try_get_query, with_parser,
};

pub fn extract_l2(lang: LangId, source: &[u8]) -> Result<FileMapL2, ExtractError> {
    // Use the timeout-bounded parse so L2 gets the same protection as L1 against
    // pathological inputs that spin the parser's error-recovery loop indefinitely.
    let outcome = with_parser(lang, |p| parse_with_default_timeout(p, source))?;
    let tree = match outcome {
        ParseOutcome::Ok(t) => t,
        ParseOutcome::Failed => return Err(ExtractError::ParseFailure),
        ParseOutcome::TimedOut => {
            return Err(ExtractError::ParseTimeout(
                crate::lang::DEFAULT_PARSE_TIMEOUT,
            ));
        }
    };
    extract_l2_from_tree(lang, &tree, source)
}

/// Extract L2 data (calls + docs) from a pre-parsed tree-sitter `Tree`. Separated from
/// `extract_l2` so the scanner can share one parse between L1 and L2 when eager L2 is
/// enabled, avoiding a second full parse per file on the hot path.
pub(crate) fn extract_l2_from_tree(
    lang: LangId,
    tree: &tree_sitter::Tree,
    source: &[u8],
) -> Result<FileMapL2, ExtractError> {
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
