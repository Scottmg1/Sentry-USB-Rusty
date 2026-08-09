# BLE Telemetry Health Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface Tesla BLE authentication failures and degraded telemetry in Settings and on the dashboard with specific recovery guidance.

**Architecture:** Add a small portable crate that owns the durable health record and severity classifier. The BLE session manager records definitive and transient outcomes, the API combines that record with authenticated telemetry freshness and radio ownership, and both React surfaces consume the same structured health payload through one tested presentation helper.

**Tech Stack:** Rust 2024, Serde/JSON, Axum, SQLite, React 19, TypeScript 6, Node 22 test runner, Tailwind CSS.

## Global Constraints

- Red is reserved for Tesla explicitly returning `KeyNotPaired`, including `MESSAGEFAULT_ERROR_UNKNOWN_KEY_ID`.
- Sleeping, radio contention, temporary failures, and stale telemetry must be yellow with reason-specific guidance.
- A GATT connection or unauthenticated body-controller response must not clear `repair_required`.
- Only a successful pairing probe or authenticated query may clear `repair_required`.
- Do not add periodic probes, wake the car, expose raw error strings, change sampling cadence, or add a frontend test dependency.
- Existing installs without a health record must fall back to freshness-based status.

---

### Task 1: Portable Durable Health Model and Classifier

**Files:**
- Create: `crates/ble_health/Cargo.toml`
- Create: `crates/ble_health/src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: `DEFAULT_HEALTH_PATH: &str`
- Produces: `FaultKind::{RepairRequired, TransportError, ProtocolError}`
- Produces: `HealthRecord { fault: FaultKind, since_ts: i64 }`
- Produces: `record_fault_at(path, fault, now_ts)`, `clear_transient_at(path)`, `clear_all_at(path)`, and `read_at(path)`
- Produces: `HealthInput` and `classify_health(&HealthInput) -> HealthStatus`
- Produces: `HealthStatus { severity, code, since_ts, label, guidance }`

- [ ] **Step 1: Add failing durable-state tests**

Add tests in `crates/ble_health/src/lib.rs` that call the wished-for API:

```rust
#[test]
fn repair_required_cannot_be_overwritten_by_transient_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("health.json");
    record_fault_at(&path, FaultKind::RepairRequired, 100).unwrap();
    record_fault_at(&path, FaultKind::TransportError, 200).unwrap();
    assert_eq!(read_at(&path).unwrap().unwrap(), HealthRecord {
        fault: FaultKind::RepairRequired,
        since_ts: 100,
    });
}

#[test]
fn transient_clear_preserves_repair_required_but_full_clear_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("health.json");
    record_fault_at(&path, FaultKind::RepairRequired, 100).unwrap();
    clear_transient_at(&path).unwrap();
    assert!(read_at(&path).unwrap().is_some());
    clear_all_at(&path).unwrap();
    assert_eq!(read_at(&path).unwrap(), None);
}

#[test]
fn atomic_write_leaves_no_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("health.json");
    record_fault_at(&path, FaultKind::ProtocolError, 123).unwrap();
    assert!(path.exists());
    assert!(!dir.path().join("health.json.tmp").exists());
}
```

- [ ] **Step 2: Run the new crate test and verify RED**

Run: `cargo test -p sentryusb-ble-health`

Expected: FAIL because the workspace member and crate do not exist yet.

- [ ] **Step 3: Implement the portable record API**

Create the crate with `serde`, `serde_json`, and `tempfile` as a dev dependency. Implement JSON read/write with a sibling `.tmp` file and `std::fs::rename`. `record_fault_at` must keep an existing `RepairRequired` record when the new fault is transient. Missing files return `Ok(None)`; malformed files return `InvalidData` so callers can safely fall back without inventing red.

- [ ] **Step 4: Add failing classifier tests**

Add table-driven tests with literal expectations:

```rust
#[test]
fn classifier_uses_required_precedence_and_guidance() {
    let cases = [
        (input(Some(repair(90)), 99, None, false, 100), Severity::Red, "repair_required"),
        (input(None, 50, Some("keep_awake"), true, 100), Severity::Yellow, "paused_archiving"),
        (input(Some(transport(95)), 90, None, false, 100), Severity::Yellow, "reconnecting"),
        (input(None, 70, None, false, 100), Severity::Green, "connected"),
        (input(None, 0, None, false, 100), Severity::Yellow, "idle"),
    ];
    for (input, severity, code) in cases {
        let got = classify_health(&input);
        assert_eq!(got.severity, severity);
        assert_eq!(got.code, code);
    }
}
```

- [ ] **Step 5: Run the classifier test and verify RED**

Run: `cargo test -p sentryusb-ble-health classifier_uses_required_precedence_and_guidance`

Expected: FAIL because `HealthInput`, `Severity`, and `classify_health` are not implemented.

- [ ] **Step 6: Implement classification**

Implement precedence in this order: `RepairRequired`; `keep_awake` with archive-specific code; transient record newer than the last authenticated success; freshness `< 60`; freshness `< 600`; otherwise idle. Return the exact labels and guidance approved in the design.

- [ ] **Step 7: Verify GREEN and commit**

Run: `cargo test -p sentryusb-ble-health`

Expected: all health-model tests PASS.

Commit: `feat(ble): add durable telemetry health model`

---

### Task 2: Record Session Outcomes Without Clearing Red on Transport-Only Success

**Files:**
- Modify: `crates/tesla_ble/Cargo.toml`
- Modify: `crates/tesla_ble/src/manager.rs`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: health record functions from `sentryusb_ble_health`
- Produces: manager transitions that record key rejection, transient failure, and authenticated recovery

- [ ] **Step 1: Add failing session-error classification tests**

Add manager tests proving the regression branch:

```rust
#[test]
fn key_not_paired_is_a_repair_required_fault() {
    assert_eq!(
        fault_for_session_error(&session::SessionError::KeyNotPaired),
        FaultKind::RepairRequired,
    );
}

