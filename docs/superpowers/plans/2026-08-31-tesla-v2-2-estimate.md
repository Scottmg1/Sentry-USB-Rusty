# Tesla v2.2 Estimate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the custom Safety Score with an explicitly labeled Tesla v2.2-compatible estimate using only observable SEI factors, exact assisted-driving eligibility, measured-IMU coverage, and mileage-weighted daily scores.

**Architecture:** Compute route-local observable factor scalars once, persist compact cross-clip grace evidence, and resolve five-second assisted-driving grace at grouping time without decoding route blobs in API handlers. Score eligible native drives and local calendar days with pinned Tesla v2.2 PCF constants, then mileage-weight daily scores for period analytics while preserving the existing FSD analytics independently.

**Tech Stack:** Rust 2024, rusqlite/SQLite, serde/serde_json, chrono, React 19, TypeScript 6, Vite, Node test runner, happy-dom.

**Spec:** `docs/superpowers/specs/2026-08-31-tesla-v2-2-estimate-design.md`

## Global Constraints

- Model ID: `tesla-v2.2-estimate-1`; UI label: `Tesla v2.2 Estimate`.
- Included factors: hard braking, aggressive turning, excessive speeding above 85 mph, and weighted late-night driving.
- Omit unsafe following, forced Autopilot disengagement, unbuckled driving, lead-relative speeding, and yellow-light braking exemption; never synthesize them.
- Hard braking, aggressive turning, and speeding are ineligible while FSD/Autosteer/TACC is active and for five seconds after disengagement, except TACC with accelerator position above 1% counts as driver-controlled.
- FSD usage remains visible but provides no blanket relief or score discount.
- Late-night time includes assisted driving and uses moving milliseconds from 11 PM through 4 AM with weights `0.21`, `0.53`, `0.71`, `0.82`, and `1.00`.
- Require at least 90% measured-IMU moving-time coverage and at least 0.1 miles; incompatible history is excluded, not scored as clean.
- Imported drives and Summon sessions remain excluded.
- Period scores are mileage-weighted averages of eligible local-day scores; `all` remains a product extension.
- Schema migration is additive and restart-safe; existing compatible blobs recompute without snapshots or MP4 files.
- Preserve old response fields for deserialization; `fsdReliefPct` remains present as `0`, and penalty fields become non-additive leave-one-factor-out impacts.
- Full Rust verification runs in Linux/WSL; web verification includes tests, lint, type checking, and production build.

---

### Task 1: Pin the v2.2 scoring model and coverage contract

**Files:**
- Modify: `crates/drives/src/safety.rs`

**Interfaces:**
- Produces: `MODEL_ID`, `MODEL_LABEL`, `MIN_COMPATIBLE_IMU_COVERAGE`, `MIN_SCORED_MILES`, `SafetyTotals`, `SafetyScore`, `compute_safety_score(&SafetyTotals) -> Option<SafetyScore>`, and `mileage_weighted_score(&[(f64, f64)]) -> Option<f64>`.
- Consumes: `calc::round1`.

- [ ] **Step 1: Replace the custom-formula tests with failing v2.2 fixtures**

Add tests that assert exact constants, percentage-unit exponents, caps, a clean baseline score, leave-one-factor-out impacts, zero FSD relief, 0.1-mile eligibility, 89.9/90/100% coverage, and mileage-weighted daily aggregation. Use a reference helper in tests:

```rust
fn reference_score(hb: f64, turn: f64, night: f64, speed: f64) -> f64 {
    let pcf = 0.57198191
        * 1.23599110_f64.powf(hb.min(5.2))
        * 1.01219290_f64.powf(turn.min(13.2))
        * 1.03231810_f64.powf(night.min(14.2))
        * 1.02439511_f64.powf(speed.min(10.0));
    (122.15240383 - 38.72920381 * pcf).clamp(0.0, 100.0)
}
```

- [ ] **Step 2: Run the focused tests and verify the old model fails**

Run: `wsl bash -lc 'cd "/mnt/c/Nextcloud/Documents/Visual Studio Code/Sentry-Six-Assets/Sentry-USB-Rusty" && cargo test -p sentryusb-drives safety::tests -- --nocapture'`

