# Tesla v2.2-Compatible Safety Score Estimate

**Status:** Approved design, pending implementation
**Date:** 2026-08-31

## Summary

Replace the current custom subtractive Safety Score with a versioned **Tesla v2.2 Estimate**. The estimate will use Tesla's published v2.2 Predicted Collision Frequency (PCF) coefficients for the factors that SentryUSB can measure honestly from Tesla dashcam SEI telemetry. Factors unavailable in the clips will be omitted from the equation, never synthesized or silently treated as observed.

This remains an estimate rather than Tesla's insurance score. The API and UI must say so explicitly.

The change must be safe for both existing and new installations. Existing installations will recompute compatible route aggregates from telemetry blobs already stored in `drive-data.db`; users will not need old snapshots or MP4 files. Historical rows that lack measured acceleration will be marked incompatible and excluded from the estimate instead of contributing zero-risk behavior.

## Goals

- Follow Tesla Safety Score v2.2 definitions and PCF math wherever the stored SEI channels support them.
- Fix the measured longitudinal-acceleration sign error.
- Stop treating missing historical acceleration as clean driving.
- Preserve the Drives API shape where practical and add explicit model and coverage metadata.
- Keep deleting already-processed snapshots independent from the score.
- Migrate existing databases additively and make clean installations produce the same schema and behavior.
- Preserve period selection, including the product-specific `all` period.

## Non-goals

- Claim parity with Tesla's private insurance score.
- Infer unsafe following, forced Autopilot disengagement, seatbelt state, lead-vehicle relative speeding, or yellow-traffic-light detection without telemetry evidence.
- Reconstruct acceleration for historical rows whose MP4 and acceleration blobs no longer exist.
- Keep the current custom penalty weights or FSD relief for compatibility.

## Model identity and source

The model identifier is `tesla-v2.2-estimate-1` and must be returned by the API. Tesla v2.2 constants are pinned in code with tests and a source comment referencing Tesla's published v2.2 documentation captured on 2025-04-07.

The UI label is **Tesla v2.2 Estimate**. Help text must state that unavailable factors are omitted and that this score cannot be expected to match the Tesla app.

## Included and unavailable factors

### Included

1. Hard Braking
2. Aggressive Turning
3. Excessive Speeding above 85 mph
4. Weighted Late-Night Driving

### Unavailable and omitted

1. Unsafe Following
2. Forced Autopilot Disengagement
3. Unbuckled Driving
4. Speeding relative to a lead vehicle
5. Yellow-traffic-light braking exemption

Omitting a PCF factor is mathematically equivalent to using exponent zero for that term. This makes the estimate optimistic, which is why model labeling and the unavailable-factor list are required.

## Official PCF and score conversion

For the observable factors:

```text
PCF = 0.57198191
    * 1.23599110 ^ HardBrakingPct
    * 1.01219290 ^ AggressiveTurningPct
    * 1.03231810 ^ WeightedLateNightPct
    * 1.02439511 ^ ExcessiveSpeedingPct

SafetyScore = clamp(122.15240383 - 38.72920381 * PCF, 0, 100)
```

Each exponent is a percentage value, not a 0-to-1 fraction. Cap the exponent before applying the PCF term:

| Factor | Tesla v2.2 cap |
| --- | ---: |
| Hard Braking | 5.2% |
| Aggressive Turning | 13.2% |
| Weighted Late-Night Driving | 14.2% |
| Excessive Speeding | 10.0% |

Remove the existing subtractive weights, gamma curve, denominator floor, and assisted-mile FSD relief from the scoring path.

## Factor semantics

### Hard Braking

- Use measured SEI longitudinal acceleration when both acceleration arrays align with the route's point array.
- Positive `accel_y` is deceleration in the stored Tesla SEI samples. The current negation is removed.
- Hard braking numerator: eligible time above 0.3g backward acceleration.
- Denominator: eligible time above 0.1g backward acceleration.
- The displayed factor is numerator divided by denominator, expressed as a percentage and capped at 5.2 for PCF.
- The GPS/speed-derived fallback may remain available for diagnostics, but it does not satisfy v2.2 compatibility coverage and cannot make a historical drive scoreable.

### Aggressive Turning

- Use the absolute measured SEI lateral acceleration.
- Numerator: eligible time above 0.4g lateral acceleration.
- Denominator: eligible time above 0.2g lateral acceleration.
- Express as a percentage and cap at 13.2 for PCF.
- Correct the fallback Menger-curvature side construction even though fallback output does not qualify as measured coverage.

