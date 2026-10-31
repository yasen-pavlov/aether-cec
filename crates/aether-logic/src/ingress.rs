//! Bounded MQTT fragment reassembly, independent of ESP-IDF event types.

/// Maximum complete JSON message accepted by the firmware.
pub const MAX_PAYLOAD: usize = 2048;

/// A complete message; empty data intentionally produces a malformed rejection.
pub struct Message {
    /// MQTT topic.
    pub topic: String,
    /// Complete JSON bytes, or empty for oversized input.
    pub data: Vec<u8>,
}

/// One in-progress MQTT message (MQTT fragments arrive consecutively).
#[derive(Default)]
pub struct Assembler {
    partial: Option<(String, Vec<u8>, usize)>,
}
impl Assembler {
    /// Forget partial input across disconnects.
    pub fn clear(&mut self) {
        self.partial = None;
    }

    /// Accept the first or complete fragment, preserving the bounded topic.
    pub fn start(&mut self, topic: &str, data: &[u8], total: usize) -> Option<Message> {
        self.clear();
        if total > MAX_PAYLOAD || data.len() > total {
            return Some(Message {
                topic: topic.into(),
                data: Vec::new(),
            });
        }
        if data.len() == total {
            return Some(Message {
                topic: topic.into(),
                data: data.to_vec(),
            });
        }
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(data);
        self.partial = Some((topic.into(), bytes, total));
        None
    }

    /// Append a fragment at its declared offset. Broken sequences become malformed.
    pub fn append(&mut self, data: &[u8], offset: usize, total: usize) -> Option<Message> {
        let (topic, mut bytes, expected) = self.partial.take()?;
        if offset != bytes.len() || total != expected || bytes.len() + data.len() > total {
            return Some(Message {
                topic,
                data: Vec::new(),
            });
        }
        bytes.extend_from_slice(data);
        if bytes.len() == total {
            Some(Message { topic, data: bytes })
        } else {
            self.partial = Some((topic, bytes, total));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_are_ordered_bounded_and_reset_on_disconnect() {
        let mut assembler = Assembler::default();
        assert!(assembler.start("tx", b"{", 2).is_none());
        assert_eq!(assembler.append(b"}", 1, 2).unwrap().data, b"{}");
        assert!(assembler.start("tx", b"{", 3).is_none());
        assert_eq!(assembler.append(b"}", 2, 3).unwrap().data, b"");
        assert_eq!(
            assembler
                .start("cmd/ping", b"{", MAX_PAYLOAD + 1)
                .unwrap()
                .data,
            b""
        );
        assert!(assembler.start("tx", b"{", 2).is_none());
        assembler.clear();
        assert!(assembler.append(b"}", 1, 2).is_none());
        assert_eq!(assembler.start("tx", b"{}", 2).unwrap().data, b"{}");
    }
}
