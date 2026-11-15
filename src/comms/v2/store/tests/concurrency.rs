//! Posts from many threads at once on one store.

use super::*;

const POSTERS: usize = 6;
const POSTS_EACH: usize = 40;

#[test]
fn concurrent_posts_lose_nothing_and_sequence_numbers_stay_dense() {
    let harness = Harness::new();
    let posters: Vec<String> = (0..POSTERS).map(|n| format!("p{n}")).collect();
    let names: Vec<&str> = posters.iter().map(String::as_str).collect();
    let shared = start(&harness, "a", "shared", &names);
    let before = harness.arrivals();

    let mut seqs: Vec<u64> = std::thread::scope(|scope| {
        let (harness, shared) = (&harness, &shared.thread);
        let posting: Vec<_> = names
            .iter()
            .map(|name| {
                scope.spawn(move || {
                    (0..POSTS_EACH)
                        .map(|n| say(harness, name, shared, &format!("{name} {n}")).id.seq())
                        .collect::<Vec<u64>>()
                })
            })
            .collect();
        posting
            .into_iter()
            .flat_map(|thread| thread.join().expect("poster"))
            .collect()
    });

    let posted = (POSTERS * POSTS_EACH) as u64;
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        (2..=posted + 1).collect::<Vec<u64>>(),
        "every post got its own seq"
    );
    assert_eq!(harness.arrivals(), before + posted);
    let mut stored = Vec::new();
    let mut cursor = None;
    loop {
        let page = harness
            .inbox(
                &caller("a"),
                InboxBody {
                    limit: Some(100),
                    cursor: cursor.take(),
                    ..history()
                },
            )
            .expect("inbox");
        stored.extend(page.messages.into_iter().map(|message| message.id.seq()));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        stored,
        (1..=posted + 1).collect::<Vec<u64>>(),
        "in post order, none missing"
    );
}
