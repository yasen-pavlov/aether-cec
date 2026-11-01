# Aether web UI — design brief

## Goal

Aether is a Rust ESP32 HDMI-CEC bridge: it sits on the living-room HDMI-CEC bus
as the "TV" (logical address 0), reports every frame it sees over MQTT and a LAN
HTTP API, and sends frames on behalf of a home-theater controller. It has a
single-page web UI served from the device itself at `http://aether/`. Today that
page is three raw JSON dumps and a textarea. Redesign it as a small, dense,
readable **diagnostic console** for one technical owner. Not a product; a tool.

Audience: one person (the owner), on a desktop browser most of the time, on a
phone when standing at the AV rack. Dark UI only.

## What the designer should produce

Boards, in this order (keep numbering stable in later rounds):

1. **Desktop overview** (1280 wide): header strip + bus monitor + send-frame
   panel, live state.
2. **Phone** (390 wide): the same, stacked; the bus monitor must still be
   readable one-handed.
3. **Bus monitor detail**: row anatomy for each event kind (`rx`, `tx` with each
   status, `err`, `line`), the poll-row treatment, the "paused / N new" state,
   filters, empty state, and the two density directions (see Open questions).
4. **Send frame**: the form, presets, the in-flight state and how the outcome
   (`ack`, `nack ×2`, `arb_lost`, `line_error`, `busy`, `rejected: reason`)
   lands back in the monitor row.
5. **Configuration**: the form (fields listed below), inline validation error,
   the "unsaved changes" state, the raw-JSON toggle.
6. **Counters, log, OTA**: telemetry counter table with deltas; log rows by
   level; OTA states `idle → hashing → uploading (pct) → verifying → rebooting →
   confirmed | rolled_back | failed: reason`.
7. **Header states**: normal; bridge has no logical address yet; bus
   `stuck_low` / `no_pullup`; previous boot crashed (`last_crash` / `last_stage`
   banner); SSE stream disconnected / reconnecting; no API token stored (first
   visit).
8. **Visual system**: tokens, type scale, spacing, the status chip set, the
   opcode/name colouring rules.
9. **Engineering delta**: anything the design needs that the data below does
   not provide, stated as assumptions.

Recommend one direction for the monitor density and say why. State assumptions
instead of asking questions mid-run; end with a short list of open questions.

All device names, frames, counters and log lines on the boards are fictional
sample data.

## Fixed constraints (engineering)

- **One static HTML file** compiled into the firmware, ≈ 30 KB budget, no build
  step, no framework, no external requests of any kind (no web fonts, no CDN,
  no icon sets, no images). Icons: inline SVG or Unicode only, and few.
- Served by an ESP32 over plain HTTP on the LAN. One Server-Sent-Events
  connection per page (the device allows three in total). Keep DOM updates
  cheap: a live table that appends rows at up to ~10 rows/s.
- Font: the owner's desktop font is **JetBrains Mono**; use a
  `ui-monospace, "JetBrains Mono", monospace` stack for all data (addresses,
  hex, counters, times) and the system UI sans for labels/buttons. No web font.
- Auth is a bearer token stored in the browser; the UI only needs a way to
  enter/forget it.
- Must work without JavaScript frameworks and in current Firefox/Chrome; no
  need for anything older.

## Visual tokens (source of truth; sampled from the owner's desktop theme)

| Token | Value | Use |
|---|---|---|
| surface | `#212121` | page background |
| surface-variant | `#323232` | panels, table header, inputs |
| outline | `#474747` | borders, dividers |
| on-surface | `#b0bec5` | body text |
| on-surface-variant | `#8c8b8b` | secondary text, timestamps |
| primary | `#ff9800` | the one accent: focused control, primary button, "live" dot |
| secondary | `#82aaff` | links, `rx` direction, informational |
| tertiary | `#c792ea` | `local` auto-replies, OTA |
| hover | `#80cbc4` | hover/selection, `ack` |
| error | `#ff5370` | errors, `nack`, `stuck_low`, crash banner |
| on-primary / on-error | `#212121` | text on filled chips |

