//! MQTT topic construction without allocation.

use crate::Verb;
use heapless::String;

/// Topic node name, bounded to the protocol's sixteen bytes.
pub type Node = String<16>;
/// Exact retained last-will payload (also used for clean shutdown).
pub const LWT_PAYLOAD: &str = r#"{"online":false}"#;

/// Helpers for every topic in protocol section 2.
///
/// Node syntax (`[a-z0-9-]{1,16}`) is validated when configuration is applied.
/// The bounded input guarantees that every returned topic fits in 48 bytes.
pub struct Topics;

fn topic(node: &Node, suffix: &str) -> String<48> {
    let mut result = String::new();
    result.push_str("aether/").expect("constant prefix fits");
    result.push_str(node).expect("bounded node fits");
    result.push('/').expect("topic separator fits");
    result.push_str(suffix).expect("protocol suffix fits");
    result
}

impl Topics {
    /// Retained bridge status and last will.
    pub fn status(node: &Node) -> String<48> {
        topic(node, "status")
    }
    /// Single ordered bus stream.
    pub fn bus(node: &Node) -> String<48> {
        topic(node, "bus")
    }
    /// Host transmission requests.
    pub fn tx(node: &Node) -> String<48> {
        topic(node, "tx")
    }
    /// Periodic counters.
    pub fn telemetry(node: &Node) -> String<48> {
        topic(node, "telemetry")
    }
    /// Effective retained configuration.
    pub fn cfg(node: &Node) -> String<48> {
        topic(node, "cfg")
    }
    /// A command request selected by its verb.
    pub fn cmd(node: &Node, verb: Verb) -> String<48> {
        topic(
            node,
            match verb {
                Verb::Cfg => "cmd/cfg",
                Verb::State => "cmd/state",
                Verb::Ping => "cmd/ping",
                Verb::Restart => "cmd/restart",
                Verb::Ota => "cmd/ota",
                Verb::Reset => "cmd/reset",
            },
        )
    }
    /// Command outcomes.
    pub fn result(node: &Node) -> String<48> {
        topic(node, "cmd/result")
    }
    /// OTA progress.
    pub fn ota(node: &Node) -> String<48> {
        topic(node, "ota")
    }
    /// Optional diagnostic messages.
    pub fn log(node: &Node) -> String<48> {
        topic(node, "log")
    }
}