### Excessive Speeding

- Count eligible moving time strictly above 85 mph using the SEI speed channel.
- Divide by total moving time for the scored day, express as a percentage, and cap at 10.0 for PCF.
- Exclude speeding while Autopilot is active and during the post-disengagement grace period.
- The lead-vehicle-relative component is omitted because lead speed and distance are not present in the clips.

### Weighted Late-Night Driving

- Use moving seconds, not distance.
- Include all moving time, including Autopilot, from 11 PM through 4 AM.
- Apply Tesla's published hourly weights:

| Local interval | Weight |
| --- | ---: |
| 11 PM-midnight | 0.21 |
| midnight-1 AM | 0.53 |
| 1-2 AM | 0.71 |
| 2-3 AM | 0.82 |
| 3-4 AM | 1.00 |

- Weighted late-night percentage is weighted late-night moving milliseconds divided by total moving milliseconds, multiplied by 100 and capped at 14.2 for PCF.
- Clip filename timestamps are local wall-clock evidence. A route that crosses an hour boundary is classified at sample/bucket granularity rather than charging its whole minute to the starting hour.

## Autopilot eligibility

Hard braking, aggressive turning, and excessive speeding are excluded:

- while FSD, Autosteer, or TACC is active; and
- for five seconds after an assisted mode disengages.

Tesla's TACC accelerator exception is supported: behavior while TACC is active counts when the driver accelerator channel is above the existing 1% pedal threshold.

The five-second grace must survive a 60-second clip boundary. Persist compact boundary data rather than loading route blobs in request handlers:

- Autopilot mode at the route end.
- Grace milliseconds remaining at the route end.
- A compact five-second prefix bucket blob containing the factor milliseconds that may need removal when grace carries into the next route.

The grouper consumes the previous route's remaining grace against the next route's one-second prefix buckets. A maximum five-second grace can cross at most one full 60-second route boundary.

## Coverage and eligibility

Add `safety_imu_moving_ms` to route aggregates. It counts moving milliseconds where aligned measured longitudinal and lateral acceleration are available.

For each drive and day:

```text
coveragePct = 100 * imuMovingMs / movingMs
```

- A drive/day is compatible when coverage is at least 90% and normal minimum-driving requirements are met.
- Match Tesla's trip floor: exclude drives shorter than 0.1 miles. Remove the current custom 0.5-mile/60-second scoring floor from the v2.2 path.
- Incompatible drives/days return no estimate and must not contribute zero-valued hard-braking or turning factors.
- Period analytics use only compatible days and expose both included and available totals.
- The UI displays compatible miles, total native miles, compatible days, and coverage percentage.
- A period with no compatible days displays `Not enough compatible telemetry`.
- Imported drives and Summon sessions remain excluded.

The 90% threshold is deliberately based on moving time, so stationary samples and omitted protobuf zeros cannot make a usable drive fail coverage.

## Daily and period aggregation

Tesla calculates a daily PCF score and mileage-weights daily scores for its aggregate. Match that behavior:

1. Sum factor numerators and denominators for compatible native, non-Summon drives of at least 0.1 miles on one local calendar day.
2. Calculate that day's capped factors, PCF, and score.
3. Mileage-weight eligible daily scores for `week`, `month`, and `all`.
4. `day` returns the selected/current day's result directly.

The product's `all` period remains an intentional extension beyond Tesla's normal rolling display. It applies the same daily mileage-weighting across all compatible retained days.

Per-drive card scores use the same PCF model on that drive's totals when its coverage is at least 90%.

## Persistence and migration

Create an additive schema migration from v21 to v22. New route columns are nullable so interrupted upgrades remain recoverable. At minimum, persist:

- `ap_at_end`
- `safety_imu_moving_ms`
- `safety_grace_ms_end`
- `safety_grace_prefix_blob`

Update the exact column list after implementation-level review if an existing boundary column safely covers the same invariant; do not repurpose columns with different semantics.

Bump both version gates:

- Route aggregate formula version, so compatible values are recomputed from stored route blobs.
- Drive-list cache algorithm version, so cached summaries cannot retain the old score.

Upgrade behavior:

- Existing routes with stored speed, AP, pedal, and acceleration blobs are recomputed without opening MP4 files.
- Existing routes without measured acceleration get `safety_imu_moving_ms = 0` and become incompatible.
- Missing snapshots do not fail migration.
- Formula backfill remains batched and restart-safe.
- A downgrade may recompute the previous formula and a later upgrade may recompute v2.2 again; neither path corrupts the raw blobs.