Expected: failures reference the old 0.5-mile floor, subtractive weights, FSD relief, or missing coverage fields.

- [ ] **Step 3: Implement the pinned model with no global FSD discount**

Use these production constants and data shape:

```rust
pub const MODEL_ID: &str = "tesla-v2.2-estimate-1";
pub const MODEL_LABEL: &str = "Tesla v2.2 Estimate";
pub const MIN_COMPATIBLE_IMU_COVERAGE: f64 = 0.90;
pub const MIN_SCORED_MILES: f64 = 0.1;

pub struct SafetyTotals {
    pub distance_mi: f64,
    pub moving_ms: i64,
    pub imu_moving_ms: i64,
    pub hard_brake_ms: i64,
    pub brake_any_ms: i64,
    pub aggr_turn_ms: i64,
    pub turn_any_ms: i64,
    pub speeding_ms: i64,
    pub night_weighted_ms: i64,
    pub assisted_mi: f64,
    pub hard_brake_events: i32,
    pub aggr_turn_events: i32,
}
```

Compute percentage exponents without the old denominator floor, cap them at `5.2`, `13.2`, `14.2`, and `10.0`, and use the published PCF equation. Return `None` below 0.1 miles, with no moving time, or below 90% IMU moving-time coverage. Preserve `fsdSharePct` for information, return `fsdReliefPct = 0.0`, and compute each legacy penalty field as `score_without_that_factor - full_score`.

- [ ] **Step 4: Run the focused tests and verify they pass**

Run the command from Step 2.

Expected: all `safety::tests` pass.

- [ ] **Step 5: Commit the scoring core**

```bash
git add crates/drives/src/safety.rs
git commit -m "feat: implement Tesla v2.2 estimate math"
```

### Task 2: Make per-clip factors use correct IMU sign and exact assisted eligibility

**Files:**
- Modify: `crates/drives/src/safety.rs`
- Modify: `crates/drives/src/aggregate.rs`
- Modify: `crates/drives/src/types.rs` (correct the stored `accel_y` sign documentation)

**Interfaces:**
- Produces: `SafetyPrefixBucket`, `SafetyGracePrefix`, and expanded `ClipSafety` fields `imu_moving_ms`, `night_ms`, `night_weighted_ms`, `grace_ms_end`, and `grace_prefix`.
- Consumes: `Route.autopilot_states`, `Route.accel_positions`, `Route.accel_x`, `Route.accel_y`, route filename local timestamp.

- [ ] **Step 1: Add failing detector and eligibility tests**

Cover positive `accel_y` as deceleration, negative `accel_y` as acceleration, measured coverage, corrected Menger geometry, FSD/Autosteer/TACC exclusion, TACC pedal override, five-second post-disengagement grace, speeding exclusion, and exact hourly night weights across a wall-clock boundary. Assert FSD analytics counters in `aggregate.rs` are unchanged.

- [ ] **Step 2: Run detector tests and verify the sign/grace cases fail**

Run: `wsl bash -lc 'cd "/mnt/c/Nextcloud/Documents/Visual Studio Code/Sentry-Six-Assets/Sentry-USB-Rusty" && cargo test -p sentryusb-drives safety::tests aggregate::tests -- --nocapture'`

Expected: positive-Y braking and post-disengagement grace assertions fail against current code.

- [ ] **Step 3: Implement sample eligibility and compact prefix evidence**

Use a single helper:

```rust
fn driver_controls_factor(ap: u8, accel_position: f32) -> bool {
    ap == AUTOPILOT_OFF || (ap == AUTOPILOT_TACC && accel_position > 1.0)
}
```

Track a five-second grace countdown after an assisted-to-driver-controlled transition. Set `decel = r.accel_y[i] as f64`, keep `lateral = abs(accel_x)`, increment `imu_moving_ms` only for moving samples with aligned measured X/Y arrays, and never let the derived fallback qualify coverage. Record factor milliseconds that land in the first five one-second buckets so a previous clip's grace can remove them. Compute late-night moving milliseconds at sample time from the clip's local filename timestamp; do not exclude assisted samples from night time.

