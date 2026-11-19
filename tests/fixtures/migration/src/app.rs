use old_log::Logger;
use tracing::span;

pub fn run() {
    old_log::info("a");
    helper("b");
}

pub fn more() {
    old_log::info("c"); old_log::info("d");
    tracing::info("e");
}
