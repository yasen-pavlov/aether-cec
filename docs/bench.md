# Bench checks

Operator procedures for a board on a real CEC bus. They are checks to run, not
results: host logic is covered by root `cargo test --all-features`, clippy and fmt; this
does not exercise NVS, UART, Wi-Fi, TLS, MQTT delivery/LWT, or reboot on hardware.

Examples assume `BASE` and `TOKEN` as set in [`http-api.md`](http-api.md).

## Listener-first gate

Do not run two TV bridges on the same CEC bus. The operator first applies
`{"id":"listen","la":0,"replies":[],"ack":[]}` before OTA; these persisted settings
survive the image change. Fixed LA0 does not allocation-poll, and the empty ACK set
disables even the core's implicit own-address ACK entry. No unsolicited line
drive is permitted in this state; explicit host TX still works.

These are operator checks, **not executed hardware results**:

```sh
# Supply broker TLS/auth privately using your normal mosquitto client config.
mosquitto_sub -v -t 'aether/bench/status' -t 'aether/bench/bus' \
  -t 'aether/bench/telemetry'
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"listen","la":0,"replies":[],"ack":[],"telemetry_s":10}' "$BASE/api/cfg"
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/status"
curl -N -H "Authorization: Bearer $TOKEN" "$BASE/api/events"
# Only after the passive capture/timing gate, enable follower/local responses:
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"enable","ack":[0],"replies":["83","8C","8F","9F","46","91"]}' "$BASE/api/cfg"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"power-on","power":"on"}' "$BASE/api/cmd/state"
mosquitto_pub -t 'aether/bench/tx' -m '{"id":"denon-pwr","to":5,"data":"8F"}'
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"http-pwr","to":5,"data":"8F"}' "$BASE/api/tx"
# End the exercise by restoring persisted passive listener mode:
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"listen-again","la":0,"ack":[],"replies":[]}' "$BASE/api/cfg"
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/cfg"
```

Listener capture should show receiver broadcasts such as `5→15 84200005`
and Apple TV `4→0 8F` (possibly NACK-truncated while ACK is disabled), with
monotonic sequence and start timestamps and no local TX. Status becomes `bus:ok`
after a valid frame or one second of high idle; boot low with no edges for
over 100 ms is `no_pullup`, later sustained low/recovery is `stuck_low`/`ok`.
After enable, `4→0 8F` must be ACKed and answered with local `0→4 9000`;
the Denon request must yield `0→5 8F` echo, `tx` with the matching id and
`status:ack`, and receiver `5→0 90xx` (receiver response order may precede
or follow the TX result, but the send's RX echo must precede its TX result).
No reply may be generated for `85`, `82`, broadcasts or LA15 initiators.
After the final return-to-listener request, confirm effective cfg has fixed LA0,
`ack:[]` and `replies:[]`; let accepted host sends drain and stop submitting TX.
Observe addressed traffic with no local TX and no increase in
`telemetry.rx.acked_by_us`. Confirm on the scope that the bridge never pulls the
line low (including after reboot); valid incoming frames alone do not prove passivity.

## Logic-analyzer checks

With a logic analyzer, measure actual ACK-fall-to-assert below 200 us and ACK
hold near 1500 us, RMT data/start widths, seamless block chaining, per-block
first-fall origin acquisition within 300 us, and sample jitter within the core's
200-us allowance under Wi-Fi/SSE/HTTP/OTA load. On arbitration loss/collision,
verify GPIO matrix detachment releases in under 1 us and the detached RMT tail
never reaches the wire or replays during a retry. Check busy-bus start rejection.
Also check bus/console stack headroom, open-drain high=released electrically,
maximum frame lengths, NACK retry/poll behavior, arbitration recovery, config
barriers and automatic LA allocation separately. OTA must wait for TX/follower
ACK idle before reboot. Software build/tests alone do not establish any of these.
