//! BLOB encoders/decoders for the parallel-slice point data stored on each
//! route row.
//!
//! All formats are little-endian, fixed-stride. Wire format is **binary-
//! compatible with the Go implementation** so DBs can move between Go and
//! Rust builds freely, and `drive-data.json` exports from either side
//! round-trip through the DB without corruption.
//!
//! Invariants (match Go):
//!   * `encode_*(None)` returns `None` (the caller stores SQL NULL).
//!   * `decode_*(None)` returns `Ok(None)`.
//!   * Decoders reject inputs whose length isn't a multiple of the stride.
//!   * Float NaN/Inf bit patterns are preserved exactly.

use anyhow::{bail, Result};

use crate::types::{ApRun, FlagRun, GearRun, GpsPoint};
use crate::safety::{SafetyGracePrefix, SafetyPrefixBucket};

// -----------------------------------------------------------------------------
// Points: [f64; 2] per point (16 bytes)
// -----------------------------------------------------------------------------

const POINT_STRIDE: usize = 16; // 2 * f64

pub fn encode_points(pts: Option<&[GpsPoint]>) -> Option<Vec<u8>> {
    let pts = pts?;
    let mut buf = Vec::with_capacity(pts.len() * POINT_STRIDE);
    for p in pts {
        buf.extend_from_slice(&p[0].to_bits().to_le_bytes());
        buf.extend_from_slice(&p[1].to_bits().to_le_bytes());
    }
    Some(buf)
}

pub fn decode_points(buf: Option<&[u8]>) -> Result<Option<Vec<GpsPoint>>> {
    let Some(buf) = buf else { return Ok(None) };
    if buf.len() % POINT_STRIDE != 0 {
        bail!(
            "decode_points: length {} not a multiple of {}",
            buf.len(),
            POINT_STRIDE
        );
    }
    let n = buf.len() / POINT_STRIDE;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * POINT_STRIDE;
        let lat = f64::from_bits(u64::from_le_bytes(
            buf[off..off + 8].try_into().unwrap(),
        ));
        let lon = f64::from_bits(u64::from_le_bytes(
            buf[off + 8..off + 16].try_into().unwrap(),
        ));
        out.push([lat, lon]);
    }
    Ok(Some(out))
}

// -----------------------------------------------------------------------------
// u8 slices (gear states, autopilot states): identity encoding
// -----------------------------------------------------------------------------

pub fn encode_u8s(s: Option<&[u8]>) -> Option<Vec<u8>> {
    // Copy so the caller can mutate their slice without affecting what goes
    // to the DB driver — matches the defensive copy in Go.
    s.map(|s| s.to_vec())
}

pub fn decode_u8s(buf: Option<&[u8]>) -> Option<Vec<u8>> {
    buf.map(|b| b.to_vec())
}

// -----------------------------------------------------------------------------
// f32 slices (speeds, accel positions): 4 bytes per value
// -----------------------------------------------------------------------------

const F32_STRIDE: usize = 4;

pub fn encode_f32s(s: Option<&[f32]>) -> Option<Vec<u8>> {
    let s = s?;
    let mut buf = Vec::with_capacity(s.len() * F32_STRIDE);
    for v in s {
        buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    Some(buf)
}

pub fn decode_f32s(buf: Option<&[u8]>) -> Result<Option<Vec<f32>>> {
    let Some(buf) = buf else { return Ok(None) };
    if buf.len() % F32_STRIDE != 0 {
        bail!(
            "decode_f32s: length {} not a multiple of {}",
            buf.len(),
            F32_STRIDE
        );
    }
    let n = buf.len() / F32_STRIDE;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * F32_STRIDE;
        let v = f32::from_bits(u32::from_le_bytes(
            buf[off..off + 4].try_into().unwrap(),
        ));
        out.push(v);
    }
    Ok(Some(out))
}

// -----------------------------------------------------------------------------
// GearRuns: 1-byte gear + 4-byte i32 frames per run
// -----------------------------------------------------------------------------

const GEAR_RUN_STRIDE: usize = 5; // u8 + i32

pub fn encode_gear_runs(runs: Option<&[GearRun]>) -> Option<Vec<u8>> {
    let runs = runs?;
    let mut buf = Vec::with_capacity(runs.len() * GEAR_RUN_STRIDE);
    for r in runs {
        buf.push(r.gear);
        // Frames fits in i32; explicitly cast to stabilize the wire format
        // across 32/64-bit builds (explicit int32 conversion).
        let frames = r.frames as i32;
        buf.extend_from_slice(&frames.to_le_bytes());
    }
    Some(buf)
}