Correct Menger geometry by using vectors `p0->p1`, `p1->p2`, and `p0->p2 = (p0->p1) + (p1->p2)`. Update the `ExtractedGps.accel_y` comment to record that positive Y is deceleration in the stored Tesla SEI samples.

- [ ] **Step 4: Run detector and aggregate tests**

Run the command from Step 2.

Expected: all targeted tests pass and existing FSD usage/disengagement tests remain green.

- [ ] **Step 5: Commit the clip detector**

```bash
git add crates/drives/src/safety.rs crates/drives/src/aggregate.rs crates/drives/src/types.rs
git commit -m "fix: measure eligible safety factors from SEI"
```

### Task 3: Add the restart-safe v22 persistence contract

**Files:**
- Modify: `crates/drives/src/types.rs`
- Modify: `crates/drives/src/blob.rs`
- Modify: `crates/drives/src/schema.rs`
- Modify: `crates/drives/src/backfill.rs`

**Interfaces:**
- Produces route columns: `ap_at_end INTEGER`, `safety_imu_moving_ms INTEGER`, `safety_night_ms INTEGER`, `safety_night_weighted_ms INTEGER`, `safety_grace_ms_end INTEGER`, `safety_grace_prefix_blob BLOB`.
- Produces: `encode_safety_grace_prefix(&SafetyGracePrefix) -> Vec<u8>` and `decode_safety_grace_prefix(Option<&[u8]>) -> SafetyGracePrefix` with malformed input returning an empty prefix.
- Consumes: expanded `RouteAggregates` from Task 2.

- [ ] **Step 1: Add failing blob, fresh-schema, v21-upgrade, and repeated-migration tests**

Assert a five-bucket prefix round-trips exactly; malformed/truncated blobs fail closed to empty. Create a v21-shaped DB without the six columns, migrate it, assert all six columns exist and `schema_version` is `22`, then run migration twice and assert no error. Verify legacy rows remain nullable until backfill.

- [ ] **Step 2: Run persistence tests and verify they fail**

Run: `wsl bash -lc 'cd "/mnt/c/Nextcloud/Documents/Visual Studio Code/Sentry-Six-Assets/Sentry-USB-Rusty" && cargo test -p sentryusb-drives blob::tests schema::tests -- --nocapture'`

Expected: missing v22 constants/columns and codec functions fail compilation or assertions.

- [ ] **Step 3: Implement additive schema and fixed-version blob encoding**

Set `CURRENT_SCHEMA_VERSION` to `22`, append `V22_ROUTE_SAFETY_COLUMNS` to the existing idempotent column chain, and document v21-to-v22 behavior. Encode a one-byte version followed by five fixed buckets of little-endian `i64` fields for hard braking, any braking, aggressive turning, any turning, and speeding. Reject unknown versions and incorrect lengths.

Extend `RouteAggregates` and `backfill_one_batch` SQL/bindings with the six fields. Existing rows with no aligned acceleration blobs must backfill `safety_imu_moving_ms = 0` without opening MP4 files.

- [ ] **Step 4: Run persistence tests and verify they pass**

Run the command from Step 2.

Expected: blob and schema tests pass.

- [ ] **Step 5: Commit the v22 persistence layer**

```bash
git add crates/drives/src/types.rs crates/drives/src/blob.rs crates/drives/src/schema.rs crates/drives/src/backfill.rs
git commit -m "feat: persist v2.2 safety compatibility data"
```

### Task 4: Wire v22 fields through every database read/write path and invalidate caches

**Files:**
- Modify: `crates/drives/src/db.rs`
- Modify: `crates/drives/src/json_compat.rs`

**Interfaces:**
- Produces current `RouteSummary.aggregates` in normal reads, cache rebuild reads, JSON import/upsert, and formula backfill.
- Consumes the v22 fields and prefix codec from Task 3.

- [ ] **Step 1: Add failing round-trip, legacy-import, and formula-gate tests**

Insert a route with nonzero v22 values, reopen the store, and assert every value survives all summary SQL paths. Import pre-v22 JSON and assert defaults make it unscoreable rather than clean. Seed the old aggregate formula/cache markers and assert opening the store nulls/recomputes safety columns and rebuilds cached drive summaries.

