//! What is true of all three agent tools, and the fixtures their own tests share.

use std::cell::Cell;
use std::fmt::Debug;
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{ContentBlock, ErrorCode, Tool};
use serde_json::{Value, json};

use super::{
    AGENT_INBOX, AGENT_INBOX_DESCRIPTION, AGENT_POST, AGENT_POST_DESCRIPTION, AGENT_THREADS, AGENT_THREADS_DESCRIPTION,
    AGENT_TOOLS, AGENT_TOOLS_BYTE_CEILING, AgentInboxParams, AgentInboxResponse, AgentPostParams, AgentPostResponse,
    AgentSession, AgentThreadsParams, AgentThreadsResponse, refusal, run_agent_inbox, run_agent_post,
    run_agent_threads,
};
use crate::comms::v2::api::{
    Actor, AgentOps, Caller, CommsError, InboxBody, InboxReply, MessageId, PostBody, PostReply, Principal, RepoKey,
    ThreadId, ThreadQuery, ThreadUpdate, ThreadsReply, WaitOutcome,
};
use crate::comms::v2::fake::FakeAgents;
use crate::mcp::strict::{ToolArgs, parse};
use crate::mcp::tool_contract::into_tool_result;

/// The repository the scenario agents work in.
pub(super) const HOME: &str = "github.com/acme/app";

/// One session: a principal in a repository, with no replay key and no notices of its own.
pub(super) struct Agent {
    principal: Principal,
    repo: RepoKey,
}

impl Agent {
    /// A session of `name` in [`HOME`].
    pub(super) fn new(name: &str) -> Self {
        Self::in_repo(name, HOME)
    }

    pub(super) fn in_repo(name: &str, repo: &str) -> Self {
        Self {
            principal: Principal::parse(name).expect("a valid principal"),
            repo: RepoKey::parse(repo).expect("a valid repository key"),
        }
    }

    pub(super) fn session(&self) -> AgentSession<'_> {
        AgentSession {
            principal: &self.principal,
            repo: &self.repo,
            replay_key: None,
            notices: &[],
        }
    }

    /// Call `agent_post` with `arguments` as a client would send them.
    pub(super) fn post(&self, fake: &FakeAgents, arguments: Value) -> Result<AgentPostResponse, McpError> {
        run_agent_post(fake, &self.session(), args(arguments))
    }

    /// Call `agent_inbox` with `arguments`. The call must not ask to wait: the hook panics, which
    /// also holds every test made through here to "no wait asked, no hook called".
    pub(super) fn inbox(&self, fake: &FakeAgents, arguments: Value) -> Result<AgentInboxResponse, McpError> {
        run_agent_inbox(fake, &self.session(), args(arguments), &never_waits)
    }

    /// Call `agent_threads` with `arguments`.
    pub(super) fn threads(&self, fake: &FakeAgents, arguments: Value) -> Result<AgentThreadsResponse, McpError> {
        run_agent_threads(fake, &self.session(), args(arguments))
    }
}

/// The wait hook of a call that asks for no wait.
pub(super) fn never_waits(_: &Caller, _: InboxBody, wait: Duration) -> Result<InboxReply, CommsError> {
    panic!("the wait hook was called with {wait:?} by a call that asked for no wait")
}

/// `arguments` through the tool's own strict parse, as the shim hands them to the body.
pub(super) fn args<T: ToolArgs>(arguments: Value) -> T {
    parse::<T>(arguments).unwrap_or_else(|message| panic!("the arguments were refused: {message}"))
}

/// The message the strict parse refuses `arguments` with.
pub(super) fn parse_error<T: ToolArgs + Debug>(arguments: Value) -> String {
    match parse::<T>(arguments) {
        Ok(params) => panic!("expected the parse to refuse, got {params:?}"),
        Err(message) => message,
    }
}

/// The message a tool body refuses with.
pub(super) fn refused<T: Debug>(result: Result<T, McpError>) -> String {
    match result {
        Ok(response) => panic!("expected a refusal, got {response:?}"),
        Err(error) => error.message.into_owned(),
    }
}

pub(super) fn thread(id: &str) -> ThreadId {
    ThreadId::parse(id).expect("a valid thread id")
}

pub(super) fn message(id: &str) -> MessageId {
    MessageId::parse(id).expect("a valid message id")
}

pub(super) fn actor(id: &str) -> Actor {
    Actor::parse(id).expect("a valid agent id")
}

/// The example call a description ends with.
fn example(description: &str) -> Value {
    assert!(
        description.starts_with("Use "),
        "must open with when to use it: {description}"
    );
    let (_, example) = description
        .split_once("Example: ")
        .unwrap_or_else(|| panic!("no `Example: {{…}}` in: {description}"));
    serde_json::from_str(example).unwrap_or_else(|error| panic!("the example is not JSON ({error}): {example}"))
}

