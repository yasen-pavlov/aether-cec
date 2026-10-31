//! USB-console syntax. Errors never contain the input or credentials.

/// Maximum accepted console line, excluding CR/LF.
pub const MAX_LINE: usize = 200;

/// Parsed provisioning action. Deliberately does not implement Debug.
pub enum ConsoleCommand {
    /// Store Wi-Fi credentials.
    Wifi {
        /// Network identifier (never displayed).
        ssid: String,
        /// Wi-Fi passphrase.
        pass: String,
    },
    /// Store broker credentials.
    Mqtt {
        /// Broker DNS name.
        host: String,
        /// Broker TCP port.
        port: u16,
        /// Authentication username.
        user: String,
        /// Authentication password.
        pass: String,
        /// Verify broker certificates over TLS.
        tls: bool,
    },
    /// Set node for the next restart.
    Node(String),
    /// Display redacted state.
    Show,
    /// Restart the device.
    Reboot,
    /// Erase config and secrets.
    FactoryReset,
}

/// Split a bounded line into words; double quotes preserve whitespace, including empty strings.
pub fn words(line: &str) -> Result<Vec<String>, &'static str> {
    if line.len() > MAX_LINE {
        return Err("line_too_long");
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in line.chars() {
        if ch == '"' {
            quoted = !quoted;
            started = true;
        } else if ch.is_whitespace() && !quoted {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else if ch.is_control() {
            return Err("invalid_character");
        } else {
            word.push(ch);
            started = true;
        }
    }
    if quoted {
        return Err("unclosed_quote");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// Parse provisioning commands without ever echoing secrets.
pub fn parse(line: &str) -> Result<ConsoleCommand, &'static str> {
    let args = words(line)?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["set", "wifi", ssid, pass] if valid_wifi(ssid, pass) => Ok(ConsoleCommand::Wifi {
            ssid: (*ssid).into(),
            pass: (*pass).into(),
        }),
        ["set", "mqtt", host, port, user, pass]
        | ["set", "mqtt", host, port, user, pass, "tls"] => mqtt(host, port, user, pass, true),
        ["set", "mqtt", host, port, user, pass, "plain"] => mqtt(host, port, user, pass, false),
        ["set", "node", node] if crate::config::valid_node(node) => {
            Ok(ConsoleCommand::Node((*node).into()))
        }
        ["show"] => Ok(ConsoleCommand::Show),
        ["reboot"] => Ok(ConsoleCommand::Reboot),
        ["factory-reset"] => Ok(ConsoleCommand::FactoryReset),
        _ => Err("invalid_command"),
    }
}

/// WPA passphrase rules shared by console parsing and stored-secret validation.
pub fn valid_wifi(ssid: &str, pass: &str) -> bool {
    !ssid.is_empty()
        && ssid.len() <= 32
        && !ssid.chars().any(char::is_control)
        && (pass.is_empty()
            || ((8..=63).contains(&pass.len())
                && pass.bytes().all(|byte| (0x20..=0x7e).contains(&byte)))
            || (pass.len() == 64 && pass.bytes().all(|byte| byte.is_ascii_hexdigit())))
}

/// Stored MQTT settings must obey the same rules as serial provisioning.
pub fn valid_mqtt(host: &str, port: u16, user: &str, pass: &str) -> bool {
    port != 0
        && !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && user.len() <= 64
        && pass.len() <= 128
        && !user.chars().chain(pass.chars()).any(char::is_control)
}

fn mqtt(
    host: &str,
    port: &str,
    user: &str,
    pass: &str,
    tls: bool,
) -> Result<ConsoleCommand, &'static str> {
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|&port| port > 0)
        .ok_or("bad_port")?;
    if !valid_mqtt(host, port, user, pass) {
        return Err("bad_mqtt_settings");
    }
    Ok(ConsoleCommand::Mqtt {
        host: host.into(),
        port,
        user: user.into(),
        pass: pass.into(),
        tls,
    })
}