pub fn decode_gear_runs(buf: Option<&[u8]>) -> Result<Option<Vec<GearRun>>> {
    let Some(buf) = buf else { return Ok(None) };
    if buf.len() % GEAR_RUN_STRIDE != 0 {
        bail!(
            "decode_gear_runs: length {} not a multiple of {}",
            buf.len(),
            GEAR_RUN_STRIDE
        );
    }
    let n = buf.len() / GEAR_RUN_STRIDE;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * GEAR_RUN_STRIDE;
        let gear = buf[off];
        let frames = i32::from_le_bytes(buf[off + 1..off + 5].try_into().unwrap()) as u32;
        out.push(GearRun { gear, frames });
    }
    Ok(Some(out))
}

// -----------------------------------------------------------------------------
// ApRuns: 1-byte autopilot state + 4-byte i32 frames per run
//
// Deliberately the GEAR-run wire shape, not the flag-run one: an ap run
// carries no per-run speed, so stride 5 rather than 9.
// -----------------------------------------------------------------------------

const AP_RUN_STRIDE: usize = 5; // u8 + i32

pub fn encode_ap_runs(runs: Option<&[ApRun]>) -> Option<Vec<u8>> {
    let runs = runs?;
    let mut buf = Vec::with_capacity(runs.len() * AP_RUN_STRIDE);
    for r in runs {
        buf.push(r.ap);
        // Frames fits in i32; explicitly cast to stabilize the wire format
        // across 32/64-bit builds (explicit int32 conversion).
        let frames = r.frames as i32;
        buf.extend_from_slice(&frames.to_le_bytes());
    }
    Some(buf)
}

pub fn decode_ap_runs(buf: Option<&[u8]>) -> Result<Option<Vec<ApRun>>> {
    let Some(buf) = buf else { return Ok(None) };
    if buf.len() % AP_RUN_STRIDE != 0 {
        bail!(
            "decode_ap_runs: length {} not a multiple of {}",
            buf.len(),
            AP_RUN_STRIDE
        );
    }
    let n = buf.len() / AP_RUN_STRIDE;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * AP_RUN_STRIDE;
        let ap = buf[off];
        let frames = i32::from_le_bytes(buf[off + 1..off + 5].try_into().unwrap()) as u32;
        out.push(ApRun { ap, frames });
    }
    Ok(Some(out))
}

// -----------------------------------------------------------------------------
// Tesla v2.2 estimate grace prefix: version + 5 one-second buckets.
// Each bucket stores five little-endian i64 factor durations.
// -----------------------------------------------------------------------------

const SAFETY_GRACE_PREFIX_VERSION: u8 = 1;
const SAFETY_GRACE_BUCKET_FIELDS: usize = 5;
const SAFETY_GRACE_PREFIX_LEN: usize =
    1 + 5 * SAFETY_GRACE_BUCKET_FIELDS * std::mem::size_of::<i64>();