/// A store whose every reply carries a notice, as the daemon's do while an identity is shared.
struct Noticing(FakeAgents);

const DAEMON_NOTICE: &str = "2 live sessions share the identity alice";

impl AgentOps for Noticing {
    fn post(&self, caller: &Caller, body: PostBody) -> Result<PostReply, CommsError> {
        let mut reply = self.0.post(caller, body)?;
        reply.notices.push(DAEMON_NOTICE.to_string());
        Ok(reply)
    }

    fn inbox(&self, caller: &Caller, body: InboxBody) -> Result<InboxReply, CommsError> {
        let mut reply = self.0.inbox(caller, body)?;
        reply.notices.push(DAEMON_NOTICE.to_string());
        Ok(reply)
    }

    fn threads_query(&self, caller: &Caller, query: ThreadQuery) -> Result<ThreadsReply, CommsError> {
        let mut reply = self.0.threads_query(caller, query)?;
        reply.notices.push(DAEMON_NOTICE.to_string());
        Ok(reply)
    }

    fn threads_update(&self, caller: &Caller, update: ThreadUpdate) -> Result<ThreadsReply, CommsError> {
        let mut reply = self.0.threads_update(caller, update)?;
        reply.notices.push(DAEMON_NOTICE.to_string());
        Ok(reply)
    }

    fn arrivals(&self) -> u64 {
        self.0.arrivals()
    }
}

/// The three tools as `tools/list` will carry them: name, description and input schema.
fn advertised() -> Vec<Tool> {
    let schema = |schema: Result<std::sync::Arc<rmcp::model::JsonObject>, String>, tool: &str| {
        schema.unwrap_or_else(|error| panic!("`{tool}` has no input schema: {error}"))
    };
    vec![
        Tool::new(
            AGENT_POST,
            AGENT_POST_DESCRIPTION,
            schema(schema_for_input::<AgentPostParams>(), AGENT_POST),
        ),
        Tool::new(
            AGENT_INBOX,
            AGENT_INBOX_DESCRIPTION,
            schema(schema_for_input::<AgentInboxParams>(), AGENT_INBOX),
        ),
        Tool::new(
            AGENT_THREADS,
            AGENT_THREADS_DESCRIPTION,
            schema(schema_for_input::<AgentThreadsParams>(), AGENT_THREADS),
        ),
    ]
}

/// Bytes of `tools` in a `tools/list` reply: the measure `tests/mcp_schema_wire.rs` holds the verb
/// tools to.
fn wire_bytes(tools: &[Tool]) -> usize {
    serde_json::to_string(tools).expect("serialize the tools").len()
}

#[test]
fn the_three_tools_fit_their_byte_ceiling_as_tools_list_serializes_them() {
    let tools = advertised();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(names, AGENT_TOOLS);

    for tool in &tools {
        let properties = tool
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .map_or(0, |properties| properties.len());
        assert!(properties >= 7, "`{}` advertises {properties} fields", tool.name);
        let schema = serde_json::to_string(&*tool.input_schema).expect("serialize the schema");
        for keyword in ["$ref", "$defs", "oneOf", "anyOf", "allOf", "\"default\""] {
            assert!(
                !schema.contains(keyword),
                "`{}` schema uses `{keyword}`: {schema}",
                tool.name
            );
        }
    }

    let total = wire_bytes(&tools);
    println!("agent tools: {total} bytes in tools/list");
    assert!(
        total <= AGENT_TOOLS_BYTE_CEILING,
        "the agent tools take {total} bytes, over the {AGENT_TOOLS_BYTE_CEILING}-byte ceiling of ADR-0021: \
         shorten a description or a field doc"
    );
    assert!(
        total > AGENT_TOOLS_BYTE_CEILING / 2,
        "{total} bytes is too few to be three tools"
    );
}

#[test]
fn the_byte_measure_counts_every_byte_of_a_description() {
    let mut tools = advertised();
    let before = wire_bytes(&tools);
    let padding = AGENT_TOOLS_BYTE_CEILING - before + 1;
    let padded = format!("{AGENT_POST_DESCRIPTION}{}", "x".repeat(padding));

    tools[0].description = Some(padded.into());

    assert_eq!(wire_bytes(&tools), AGENT_TOOLS_BYTE_CEILING + 1);
}

#[test]
fn every_schema_requires_only_what_the_tool_cannot_run_without() {
    let required = |schema: Result<std::sync::Arc<rmcp::model::JsonObject>, String>| {
        schema.expect("an input schema").get("required").cloned()
    };

    assert_eq!(required(schema_for_input::<AgentPostParams>()), Some(json!(["body"])));
    assert_eq!(required(schema_for_input::<AgentInboxParams>()), None);
    assert_eq!(required(schema_for_input::<AgentThreadsParams>()), None);
}

