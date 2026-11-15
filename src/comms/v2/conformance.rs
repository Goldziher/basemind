//! The rules of ADR-0021 as scenarios any [`AgentOps`] must pass.
//!
//! Each scenario is a `pub fn` taking a fresh, empty [`ConformanceHarness`]. The in-memory
//! [`FakeAgents`](crate::comms::v2::fake::FakeAgents) passes them, and a store-backed implementation is
//! correct when it does too:
//!
//! ```ignore
//! mod conformance {
//!     basemind::conformance_tests!(MyStoreHarness::in_temp_dir());
//! }
//! ```
//!
//! Scenarios take every thread id, message id and cursor from a reply, compare timestamps only
//! with each other, and move time only through [`ConformanceHarness::advance`] and
//! [`ConformanceHarness::rewind`], so they hold for a store with random ids and a real layout.
//!
//! Only a sweep removes a record, and an implementation may run one after any call that changed
//! something. A scenario therefore calls [`ConformanceHarness::sweep`] before it asserts that a
//! record is gone, and asserts that an old record is still there only on the first call after
//! it moved the clock.

use std::time::{Duration, Instant};

use super::api::{
    Actor, AgentOps, Caller, CommsError, InboxBody, InboxReply, InboxSelect, MESSAGE_TTL, Mark, MessageId, PostBody,
    PostReply, PostTarget, Principal, ReadKind, RepoKey, ThreadAction, ThreadFilter, ThreadId, ThreadQuery,
    ThreadSummary, ThreadUpdate, ThreadsReply, WaitOutcome,
};
use crate::path::RelPath;

pub mod inbox;
pub mod post;
pub mod retention;
pub mod threads;

pub use inbox::*;
pub use post::*;
pub use retention::*;
pub use threads::*;

/// The two repositories the scenarios work in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repo {
    /// Where almost everything happens.
    Home,
    /// A second repository, for the rules about crossing one.
    Other,
}

/// What a scenario needs beyond [`AgentOps`]: a clock it can move and a view of what the store
/// holds. `Self` must be a fresh, empty store at
/// [`COMMS_STORE_VER`](super::api::COMMS_STORE_VER) each time a scenario starts.
pub trait ConformanceHarness: AgentOps {
    /// Move the store's clock forward: what posts are stamped from and a sweep judges age by.
    fn advance(&self, by: Duration);

    /// Move the store's clock back, the way a clock that ran ahead is corrected.
    fn rewind(&self, by: Duration);

    /// Run the implementation's sweep until it has nothing left to remove at the clock's
    /// reading. Does nothing on a store newer than the binary.
    fn sweep(&self);

    /// `(read_seq, notified_seq)` of `actor` in `thread`: its marks, or where it starts when it
    /// has none yet. `None` when its principal is not a member.
    fn position(&self, thread: &ThreadId, actor: &Actor) -> Option<(u64, u64)>;

    /// Make the store report layout version `version`, as if another binary had written it.
    fn set_store_version(&self, version: u32);

    /// Number of threads the store holds, archived ones included.
    fn thread_count(&self) -> usize;

    /// The caller a scenario's agent `name` is, working in `repo`, without a replay key.
    ///
    /// Scenarios use short lowercase names (`a`, `b`, ...) and sub-identities of them, such as
    /// `b.helper`.
    /// Distinct names must give distinct actors, the same name the same actor in both
    /// repositories, and the two repositories distinct keys. The default satisfies that.
    fn caller(&self, name: &str, repo: Repo) -> Caller {
        default_caller(name, repo)
    }
}

/// The caller [`ConformanceHarness::caller`] makes by default: the actor `name`, in
/// `github.com/acme/app` for [`Repo::Home`] and `path:/work/elsewhere` for [`Repo::Other`].
pub fn default_caller(name: &str, repo: Repo) -> Caller {
    let key = match repo {
        Repo::Home => "github.com/acme/app",
        Repo::Other => "path:/work/elsewhere",
    };
    Caller::new(
        Actor::parse(name).expect("scenario agent names are valid ids"),
        RepoKey::parse(key).expect("scenario repository keys are valid"),
    )
}

/// How often [`wait_then_read`] looks at [`AgentOps::arrivals`] while it waits.
const WAIT_POLL: Duration = Duration::from_millis(1);

