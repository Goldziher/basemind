//! The lead filter against a stand-in relay: what reaches the relay, what the host reads back.

use std::sync::Mutex as StdMutex;

use serde_json::json;

use super::*;

/// Answers the lead tools with their name, as the host would with a result.
struct Echo;

impl LeadCalls for Echo {
    fn call(self: Arc<Self>, name: String, arguments: Value) -> Pin<Box<dyn Future<Output = Value> + Send>> {
        Box::pin(async move { json!({ "answered_here": name, "arguments": arguments }) })
    }
}

fn on() -> Mode {
    Mode::On {
        calls: Arc::new(Echo),
        notice: "lead notice".to_owned(),
    }
}

fn off() -> Mode {
    Mode::Off {
        notice: "agent launching is off: no operator policy".to_owned(),
    }
}

/// A stand-in relay: records every line it is given, answers `initialize` and `tools/list` the way
/// the daemon does, and answers any other request with an unknown-tool error.
async fn stand_in(
    input: tokio::io::ReadHalf<DuplexStream>,
    mut output: tokio::io::WriteHalf<DuplexStream>,
    seen: Arc<StdMutex<Vec<Vec<u8>>>>,
) -> io::Result<()> {
    let mut input = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).await? == 0 {
            return output.shutdown().await;
        }
        seen.lock().unwrap().push(line.clone());
        let Ok(Value::Object(message)) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else { continue };
        let answer = match message.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({ "jsonrpc": "2.0", "id": id, "result": { "instructions": "basemind" } }),
            Some("tools/list") => {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [{ "name": "code_read" }] } })
            }
            _ => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": "tool not found" } }),
        };
        let mut bytes = serde_json::to_vec(&answer).unwrap();
        bytes.push(b'\n');
        output.write_all(&bytes).await?;
        output.flush().await?;
    }
}

/// Feed `lines` to the filter in `mode`, close the input, and return what reached the relay and
/// what the host read back, one JSON value per line.
async fn drive(mode: Mode, lines: Vec<Vec<u8>>) -> (Vec<Vec<u8>>, Vec<Value>) {
    let (mut host_write, filter_read) = tokio::io::duplex(1 << 20);
    let (filter_write, host_read) = tokio::io::duplex(1 << 20);
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let relay_seen = Arc::clone(&seen);
    let filter = tokio::spawn(run(filter_read, filter_write, mode, move |input, output| {
        stand_in(input, output, relay_seen)
    }));
    let reader = tokio::spawn(async move {
        let mut read = BufReader::new(host_read);
        let mut out = Vec::new();
        let mut line = String::new();
        while read.read_line(&mut line).await.unwrap() > 0 {
            out.push(serde_json::from_str(&line).unwrap());
            line.clear();
        }
        out
    });
    for line in lines {
        host_write.write_all(&line).await.unwrap();
    }
    // Local answers are written while the relay still runs: give them a moment before the input
    // closes and the relay ends.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(host_write);
    tokio::time::timeout(std::time::Duration::from_secs(10), filter)
        .await
        .expect("the filter ends with its input")
        .unwrap()
        .unwrap();
    let read = reader.await.unwrap();
    let seen = seen.lock().unwrap().clone();
    (seen, read)
}

fn line(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn by_id(read: &[Value], id: &Value) -> Value {
    read.iter()
        .find(|answer| answer["id"] == *id)
        .cloned()
        .unwrap_or(Value::Null)
}

fn call(id: u64, name: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": { "session": 1 } } })
}

#[tokio::test]
async fn a_lead_answers_its_tools_here_and_relays_everything_else_byte_for_byte() {
    let initialize = line(&json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {} }));
    // Spacing the relay would not produce: proof that a relayed line is not re-serialized.
    let verb =
        b"{ \"jsonrpc\":\"2.0\", \"id\":3, \"method\":\"tools/call\", \"params\":{\"name\":\"code_read\"} }\n".to_vec();
    let lines = vec![
        initialize.clone(),
        line(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
        line(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": { "cursor": "next" } })),
        verb.clone(),
        line(&call(4, "agent_launch")),
        line(&call(5, "agent_stop")),
    ];
    let (relayed, read) = drive(on(), lines).await;

    assert_eq!(relayed.len(), 4, "the lead tools never reach the relay");
    assert_eq!(relayed[0], initialize);
    assert_eq!(relayed[3], verb);
    assert!(
        relayed
            .iter()
            .all(|line| !String::from_utf8_lossy(line).contains("agent_"))
    );

    let instructions = by_id(&read, &json!(0))["result"]["instructions"].clone();
    assert_eq!(instructions, "basemind\nlead notice");
    let first: Vec<Value> = by_id(&read, &json!(1))["result"]["tools"].as_array().unwrap().clone();
    let names: Vec<&str> = first.iter().filter_map(|tool| tool["name"].as_str()).collect();
    assert_eq!(names, ["code_read", "agent_launch", "agent_stop"]);
    let paged = by_id(&read, &json!(2))["result"]["tools"].as_array().unwrap().len();
    assert_eq!(paged, 1, "a later page is not extended");
    assert_eq!(by_id(&read, &json!(4))["result"]["answered_here"], "agent_launch");
    assert_eq!(by_id(&read, &json!(5))["result"]["answered_here"], "agent_stop");
}

#[tokio::test]
async fn a_lead_that_could_not_start_lists_nothing_more_and_says_why() {
    let lines = vec![
        line(&json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {} })),
        line(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
        line(&call(2, "agent_launch")),
    ];
    let (relayed, read) = drive(off(), lines).await;
    assert_eq!(relayed.len(), 3, "the call goes to the daemon, which does not know it");
    let instructions = by_id(&read, &json!(0))["result"]["instructions"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        instructions.ends_with("agent launching is off: no operator policy"),
        "{instructions}"
    );
    assert_eq!(by_id(&read, &json!(1))["result"]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(by_id(&read, &json!(2))["error"]["message"], "tool not found");
}

/// Whatever the filter cannot read as one JSON object passes through untouched and nothing panics:
/// truncated JSON, a line over the inspection bound, bytes that are not UTF-8, nesting past
/// serde_json's limit, a batch (which carries a lead tool call to the daemon), and a last line
/// without its newline.
#[tokio::test]
async fn lines_the_filter_cannot_read_pass_through_unchanged() {
    let mut huge =
        br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"agent_launch","arguments":{"brief":""#
            .to_vec();
    huge.extend(std::iter::repeat_n(b'a', MAX_INSPECTED_BYTES));
    huge.extend(b"\"}}}\n");
    let mut nested = vec![b'['; 100_000];
    nested.push(b'\n');
    let lines = vec![
        br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agent_launch""#.to_vec(),
        b"\n".to_vec(),
        huge,
        b"{\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"agent_launch\",\"x\":\"\xff\xfe\"}}\n".to_vec(),
        nested,
        line(&json!([call(3, "agent_launch"), { "jsonrpc": "2.0", "id": 4, "method": "tools/list" }])),
        line(&json!("a string")),
        br#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"agent_stop"}}"#.to_vec(),
    ];
    let expected: Vec<Vec<u8>> = lines.iter().take(7).cloned().collect();
    let (relayed, read) = drive(on(), lines).await;
    // The truncated line and its newline arrive as one line; the rest are as sent.
    let mut sent = expected[0].clone();
    sent.extend(&expected[1]);
    assert_eq!(relayed[0], sent);
    assert_eq!(relayed[1..6], expected[2..7]);
    assert_eq!(relayed.len(), 6, "the unterminated lead call is still answered here");
    assert_eq!(by_id(&read, &json!(5))["result"]["answered_here"], "agent_stop");
}
