pub mod report;
pub mod single_input;
pub mod stateful_chain;

pub fn eval_harness_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
