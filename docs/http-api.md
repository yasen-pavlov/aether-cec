# HTTP API, web console and push OTA

See [`docs/openapi.yaml`](openapi.yaml) for the OpenAPI 3.1 HTTP API contract, schemas and examples.

Port 80 is LAN-only plaintext HTTP: restrict access with the VLAN firewall.
Every route except `/` and `/healthz` requires `Authorization: Bearer <token>`.
The token is generated from 32 random bytes on first boot, with the bootloader
entropy source enabled before Wi-Fi/ADC startup, and stored in its own
`secrets/token` NVS key. Credential-load fallback never rewrites `secrets/json`.
**Only `show token` on the serial console prints it**; ordinary `show`, logs,
HTTP and MQTT never do. `set token <64 hex>` replaces only the token key immediately,
without a restart. Do not paste the token into shared transcripts or shell history.

Open `http://<bridge>/` for the embedded console (one 64 KiB HTML file, no
dependencies, no build step; design in [`design/aether-ui/`](design/aether-ui/)). Enter the
token once; it is retained in this browser's localStorage until “Forget”.
The page is a live bus monitor (every `rx`/`tx`/`err`/`line` event decoded with
CEC role and learned OSD names, pause/filter/copy, 500-row ring) with a side rail
for sending frames (presets, outcome correlated by id), telemetry counters with
deltas, the log tail, a configuration form with per-field errors and a raw-JSON
mode, and OTA upload with progress and a post-reboot verdict. Every value from
the device or the bus is rendered as text, never as HTML. It uses WebCrypto
SHA-256 when available and a local JS SHA-256 implementation on LAN HTTP (where
browsers disable WebCrypto). SSE uses authenticated fetch, not
EventSource/query-string credentials, with LF/CRLF framing and bounded reconnect.
HTTP status snapshots refresh at most once per second. Restart/reset always reboot
within main's two-second shutdown deadline (or earlier on offline PUBACK), even
if an HTTP waiter times out; the handler's response grace is only a speed-up.

The following are **bench checks for the operator**, not hardware verification
performed by the implementation. Set `BASE` to the bench board URL and read
`TOKEN` privately from serial; all JSON bodies use the MQTT protocol types.
Run destructive restart/reset/upload examples last. Replace `bench` in reset
with the provisioned node name.

```sh
BASE=http://bridge.example
read -rs -p 'Serial API token: ' TOKEN; printf '\n'
curl -fsS "$BASE/"                                  # GET / (public)
curl -fsS "$BASE/healthz"                            # GET /healthz (public)
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/status"
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/cfg"
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-cfg","telemetry_s":60}' "$BASE/api/cfg"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-ping","echo":"bench"}' "$BASE/api/cmd/ping"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-state","power":"standby"}' "$BASE/api/cmd/state"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-tx","to":8,"data":"8F"}' "$BASE/api/tx"  # 202; asynchronous bus outcome
curl -N -H "Authorization: Bearer $TOKEN" "$BASE/api/events"  # separate terminal
curl -fsS -H "Authorization: Bearer $TOKEN" "$BASE/api/log"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-restart"}' "$BASE/api/cmd/restart"
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"id":"h-reset","confirm":"bench"}' "$BASE/api/cmd/reset"
```

## Push OTA

Create an **app-only** image (no bootloader/table/offset padding) with:

```sh
espflash save-image --chip esp32 \
  firmware/target/xtensa-esp32-espidf/release/aether /tmp/aether-app.bin
SHA=$(sha256sum /tmp/aether-app.bin); SHA=${SHA%% *}
curl -fsS -X POST -T /tmp/aether-app.bin \
  -H "Authorization: Bearer $TOKEN" -H "X-Sha256: $SHA" "$BASE/api/ota"
```

The upload waits while writing 4-KiB chunks and checking SHA-256, ESP-IDF image
validity and project name `aether`; it returns `202 {"id":"o-N"}` then reboots
500 ms later. Never upload a merged full-flash image. Each slot is 1,966,080 bytes.
Watch MQTT `aether/<node>/ota` or SSE for receiving/verifying/rebooting followed
by confirmed or rolled_back; reconnect SSE after reboot.
For forced rollback, the lead must block **only the bench board's** broker access
before uploading, restore it after the 60-second rollback, and observe the old
image's `rolled_back` event. This requires a previously confirmed HTTP-capable
image in the old slot; the earlier phase-2 image cannot report the new event.

Error probes: missing/wrong bearer → 401 `unauthorized`; an authenticated
unknown path → 404 `not_found`; malformed/missing OTA headers → 400 `malformed`;
wrong digest → 400 `sha256_mismatch`; wrong project → 400 `project_mismatch`;
oversize image → 413 `too_large`; a running image still pending verification or
another accepted update awaiting reboot → 409 `busy` without allocating an OTA id.
The single httpd task streams uploads synchronously: a second upload waits in the
accept backlog while one is streaming, as can `/healthz` and other HTTP requests.
Check config validation failures leave the old cfg intact and reset preserves node,
token and Wi-Fi/MQTT credentials. Up to four sockets are allowed, including SSE;
each SSE client has a 16-event queue and is disconnected on overflow.
Logs retain the newest 64 scrubbed info/warn/error application records regardless
of `cfg.log`; MQTT `log` publication remains deferred.