/// An inbox read that waits, the way the daemon runs one on top of the non-blocking
/// [`AgentOps::inbox`].
///
/// It probes with a read that marks nothing; while that finds nothing and `timeout` has not run
/// out, it waits for [`AgentOps::arrivals`] to change and probes again; then it makes the real
/// read once and reports how the wait ended. `while_waiting` runs once, after the first probe
/// came back empty, so a test can post "during" the wait without a second thread. Only a read
/// that marks as read and a `peek` wait; any other kind is read at once.
///
/// The daemon's version differs in two ways the store cannot see: it registers the replay key
/// before waiting, and it checks that the client is still connected before the real read.
pub fn wait_then_read<O: AgentOps + ?Sized>(
    ops: &O,
    caller: &Caller,
    body: InboxBody,
    timeout: Duration,
    while_waiting: impl FnOnce(),
) -> Result<InboxReply, CommsError> {
    if !matches!(body.kind()?, ReadKind::Marking | ReadKind::Peek) {
        return ops.inbox(caller, body);
    }
    let probe_caller = Caller::new(caller.actor.clone(), caller.repo.clone());
    let probe = InboxBody {
        mark: Mark::Peek,
        limit: Some(1),
        cursor: None,
        ..body.clone()
    };
    let deadline = Instant::now() + timeout;
    let mut while_waiting = Some(while_waiting);
    let mut waited = false;
    let outcome = loop {
        let arrivals = ops.arrivals();
        if !ops.inbox(&probe_caller, probe.clone())?.messages.is_empty() {
            break if waited {
                WaitOutcome::Arrived
            } else {
                WaitOutcome::NotWaited
            };
        }
        if timeout.is_zero() {
            break WaitOutcome::NotWaited;
        }
        if Instant::now() >= deadline {
            break WaitOutcome::TimedOut;
        }
        waited = true;
        if let Some(while_waiting) = while_waiting.take() {
            while_waiting();
        }
        while ops.arrivals() == arrivals && Instant::now() < deadline {
            std::thread::sleep(WAIT_POLL);
        }
    };
    let mut reply = ops.inbox(caller, body)?;
    reply.wait = outcome;
    Ok(reply)
}

/// The text every scenario signature contains, which is how [`scenario_count`] finds them.
const SCENARIO_SIGNATURE: &str = "(h: &impl ConformanceHarness)";

/// Number of scenarios defined in this module's files. [`conformance_tests!`](crate::conformance_tests)
/// compares it with the number of tests it emits, so a scenario cannot be written and never run.
pub fn scenario_count() -> usize {
    [
        include_str!("conformance/inbox.rs"),
        include_str!("conformance/post.rs"),
        include_str!("conformance/retention.rs"),
        include_str!("conformance/threads.rs"),
    ]
    .iter()
    .map(|source| source.matches(SCENARIO_SIGNATURE).count())
    .sum()
}