Radius 10 px on panels (the owner's window rounding), 6 px on chips/inputs.
Spacing scale 4 / 8 / 12 / 16 / 24. Base type 14 px, data rows 13 px mono,
headings small and quiet (this is a console, not a landing page). Contrast:
all text ≥ 4.5:1 on its background.

## The data (what the page can show — do not invent fields)

Status (polled + pushed on change):
`online, v, fw ("0.0.0+0faa1746"), boot (counter), reset_reason
(power_on|software|panic|int_wdt|task_wdt|brownout|other), last_crash
(optional text), last_stage (optional "loop t_ms=310513"), t_us (uptime µs),
la (0–14 or null = allocating), pa ("0.0.0.0"), type (tv|playback|audio|
recorder|tuner), ip, rssi (dBm), bus (ok|stuck_low|no_pullup|null = not
measured), hpd (bool|null)`.

Bus events (SSE, in order, each with `seq` and `t_us`):
- `rx`: `from, to (0–15; 15 = broadcast), data (hex: opcode + operands; "" =
  poll), ack (bool)`.
- `tx`: `id (host request id, optional), origin (host|local), from, to, data,
  status (ack|nack|arb_lost|line_error|busy|rejected), attempts (1–5),
  reason (for rejected: id_too_long|bad_to|data_not_hex|data_too_long|
  from_not_allowed|no_la|malformed), reply_to.seq (for local)`.
- `err`: `kind (start_bit|bit_timing|eom_missing|ack_timing|overrun), from, to,
  bits, dropped`.
- `line`: `state (ok|stuck_low|no_pullup)`.

Logical address names (CEC standard): 0 TV, 1 Recording 1, 2 Recording 2,
3 Tuner 1, 4 Playback 1, 5 Audio System, 6 Tuner 2, 7 Tuner 3, 8 Playback 2,
9 Recording 3, 10 Tuner 4, 11 Playback 3, 12 Reserved, 13 Reserved,
14 Specific, 15 Broadcast/Unregistered. The page also learns OSD names from
`47 <ascii>` frames (e.g. `4 → "Apple TV"`, `11 → "PlayStation 5"`,
`5 → "AVR-X3800H"`) and can show them next to the number.

Opcodes the page decodes (examples): `04 Image View On`, `0D Text View On`,
`36 Standby`, `41 25 User Control Pressed: Play`, `44 Play`, `46 Give OSD Name`,
`47 Set OSD Name`, `70 System Audio Mode Request`, `72 Set System Audio Mode`,
`7A Report Audio Status`, `7D Give System Audio Mode Status`, `7E System Audio
Mode Status`, `82 Active Source 2.0.0.0`, `83 Give Physical Address`,
`84 Report Physical Address 2.0.0.0 (Playback)`, `85 Request Active Source`,
`86 Set Stream Path 2.0.0.0`, `87 Device Vendor ID 00:E0:91`, `89 Vendor
Command`, `8C Give Device Vendor ID`, `8E Menu Status`, `8F Give Device Power
Status`, `90 Report Power Status: On / Standby / To On / To Standby`, `91 Get
Menu Language`, `9D Inactive Source`, `9E CEC Version 1.4`, `9F Get CEC
Version`, `A0 Vendor Command With ID`, `FF Abort`, `00 Feature Abort <opcode>
<reason>`.

Telemetry (every 60 s): `rssi, link_count, heap_free, heap_min, heap_largest,
rx {frames, errors, acked_by_us}, tx {host, local, ack, nack, arb_lost,
line_error, busy, rejected, retransmits, origin_missing, sample_late,
launch_rejected, echo_error, edge_overrun, driven_low_mismatch},
queue {depth, max}, outbox_dropped`.

Log (last 64 lines): `t_us, lvl (info|warn|error), msg` — short machine-ish
messages like `wifi_connected ip=192.0.2.66`, `mqtt_connected`,
`tx_attempt_failed sample_late`, `stack_hwm task=cec-bus hwm=6724`,
`prev_boot_stage loop t_ms=310513`.

Configuration (editable): `la (auto | 0–14)`, `type`, `pa`, `osd (≤14 chars)`,
`vendor (6 hex)`, `cec_version (1.3a|1.4|2.0)`, `replies` (set of opcodes the
bridge answers locally: 83, 8C, 8F, 9F, 46, 91), `ack` (set of logical
addresses the bridge acknowledges for, 0–14), `telemetry_s`, `allow_from`
(bench only), `log (level)`, `gpio {cec, hpd}`. `node` and credentials are not
editable here.

Commands: `ping`, `restart`, `reset` (restore CEC defaults; confirm with the
node name), send frame `{to, data, retries}`, OTA upload (file + SHA-256
computed in the browser).

## Settled decisions (do not reopen)

| Decision | Why |
|---|---|
| Dark only, owner's palette above | matches the desktop it is used from |
| Bus monitor is the primary surface | it is what the owner actually looks at |
| No charts | potato device, and counters with deltas answer the questions |
| Token entry stays a plain field, not a login page | LAN tool, one user |
| Single file, no framework | flash budget and zero build step |

## Open questions for the designer to decide (state your choice)

1. Monitor density: (a) one line per event, hex and decoded text side by side,
   or (b) two-line rows (decoded on top, raw hex + ids below). Draw both,
   recommend one.
2. Where the send-frame panel lives on desktop: beside the monitor (sticky) or
   above it.
3. How much of the header stays visible while scrolling the monitor.
