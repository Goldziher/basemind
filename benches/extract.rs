//! L1+L2 tree-sitter extraction microbenchmarks.
//!
//! Benches the public `basemind::extract::extract_l1_l2` entry point — the single
//! parse + query-walk the scanner pays per file. Two representative inputs (a
//! realistic Rust module and a TypeScript module) are embedded so the bench is
//! deterministic and network-free. `LangId` is resolved via the public
//! `basemind::lang::detect` from a synthetic path with the right extension.

use std::path::Path;

use basemind::extract::extract_l1_l2;
use basemind::lang::detect;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

/// ~200 lines of idiomatic Rust: structs, enums, traits, impls, generics, calls.
const RUST_SOURCE: &str = r#"
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// A node in the dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: u64,
    pub name: String,
    pub edges: Vec<u64>,
    metadata: HashMap<String, String>,
}

impl Node {
    pub fn new(id: u64, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            edges: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    pub fn add_edge(&mut self, target: u64) {
        if !self.edges.contains(&target) {
            self.edges.push(target);
        }
    }

    pub fn degree(&self) -> usize {
        self.edges.len()
    }

    pub fn annotate(&mut self, key: &str, value: &str) {
        self.metadata.insert(key.to_string(), value.to_string());
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Node({}, {})", self.id, self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
    Both,
}

pub trait Traversal {
    fn visit(&mut self, node: &Node);
    fn finished(&self) -> bool;
}

pub struct DepthFirst {
    stack: Vec<u64>,
    visited: Vec<u64>,
    limit: usize,
}

impl DepthFirst {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            stack: Vec::new(),
            visited: Vec::new(),
            limit,
        }
    }

    fn push(&mut self, id: u64) {
        self.stack.push(id);
    }
}

impl Traversal for DepthFirst {
    fn visit(&mut self, node: &Node) {
        self.visited.push(node.id);
        for &edge in &node.edges {
            self.push(edge);
        }
    }

    fn finished(&self) -> bool {
        self.visited.len() >= self.limit || self.stack.is_empty()
    }
}

/// A directed graph over `Node`s with adjacency stored by id.
pub struct Graph {
    nodes: HashMap<u64, Arc<Node>>,
    direction: Direction,
}

impl Graph {
    pub fn new(direction: Direction) -> Self {
        Self {
            nodes: HashMap::new(),
            direction,
        }
    }

    pub fn insert(&mut self, node: Node) -> u64 {
        let id = node.id;
        self.nodes.insert(id, Arc::new(node));
        id
    }

    pub fn get(&self, id: u64) -> Option<Arc<Node>> {
        self.nodes.get(&id).cloned()
    }

    pub fn traverse<T: Traversal>(&self, start: u64, walker: &mut T) -> usize {
        let mut count = 0;
        if let Some(node) = self.get(start) {
            walker.visit(&node);
            count += 1;
        }
        while !walker.finished() {
            count += 1;
            if count > 10_000 {
                break;
            }
        }
        count
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

pub const MAX_NODES: usize = 1 << 20;

pub fn build_sample() -> Graph {
    let mut graph = Graph::new(Direction::Forward);
    for i in 0..32 {
        let mut node = Node::new(i, format!("n{i}"));
        node.add_edge((i + 1) % 32);
        node.annotate("kind", "sample");
        graph.insert(node);
    }
    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_has_nodes() {
        let g = build_sample();
        assert_eq!(g.node_count(), 32);
    }
}
"#;

/// ~120 lines of TypeScript: interfaces, classes, generics, decorators, async.
const TS_SOURCE: &str = r#"
import { EventEmitter } from "events";
import type { Logger } from "./logger";

export interface Task {
    id: string;
    priority: number;
    run(): Promise<void>;
}

export type Status = "pending" | "running" | "done" | "failed";

export class Scheduler<T extends Task> extends EventEmitter {
    private queue: T[] = [];
    private status: Map<string, Status> = new Map();
    private readonly logger: Logger;

    constructor(logger: Logger) {
        super();
        this.logger = logger;
    }

    public enqueue(task: T): void {
        this.queue.push(task);
        this.status.set(task.id, "pending");
        this.queue.sort((a, b) => b.priority - a.priority);
    }

    public async drain(): Promise<number> {
        let completed = 0;
        while (this.queue.length > 0) {
            const task = this.queue.shift();
            if (!task) {
                break;
            }
            this.status.set(task.id, "running");
            try {
                await task.run();
                this.status.set(task.id, "done");
                completed += 1;
            } catch (err) {
                this.status.set(task.id, "failed");
                this.logger.error(`task ${task.id} failed`, err);
            }
            this.emit("progress", completed);
        }
        return completed;
    }

    get pending(): number {
        return this.queue.length;
    }

    set verbose(value: boolean) {
        this.logger.verbose = value;
    }
}

export function makeTask(id: string, priority: number, body: () => Promise<void>): Task {
    return {
        id,
        priority,
        run: body,
    };
}

export const DEFAULT_PRIORITY = 5;

export async function runAll(scheduler: Scheduler<Task>): Promise<void> {
    const total = await scheduler.drain();
    console.log(`completed ${total} tasks`);
}
"#;

fn bench_extract(c: &mut Criterion) {
    let rust_lang = detect(Path::new("graph.rs")).expect("rust lang detected");
    let ts_lang = detect(Path::new("scheduler.ts")).expect("typescript lang detected");
    let rust_bytes = RUST_SOURCE.as_bytes();
    let ts_bytes = TS_SOURCE.as_bytes();

    let mut group = c.benchmark_group("extract");

    group.bench_function("rust_l1", |b| {
        b.iter(|| extract_l1_l2(rust_lang, black_box(rust_bytes), false).unwrap());
    });
    group.bench_function("rust_l1_l2", |b| {
        b.iter(|| extract_l1_l2(rust_lang, black_box(rust_bytes), true).unwrap());
    });
    group.bench_function("ts_l1", |b| {
        b.iter(|| extract_l1_l2(ts_lang, black_box(ts_bytes), false).unwrap());
    });
    group.bench_function("ts_l1_l2", |b| {
        b.iter(|| extract_l1_l2(ts_lang, black_box(ts_bytes), true).unwrap());
    });

    group.finish();
}

criterion_group!(benches, bench_extract);
criterion_main!(benches);
