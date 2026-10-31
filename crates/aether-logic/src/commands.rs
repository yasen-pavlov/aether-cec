//! Pure command dispatch: executor applies effects and reports persistence failures.
use aether_proto::{Command, CommandResult, Config, PowerStatus, Verb};

/// Required side effect after successful validation.
pub enum Effect {
    /// No persistent side effect.
    None,
    /// Persist and republish the candidate config.
    Config,
    /// Update the bridge's in-memory power state.
    Power(PowerStatus),
    /// Publish offline and restart.
    Restart,
    /// Restore default CEC config, preserving provisioned node, then restart.
    Reset,
}

/// Validated command plan; nothing has been persisted yet.
pub struct Plan {
    /// Correlated response, mutable on executor failure.
    pub result: CommandResult,
    /// Ordered executor action.
    pub effect: Effect,
    /// Candidate config for a cfg or reset effect.
    pub config: Option<Config>,
}

/// Map command-topic suffixes; result and unknown topics are not commands.
pub fn verb(text: &str) -> Option<Verb> {
    match text {
        "cfg" => Some(Verb::Cfg),
        "state" => Some(Verb::State),
        "ping" => Some(Verb::Ping),
        "restart" => Some(Verb::Restart),
        "reset" => Some(Verb::Reset),
        _ => None,
    }
}

/// Construct a bounded wire failure reason.
///
/// # Panics
/// If the supplied firmware reason exceeds the protocol's 32-byte bound.
pub fn fail(result: &mut CommandResult, reason: &str) {
    result.ok = false;
    result.reason = Some(reason.try_into().expect("firmware reasons fit 32 bytes"));
}

/// Parse the complete JSON object and prepare its operation atomically.
pub fn plan(verb: Verb, payload: &[u8], cfg: &Config, now_us: u64) -> Plan {
    let mut de = serde_json::Deserializer::from_slice(payload);
    let command =
        Command::deserialize_for(verb, &mut de).and_then(|command| de.end().map(|()| command));
    let mut plan = Plan {
        result: CommandResult {
            id: None,
            verb,
            ok: true,
            reason: None,
            t_us: None,
            echo: None,
        },
        effect: Effect::None,
        config: None,
    };
    let Ok(command) = command else {
        fail(&mut plan.result, "malformed");
        return plan;
    };
    match command {
        Command::Cfg(patch) => {
            plan.result.id = Some(patch.id.clone());
            match crate::config::merge(cfg, patch) {
                Ok(candidate) => {
                    plan.config = Some(candidate);
                    plan.effect = Effect::Config;
                }
                Err(reason) => fail(&mut plan.result, reason),
            }
        }
        Command::Ping(ping) => {
            plan.result.id = Some(ping.id);
            plan.result.t_us = Some(now_us);
            plan.result.echo = Some(ping.echo);
        }
        Command::State(state) => {
            plan.result.id = Some(state.id);
            plan.effect = Effect::Power(state.power);
        }
        Command::Restart(restart) => {
            plan.result.id = Some(restart.id);
            plan.effect = Effect::Restart;
        }
        Command::Reset(reset) => {
            plan.result.id = Some(reset.id);
            if reset.confirm == cfg.node {
                plan.effect = Effect::Reset;
                let mut defaults = crate::config::defaults();
                defaults.node = cfg.node.clone();
                plan.config = Some(defaults);
            } else {
                fail(&mut plan.result, "confirm_mismatch");
            }
        }
    }
    plan
}
