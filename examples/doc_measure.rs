//! Release-build measurement harness for document extraction (issue #72, "re-measure cutovers").
//!
//! Three subcommands; `gen` and `run` are driven by `scripts/doc_measure.sh` (one process per sample
//! so peak RSS is clean):
//!
//! ```text
//! doc_measure gen <kind> <bytes> <out-path>
//! doc_measure run <path> <mime> <xberg|linear> <none|embed|full>
//! doc_measure probe <path> <mime> <plain|markdown> <pages|nopages>
//! ```
//!
//! `run` forces the chunking path through the cutovers (xberg splitter: `u64::MAX`; linear: `1`),
//! times `extract_doc` and prints one `key=value` line with chunk-quality statistics.
//!
//! `probe` calls xberg directly, with the chunking settings basemind passes it, and prints as JSON
//! what basemind currently discards: page numbers, heading paths, node ids and chunk types per
//! chunk, whether chunk byte offsets index the returned content, and where HTML entities in chunk
//! text come from. `plain|markdown` is xberg's `output_format`; `pages` turns on `extract_pages`.
#![cfg(feature = "documents")]

use std::path::Path;
use std::time::Instant;

use basemind::config::DocumentModelProfile;
use basemind::extract::doc::{DocConfig, extract_doc};
use basemind::extract::doc_guard::ChunkCutovers;
use xberg::core::config::processing::{ChunkerType, ChunkingConfig};
use xberg::core::config::{ExtractionConfig, OutputFormat, PageConfig};
use xberg::{ExtractInput, extract};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() as usize) % items.len()]
    }
}

const WORDS: &[&str] = &[
    "the",
    "scanner",
    "indexes",
    "documents",
    "while",
    "workers",
    "extract",
    "chunks",
    "from",
    "large",
    "files",
    "and",
    "embeds",
    "each",
    "span",
    "into",
    "a",
    "vector",
    "store",
    "for",
    "retrieval",
    "of",
    "relevant",
    "context",
];

fn sentence(rng: &mut Lcg, words: usize) -> String {
    let mut out = String::new();
    for i in 0..words {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(rng.pick(WORDS));
    }
    out
}

fn record(kind: &str, i: u64, rng: &mut Lcg) -> String {
    match kind {
        "log" => format!(
            "2026-10-11T12:{:02}:{:02}.{:03}Z INFO [worker-{}] request id={:08x} path=/api/v1/{} status={} latency={}ms\n",
            (i / 60) % 60,
            i % 60,
            rng.next() % 1000,
            rng.next() % 16,
            rng.next() as u32,
            rng.pick(WORDS),
            rng.pick(&["200", "200", "404", "500"]),
            rng.next() % 900
        ),
        "csv" => format!(
            "{i},{},{},{:.2},{}\n",
            rng.pick(WORDS),
            rng.pick(WORDS),
            (rng.next() % 100000) as f64 / 100.0,
            2020 + rng.next() % 6
        ),
        "yaml" => format!(
            "- id: {i}\n  name: {}\n  enabled: true\n  tags:\n    - {}\n    - {}\n  limits:\n    cpu: {}\n    mem: {}Mi\n",
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.next() % 16,
            rng.next() % 4096
        ),
        "json" => format!(
            "{{\n  \"id\": {i},\n  \"name\": \"{}\",\n  \"tags\": [\"{}\", \"{}\"],\n  \"limits\": {{\"cpu\": {}, \"mem\": {}}}\n}}",
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.next() % 16,
            rng.next() % 4096
        ),
        "xml" => format!(
            "  <item id=\"{i}\">\n    <name>{}</name>\n    <tags><tag>{}</tag><tag>{}</tag></tags>\n    <limits cpu=\"{}\" mem=\"{}\"/>\n  </item>\n",
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.next() % 16,
            rng.next() % 4096
        ),
        "prose_md" => {
            let mut s = String::new();
            if i.is_multiple_of(8) {
                s.push_str(&format!("## Section {i}: {}\n\n", sentence(rng, 4)));
            }
            s.push_str(&sentence(rng, 40));
            s.push_str(".\n\n");
            if i.is_multiple_of(5) {
                s.push_str(&format!("- {}\n- {}\n\n", sentence(rng, 6), sentence(rng, 7)));
            }
            s
        }
        "dense_md" => format!(
            "**{}** `{}` [{}](https://example.com/{i}) *{}* ~~{}~~ **{}** `{}` [{}](https://example.com/x{i}) *{}*\n",
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.pick(WORDS)
        ),
        other => panic!("unknown kind {other}"),
    }
}

fn generate(kind: &str, bytes: usize, out: &Path) {
    let mut rng = Lcg(0x5eed);
    let mut text = String::with_capacity(bytes + 1024);
    match kind {
        "csv" => text.push_str("id,first,second,amount,year\n"),
        "xml" => text.push_str("<?xml version=\"1.0\"?>\n<items>\n"),
        "json" => text.push('['),
        _ => {}
    }
    let mut i = 0;
    while text.len() < bytes {
        let rec = record(kind, i, &mut rng);
        if kind == "json" && i > 0 {
            text.push_str(",\n");
        }
        text.push_str(&rec);
        i += 1;
    }
    match kind {
        "xml" => text.push_str("</items>\n"),
        "json" => text.push_str("]\n"),
        _ => {}
    }
    std::fs::write(out, text).expect("write input");
}