#[test]
fn every_description_opens_with_its_use_and_ends_with_a_call_the_tool_runs() {
    let fake = FakeAgents::new();
    let (alice, bob) = (Agent::new("alice"), Agent::new("bob"));

    let posted = alice
        .post(&fake, example(AGENT_POST_DESCRIPTION))
        .expect("the agent_post example");
    assert_eq!(posted.id, message("t-aaaaaa.1"));

    let asked = Cell::new(None);
    let waiting_read = |caller: &Caller, body: InboxBody, wait: Duration| -> Result<InboxReply, CommsError> {
        asked.set(Some(wait));
        fake.inbox(caller, body)
    };
    run_agent_inbox(
        &fake,
        &alice.session(),
        args(example(AGENT_INBOX_DESCRIPTION)),
        &waiting_read,
    )
    .expect("the agent_inbox example");
    assert_eq!(asked.get(), Some(Duration::from_secs(30)));

    fake.queue_thread_bits([thread("t-abcdef").bits()]);
    bob.post(&fake, json!({ "subject": "open to all", "body": "hello" }))
        .expect("the thread the example names");
    let joined = alice
        .threads(&fake, example(AGENT_THREADS_DESCRIPTION))
        .expect("the agent_threads example");
    assert_eq!(joined.threads[0].id, thread("t-abcdef"));
    assert!(joined.threads[0].member);
}

#[test]
fn a_refusal_is_an_is_error_result_that_names_the_tool_and_the_fix() {
    let fake = FakeAgents::new();
    let error = Agent::new("alice")
        .post(&fake, json!({ "body": "hi" }))
        .expect_err("a post with no target");
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    let result = into_tool_result(Err(error));

    assert_eq!(result.is_error, Some(true));
    let text = match result.content.first() {
        Some(ContentBlock::Text(text)) => text.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert_eq!(
        text,
        "`agent_post`: pass exactly one of thread, reply_to, subject; this call set []: `subject` starts a thread, \
         the others post into one"
    );
}

#[test]
fn a_store_failure_is_an_internal_error_and_not_the_callers_mistake() {
    let failed = refusal(
        AGENT_INBOX,
        &CommsError::Backend {
            detail: "disk full".to_string(),
        },
    );
    assert_eq!(failed.code, ErrorCode::INTERNAL_ERROR);
    assert_eq!(
        failed.message,
        "`agent_inbox`: the comms store failed: disk full; retry, and run `basemind agent status` if it persists"
    );

    let newer = refusal(
        AGENT_POST,
        &CommsError::StoreNewer {
            store: 1001,
            binary: 1000,
        },
    );
    assert_eq!(newer.code, ErrorCode::INTERNAL_ERROR);
}

#[test]
fn the_tool_layers_notices_follow_the_daemons_in_every_reply() {
    let store = Noticing(FakeAgents::new());
    let alice = Agent::new("alice");
    let rejected = ["the configured agent id \"a.b\" is not a valid id and is not used".to_string()];
    let session = AgentSession {
        notices: &rejected,
        ..alice.session()
    };
    let expected = vec![DAEMON_NOTICE.to_string(), rejected[0].clone()];

    let posted = run_agent_post(&store, &session, args(json!({ "subject": "topic", "body": "hi" }))).expect("post");
    assert_eq!(posted.notices, expected);

    let read = run_agent_inbox(&store, &session, args(json!({})), &never_waits).expect("inbox");
    assert_eq!(read.notices, expected);

    let listed = run_agent_threads(&store, &session, args(json!({}))).expect("threads");
    assert_eq!(listed.notices, expected);
    assert_eq!(listed.threads.len(), 1);
}

#[test]
fn a_waiting_reply_keeps_the_notices_and_the_outcome_the_daemon_set() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");
    let waiting_read = |caller: &Caller, body: InboxBody, _: Duration| -> Result<InboxReply, CommsError> {
        let mut reply = fake.inbox(caller, body)?;
        reply.notices.push(DAEMON_NOTICE.to_string());
        reply.wait = WaitOutcome::TimedOut;
        Ok(reply)
    };

    let read = run_agent_inbox(&fake, &alice.session(), args(json!({ "wait_secs": 1 })), &waiting_read).expect("inbox");

    assert_eq!(read.notices, [DAEMON_NOTICE]);
    assert_eq!(read.wait, WaitOutcome::TimedOut);
    assert_eq!(
        serde_json::to_value(&read).expect("serialize"),
        json!({ "messages": [], "more": false, "wait": "timed_out", "notices": [DAEMON_NOTICE] })
    );
}