/// Emit one `#[test]` per conformance scenario, each run against a fresh `$harness`.
///
/// `$harness` is an expression building an empty [`ConformanceHarness`]; it is evaluated once
/// per test. Invoke it inside a module of its own, since the test names are the scenario names.
#[macro_export]
macro_rules! conformance_tests {
    ($harness:expr) => {
        $crate::conformance_tests!(@emit $harness;
            creating_post_puts_creator_and_recipients_at_position_zero,
            subject_is_not_a_key_so_two_posts_make_two_threads,
            pending_lists_only_recipients_that_made_no_call,
            being_named_in_to_does_not_make_an_agent_seen,
            failed_call_does_not_make_an_agent_seen,
            seen_record_is_swept_thirty_to_thirty_one_days_after_the_last_call,
            agent_that_keeps_calling_stays_seen,
            marks_go_with_the_seen_record_and_a_returning_actor_starts_again,
            posting_requires_membership,
            sub_identity_acts_in_the_threads_of_its_principal,
            naming_a_sub_identity_makes_its_principal_the_member,
            reply_goes_into_the_thread_of_the_message_it_answers,
            invalid_posts_are_refused_before_anything_is_stored,
            body_is_capped_at_sixteen_kibibytes,
            recipients_and_paths_are_capped,
            to_naming_only_the_creator_is_refused,
            replayed_post_into_a_thread_returns_the_first_reply,
            post_without_a_replay_key_always_makes_a_new_message,
            replayed_creating_post_returns_the_thread_already_created,
            same_replay_key_in_another_thread_is_a_new_post,
            replayed_reply_returns_the_first_reply,
            arrivals_increase_with_every_stored_post,
            recipient_that_never_connected_finds_the_thread_on_its_first_inbox,
            reading_advances_read_seq_to_the_last_message_returned,
            peek_returns_messages_without_moving_any_position,
            notify_announces_front_matter_once_and_leaves_messages_unread,
            own_posts_are_never_unread,
            posts_of_a_sibling_actor_are_unread,
            new_actor_starts_where_its_principal_has_read_to,
            history_read_returns_everything_and_moves_no_position,
            inbox_spans_threads_in_post_order,
            thread_selection_reads_one_thread_only,
            reading_requires_membership,
            inbox_on_an_unknown_thread_is_refused,
            long_body_is_returned_cut_and_counts_as_read,
            reading_by_id_returns_the_whole_body_and_moves_no_position,
            reading_by_id_refuses_fields_that_do_not_apply,
            reading_an_unknown_message_id_is_refused,
            response_budget_counts_more_than_the_body_and_never_skips,
            limit_cuts_the_page_and_the_rest_stays_unread,
            limit_is_clamped_to_one_and_to_the_maximum,
            read_that_marks_takes_no_cursor,
            cursor_pages_a_peek_without_marking,
            cursor_pages_history_across_threads,
            cursor_is_bound_to_actor_selection_and_mode,
            replayed_inbox_read_returns_the_range_first_delivered,
            replay_key_of_an_empty_read_is_not_remembered,
            inbox_replay_key_is_scoped_to_its_agent,
            limit_across_threads_never_skips_an_unread_message,
            wait_returns_when_a_message_arrives,
            wait_times_out_when_nothing_arrives,
            wait_ignores_the_callers_own_posts,
            wait_is_skipped_when_something_is_unread,
            no_arguments_lists_the_callers_threads_and_their_peers,
            summary_counts_unread_messages_from_others,
            query_discovers_open_threads_of_the_callers_repository_only,
            query_also_filters_the_callers_own_threads,
            path_discovers_threads_about_a_prefix_or_something_under_it,
            path_also_filters_the_callers_own_threads,
            query_and_path_together_narrow,
            thread_paths_are_stored_normalised_and_bad_ones_are_refused,
            describing_one_thread_needs_membership_or_an_open_thread_here,
            archived_threads_are_listed_only_on_request,
            listing_is_ordered_by_the_newest_post_newest_first,
            thread_listing_pages_by_cursor,
            joining_an_open_thread_of_the_callers_repository_starts_at_the_head,
            joining_again_keeps_the_read_position,
            joining_is_refused_for_invitation_only_and_foreign_threads,
            any_member_may_invite_and_the_invited_start_at_the_head,
            inviting_requires_membership_and_someone_to_invite,
            invitation_crosses_repositories,
            leaving_ends_membership,
            only_the_creator_may_archive,
            creator_who_left_may_still_archive,
            last_member_leaving_archives_the_thread,
            membership_changes_are_idempotent_under_replay,
            archived_thread_is_read_by_naming_it_and_closed_to_posts_joins_and_invitations,
            creating_a_thread_at_the_cap_archives_the_least_recently_active,
            members_are_capped_per_thread,
            sweep_drops_messages_outside_the_newest_of_their_thread,
            record_past_its_retention_is_returned_until_a_sweep_removes_it,
            sweep_removes_old_messages_inside_a_live_thread,
            cursor_pages_history_after_a_sweep_removed_the_oldest_messages,
            archived_thread_is_swept_like_any_other,
            forward_clock_jump_pins_only_the_threads_posted_to_meanwhile,
            sweep_under_a_clock_far_ahead_removes_everything_for_good,
            store_newer_than_the_binary_is_refused_for_writing_only,
            nothing_is_recorded_or_swept_on_a_newer_store,
        );
    };
    (@emit $harness:expr; $($scenario:ident),+ $(,)?) => {
        $(
            #[test]
            fn $scenario() {
                let harness = $harness;
                $crate::comms::v2::conformance::$scenario(&harness);
            }
        )+

        #[test]
        fn every_scenario_is_instantiated() {
            let emitted = [$(stringify!($scenario)),+].len();
            assert_eq!(emitted, $crate::comms::v2::conformance::scenario_count());
        }
    };
}

fn who<H: ConformanceHarness>(harness: &H, name: &str) -> Caller {
    harness.caller(name, Repo::Home)
}

fn elsewhere<H: ConformanceHarness>(harness: &H, name: &str) -> Caller {
    harness.caller(name, Repo::Other)
}

fn agent<H: ConformanceHarness>(harness: &H, name: &str) -> Actor {
    who(harness, name).actor
}

fn agents<H: ConformanceHarness>(harness: &H, names: &[&str]) -> Vec<Actor> {
    names.iter().map(|name| agent(harness, name)).collect()
}

/// The principals of the agents `names`: what a thread lists as its members.
fn principals<H: ConformanceHarness>(harness: &H, names: &[&str]) -> Vec<Principal> {
    names.iter().map(|name| agent(harness, name).principal_id()).collect()
}

fn new_thread<H: ConformanceHarness>(harness: &H, subject: &str, to: &[&str], paths: &[&str], body: &str) -> PostBody {
    PostBody {
        target: PostTarget::NewThread {
            subject: subject.to_string(),
            to: agents(harness, to),
            paths: paths.iter().map(|path| RelPath::from(*path)).collect(),
        },
        body: body.to_string(),
    }
}

