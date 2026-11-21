use super::*;
use crate::engine::memory::ops::put_core;
use crate::engine::memory::test_support::workspace;
use crate::engine::types::memory::Visibility;

/// A model that is not there: every text is refused.
fn offline(_text: &str) -> anyhow::Result<Vec<f32>> {
    anyhow::bail!("the model is not downloaded")
}

fn note(key: &str, value: &str, tags: &[&str]) -> WireMemoryEntry {
    WireMemoryEntry {
        key: key.into(),
        value: value.into(),
        tags: tags.iter().map(|tag| tag.to_string()).collect(),
        created_at: 1,
        updated_at: 1,
    }
}

fn save(ws: &Workspace, key: &str, value: &str, tags: &[&str]) {
    let store = ws.store_read();
    let idx = store.index_db.as_ref().unwrap();
    let tags: Vec<String> = tags.iter().map(|tag| tag.to_string()).collect();
    put_core(idx, &ws.scope, Visibility::Group.vis_byte(), "", key, value, &tags).unwrap();
}

fn search_json(ws: &Workspace, query: &str) -> serde_json::Value {
    let params = MemorySearchParams {
        query: query.into(),
        limit: None,
        tags: Vec::new(),
        visibility: Visibility::Group,
    };
    serde_json::to_value(crate::engine::memory::vectors::search(ws, params).unwrap()).unwrap()
}

fn hits(response: &serde_json::Value) -> Vec<(String, f64)> {
    response["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            (
                hit["key"].as_str().unwrap().to_string(),
                hit["distance"].as_f64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn words_are_the_lowercase_alphanumeric_runs_each_once() {
    assert_eq!(
        words("Decision/auth-Tokens: rotate AUTH tokens 24h"),
        ["decision", "auth", "tokens", "rotate", "24h"]
    );
    assert!(words(" -/- ").is_empty());
}

#[test]
fn a_query_word_matches_the_start_of_a_word_in_the_key_value_or_tags() {
    let rotation = note("decision/auth-tokens", "Tokens rotate every 24h.", &["security"]);
    assert_eq!(matched(&words("token rotation"), &rotation), 1);
    assert_eq!(matched(&words("decision secur"), &rotation), 2);
    assert_eq!(matched(&words("okens"), &rotation), 0);
}

#[test]
fn a_hit_holds_at_least_half_of_the_query_words() {
    assert!(is_hit(1, 1));
    assert!(is_hit(1, 2));
    assert!(!is_hit(1, 3));
    assert!(is_hit(2, 3));
    assert!(!is_hit(0, 0));
}

/// With no model to embed the query, a search matches the notes by its words, ranks the note
/// holding more of them first, says so, and answers an unrelated query with no hit.
#[test]
fn an_offline_search_matches_the_notes_by_words() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path(), "fake", offline);
    save(&ws, "decision/auth-tokens", "Tokens rotate every 24h.", &["auth"]);
    save(&ws, "convention/tokens", "Name tokens after their issuer.", &[]);
    save(&ws, "finding/build", "The release build needs SDKROOT.", &[]);

    let found = search_json(&ws, "auth token rotation");
    // Two of the three words: `rotation` begins no word of the note. ~keep
    let a_third = f64::from(1.0_f32 - 2.0_f32 / 3.0_f32);
    assert_eq!(hits(&found), [("decision/auth-tokens".to_string(), a_third)]);
    assert_eq!(found["lexical"], true);
    assert_eq!(found["notice"], notice("embed: the model is not downloaded"));

    let ranked = search_json(&ws, "tokens");
    assert_eq!(
        hits(&ranked),
        [
            ("convention/tokens".to_string(), 0.0),
            ("decision/auth-tokens".to_string(), 0.0)
        ]
    );

    let unrelated = search_json(&ws, "kubernetes ingress");
    assert!(hits(&unrelated).is_empty(), "{unrelated}");
    assert_eq!(unrelated["lexical"], true);
}

/// An empty query is the caller's to fix, model or no model.
#[test]
fn an_empty_query_is_refused_rather_than_matched() {
    let root = tempfile::tempdir().unwrap();
    let ws = workspace(root.path(), "fake", offline);
    let params = MemorySearchParams {
        query: "  ".into(),
        limit: None,
        tags: Vec::new(),
        visibility: Visibility::Group,
    };
    let refused = crate::engine::memory::vectors::search(&ws, params).unwrap_err();
    assert!(matches!(refused, EngineError::InvalidArgument(_)), "{refused:?}");
}
