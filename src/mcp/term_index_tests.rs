//! The term index must answer exactly what the streaming sweeps it replaces answered.

use super::TermIndex;
use crate::extract::{FileMapL1, Import, Symbol, SymbolKind};
use crate::mcp::MapCache;
use crate::path::RelPath;

const KINDS: [SymbolKind; 4] = [
    SymbolKind::Function,
    SymbolKind::Class,
    SymbolKind::Method,
    SymbolKind::Variable,
];
const WORDS: [&str; 9] = ["get", "user", "Name", "_x", "parse", "ab", "b", "unicodé", ""];

fn lcg(state: &mut u64) -> usize {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 33) as usize
}

fn corpus() -> Vec<(RelPath, FileMapL1)> {
    let mut rng = 7u64;
    (0..60)
        .map(|f| {
            let symbols = (0..lcg(&mut rng) % 12)
                .map(|_| {
                    let name: String = (0..1 + lcg(&mut rng) % 3)
                        .map(|_| WORDS[lcg(&mut rng) % WORDS.len()])
                        .collect();
                    Symbol {
                        name,
                        kind: KINDS[lcg(&mut rng) % KINDS.len()],
                        start_byte: 0,
                        end_byte: 0,
                        start_row: lcg(&mut rng) as u32 % 100,
                        start_col: 0,
                        signature: None,
                        decorators: Vec::new(),
                    }
                })
                .collect();
            let imports = (0..lcg(&mut rng) % 4)
                .map(|_| Import {
                    module: (!lcg(&mut rng).is_multiple_of(3))
                        .then(|| format!("pkg.{}.{}", WORDS[lcg(&mut rng) % 5], f % 7)),
                    raw: format!("import {} from {}", WORDS[lcg(&mut rng) % 5], WORDS[lcg(&mut rng) % 5]),
                    start_byte: 0,
                    end_byte: 0,
                })
                .collect();
            let l1 = FileMapL1 {
                schema_ver: crate::extract::SCHEMA_VER,
                language: "python".into(),
                size_bytes: 0,
                had_errors: false,
                error_count: 0,
                symbols,
                imports,
                implementations: Vec::new(),
                rationale: Vec::new(),
                extract_epoch: crate::extract::EXTRACT_EPOCH,
            };
            (RelPath::from(format!("d{}/f{f:02}.py", f % 5).as_str()), l1)
        })
        .collect()
}

fn needles() -> Vec<String> {
    let mut out: Vec<String> = WORDS
        .iter()
        .filter(|w| !w.is_empty())
        .map(|w| (*w).to_string())
        .collect();
    out.extend(
        [
            "getuser", "userName", "ab", "xyz", "pkg.get", "import", "é", "_xparse", "bb",
        ]
        .map(String::from),
    );
    out
}

fn brute_symbols(cache: &MapCache, needle: &str, kind: Option<SymbolKind>) -> Vec<(RelPath, usize)> {
    let mut out = Vec::new();
    cache.for_each(|p, l1| {
        for (i, s) in l1.symbols.iter().enumerate() {
            if s.name.contains(needle) && kind.is_none_or(|k| s.kind == k) {
                out.push((p.clone(), i));
            }
        }
    });
    out
}

fn indexed_symbols(index: &TermIndex, needle: &str, kind: Option<SymbolKind>) -> Vec<(RelPath, usize)> {
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    index
        .iter()
        .flat_map(|(p, t)| {
            t.matching_symbols(&finder, kind)
                .map(|i| (p.clone(), i))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn brute_dependents(cache: &MapCache, needle: &str) -> Vec<RelPath> {
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    let mut out = Vec::new();
    cache.for_each(|p, l1| {
        if crate::extract::l3::imports_mention(needle, &finder, &l1.imports) {
            out.push(p.clone());
        }
    });
    out
}

fn indexed_dependents(index: &TermIndex, needle: &str) -> Vec<RelPath> {
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    index
        .iter()
        .filter(|(_, t)| t.mentions_import(&finder))
        .map(|(p, _)| p.clone())
        .collect()
}

#[test]
fn sweeps_match_the_streaming_sweeps() {
    let cache = MapCache::from_synthetic(corpus());
    let index = cache.terms();
    for needle in needles() {
        for kind in std::iter::once(None).chain(KINDS.iter().copied().map(Some)) {
            assert_eq!(
                indexed_symbols(index, &needle, kind),
                brute_symbols(&cache, &needle, kind),
                "symbols needle={needle:?} kind={kind:?}"
            );
        }
        assert_eq!(
            indexed_dependents(index, &needle),
            brute_dependents(&cache, &needle),
            "dependents needle={needle:?}"
        );
    }
}

#[test]
fn patched_index_equals_a_rebuild() {
    let all = corpus();
    let old = MapCache::from_synthetic(all.clone());
    let base = old.terms();
    let updated = vec![all[3].0.clone(), all[10].0.clone()];
    let removed = vec![all[7].0.clone()];
    let mut changed = all.clone();
    changed[3].1.symbols.clear();
    changed[10].1.imports.clear();
    changed.remove(7);
    let fresh = MapCache::from_synthetic(changed);
    let patched = base.patched(&updated, &removed, |p| fresh.get(p));
    for needle in needles() {
        assert_eq!(
            indexed_symbols(&patched, &needle, None),
            brute_symbols(&fresh, &needle, None),
            "symbols needle={needle:?}"
        );
        assert_eq!(
            indexed_dependents(&patched, &needle),
            brute_dependents(&fresh, &needle),
            "dependents needle={needle:?}"
        );
    }
}