#[test]
fn other_session_error_is_a_protocol_fault() {
    assert_eq!(
        fault_for_session_error(&session::SessionError::Other(anyhow::anyhow!("bad reply"))),
        FaultKind::ProtocolError,
    );
}
```

- [ ] **Step 2: Run focused manager tests and verify RED**

Run: `cargo test -p sentryusb-tesla-ble --lib manager::tests::key_not_paired_is_a_repair_required_fault`

Expected: FAIL because `fault_for_session_error` does not exist.

- [ ] **Step 3: Implement outcome transitions**

Add `fault_for_session_error`. On `SessionError::KeyNotPaired` in both domain-session establishment and `CheckPairing`, persist `RepairRequired` before returning. Record transport/protocol failures through the existing error handler without overwriting red. Clear all faults after a successful authenticated `Query`, `SignedRequest`, or `CheckPairing::Paired`. Clear only transient faults after `BodyController` or `AddKey` success because those operations do not prove Tesla accepts the key.

- [ ] **Step 4: Verify GREEN and commit**

Run: `cargo test -p sentryusb-tesla-ble --lib -j 2`

Expected: 42 or more tests PASS with 0 failures.

Commit: `feat(ble): persist authentication and transport health`

---

### Task 3: Expose One API Health Result Based on Authenticated Samples

**Files:**
- Modify: `crates/api/Cargo.toml`
- Modify: `crates/api/src/ble.rs`
- Modify: `crates/api/src/system.rs`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: `read_at(DEFAULT_HEALTH_PATH)` and `classify_health`
- Produces: `/api/system/ble-connected.health`
- Produces: quick BLE status `repair_required` when the durable fault is red

- [ ] **Step 1: Add a failing authenticated-freshness regression test**

Extract the telemetry aggregate SQL into `authenticated_activity(conn, since)` and add a test fixture containing a recent `body_controller` row plus an old `state` row:

```rust
#[test]
fn body_controller_ping_does_not_count_as_authenticated_activity() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE telemetry_samples (ts INTEGER PRIMARY KEY, source TEXT NOT NULL);\
         INSERT INTO telemetry_samples (ts, source) VALUES (100, 'state');\
         INSERT INTO telemetry_samples (ts, source) VALUES (999, 'body_controller');",
    ).unwrap();
    assert_eq!(authenticated_activity(&conn, 900), (100, 0));
}
```

- [ ] **Step 2: Run the API test in the Linux-capable environment and verify RED**

Run in Linux/CI: `cargo test -p sentryusb-api body_controller_ping_does_not_count_as_authenticated_activity`

Expected: FAIL because `authenticated_activity` does not exist. On Windows, document the existing `sentryusb-setup` Unix-API compile blocker and continue with the portable classifier tests plus source review.

- [ ] **Step 3: Implement API aggregation and health payload**

Change both `MAX(ts)` and the 10-minute count to `WHERE source = 'state'`. Read the durable record, ignoring read/parse errors with a warning and fallback. Call `classify_health` and serialize the result as `health` while retaining all existing response fields.

- [ ] **Step 4: Honor red in quick pairing status**

Before returning `paired` from `/root/.ble/paired`, read the durable record. If it is `RepairRequired`, return:

```json
{
  "status": "repair_required",
  "note": "Re-pair from the BLE card and tap your key card on the center console"
}
```

The full successful `PAIRED` probe must call `clear_all_at(DEFAULT_HEALTH_PATH)`; `NOT_PAIRED` must persist `RepairRequired` as defense in depth.

- [ ] **Step 5: Verify and commit**

Run: `cargo test -p sentryusb-ble-health`

Run in Linux/CI: `cargo test -p sentryusb-api body_controller_ping_does_not_count_as_authenticated_activity`

Commit: `feat(api): expose BLE telemetry health`

---

### Task 4: Tested Frontend Health Presentation Contract

**Files:**
- Create: `web/src/lib/bleHealth.ts`
- Create: `web/src/lib/bleHealth.test.ts`
- Modify: `web/package.json`

**Interfaces:**
- Produces: `BleHealth`, `BleHealthSeverity`, and `BleHealthCode`
- Produces: `presentBleHealth(health, secondsAgo) -> BleHealthPresentation`

- [ ] **Step 1: Add the failing Node test and script**

Add `"test:ble-health": "node --experimental-strip-types --test src/lib/bleHealth.test.ts"` and tests that import `presentBleHealth`:

```typescript
test("confirmed key rejection stays red and gives the re-pair action", () => {
  assert.deepEqual(presentBleHealth(repairRequired, 0), {
    severity: "red",
    code: "repair_required",
    label: "Re-pair required",
    guidance: "Open Settings, select Re-pair, then tap your key card on the center console.",
    repairRequired: true,
  })
})

