# BLE Telemetry Health Status

## Problem

SentryUSB currently reports two different facts as if they were one:

- the Raspberry Pi has a Bluetooth connection to the car; and
- Tesla accepts SentryUSB's key and authenticated telemetry is flowing.

Tesla can keep the Bluetooth transport connected while returning
`MESSAGEFAULT_ERROR_UNKNOWN_KEY_ID`. The telemetry daemon maps that response to
`KeyNotPaired` and logs the correct recovery instruction, but it does not expose
the fault to the API. The Settings card therefore continues to trust the saved
paired marker, while the dashboard continues to present old values and a
"Parked" heading. A user may not discover the failure until reading Bluetooth
logs days later.

## Goals

- Make confirmed Tesla key rejection impossible to display as healthy.
- Distinguish a required repair from ordinary delay, sleep, radio contention,
  or a temporary transport failure.
- Give the user a reason-specific next action in Settings and on the dashboard.
- Use one backend health result so both screens agree.
- Preserve last-known vehicle values, but never present them as current.

## Non-goals

- Do not wake the car or add periodic pairing probes solely for UI status.
- Do not treat telemetry age alone as proof that pairing is broken.
- Do not expose raw backend errors or journal text in the UI.
- Do not change the BLE sampling cadence or pairing protocol.

## Health Model

The API will return a structured health object alongside the existing BLE
freshness fields:

| Severity | Code | Trigger | User guidance |
| --- | --- | --- | --- |
| Green | `connected` | Authenticated telemetry succeeded less than 60 seconds ago | Connected; no action |
| Yellow | `paused_archiving` | Keep-awake owns the BLE radio while archiving | Paused for archiving; no action needed |
| Yellow | `paused_keep_awake` | Keep-awake owns the BLE radio outside an archive | Paused for keep-awake; resumes automatically |
| Yellow | `reconnecting` | A classified transient transport/protocol failure is newer than the last success | Reconnecting automatically; check Bluetooth Logs if it persists |
| Yellow | `delayed` | Last authenticated success is 60 to 599 seconds old | Telemetry delayed; wait for retry |
| Yellow | `idle` | No authenticated success exists or it is at least 600 seconds old | Car may be asleep; wake it, then check Bluetooth Logs if values do not refresh |
| Red | `repair_required` | Tesla explicitly returns `KeyNotPaired`, including `MESSAGEFAULT_ERROR_UNKNOWN_KEY_ID` | Re-pair in Settings and tap the key card on the console |

Precedence is red repair-required, reason-specific pause, a current transient
failure, and then freshness. This prevents a live GATT connection or an old
database row from hiding an authentication failure.

The health response includes `severity`, `code`, `since_ts`, a short display
label, and guidance. Existing fields such as `last_success_ts`, `seconds_ago`,
`sample_count_10min`, `radio_owner`, and `archiving` remain for compatibility.

## Backend Design

The persistent BLE session will maintain a small durable health record under
`/mutable`. It will contain only a curated code and timestamp, not a raw error
string.

- Any `SessionError::KeyNotPaired` records `repair_required` immediately.
- A transport or other protocol failure records a transient failure code.
- A successful authenticated query or successful pairing probe clears the
  applicable fault and records health.
- Establishing a GATT connection does not clear a protocol fault.
- An unreachable pairing probe does not create or clear `repair_required`.
- Completing a verified re-pair clears `repair_required` immediately; the next
  authenticated telemetry query independently confirms recovery.

Health-record I/O will be isolated behind testable read/write/transition
helpers. Writes will use a temporary file plus rename so an interruption cannot
leave a partially written status.

`GET /api/system/ble-connected` will combine the durable record with telemetry
freshness, current radio ownership, and archive state to produce the health
object. `GET /api/system/ble-status?quick=true` will also honor a persisted
`repair_required` fault rather than returning `paired` from the marker alone.

## Settings Behavior

The BLE Pairing card will render the backend health object instead of deriving
health only from sample age.

- Green retains the current `Paired` and `Connected` presentation.
- Yellow keeps pairing marked as paired but changes the live pill, icon, and
  explanatory text to the specific paused, reconnecting, delayed, or idle
  state. The message states whether the user should wait, wake the car, or open
  Bluetooth Logs.
- Red removes the misleading green connected treatment, shows
  `Re-pair required`, explains the console key-card step, and provides the
  existing Re-pair action directly.

The Settings card will continue polling the existing connection endpoint, so a
new fault appears without a page reload.

## Dashboard Behavior

The dashboard will fetch the same BLE health object with its existing car
sample refresh. When health is yellow or red, the car-status card will not use
a stale gear value to claim the current state is `Parked`.

- Yellow shows a compact amber telemetry status and the reason-specific
  guidance.
- Red shows `Re-pair required` with a link to the BLE Pairing card in Settings.
- Last-known battery, temperature, and tire values remain visible with their
  existing age labels, making historical information available without
  implying that it is live.

## Error Handling and Recovery

- Unknown, malformed, or unavailable health data falls back to the existing
  freshness calculation and never invents a red pairing fault.
- Raw error strings stay in logs and diagnostics; the UI receives only stable
  codes and curated guidance.
- A successful authenticated operation clears transient yellow failures.
- Only a successful pairing probe or authenticated operation clears the red
  repair-required condition.
- Radio-busy and archive states are derived live and disappear automatically
  when ownership changes.

## Testing

Implementation will follow a failing-test-first sequence:

1. Rust unit tests for durable health-record parsing, atomic transitions, and
   recovery rules.
2. Rust tests for health-classification precedence, including the regression:
   a connected transport plus `KeyNotPaired` must return red
   `repair_required`.
3. Tests proving sleep/staleness, radio contention, and transient failures
   produce yellow states rather than red.
4. A pure TypeScript presentation-mapping test using Node's built-in test
   runner, covering Settings and dashboard labels/actions without introducing a
   new frontend test dependency.
5. Focused Cargo tests, frontend type-check/build, lint for touched frontend
   code, and the relevant broader workspace tests before completion.

## Acceptance Criteria

- The reported `MESSAGEFAULT_ERROR_UNKNOWN_KEY_ID` scenario turns Settings and
  the dashboard red within their next status poll.
- Neither screen simultaneously shows green `Connected` for that condition.
- The red message tells the user to re-pair and tap the key card on the center
  console.
- Sleeping, contention, temporary failures, and stale telemetry render yellow
  with appropriate guidance.
- Successful re-pairing and authenticated telemetry restore green without a
  service restart or manual status-file cleanup.
- Existing installations without a health record continue to work through the
  freshness fallback.