/// Start a thread in [`Repo::Home`] with the body `first`.
#[track_caller]
fn start<H: ConformanceHarness>(harness: &H, creator: &str, subject: &str, to: &[&str]) -> PostReply {
    harness
        .post(&who(harness, creator), new_thread(harness, subject, to, &[], "first"))
        .expect("thread created")
}

fn into(thread: &ThreadId, body: &str) -> PostBody {
    PostBody {
        target: PostTarget::Thread(thread.clone()),
        body: body.to_string(),
    }
}

fn say<H: ConformanceHarness>(harness: &H, name: &str, thread: &ThreadId, body: &str) -> Result<PostReply, CommsError> {
    harness.post(&who(harness, name), into(thread, body))
}

/// The tool's default read: every thread, unread, marking as read.
#[track_caller]
fn read<H: ConformanceHarness>(harness: &H, name: &str) -> InboxReply {
    harness.inbox(&who(harness, name), InboxBody::default()).expect("inbox")
}

fn read_as<H: ConformanceHarness>(harness: &H, name: &str, body: InboxBody) -> Result<InboxReply, CommsError> {
    harness.inbox(&who(harness, name), body)
}

fn peek() -> InboxBody {
    InboxBody {
        mark: Mark::Peek,
        ..InboxBody::default()
    }
}

fn history() -> InboxBody {
    InboxBody {
        unread: false,
        ..InboxBody::default()
    }
}

fn notify() -> InboxBody {
    InboxBody {
        mark: Mark::Notify,
        ..InboxBody::default()
    }
}

fn only(thread: &ThreadId) -> InboxBody {
    InboxBody {
        select: InboxSelect::Thread(thread.clone()),
        ..InboxBody::default()
    }
}

fn by_id(id: &MessageId) -> InboxBody {
    InboxBody {
        select: InboxSelect::Message(id.clone()),
        ..InboxBody::default()
    }
}

fn bodies(reply: &InboxReply) -> Vec<&str> {
    reply.messages.iter().map(|message| message.body.as_str()).collect()
}

fn ids(reply: &InboxReply) -> Vec<MessageId> {
    reply.messages.iter().map(|message| message.id.clone()).collect()
}

/// The tool with no arguments.
#[track_caller]
fn list<H: ConformanceHarness>(harness: &H, name: &str) -> ThreadsReply {
    harness
        .threads_query(&who(harness, name), ThreadQuery::default())
        .expect("threads")
}

fn find<H: ConformanceHarness>(harness: &H, caller: &Caller, filter: ThreadFilter) -> Result<ThreadsReply, CommsError> {
    harness.threads_query(caller, ThreadQuery::List(filter))
}

fn describe<H: ConformanceHarness>(
    harness: &H,
    caller: &Caller,
    thread: &ThreadId,
) -> Result<ThreadSummary, CommsError> {
    let mut reply = harness.threads_query(caller, ThreadQuery::Describe(thread.clone()))?;
    assert_eq!(reply.threads.len(), 1, "describing one thread returns one summary");
    Ok(reply.threads.remove(0))
}

/// Apply `action` to `thread` and return the summary of the thread as changed.
fn change<H: ConformanceHarness>(
    harness: &H,
    caller: &Caller,
    thread: &ThreadId,
    action: ThreadAction,
) -> Result<ThreadsReply, CommsError> {
    let reply = harness.threads_update(
        caller,
        ThreadUpdate {
            thread: thread.clone(),
            action,
        },
    )?;
    assert_eq!(reply.threads.len(), 1, "an update returns the one thread changed");
    assert_eq!(&reply.threads[0].id, thread);
    Ok(reply)
}

fn thread_ids(reply: &ThreadsReply) -> Vec<ThreadId> {
    reply.threads.iter().map(|summary| summary.id.clone()).collect()
}

/// Let time pass between two posts whose order a scenario asserts, so a store with a coarse
/// clock cannot stamp them alike.
fn tick<H: ConformanceHarness>(harness: &H) {
    harness.advance(Duration::from_secs(1));
}

/// The id of a thread that existed and was swept: an id the store does not hold, taken from a
/// reply rather than invented. It moves the clock past [`MESSAGE_TTL`], so scenarios call it
/// before anything else.
#[track_caller]
fn gone_thread<H: ConformanceHarness>(harness: &H) -> PostReply {
    let gone = start(harness, "ghost", "short-lived", &[]);
    harness.advance(MESSAGE_TTL + Duration::from_secs(1));
    harness.sweep();
    assert_eq!(harness.thread_count(), 0, "a sweep removes a thread past its retention");
    gone
}
