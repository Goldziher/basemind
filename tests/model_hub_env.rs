//! The Hugging Face model downloads under `HF_ENDPOINT` and `BASEMIND_OFFLINE` (#208).
//!
//! A local listener stands in for the hub, so the test never reaches the network: with
//! `HF_ENDPOINT` pointing at it, a model missing from an empty cache is requested from it; once
//! `BASEMIND_OFFLINE` is exported to the hub, a missing model is an error naming it and the listener
//! is not contacted again.
//!
//! Each probe runs in a child process of this test binary, with its environment set by `Command`
//! before it starts: the embedding library keeps worker threads, and the environment is not safe to
//! change while another thread may read it.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

use basemind::embeddings::SharedEmbedder;

const MODEL_REPO: &str = "xberg-io/embedding-models";

/// Answer every request with a 404 and record its request line.
fn serve_not_found(listener: TcpListener) -> Arc<Mutex<Vec<String>>> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone the connection"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            seen.lock().expect("requests").push(line.trim_end().to_string());
            let mut header = String::new();
            while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
                header.clear();
            }
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    requests
}

/// Run [`probe`] in a child of this binary with `env` set and the hub variables otherwise cleared.
fn run_probe(mode: &str, endpoint: &str, cache: &std::path::Path, offline: bool) {
    let mut child = Command::new(std::env::current_exe().expect("this test binary"));
    child
        .args(["--exact", "probe", "--ignored", "--nocapture", "--test-threads=1"])
        .env("PROBE_MODE", mode)
        .env("HF_ENDPOINT", endpoint)
        .env("HF_HUB_CACHE", cache)
        .env("XBERG_MODEL_DOWNLOAD_TIMEOUT_SECS", "30")
        .env_remove("HF_HUB_OFFLINE")
        .env_remove("HUGGINGFACE_HUB_OFFLINE")
        .env_remove("BASEMIND_OFFLINE");
    if offline {
        child.env("BASEMIND_OFFLINE", "1");
    }
    let output = child.output().expect("run the probe");
    assert!(
        output.status.success(),
        "the {mode} probe failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hf_endpoint_redirects_the_model_download_and_basemind_offline_stops_it() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stand-in hub");
    let endpoint = format!("http://{}", listener.local_addr().expect("listener address"));
    let cache = tempfile::tempdir().expect("empty model cache");
    let requests = serve_not_found(listener);

    run_probe("online", &endpoint, cache.path(), false);
    let asked = requests.lock().expect("requests").clone();
    assert!(
        asked
            .iter()
            .any(|line| line.contains(&format!("/{MODEL_REPO}/resolve/"))),
        "the model was requested from HF_ENDPOINT: {asked:?}"
    );

    run_probe("offline", &endpoint, cache.path(), true);
    assert_eq!(
        requests.lock().expect("requests").len(),
        asked.len(),
        "offline, the hub is not contacted"
    );
}

/// The body of each child process; ignored so the suite never runs it directly.
#[test]
#[ignore = "run by hf_endpoint_redirects_the_model_download_and_basemind_offline_stops_it"]
fn probe() {
    match std::env::var("PROBE_MODE").as_deref() {
        Ok("online") => {
            let online = SharedEmbedder::load("fast", 1, 1).expect("the fast preset exists");
            assert!(online.embed("hello").is_err(), "the stand-in hub serves no model");
        }
        Ok("offline") => {
            // SAFETY: nothing else runs in this child yet: no embedder is loaded, so no library
            // thread exists to read the environment while it is changed.
            unsafe { basemind::cli::env_config::export_model_hub_offline() };
            assert_eq!(std::env::var("HF_HUB_OFFLINE").as_deref(), Ok("1"));
            let offline = SharedEmbedder::load("balanced", 1, 1).expect("the balanced preset exists");
            let error = offline.embed("hello").expect_err("no model is cached");
            let message = format!("{error:#}");
            assert!(message.contains("offline mode"), "{message}");
            assert!(message.contains(MODEL_REPO), "{message}");
            assert!(basemind::embeddings::is_load_failure(&error), "{message}");
        }
        other => panic!("PROBE_MODE is not set by the parent test: {other:?}"),
    }
}