pub fn encode_safety_grace_prefix(prefix: &SafetyGracePrefix) -> Vec<u8> {
    let mut out = Vec::with_capacity(SAFETY_GRACE_PREFIX_LEN);
    out.push(SAFETY_GRACE_PREFIX_VERSION);
    for bucket in prefix {
        for value in [
            bucket.hard_brake_ms,
            bucket.brake_any_ms,
            bucket.aggr_turn_ms,
            bucket.turn_any_ms,
            bucket.speeding_ms,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out
}

pub fn decode_safety_grace_prefix(buf: Option<&[u8]>) -> SafetyGracePrefix {
    let Some(buf) = buf else { return SafetyGracePrefix::default() };
    if buf.len() != SAFETY_GRACE_PREFIX_LEN || buf[0] != SAFETY_GRACE_PREFIX_VERSION {
        return SafetyGracePrefix::default();
    }
    let mut prefix = SafetyGracePrefix::default();
    let mut offset = 1;
    for bucket in &mut prefix {
        let mut next = || {
            let value = i64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap());
            offset += 8;
            value
        };
        *bucket = SafetyPrefixBucket {
            hard_brake_ms: next(),
            brake_any_ms: next(),
            aggr_turn_ms: next(),
            turn_any_ms: next(),
            speeding_ms: next(),
        };
    }
    prefix
}

// -----------------------------------------------------------------------------
// FlagRuns: 1-byte flags + 4-byte i32 frames + 4-byte f32 max_mps per run
// (gear-run wire shape plus the per-run |SEI speed| max; NaN encodes an
// absent max_mps so legacy no-speed runs survive the round-trip as None)
// -----------------------------------------------------------------------------

const FLAG_RUN_STRIDE: usize = 9; // u8 + i32 + f32

pub fn encode_flag_runs(runs: Option<&[FlagRun]>) -> Option<Vec<u8>> {
    let runs = runs?;
    let mut buf = Vec::with_capacity(runs.len() * FLAG_RUN_STRIDE);
    for r in runs {
        buf.push(r.flags);
        let frames = r.frames as i32;
        buf.extend_from_slice(&frames.to_le_bytes());
        let max = r.max_mps.map(|m| m as f32).unwrap_or(f32::NAN);
        buf.extend_from_slice(&max.to_le_bytes());
    }
    Some(buf)
}

pub fn decode_flag_runs(buf: Option<&[u8]>) -> Result<Option<Vec<FlagRun>>> {
    let Some(buf) = buf else { return Ok(None) };
    if buf.len() % FLAG_RUN_STRIDE != 0 {
        bail!(
            "decode_flag_runs: length {} not a multiple of {}",
            buf.len(),
            FLAG_RUN_STRIDE
        );
    }
    let n = buf.len() / FLAG_RUN_STRIDE;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * FLAG_RUN_STRIDE;
        let flags = buf[off];
        let frames = i32::from_le_bytes(buf[off + 1..off + 5].try_into().unwrap()) as u32;
        let max_raw = f32::from_le_bytes(buf[off + 5..off + 9].try_into().unwrap());
        // Values are 1-decimal by contract (computeFlagRuns rounds); re-round
        // after the f32 round-trip so 2.7 comes back as exactly 2.7, not
        // 2.700000047… — the JSON export must match Sentry-Drive's digits.
        let max_mps = if max_raw.is_finite() {
            Some(((max_raw as f64) * 10.0).round() / 10.0)
        } else {
            None
        };
        out.push(FlagRun { flags, frames, max_mps });
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::{SafetyGracePrefix, SafetyPrefixBucket};

    #[test]
    fn roundtrip_points() {
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [40.7128, -74.0060]];
        let buf = encode_points(Some(&pts)).unwrap();
        assert_eq!(buf.len(), 2 * POINT_STRIDE);
        let back = decode_points(Some(&buf)).unwrap().unwrap();
        assert_eq!(back, pts);
    }

    #[test]
    fn roundtrip_empty_points() {
        let pts: Vec<GpsPoint> = vec![];
        let buf = encode_points(Some(&pts)).unwrap();
        assert_eq!(buf.len(), 0);
        let back = decode_points(Some(&buf)).unwrap().unwrap();
        assert_eq!(back, pts);
    }

    #[test]
    fn none_roundtrips_to_none() {
        assert!(encode_points(None).is_none());
        assert!(decode_points(None).unwrap().is_none());
        assert!(encode_f32s(None).is_none());
        assert!(decode_f32s(None).unwrap().is_none());
        assert!(encode_gear_runs(None).is_none());
        assert!(decode_gear_runs(None).unwrap().is_none());
        assert!(encode_flag_runs(None).is_none());
        assert!(decode_flag_runs(None).unwrap().is_none());
    }

    #[test]
    fn decode_points_rejects_misaligned_input() {
        let buf = vec![0u8; 15];
        assert!(decode_points(Some(&buf)).is_err());
    }

    #[test]
    fn decode_f32s_rejects_misaligned_input() {
        let buf = vec![0u8; 7];
        assert!(decode_f32s(Some(&buf)).is_err());
    }

    #[test]
    fn decode_gear_runs_rejects_misaligned_input() {
        let buf = vec![0u8; 6];
        assert!(decode_gear_runs(Some(&buf)).is_err());
    }

    #[test]
    fn roundtrip_f32s() {
        let s = vec![0.0f32, 1.5, -42.0, f32::INFINITY, f32::NAN];
        let buf = encode_f32s(Some(&s)).unwrap();
        let back = decode_f32s(Some(&buf)).unwrap().unwrap();
        assert_eq!(back.len(), s.len());
        for (a, b) in s.iter().zip(back.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "NaN bit pattern must round-trip");
        }
    }

    #[test]
    fn roundtrip_gear_runs() {
        let runs = vec![
            GearRun { gear: 0, frames: 60 },
            GearRun { gear: 4, frames: 1200 },
            GearRun { gear: 2, frames: 30 },
        ];
        let buf = encode_gear_runs(Some(&runs)).unwrap();
        let back = decode_gear_runs(Some(&buf)).unwrap().unwrap();
        assert_eq!(back.len(), runs.len());
        for (a, b) in runs.iter().zip(back.iter()) {
            assert_eq!(a.gear, b.gear);
            assert_eq!(a.frames, b.frames);
        }
    }

    #[test]
    fn roundtrip_flag_runs() {
        // Mixed per-run max_mps states: known 1-decimal values must come
        // back exact (decode re-rounds after the f32 leg), zero stays a
        // real Some(0.0), and None survives via the NaN sentinel.
        let runs = vec![
            FlagRun { flags: 0, frames: 27, max_mps: Some(0.0) },
            FlagRun { flags: 3, frames: 123, max_mps: Some(2.7) },
            FlagRun { flags: 1, frames: 873, max_mps: None },
            FlagRun { flags: 8, frames: 36, max_mps: Some(4.7) },
        ];
        let buf = encode_flag_runs(Some(&runs)).unwrap();
        assert_eq!(buf.len(), runs.len() * FLAG_RUN_STRIDE);
        let back = decode_flag_runs(Some(&buf)).unwrap().unwrap();
        assert_eq!(back, runs);
    }

    #[test]
    fn roundtrip_ap_runs() {
        // Every autopilot state, including the Off runs that bracket a
        // maneuver — the detector counts those toward the FSD share
        // denominator, so they must survive. HW3 clips are a single
        // all-Off run, which must round-trip just as faithfully.
        let runs = vec![
            ApRun { ap: 0, frames: 40 },
            ApRun { ap: 1, frames: 513 },
            ApRun { ap: 2, frames: 7 },
            ApRun { ap: 3, frames: 1 },
        ];
        let buf = encode_ap_runs(Some(&runs)).unwrap();
        assert_eq!(buf.len(), runs.len() * AP_RUN_STRIDE);
        assert_eq!(AP_RUN_STRIDE, GEAR_RUN_STRIDE, "ap runs use the gear wire shape");
        assert_eq!(decode_ap_runs(Some(&buf)).unwrap().unwrap(), runs);

        let hw3 = vec![ApRun { ap: 0, frames: 1214 }];
        let buf = encode_ap_runs(Some(&hw3)).unwrap();
        assert_eq!(decode_ap_runs(Some(&buf)).unwrap().unwrap(), hw3);
    }

    #[test]
    fn decode_ap_runs_rejects_misaligned_input() {
        let buf = vec![0u8; 6];
        assert!(decode_ap_runs(Some(&buf)).is_err());
        assert!(encode_ap_runs(None).is_none());
        assert!(decode_ap_runs(None).unwrap().is_none());
    }

    #[test]
    fn decode_flag_runs_rejects_misaligned_input() {
        let buf = vec![0u8; 6];
        assert!(decode_flag_runs(Some(&buf)).is_err());
    }

    #[test]
    fn safety_grace_prefix_roundtrips_all_five_buckets() {
        let mut prefix = SafetyGracePrefix::default();
        prefix[0] = SafetyPrefixBucket {
            hard_brake_ms: 1,
            brake_any_ms: 2,
            aggr_turn_ms: 3,
            turn_any_ms: 4,
            speeding_ms: 5,
        };
        prefix[4] = SafetyPrefixBucket {
            hard_brake_ms: 101,
            brake_any_ms: 102,
            aggr_turn_ms: 103,
            turn_any_ms: 104,
            speeding_ms: 105,
        };

        let encoded = encode_safety_grace_prefix(&prefix);

        assert_eq!(decode_safety_grace_prefix(Some(&encoded)), prefix);
    }

    #[test]
    fn malformed_safety_grace_prefix_fails_closed_to_empty() {
        assert_eq!(decode_safety_grace_prefix(None), SafetyGracePrefix::default());
        assert_eq!(decode_safety_grace_prefix(Some(&[])), SafetyGracePrefix::default());
        assert_eq!(decode_safety_grace_prefix(Some(&[99; 201])), SafetyGracePrefix::default());
    }

    #[test]
    fn go_compat_point_bytes() {
        // Bit-level golden: encode one known point and verify the exact 16
        // bytes a Go encoder would produce. Guards against silent endian /
        // stride drift on any platform.
        let pts: Vec<GpsPoint> = vec![[1.0, 2.0]];
        let buf = encode_points(Some(&pts)).unwrap();
        // 1.0 f64 LE = 00 00 00 00 00 00 F0 3F
        // 2.0 f64 LE = 00 00 00 00 00 00 00 40
        assert_eq!(
            buf,
            vec![
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40,
            ]
        );
    }
}