- [ ] **Step 2: Run database tests and verify missing SQL plumbing fails**

Run: `wsl bash -lc 'cd "/mnt/c/Nextcloud/Documents/Visual Studio Code/Sentry-Six-Assets/Sentry-USB-Rusty" && cargo test -p sentryusb-drives db::tests json_compat::tests -- --nocapture'`

Expected: v22 round-trip or invalidation assertions fail.

- [ ] **Step 3: Update all explicit SQL column/binding lists and version gates**

Bump:

```rust
const DRIVE_LIST_CACHE_ALGO_VERSION: &str = "12";
const AGGREGATE_FORMULA_VERSION: &str = "tesla-v2.2-estimate-1";
```

Add the six fields to insert/update, summary reads, backfill resets, cache reconstruction, and JSON compatibility paths. Keep nullable reads tolerant of interrupted upgrades. Ensure formula invalidation only clears recomputable aggregate columns and never raw speed/AP/accelerator/IMU blobs.

- [ ] **Step 4: Run database tests and verify they pass**

Run the command from Step 2.

Expected: all targeted database and JSON compatibility tests pass.

- [ ] **Step 5: Commit database plumbing**

```bash
git add crates/drives/src/db.rs crates/drives/src/json_compat.rs
git commit -m "feat: backfill v2.2 safety aggregates"
```

### Task 5: Resolve cross-clip grace and compute eligible per-drive summaries

**Files:**
- Modify: `crates/drives/src/grouper.rs`
- Modify: `crates/drives/src/types.rs`

**Interfaces:**
- Produces `DriveSummary` fields `safety_imu_moving_ms`, `safety_night_weighted_ms`, `safety_coverage_pct`, and the v2.2 `safety_score`.
- Consumes route `ap_at_start`, `ap_at_end`, `safety_grace_ms_end`, and decoded five-bucket prefixes.

- [ ] **Step 1: Add failing grouping tests for clip seams and eligibility**

Cover: AP ends in clip A and clip B starts manual; an in-clip disengagement leaves three seconds of grace; TACC pedal override; a gap that prevents clip continuity; 89.9% coverage yields no card score; 90% yields a score; a 0.09-mile drive is omitted; imported and Summon drives remain omitted. Assert FSD usage/disengagement totals are byte-equivalent before and after the safety change.

- [ ] **Step 2: Run grouper tests and verify failures**

Run: `wsl bash -lc 'cd "/mnt/c/Nextcloud/Documents/Visual Studio Code/Sentry-Six-Assets/Sentry-USB-Rusty" && cargo test -p sentryusb-drives grouper::tests -- --nocapture'`

Expected: seam grace, coverage, and 0.1-mile cases fail.

- [ ] **Step 3: Implement a BLOB-free grace resolver in the aggregate walk**

Carry only the prior contiguous route's `ap_at_end` and remaining grace. If the prior route ended assisted and the next begins driver-controlled, start a 5,000 ms grace; otherwise continue the persisted remainder. Consume prefix buckets in order and subtract only hard/any-brake, aggressive/any-turn, and speeding milliseconds. Do not subtract moving, IMU coverage, or late-night time. Clear state on a non-contiguous clip boundary.

Build `SafetyTotals` with compatible coverage and compute the drive card score through Task 1. Preserve all existing FSD metrics and generic disengagement displays independently.

- [ ] **Step 4: Run grouper tests and verify they pass**

Run the command from Step 2.

Expected: all grouper tests pass.

- [ ] **Step 5: Commit cross-clip and card scoring**

```bash
git add crates/drives/src/grouper.rs crates/drives/src/types.rs
git commit -m "feat: score compatible drives across clip seams"
```

### Task 6: Replace period scoring with mileage-weighted eligible daily scores

**Files:**
- Modify: `crates/drives/src/grouper.rs`
- Modify: `crates/drives/src/types.rs`
- Modify: `crates/api/src/drives_handler.rs`

**Interfaces:**
- Produces expanded `SafetyDayStats` eligibility/coverage fields and `SafetyAnalytics` model/disclosure/coverage fields.
- Consumes eligible `DriveSummary` records from Task 5.

- [ ] **Step 1: Add failing analytics/API contract tests**

