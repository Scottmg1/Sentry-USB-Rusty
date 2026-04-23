//! BLOB encoders/decoders for the parallel-slice point data stored on each
//! route row. Line-by-line port of Go `server/drives/blob.go`.
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

use crate::types::{GearRun, GpsPoint};

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
        // across 32/64-bit builds (matches Go's int32 explicit conversion).
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

#[cfg(test)]
mod tests {
    use super::*;

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