test("missing backend health falls back to yellow idle instead of red", () => {
  const got = presentBleHealth(null, 86_400)
  assert.equal(got.severity, "yellow")
  assert.equal(got.code, "idle")
  assert.equal(got.repairRequired, false)
})
```

- [ ] **Step 2: Run the frontend test and verify RED**

Run: `npm run test:ble-health`

Expected: FAIL with module-not-found because `bleHealth.ts` does not exist.

- [ ] **Step 3: Implement the presentation helper**

Use backend `label` and `guidance` for structured health. For missing/malformed health, derive green `<60`, yellow delayed `<600`, or yellow idle otherwise. Never infer red from time alone.

- [ ] **Step 4: Verify GREEN and commit**

Run: `npm run test:ble-health`

Expected: all presentation tests PASS.

Run: `npm run build`

Expected: TypeScript and Vite build PASS.

Commit: `test(web): define BLE health presentation contract`

---

### Task 5: Render Health Consistently in Settings and Dashboard

**Files:**
- Modify: `web/src/components/settings/sections/BlePairButton.tsx`
- Modify: `web/src/pages/Dashboard.tsx`
- Modify: `web/src/components/dashboard/CarStatusCard.tsx`

**Interfaces:**
- Consumes: `BleHealth` and `presentBleHealth`
- Consumes: `/api/system/ble-connected.health`
- Produces: red re-pair UI and yellow degraded/idle UI in both surfaces

- [ ] **Step 1: Integrate Settings**

Store `health` from the existing 10-second `/ble-connected` poll. Drive halo, icon, live pill, message color, and guidance from `presentBleHealth`. Red must suppress the green `Paired`/`Connected` treatment and show `Re-pair required`; its button calls the existing pairing handler and remains labeled `Re-pair`. Yellow retains paired state but uses an amber pill and reason-specific text.

- [ ] **Step 2: Integrate Dashboard fetch**

Fetch `/api/system/ble-connected` alongside `/api/system/ble-latest-sample` in the existing 30-second car-status refresh and pass the health object into `CarStatusCard`.

- [ ] **Step 3: Integrate dashboard presentation**

When severity is yellow or red, replace the potentially stale `Parked`/`Driving` heading with the health label. Show guidance beneath it; for red, make `Re-pair required` link to `/settings?tab=Car%20%26%20Network`. Keep last-known chips and their age labels visible.

- [ ] **Step 4: Run focused and full frontend verification**

Run: `npm run test:ble-health`

Run: `npm run build`

Run: `npm run lint`

Expected: tests/build PASS; lint has 0 errors and no new warnings beyond the 29 baseline warnings.

- [ ] **Step 5: Review the complete diff and commit**

Run: `git diff --check`

Run: `git diff --stat b2e3b58..HEAD`

Confirm the mutation checks: red precedence, body-controller freshness exclusion, transient recovery, stale fallback, and both screen links/actions are each protected by a test or build-time contract.

Commit: `feat(web): show actionable BLE telemetry health`

---

### Task 6: Final Cross-Layer Verification

**Files:**
- No production changes expected

**Interfaces:**
- Verifies the complete feature against the approved acceptance criteria

- [ ] **Step 1: Run portable Rust tests**

Run: `cargo test -p sentryusb-ble-health`

Run: `cargo test -p sentryusb-tesla-ble --lib -j 2`

- [ ] **Step 2: Run frontend verification**

Run: `npm run test:ble-health`

Run: `npm run build`

Run: `npm run lint`

- [ ] **Step 3: Record platform-limited verification**

Run on Linux/CI: `cargo test -p sentryusb-api body_controller_ping_does_not_count_as_authenticated_activity` and `cargo test --workspace`.

On Windows, report the pre-existing Unix-only `sentryusb-setup` compile errors rather than claiming the full workspace passed locally.

- [ ] **Step 4: Inspect repository state**

Run: `git status --short`

Run: `git log --oneline b2e3b58..HEAD`

Expected: only intentional committed feature changes; no generated `web/dist`, `target`, or dependency directories tracked.
