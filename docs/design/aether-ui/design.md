# Design: aether-ui

Managed by cc-design (OMP skill `claude-design`). Edit the log freely; keep the field lines.

- url: https://claude.ai/artifact/<redacted>
- session: <redacted>
- model: claude-opus-5-5
- effort: high
- cwd: .
- created: 2026-11-01T09:37:17.504Z

## Log

### 2026-11-01T09:37:18.324Z

**new** from brief `docs/ui-brief.md` (model claude-opus-5-5, effort high)

### 2026-11-01T10:01:17.102Z

**designer reply**

I've read the brief and started the canvas. I'm checking a few details in the protocol doc so the sample data and engineering delta are accurate, then I'll build the boards.

I've got what I need from the protocol. I'm writing the index and Board 1 now. Board 1 will be a working prototype with a simulated stream, not a static comp.

Board 1 is up. Next is Board 2, the phone layout.

The canvas is published with all nine boards, but **I haven't opened or rendered any of them**, because the Design type says not to unless asked. Layout, frame heights and Board 1's interactions are all untested.

**Source location:** the canvas source isn't where you asked. The harness blocks this background job from writing to the shared checkout, so it's in the worktree at `.claude/worktrees/aether-ui-design/docs/design/aether-ui/canvas`, on branch `worktree-aether-ui-design`. It's uncommitted, since `AGENTS.md` leaves commits to the lead, and it's lost if the worktree is deleted. To copy it to the path you asked for:

`! cp -r ./.claude/worktrees/aether-ui-design/docs/design/aether-ui/canvas/project ./docs/design/aether-ui/canvas/`

## What's on the canvas
1. **Desktop overview:** a working prototype with a simulated stream, pause, filters, the send flow and panel tabs. It also has a one-line/two-line density tweak.
2. **Phone:** following, paused, and Send.
3. **Bus monitor detail:** row anatomy, polls, paused / N new, filters, empty states, and both density directions.
4. **Send frame:** the form, presets, and every outcome landing back in its row.
5. **Configuration:** clean, unsaved with an inline error, raw JSON, plus commands.
6. **Counters, log, OTA:** counter table with deltas, log rows by level, and all eight OTA states.
7. **Header states:** seven states, each at desktop and phone width.
8. **Visual system:** tokens with measured contrast, type, spacing, chips and colouring rules.
9. **Engineering delta:** what the design needs that the data doesn't provide, and where brief and protocol disagree.

## Decisions on your open questions
- **Monitor density:** I recommend one line per event. That fits 27 rows at 1280×800 against 15, so a row stays on screen about 2.7 s rather than 1.5 s at 10 rows/s. On phone the same row reflows to two lines.
- **Send panel:** it sits beside the monitor in a fixed 320 px rail. I assumed Counters, Log, Config and OTA share that rail as tabs, so the monitor never leaves the screen.
- **Header:** the whole 44 px strip stays visible because only the monitor scrolls. Fault banners sit under it, and phone keeps one line with a chevron for the rest.

## Conflicts I resolved
- **Contrast:** `on-surface-variant` (3.77:1) and `error` (4.10:1) fail 4.5:1 on `surface-variant`. I kept your tokens: data lists on panels sit on `surface`, secondary text on panels uses `on-surface`, and error on panels is always filled.
- **Warn colour:** the palette has no warn colour, so log `warn` uses `primary`.
- **Opcodes:** the brief has 41 and 44 swapped; in CEC, 41 is Play and 44 is User Control Pressed. Both are left out of the sample rows, and the decode table should follow the spec.

## Open questions
1. Does the 30 KB budget mean raw or compressed size? Board 1 is 36 KB raw and 10.4 KB gzipped. It includes sample data but has no real stream, config-save or OTA code, so 30 KB raw looks unreachable with no build step.
2. Can firmware expose the last `ota` event through a read endpoint? `confirmed` and `rolled_back` only arrive on the stream, which doesn't replay, so a page that reconnects late misses the verdict.
3. Will `PUT /api/cfg` reasons always start with `<field>:`? If not, errors fall back to one message above Save instead of sitting on the field.

https://claude.ai/artifact/<redacted>


### 2026-11-01T10:10:00Z

**lead decisions** on the designer's open questions and engineering delta
- Density: direction A (one line per event) accepted; send panel in the 320 px right rail with Counters/Log/Config/OTA as tabs; header strip sticky. All accepted as drawn.
- Budget: raw ≤ 64 KB (flash has ~400 KB headroom in the OTA slot; LAN transfer is not a concern). No gzip build step. Keep the SHA-256 fallback.
- OTA verdict after reconnect: no new endpoint. On (re)connect the page reads `/api/log` (`pending_image_confirm`, `ota_confirmed`, `rolled_back`) and `/healthz` (`fw`, `boot`).
- Config errors: reasons are static codes, not `field:` prefixed. The page maps codes to fields: `bad_la`→la, `bad_node`→node, `edid_unsupported`→pa, `forbidden_reply`→replies, `bad_ack`→ack, `bad_cec_gpio`/`gpio_conflict`→gpio.cec, `bad_hpd_gpio`→gpio.hpd, `malformed`/`too_large`→form-level.
- History on load: none; monitor starts empty (accepted).
- Opcodes: decode table follows the CEC spec (41 = Play, 44 = User Control Pressed); the brief's swap was an error.
- Canvas source copied from the Claude Code worktree into `docs/design/aether-ui/canvas/project/`; worktree removed. Boards render only on claude.ai (DC templating).