Create two eligible days with different mileage and scores and assert the period score equals `sum(day_score * day_miles) / sum(day_miles)`, not a score over period totals and not an unweighted mean. Mix compatible and incompatible native drives and assert `compatibleMiles`, `totalNativeMiles`, `compatibleDays`, `coveragePct`, `modelId`, `modelLabel`, `isEstimate`, and the five unavailable factors. Verify `all` has no date cutoff and generic `fsdDisengagements` remains informational.

- [ ] **Step 2: Run analytics tests and verify old aggregation fails**

Run: `wsl bash -lc 'cd "/mnt/c/Nextcloud/Documents/Visual Studio Code/Sentry-Six-Assets/Sentry-USB-Rusty" && cargo test -p sentryusb-drives safety_analytics -- --nocapture && cargo test -p sentryusb-api drives_handler -- --nocapture'`

Expected: weighted daily/model metadata assertions fail.

- [ ] **Step 3: Implement daily eligibility and period weighting**

Extend serialized contracts with:

```rust
pub model_id: String,
pub model_label: String,
pub is_estimate: bool,
pub coverage_pct: f64,
pub compatible_miles: f64,
pub total_native_miles: f64,
pub compatible_days: i32,
pub unavailable_factors: Vec<String>,
```

Group drives by local start date, sum factor numerators/denominators inside each day, score eligible days, and call `mileage_weighted_score`. Keep incompatible daily rows visible with `eligible = false` and `score = None`. Return `score = None` and the model/coverage explanation when no day qualifies.

- [ ] **Step 4: Run analytics/API tests and verify they pass**

Run the command from Step 2.

Expected: drives and API tests pass.

- [ ] **Step 5: Commit analytics contract**

```bash
git add crates/drives/src/grouper.rs crates/drives/src/types.rs crates/api/src/drives_handler.rs
git commit -m "feat: expose v2.2 estimate coverage"
```

### Task 7: Update web types, copy, coverage, and drive-card semantics

**Files:**
- Modify: `web/src/lib/api.ts`
- Modify: `web/src/types/drives.ts`
- Modify: `web/src/pages/SafetyScore.tsx`
- Modify: `web/src/pages/SafetyScore.test.tsx`
- Modify: `web/src/components/drives/DriveRow.tsx`

**Interfaces:**
- Produces a UI that labels the model as an estimate, lists unavailable factors, shows compatible/total mileage and coverage, and explains why history may be excluded.
- Consumes the camelCase API fields from Task 6 while retaining snake_case normalization for older deployed responses.

- [ ] **Step 1: Add failing web tests for labeling and removed FSD relief**

Render a compatible response and assert `Tesla v2.2 Estimate`, coverage, compatible miles, unavailable factors, and non-additive `Estimated impact` copy. Assert the old “trims ... penalties” text is absent and FSD usage remains visible. Render an incompatible response and assert `Not enough compatible telemetry` plus historical-IMU guidance. Preserve the existing period-persistence test.

- [ ] **Step 2: Run web tests and verify the new contract fails**

Run: `npm test` from `web`.

Expected: model/coverage/disclosure assertions fail against the old page.

- [ ] **Step 3: Implement the typed normalization and UI**

Add fields matching Task 6 to `SafetyAnalytics`, `SafetyDayStats`, and drive types. Normalize both camelCase and snake_case keys. Replace generic Safety Score headings where space permits, remove all FSD-relief callouts, retain FSD share as informational usage, label legacy penalty values `Estimated impact` with text saying they are not additive, and show the unavailable-factor list in help content. Keep the shield card only when `safetyScore` is numeric; add an accessible title identifying it as a Tesla v2.2 estimate.

- [ ] **Step 4: Run web tests, lint, and build**

Run: `npm test && npm run lint && npm run build` from `web`.

Expected: all commands exit 0 and `web/dist/index.html` exists and is non-empty.

- [ ] **Step 5: Commit the web update**

```bash
git add web/src/lib/api.ts web/src/types/drives.ts web/src/pages/SafetyScore.tsx web/src/pages/SafetyScore.test.tsx web/src/components/drives/DriveRow.tsx
git commit -m "feat: label and explain v2.2 safety estimate"
```

