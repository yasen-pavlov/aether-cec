# Fixtures

Distilled from two days of a real living-room bus (TV-less: the bridge is logical
address 0; Kodi box at 1, Apple TV at 4, AV receiver at 5, game consoles at 8 and 11)
captured through the patched-Tasmota bridge. Only `from`, `to`, `data` and the
Tasmota per-frame `Ack` survive; timestamps, IPs, host state and identities are
dropped at the source. `data` is opcode + operands in uppercase hex; `""` is a
polling message.

| File | Content | Use |
|---|---|---|
| `rx-frames.jsonl` | 79 distinct frames seen on the bus with occurrence counts, incl. 15-byte `47` OSD-name frames, polls, and frames from the unregistered initiator 15 | codec round-trips, policy "never reply to from=15", length bounds |
| `tx-outcomes.jsonl` | 20 distinct host send requests with the acknowledgement the old bridge reported (only pairs whose correlation was unambiguous) | expected `ack`/`nack` for directed and broadcast sends |
| `kodi-boot-storm.jsonl` | 120 consecutive frames while a Kodi box's CEC adapter was wedged and re-announcing itself every ~200 ms | rx throughput, duplicate handling, bus-health signal |
| `all-off-takeover.jsonl` | ordered `tx`/`result`/`rx` sequence of a room all-off: Play[Still] + Standby + Give Power Status to the Apple TV, Standby + poll to a console | state-machine scenario: directed ack, nack on a sleeping device, the `90 01` reply |