/// True when the chunk starting at `byte_start` begins inside a word of `content`.
fn starts_mid_word(content: &str, byte_start: usize) -> bool {
    let (Some(before), Some(after)) = (content.get(..byte_start), content.get(byte_start..)) else {
        return false;
    };
    before.chars().next_back().is_some_and(char::is_alphanumeric)
        && after.chars().next().is_some_and(char::is_alphanumeric)
}

/// HTML entities (`&amp;`, `&#39;`, `&#x27;`) in `text`, skipping fenced blocks and inline code spans,
/// where an entity is literal content.
fn entities_outside_code(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        for (at, prose) in line.split('`').enumerate() {
            if at % 2 == 1 {
                continue;
            }
            let mut rest = prose;
            while let Some(start) = rest.find('&') {
                rest = &rest[start..];
                let name_len = rest[1..]
                    .bytes()
                    .take(33)
                    .take_while(|b| b.is_ascii_alphanumeric() || *b == b'#')
                    .count();
                if (2..=32).contains(&name_len) && rest.as_bytes().get(1 + name_len) == Some(&b';') {
                    found.push(rest[..name_len + 2].to_string());
                }
                rest = &rest[1..];
            }
        }
    }
    found
}

fn probe(path: &Path, mime: &str, format: &str, pages: &str) {
    let output_format = match format {
        "plain" => OutputFormat::Plain,
        "markdown" => OutputFormat::Markdown,
        other => panic!("unknown output format {other}"),
    };
    let defaults = DocConfig::default();
    let config = ExtractionConfig {
        use_cache: false,
        chunking: Some(ChunkingConfig {
            max_characters: defaults.max_characters,
            overlap: defaults.overlap,
            embedding: None,
            preset: defaults.embedding_preset.clone(),
            chunker_type: ChunkerType::Markdown,
            ..Default::default()
        }),
        disable_ocr: true,
        output_format,
        pages: (pages == "pages").then(|| PageConfig {
            extract_pages: true,
            insert_page_markers: false,
            ..Default::default()
        }),
        extraction_timeout_secs: Some(3600),
        ..Default::default()
    };
    let mut input = ExtractInput::from_uri(path.to_string_lossy().into_owned());
    input.mime_type = Some(mime.to_string());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut extraction = match runtime.block_on(extract(input, &config)) {
        Ok(extraction) => extraction,
        Err(error) => {
            println!("{}", serde_json::json!({"status": "error", "error": error.to_string()}));
            return;
        }
    };
    let Some(result) = extraction.results.pop() else {
        println!("{}", serde_json::json!({"status": "error", "error": "no result"}));
        return;
    };
    let source = std::fs::read(path).expect("read source");
    let source = String::from_utf8_lossy(&source);
    let content = &result.content;
    let chunks = result.chunks.unwrap_or_default();
    let count = |keep: &dyn Fn(&xberg::types::Chunk) -> bool| chunks.iter().filter(|c| keep(c)).count();
    let mut kinds: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let (mut entity_chunks, mut in_source, mut not_in_source) = (0, 0, 0);
    let mut entity_samples: Vec<String> = Vec::new();
    for chunk in &chunks {
        *kinds.entry(format!("{:?}", chunk.chunk_type)).or_default() += 1;
        let entities = entities_outside_code(&chunk.content);
        entity_chunks += usize::from(!entities.is_empty());
        for entity in entities {
            if source.contains(&entity) {
                in_source += 1;
            } else {
                not_in_source += 1;
            }
            if entity_samples.len() < 5 && !entity_samples.contains(&entity) {
                entity_samples.push(entity);
            }
        }
    }
    let sample = |pick: &dyn Fn(&xberg::types::Chunk) -> Option<String>| -> Vec<String> {
        chunks.iter().filter_map(pick).take(3).collect()
    };
    println!(
        "{}",
        serde_json::json!({
            "status": "ok",
            "path": path.file_name().map(|n| n.to_string_lossy().into_owned()),
            "mime": result.mime_type,
            "output_format": format,
            "extract_pages": pages == "pages",
            "content_bytes": content.len(),
            "content_heading_lines": content.lines().filter(|l| l.starts_with('#')).count(),
            "content_table_lines": content.lines().filter(|l| l.starts_with('|')).count(),
            "chunks": chunks.len(),
            "chunk_types": kinds,
            "with_first_page": count(&|c| c.metadata.first_page.is_some()),
            "with_last_page": count(&|c| c.metadata.last_page.is_some()),
            "with_page_spans": count(&|c| !c.metadata.page_spans.is_empty()),
            "with_heading_path": count(&|c| !c.metadata.heading_path.is_empty()),
            "with_heading_context": count(&|c| c.metadata.heading_context.is_some()),
            "with_node_ids": count(&|c| !c.metadata.node_ids.is_empty()),
            "offsets_slice_equals_chunk": count(&|c| {
                content.get(c.metadata.byte_start..c.metadata.byte_end) == Some(c.content.as_str())
            }),
            "offsets_slice_trimmed_equals_chunk": count(&|c| {
                content
                    .get(c.metadata.byte_start..c.metadata.byte_end)
                    .is_some_and(|slice| slice.trim() == c.content.trim())
            }),
            "offsets_out_of_bounds": count(&|c| content.get(c.metadata.byte_start..c.metadata.byte_end).is_none()),
            "starts_mid_word": count(&|c| starts_mid_word(content, c.metadata.byte_start)),
            "chunks_with_entities_outside_code": entity_chunks,
            "entities_also_in_source_file": in_source,
            "entities_absent_from_source_file": not_in_source,
            "entity_samples": entity_samples,
            "page_samples": sample(&|c| {
                c.metadata.first_page.map(|first| format!("{first}-{:?}", c.metadata.last_page))
            }),
            "heading_path_samples": sample(&|c| {
                (!c.metadata.heading_path.is_empty()).then(|| c.metadata.heading_path.join(" > "))
            }),
            "node_id_samples": sample(&|c| c.metadata.node_ids.first().cloned()),
        })
    );
}

