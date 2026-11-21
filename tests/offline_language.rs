//! `lang::language`, the grammar loader every parse goes through, while offline (#208). The language
//! pack downloads a grammar it does not have on first use, so with an offline switch on and an empty
//! grammar cache the call must be refused before the pack is asked. The manifest URL points where
//! nothing listens, so a regression fails at the connection instead of reaching the network.

use basemind::lang::LangError;

const NOWHERE: &str = "http://127.0.0.1:1/manifest.json";
const REFUSAL: &str = "offline mode: missing grammars [\"rust\"] and BASEMIND_OFFLINE or BASEMIND_GRAMMAR_OFFLINE \
                       is set; copy a grammar cache that holds them into ";

#[test]
fn offline_a_grammar_that_is_not_downloaded_is_refused_before_any_download() {
    // SAFETY: this binary holds only this test, and nothing reads the environment concurrently.
    unsafe { std::env::set_var("TREE_SITTER_LANGUAGE_PACK_MANIFEST_URL", NOWHERE) };
    let grammars = tempfile::tempdir().expect("grammar cache");
    let cache = grammars.path().to_path_buf();
    basemind::config::engine::update(|engine| {
        engine.grammars.offline = true;
        engine.grammars.cache_dir = Some(cache);
    });

    let error = basemind::lang::language("rust").expect_err("no grammar is cached and downloads are off");

    let LangError::Download(message) = &error else {
        panic!("expected the offline refusal, got {error:?}");
    };
    assert!(message.starts_with(REFUSAL), "{message}");
}