### Task 8: Verify upgrades, fresh installs, regressions, and the audited score

**Files:**
- Correction scope is restricted to the files named in Tasks 1-7
- Review: `docs/superpowers/specs/2026-08-31-tesla-v2-2-estimate-design.md`

**Interfaces:**
- Produces release-ready evidence; no production Pi writes.
- Consumes the complete implementation.

- [ ] **Step 1: Run formatting and focused Rust verification**

Run:

```bash
wsl bash -lc 'cd "/mnt/c/Nextcloud/Documents/Visual Studio Code/Sentry-Six-Assets/Sentry-USB-Rusty" && cargo fmt --check && cargo test -p sentryusb-drives && cargo test -p sentryusb-api'
```

Expected: all commands exit 0.

- [ ] **Step 2: Run full Rust workspace verification in WSL**

Run:

```bash
wsl bash -lc 'cd "/mnt/c/Nextcloud/Documents/Visual Studio Code/Sentry-Six-Assets/Sentry-USB-Rusty" && cargo test --workspace'
```

Expected: all workspace tests pass.

- [ ] **Step 3: Run complete web verification**

Run: `npm test && npm run lint && npm run build` from `web`.

Expected: all commands exit 0 and the production bundle is generated.

- [ ] **Step 4: Exercise both database cohorts**

Run the named fresh-v22 and v21-upgrade tests with `--exact --nocapture`, then inspect test output to confirm the upgrade fixture does not include snapshot/MP4 files. Expected: both paths pass and raw blobs remain unchanged.

- [ ] **Step 5: Re-run the read-only score simulation against the Pi copy**

Copy the live database to a local temporary path using read-only SSH/SCP access, run the new code against the copy only, and compare the resulting score, compatible days, compatible miles, and factor percentages with the pre-implementation estimate of about 78, 11 days, and 761 miles. Do not migrate or modify `/backingfiles/drive-data.db` on the Pi.

- [ ] **Step 6: Inspect the final diff and commit any verification-only corrections**

Run: `git diff --check && git status --short && git log --oneline --decorate -12`.

Expected: no whitespace errors, no unrelated files, and each checkpoint commit is present.

- [ ] **Step 7: Update the Graphify index only with user approval**

Meaningful schema and scoring structure changed. Ask whether to run `/graphify --update`; do not rebuild silently.

### Task 9: Prepare the prerelease after verification

**Files:**
- No tracked version file: the installer version is derived from the Git tag
- Create temporarily, then remove after publication: `.release-notes-v3.21.4.md`

**Interfaces:**
- Produces a bumped prerelease tag and GitHub prerelease with stable-tone notes.
- Consumes clean verification evidence from Task 8.

- [ ] **Step 1: Confirm the release base and tag-derived version convention**

Run: `git describe --tags --abbrev=0 && gh release view v3.21.3 --json tagName,name,isPrerelease,targetCommitish,body,url`.

Expected: `v3.21.3` is the current prerelease, targets `main`, and no tracked source version file needs editing.

- [ ] **Step 2: Write the v3.21.4 user-facing notes**

Create `.release-notes-v3.21.4.md` with bullets describing the corrected braking sign, v2.2 estimate, coverage-based historical eligibility, exact assisted-interval exclusion, retained FSD analytics, and database-only recomputation, followed by `**Full Changelog**: https://github.com/Sentry-Six/Sentry-USB-Rusty/compare/v3.21.3...v3.21.4`. Do not claim exact Tesla parity.

- [ ] **Step 3: Re-run release verification**

Repeat Task 8 Steps 1-3 after the version change.

- [ ] **Step 4: Commit, tag, push, and create a GitHub prerelease**

Use `git tag -a v3.21.4 -m "v3.21.4"`, push the verified `main` commit and tag, then run `gh release create v3.21.4 --title v3.21.4 --prerelease --target main --notes-file .release-notes-v3.21.4.md`. Delete only the temporary notes file after GitHub readback succeeds.

- [ ] **Step 5: Read back remote state**

Verify `origin/main`, the exact remote tag target, prerelease status, title, and rendered notes URL. Report the commit SHA, tag, and release URL.