fn run(path: &Path, mime: &str, mode: &str, profile: &str) {
    let chunk_cutovers = match mode {
        "xberg" => ChunkCutovers {
            markdown_bytes: u64::MAX,
            plain_text_bytes: u64::MAX,
        },
        "linear" => ChunkCutovers {
            markdown_bytes: 1,
            plain_text_bytes: 1,
        },
        other => panic!("unknown mode {other}"),
    };
    let (embed, document_models) = match profile {
        "none" => (false, DocumentModelProfile::None_),
        "embed" => (true, DocumentModelProfile::CodeOnly),
        "full" => (false, DocumentModelProfile::Full),
        other => panic!("unknown profile {other}"),
    };
    let config = DocConfig {
        extraction_timeout_secs: 3600,
        max_chunks_per_document: usize::MAX,
        chunk_cutovers,
        embed,
        document_models,
        ..DocConfig::default()
    };
    let started = Instant::now();
    let doc = match extract_doc(path, Some(mime), &config) {
        Ok(doc) => doc,
        Err(error) => {
            println!(
                "status=error error={error:?} secs={:.3}",
                started.elapsed().as_secs_f64()
            );
            return;
        }
    };
    let secs = started.elapsed().as_secs_f64();
    let n = doc.chunks.len();
    let mut lens: Vec<usize> = doc.chunks.iter().map(|c| c.text.len()).collect();
    lens.sort_unstable();
    let total: usize = lens.iter().sum();
    let ends_at_break = doc.chunks.iter().filter(|c| c.text.ends_with('\n')).count();
    let starts_mid_line = doc
        .chunks
        .iter()
        .filter(|c| {
            let s = c.byte_start as usize;
            s > 0 && doc.content.as_bytes().get(s - 1).is_some_and(|b| *b != b'\n')
        })
        .count();
    let starts_mid_word_count = doc
        .chunks
        .iter()
        .filter(|c| starts_mid_word(&doc.content, c.byte_start as usize))
        .count();
    let entity_chunks = doc
        .chunks
        .iter()
        .filter(|c| !entities_outside_code(&c.text).is_empty())
        .count();
    let odd_backticks = doc
        .chunks
        .iter()
        .filter(|c| c.text.matches('`').count() % 2 == 1)
        .count();
    let broken_link = doc
        .chunks
        .iter()
        .filter(|c| c.text.matches('[').count() != c.text.matches(']').count())
        .count();
    let pct = |x: usize| if n == 0 { 0.0 } else { 100.0 * x as f64 / n as f64 };
    println!(
        "status=ok secs={secs:.3} content_bytes={} chunks={n} mean_chunk={} p50={} max={} coverage={:.2} end_nl_pct={:.1} start_midline_pct={:.1} odd_backtick_pct={:.1} unbalanced_bracket_pct={:.1} start_midword_pct={:.1} entity_outside_code_pct={:.1} embedded={}",
        doc.content.len(),
        total.checked_div(n).unwrap_or(0),
        lens.get(n / 2).copied().unwrap_or(0),
        lens.last().copied().unwrap_or(0),
        total as f64 / doc.content.len().max(1) as f64,
        pct(ends_at_break),
        pct(starts_mid_line),
        pct(odd_backticks),
        pct(broken_link),
        pct(starts_mid_word_count),
        pct(entity_chunks),
        doc.embedding_dim
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen") => generate(&args[2], args[3].parse().expect("bytes"), Path::new(&args[4])),
        Some("run") => run(Path::new(&args[2]), &args[3], &args[4], &args[5]),
        Some("probe") => probe(Path::new(&args[2]), &args[3], &args[4], &args[5]),
        _ => eprintln!(
            "usage: doc_measure gen <kind> <bytes> <out> | run <path> <mime> <xberg|linear> <none|embed|full> \
             | probe <path> <mime> <plain|markdown> <pages|nopages>"
        ),
    }
}
