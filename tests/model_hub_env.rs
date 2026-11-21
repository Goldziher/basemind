//! The Hugging Face model downloads under `HF_ENDPOINT` and `BASEMIND_OFFLINE` (#208).
//!
//! A local listener stands in for the hub, so the test never reaches the network: with
//! `HF_ENDPOINT` pointing at it, a model missing from an empty cache is requested from it; once
//! `BASEMIND_OFFLINE` is exported to the hub, a missing model is an error naming it and the listener
//! is not contacted again.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
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

/// Deliberately the only test function in this binary: it sets process environment variables,
/// which is unsound while another thread reads the environment.
#[test]
fn hf_endpoint_redirects_the_model_download_and_basemind_offline_stops_it() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stand-in hub");
    let endpoint = format!("http://{}", listener.local_addr().expect("listener address"));
    let cache = tempfile::tempdir().expect("empty model cache");
    // SAFETY: the only test in this binary, and no thread of the library runs yet.
    unsafe {
        std::env::set_var("HF_ENDPOINT", &endpoint);
        std::env::set_var("HF_HUB_CACHE", cache.path());
        std::env::set_var("XBERG_MODEL_DOWNLOAD_TIMEOUT_SECS", "30");
        std::env::remove_var("HF_HUB_OFFLINE");
        std::env::remove_var("HUGGINGFACE_HUB_OFFLINE");
        std::env::remove_var("BASEMIND_OFFLINE");
    }
    let requests = serve_not_found(listener);

    let online = SharedEmbedder::load("fast", 1, 1).expect("the fast preset exists");
    assert!(online.embed("hello").is_err(), "the stand-in hub serves no model");
    let asked = requests.lock().expect("requests").clone();
    assert!(
        asked
            .iter()
            .any(|line| line.contains(&format!("/{MODEL_REPO}/resolve/"))),
        "the model was requested from HF_ENDPOINT: {asked:?}"
    );

    // SAFETY: the only test in this binary, and the first embed has returned: the library's threads
    // are idle, so none reads the environment meanwhile.
    unsafe {
        std::env::set_var("BASEMIND_OFFLINE", "1");
        basemind::cli::env_config::export_model_hub_offline();
    }
    assert_eq!(std::env::var("HF_HUB_OFFLINE").as_deref(), Ok("1"));
    let offline = SharedEmbedder::load("balanced", 1, 1).expect("the balanced preset exists");
    let error = offline.embed("hello").expect_err("no model is cached");
    let message = format!("{error:#}");
    assert!(message.contains("offline mode"), "{message}");
    assert!(message.contains(MODEL_REPO), "{message}");
    assert!(basemind::embeddings::is_load_failure(&error), "{message}");
    assert_eq!(
        requests.lock().expect("requests").len(),
        asked.len(),
        "offline, the hub is not contacted"
    );
}
