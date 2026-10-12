//! Release-build measurement harness for document extraction (issue #72, "re-measure cutovers").
//!
//! Two subcommands, driven by `scripts/doc_measure.sh` (one process per sample so peak RSS is clean):
//!
//! ```text
//! doc_measure gen <kind> <bytes> <out-path>
//! doc_measure run <path> <mime> <xberg|linear> <none|embed|full>
//! ```
//!
//! `run` forces the chunking path through the cutovers (xberg splitter: `u64::MAX`; linear: `1`),
//! times `extract_doc` and prints one `key=value` line with chunk-quality statistics.
#![cfg(feature = "documents")]

use std::path::Path;
use std::time::Instant;

use basemind::config::DocumentModelProfile;
use basemind::extract::doc::{DocConfig, extract_doc};
use basemind::extract::doc_guard::ChunkCutovers;

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
        "status=ok secs={secs:.3} content_bytes={} chunks={n} mean_chunk={} p50={} max={} coverage={:.2} end_nl_pct={:.1} start_midline_pct={:.1} odd_backtick_pct={:.1} unbalanced_bracket_pct={:.1} embedded={}",
        doc.content.len(),
        total.checked_div(n).unwrap_or(0),
        lens.get(n / 2).copied().unwrap_or(0),
        lens.last().copied().unwrap_or(0),
        total as f64 / doc.content.len().max(1) as f64,
        pct(ends_at_break),
        pct(starts_mid_line),
        pct(odd_backticks),
        pct(broken_link),
        doc.embedding_dim
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen") => generate(&args[2], args[3].parse().expect("bytes"), Path::new(&args[4])),
        Some("run") => run(Path::new(&args[2]), &args[3], &args[4], &args[5]),
        _ => eprintln!(
            "usage: doc_measure gen <kind> <bytes> <out> | run <path> <mime> <xberg|linear> <none|embed|full>"
        ),
    }
}
