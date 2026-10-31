#![allow(dead_code)]
use cec_core::{Edge, Frame, Level, timing};

pub fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("\"{name}\":");
    let rest = line.split_once(&key)?.1.trim_start();
    if let Some(rest) = rest.strip_prefix('"') {
        Some(rest.split_once('"')?.0)
    } else {
        Some(rest.split([',', '}']).next()?.trim())
    }
}
pub fn fixture_frame(line: &str) -> Frame {
    let data = field(line, "data").expect("data");
    let bytes: Vec<_> = data
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let hex = core::str::from_utf8(pair).unwrap();
            u8::from_str_radix(hex, 16).unwrap()
        })
        .collect();
    Frame::new(
        field(line, "from").unwrap_or("0").parse().unwrap(),
        field(line, "to").unwrap().parse().unwrap(),
        &bytes,
    )
    .unwrap()
}
/// Renders nominal pulses with an optional signed displacement of each edge.
pub fn render(
    frame: &Frame,
    ack: bool,
    start: u64,
    jitter: Option<&dyn Fn(usize) -> i64>,
) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut pulse = |fall: u64, low: u32| {
        edges.push(Edge {
            level: Level::Low,
            t_us: fall,
        });
        edges.push(Edge {
            level: Level::High,
            t_us: fall + u64::from(low),
        });
    };
    pulse(start, timing::START_LOW.nominal);
    let mut now = start + u64::from(timing::START_TOTAL.nominal);
    for (index, byte) in core::iter::once(frame.header())
        .chain(frame.data.iter().copied())
        .enumerate()
    {
        for bit in (0..8).rev() {
            pulse(
                now,
                if byte & (1 << bit) != 0 {
                    timing::DATA_LOW_1.nominal
                } else {
                    timing::DATA_LOW_0.nominal
                },
            );
            now += u64::from(timing::BIT);
        }
        pulse(
            now,
            if index + 1 == frame.blocks() {
                timing::DATA_LOW_1.nominal
            } else {
                timing::DATA_LOW_0.nominal
            },
        );
        now += u64::from(timing::BIT);
        let pulled_low = ack != frame.is_broadcast();
        pulse(
            now,
            if pulled_low {
                timing::ACK_LOW
            } else {
                timing::DATA_LOW_1.nominal
            },
        );
        now += u64::from(timing::BIT);
    }
    if let Some(jitter) = jitter {
        for (i, edge) in edges.iter_mut().enumerate() {
            edge.t_us = edge
                .t_us
                .checked_add_signed(jitter(i))
                .expect("nonnegative jittered time");
        }
    }
    edges
}