New-install behavior:

- The fresh schema contains v22 columns.
- Newly processed Tesla clips populate all v2.2 aggregates on first insertion.
- Imported and malformed routes remain unscoreable without failing ingestion.

## API compatibility

Retain existing score and factor percentage fields where their meaning remains valid. Add:

- `modelId`
- `modelLabel`
- `isEstimate`
- `coveragePct`
- `compatibleMiles`
- `totalNativeMiles`
- `compatibleDays`
- `unavailableFactors`
- daily `coveragePct` and eligibility state

The current additive `*Penalty` fields do not map to multiplicative PCF terms. Keep them temporarily for wire compatibility, mark them deprecated, and populate them with leave-one-factor-out score impact values. The UI must label these as estimated impacts and must not sum them.

Clients that know only the old fields continue to deserialize successfully. Updated clients use `modelId` and the explicit factor list.

## UI behavior

- Replace generic `Safety Score` copy with `Tesla v2.2 Estimate` where space permits.
- Keep the shield score on eligible drive cards.
- Add a coverage indicator or tooltip on the analytics page.
- Explain that missing factors are omitted and list them.
- Explain when historical miles are excluded because measured acceleration was not stored.
- Do not suggest restoring deleted snapshots.
- Preserve period selection, including the previously added persistence for `all`.

## Verification strategy

Implementation follows test-driven development.

### Unit tests

- Longitudinal sign: positive measured Y counts deceleration; negative Y does not.
- Official thresholds, percentage units, caps, PCF coefficients, and score conversion.
- Official late-night hours and five weights, including hour boundaries.
- Autopilot exclusion, five-second in-clip grace, cross-clip grace, and TACC pedal exception.
- Speeding excludes assisted samples and counts only above 85 mph.
- Correct Menger-curvature geometry.
- Coverage threshold behavior at 89.9%, 90%, and 100%.
- Tesla's 0.1-mile trip exclusion and removal of the old custom score floor.
- Daily PCF followed by mileage-weighted period aggregation.
- Missing factors contribute no invented measurements.

### Database tests

- Fresh v22 database.
- v21-to-v22 migration with compatible blobs.
- v21-to-v22 migration without acceleration blobs or original MP4 files.
- Interrupted/repeated migration is idempotent.
- Aggregate formula gate recomputes route values.
- Cache gate rebuilds old summaries.

### API and web tests

- Existing response fields remain deserializable.
- New model, coverage, and unavailable-factor fields are present.
- Ineligible periods and drives render without a fake score.
- Eligible cards and daily/period analytics use the same formula.
- Period persistence remains intact.

### Full verification

- Focused `sentryusb-drives` tests.
- Full Rust workspace tests in Linux/WSL as required by this repository.
- Web unit tests, type checking, and production build.
- Migration test from a v21 fixture and fresh-install test from an empty database.
- Read-only simulation against the user's Pi database before release; no production database writes during validation.

## Expected effect on the audited installation

The read-only simulation performed before implementation estimates an aggregate score near 78 using 11 compatible days and approximately 761 compatible miles. This is validation guidance, not a golden test value. Exact grouping, boundary grace, trip eligibility, and rounding may move the installed result by roughly one point.

## Risks and mitigations

- **Partial model appears official:** Always include `Estimate`, model ID, coverage, and unavailable-factor disclosure.
- **Historical score changes sharply:** Show compatible versus total mileage and exclude missing data instead of silently scoring it.
- **Migration delays startup:** Reuse the existing batched, restart-safe aggregate backfill.
- **Cross-clip grace regresses performance:** Persist compact prefix/boundary aggregates; do not decode full route blobs in API request paths.
- **Tesla changes its model:** Pin this implementation as v2.2; a future model becomes a new version rather than silently changing constants.
- **Older clients assume additive penalties:** Preserve fields temporarily and document that their replacement values are leave-one-out impacts.

## Acceptance criteria

1. An existing v21 database upgrades without snapshots and retains raw route data.
2. A fresh database reaches the same v22 schema and scoring behavior.
3. Deleting a processed snapshot does not change an eligible score.
4. Missing acceleration makes history visibly incompatible rather than clean.
5. The published v2.2 equations and observable factor definitions are pinned by tests.
6. Unavailable factors are disclosed and never inferred.
7. The current custom FSD relief and subtractive score path no longer determine displayed scores.
8. All focused, migration, full Rust, and web verification pass before release.
