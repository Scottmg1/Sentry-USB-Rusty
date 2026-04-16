//! MP4 mdat scanning, H.264 NAL parsing, Tesla SEI protobuf GPS extraction.
//!
//! Extracts GPS coordinates, gear state, autopilot state, speed, and
//! accelerator pedal position from Tesla dashcam MP4 files.
//!
//! Memory-efficient: reads the mdat box in chunks rather than loading the
//! entire file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, Result};

use crate::types::{ExtractedGps, GearRun, GpsPoint};

/// Gear constants matching Tesla's SeiMetadata.Gear enum.
pub const GEAR_PARK: u8 = 0;
pub const GEAR_DRIVE: u8 = 1;
pub const GEAR_REVERSE: u8 = 2;
pub const GEAR_NEUTRAL: u8 = 3;

/// Autopilot state constants matching Tesla's Dashcam.proto.
pub const AUTOPILOT_OFF: u8 = 0;
pub const AUTOPILOT_FSD: u8 = 1;
pub const AUTOPILOT_AUTOSTEER: u8 = 2;
pub const AUTOPILOT_TACC: u8 = 3;

/// Extract GPS data from a Tesla dashcam MP4 file.
///
/// Reads the mdat box, scans for H.264 SEI NAL units containing Tesla's
/// custom protobuf GPS data, and returns deduplicated GPS points with
/// gear state, autopilot state, speed, and accelerator position.
pub fn extract_gps_from_file(path: &str) -> Result<ExtractedGps> {
    let mut f = File::open(path)
        .with_context(|| format!("failed to open MP4 file: {}", path))?;

    let (mdat_offset, mdat_size) = find_mdat_box(&mut f)?;
    if mdat_size == 0 {
        return Ok(ExtractedGps::empty());
    }

    let (mut points, mut gears, mut ap_states, mut speeds, mut accel_positions) =
        extract_from_mdat(&mut f, mdat_offset, mdat_size)?;

    // Deduplicate consecutive identical GPS points
    dedup_consecutive(&mut points, &mut gears, &mut ap_states, &mut speeds, &mut accel_positions);

    // Compute gear runs
    let gear_runs = compute_gear_runs(&gears);

    // Count raw park frames
    let raw_park_count = gears.iter().filter(|&&g| g == GEAR_PARK).count() as u32;
    let raw_frame_count = gears.len() as u32;

    Ok(ExtractedGps {
        points,
        gear_states: gears,
        autopilot_states: ap_states,
        speeds,
        accel_positions,
        raw_park_count,
        raw_frame_count,
        gear_runs,
    })
}

/// Scan MP4 top-level boxes to find the mdat box.
/// Returns (data_offset, data_size) where data_offset is after the box header.
fn find_mdat_box(f: &mut File) -> Result<(u64, u64)> {
    let file_size = f.metadata()?.len();
    let mut pos: u64 = 0;
    let mut header = [0u8; 16];

    while pos < file_size {
        f.seek(SeekFrom::Start(pos))?;
        if f.read(&mut header[..8])? < 8 {
            break;
        }

        let mut box_size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let is_mdat = header[4..8] == *b"mdat";
        let mut header_size: u64 = 8;

        if box_size == 1 {
            // Extended size (64-bit)
            f.seek(SeekFrom::Start(pos + 8))?;
            if f.read(&mut header[8..16])? < 8 {
                break;
            }
            box_size = u64::from_be_bytes([
                header[8], header[9], header[10], header[11],
                header[12], header[13], header[14], header[15],
            ]);
            header_size = 16;
        } else if box_size == 0 {
            box_size = file_size - pos;
        }

        if is_mdat {
            return Ok((pos + header_size, box_size - header_size));
        }

        if box_size < 8 {
            break;
        }
        pos += box_size;
    }

    Ok((0, 0))
}

/// Reads through the mdat box parsing NAL units and extracting GPS from SEI.
/// Uses a 64KB read buffer to avoid loading large mdat sections into memory.
fn extract_from_mdat(
    f: &mut File,
    offset: u64,
    size: u64,
) -> Result<(Vec<GpsPoint>, Vec<u8>, Vec<u8>, Vec<f32>, Vec<f32>)> {
    const BUF_SIZE: u64 = 64 * 1024;

    let mut points = Vec::new();
    let mut gears = Vec::new();
    let mut ap_states = Vec::new();
    let mut speeds = Vec::new();
    let mut accel_positions = Vec::new();

    let end = offset + size;
    let mut cursor = offset;
    let mut size_buf = [0u8; 4];

    while cursor + 4 <= end {
        // Read NAL size (4 bytes, big-endian)
        f.seek(SeekFrom::Start(cursor))?;
        if f.read(&mut size_buf)? < 4 {
            break;
        }
        cursor += 4;

        let nal_size = u32::from_be_bytes(size_buf) as u64;
        if nal_size < 2 || cursor + nal_size > end {
            break;
        }

        // Read NAL type byte
        let mut type_buf = [0u8; 1];
        f.seek(SeekFrom::Start(cursor))?;
        if f.read(&mut type_buf)? < 1 {
            break;
        }

        let nal_type = type_buf[0] & 0x1F;

        // NAL type 6 = SEI
        if nal_type == 6 && nal_size <= BUF_SIZE {
            let mut nal = vec![0u8; nal_size as usize];
            f.seek(SeekFrom::Start(cursor))?;
            if f.read_exact(&mut nal).is_ok() {
                if let Some((lat, lon, gear, ap_state, speed, accel_pos)) = parse_tesla_sei(&nal) {
                    points.push([
                        (lat * 1e6).round() / 1e6,
                        (lon * 1e6).round() / 1e6,
                    ]);
                    gears.push(gear);
                    ap_states.push(ap_state);
                    speeds.push(speed);
                    accel_positions.push(accel_pos);
                }
            }
        }

        cursor += nal_size;
    }

    Ok((points, gears, ap_states, speeds, accel_positions))
}

/// Finds the Tesla magic bytes (0x42...0x69) in a SEI NAL and decodes GPS + metadata.
fn parse_tesla_sei(nal: &[u8]) -> Option<(f64, f64, u8, u8, f32, f32)> {
    // Skip NAL header, look for 0x42 sequence followed by 0x69
    let mut i = 3;
    while i < nal.len() && nal[i] == 0x42 {
        i += 1;
    }
    if i <= 3 || i + 1 >= nal.len() || nal[i] != 0x69 {
        return None;
    }

    // Payload starts after 0x69, ends before trailing byte
    let mut payload = &nal[i + 1..];
    if payload.len() > 1 {
        payload = &payload[..payload.len() - 1];
    }

    let stripped = strip_emulation_bytes(payload);
    decode_sei_gps(&stripped)
}

/// Removes H.264 emulation prevention bytes (0x00 0x00 0x03 -> 0x00 0x00).
fn strip_emulation_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0u32;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

/// Decodes protobuf SeiMetadata to extract:
/// - latitude (field 11, double/fixed64)
/// - longitude (field 12, double/fixed64)
/// - gear_state (field 2, varint)
/// - autopilot_state (field 10, varint)
/// - vehicle_speed_mps (field 4, float/fixed32)
/// - accelerator_pedal_position (field 5, float/fixed32)
///
/// Hand-parses protobuf wire format to avoid external dependencies.
fn decode_sei_gps(data: &[u8]) -> Option<(f64, f64, u8, u8, f32, f32)> {
    let mut lat: f64 = 0.0;
    let mut lon: f64 = 0.0;
    let mut gear: u8 = 0;
    let mut ap_state: u8 = 0;
    let mut speed: f32 = 0.0;
    let mut accel_pos: f32 = 0.0;

    let mut i = 0;
    while i < data.len() {
        let (tag, n) = decode_varint(&data[i..]);
        if n == 0 {
            break;
        }
        i += n;

        let field_num = tag >> 3;
        let wire_type = tag & 0x7;

        match wire_type {
            0 => {
                // varint
                let (val, vn) = decode_varint(&data[i..]);
                if vn == 0 {
                    return None;
                }
                i += vn;
                if field_num == 2 {
                    gear = val as u8;
                } else if field_num == 10 {
                    ap_state = val as u8;
                }
            }
            1 => {
                // 64-bit (fixed64, double)
                if i + 8 > data.len() {
                    return None;
                }
                let bits = u64::from_le_bytes([
                    data[i], data[i + 1], data[i + 2], data[i + 3],
                    data[i + 4], data[i + 5], data[i + 6], data[i + 7],
                ]);
                let val = f64::from_bits(bits);
                i += 8;
                if field_num == 11 {
                    lat = val;
                } else if field_num == 12 {
                    lon = val;
                }
            }
            2 => {
                // length-delimited
                let (length, vn) = decode_varint(&data[i..]);
                if vn == 0 {
                    return None;
                }
                i += vn + length as usize;
            }
            5 => {
                // 32-bit (fixed32, float)
                if i + 4 > data.len() {
                    return None;
                }
                let bits = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
                let val = f32::from_bits(bits);
                if field_num == 4 {
                    speed = val;
                } else if field_num == 5 {
                    accel_pos = val;
                }
                i += 4;
            }
            _ => return None,
        }
    }

    // Validate GPS coordinates
    let ok = !lat.is_infinite()
        && !lon.is_infinite()
        && !lat.is_nan()
        && !lon.is_nan()
        && !(lat == 0.0 && lon == 0.0)
        && lat.abs() <= 90.0
        && lon.abs() <= 180.0;

    if ok {
        Some((lat, lon, gear, ap_state, speed, accel_pos))
    } else {
        None
    }
}

/// Reads a protobuf varint. Returns (value, bytes_consumed).
fn decode_varint(data: &[u8]) -> (u64, usize) {
    let mut val: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &b) in data.iter().enumerate() {
        if i >= 10 {
            return (0, 0);
        }
        val |= ((b & 0x7F) as u64) << shift;
        if b < 0x80 {
            return (val, i + 1);
        }
        shift += 7;
    }
    (0, 0)
}

/// Deduplicate consecutive identical GPS points (same lat/lon).
fn dedup_consecutive(
    points: &mut Vec<GpsPoint>,
    gears: &mut Vec<u8>,
    ap_states: &mut Vec<u8>,
    speeds: &mut Vec<f32>,
    accel_positions: &mut Vec<f32>,
) {
    if points.len() <= 1 {
        return;
    }

    let mut write = 0;
    for read in 1..points.len() {
        if points[read] != points[write] {
            write += 1;
            points[write] = points[read];
            gears[write] = gears[read];
            ap_states[write] = ap_states[read];
            speeds[write] = speeds[read];
            accel_positions[write] = accel_positions[read];
        }
    }
    let new_len = write + 1;
    points.truncate(new_len);
    gears.truncate(new_len);
    ap_states.truncate(new_len);
    speeds.truncate(new_len);
    accel_positions.truncate(new_len);
}

/// Compute contiguous gear runs from a gear state array.
fn compute_gear_runs(gears: &[u8]) -> Vec<GearRun> {
    if gears.is_empty() {
        return Vec::new();
    }

    let mut runs = Vec::new();
    let mut current_gear = gears[0];
    let mut count: u32 = 1;

    for &g in &gears[1..] {
        if g == current_gear {
            count += 1;
        } else {
            runs.push(GearRun {
                gear: current_gear,
                frames: count,
            });
            current_gear = g;
            count = 1;
        }
    }
    runs.push(GearRun {
        gear: current_gear,
        frames: count,
    });

    runs
}

impl ExtractedGps {
    /// Create an empty ExtractedGps result.
    pub fn empty() -> Self {
        ExtractedGps {
            points: Vec::new(),
            gear_states: Vec::new(),
            autopilot_states: Vec::new(),
            speeds: Vec::new(),
            accel_positions: Vec::new(),
            raw_park_count: 0,
            raw_frame_count: 0,
            gear_runs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_varint() {
        assert_eq!(decode_varint(&[0x00]), (0, 1));
        assert_eq!(decode_varint(&[0x01]), (1, 1));
        assert_eq!(decode_varint(&[0x96, 0x01]), (150, 2));
        assert_eq!(decode_varint(&[0xAC, 0x02]), (300, 2));
        assert_eq!(decode_varint(&[]), (0, 0));
    }

    #[test]
    fn test_strip_emulation_bytes() {
        // 00 00 03 should become 00 00
        assert_eq!(
            strip_emulation_bytes(&[0x00, 0x00, 0x03, 0x01]),
            vec![0x00, 0x00, 0x01]
        );
        // No emulation bytes
        assert_eq!(
            strip_emulation_bytes(&[0x01, 0x02, 0x03]),
            vec![0x01, 0x02, 0x03]
        );
        // Multiple occurrences
        assert_eq!(
            strip_emulation_bytes(&[0x00, 0x00, 0x03, 0x00, 0x00, 0x03]),
            vec![0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn test_dedup_consecutive() {
        let mut points = vec![[1.0, 2.0], [1.0, 2.0], [3.0, 4.0], [3.0, 4.0], [5.0, 6.0]];
        let mut gears = vec![1, 1, 1, 1, 0];
        let mut ap = vec![0, 0, 1, 1, 0];
        let mut speeds = vec![10.0, 10.0, 20.0, 20.0, 0.0];
        let mut accel = vec![0.5, 0.5, 0.6, 0.6, 0.0];

        dedup_consecutive(&mut points, &mut gears, &mut ap, &mut speeds, &mut accel);

        assert_eq!(points.len(), 3);
        assert_eq!(points[0], [1.0, 2.0]);
        assert_eq!(points[1], [3.0, 4.0]);
        assert_eq!(points[2], [5.0, 6.0]);
    }

    #[test]
    fn test_compute_gear_runs() {
        let gears = vec![1, 1, 1, 0, 0, 1, 1, 2];
        let runs = compute_gear_runs(&gears);
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].gear, 1);
        assert_eq!(runs[0].frames, 3);
        assert_eq!(runs[1].gear, 0);
        assert_eq!(runs[1].frames, 2);
        assert_eq!(runs[2].gear, 1);
        assert_eq!(runs[2].frames, 2);
        assert_eq!(runs[3].gear, 2);
        assert_eq!(runs[3].frames, 1);
    }

    #[test]
    fn test_decode_sei_gps_valid() {
        // Build a minimal protobuf message with:
        // field 2 (gear) = 1 (Drive), varint
        // field 11 (lat) = 37.7749, double
        // field 12 (lon) = -122.4194, double
        let mut data = Vec::new();

        // Field 2, wire type 0 (varint): tag = (2 << 3) | 0 = 16
        data.push(0x10);
        data.push(0x01); // gear = Drive

        // Field 11, wire type 1 (64-bit): tag = (11 << 3) | 1 = 89
        data.push(0x59);
        data.extend_from_slice(&37.7749f64.to_le_bytes());

        // Field 12, wire type 1 (64-bit): tag = (12 << 3) | 1 = 97
        data.push(0x61);
        data.extend_from_slice(&(-122.4194f64).to_le_bytes());

        let result = decode_sei_gps(&data);
        assert!(result.is_some());
        let (lat, lon, gear, _, _, _) = result.unwrap();
        assert!((lat - 37.7749).abs() < 1e-10);
        assert!((lon - (-122.4194)).abs() < 1e-10);
        assert_eq!(gear, 1);
    }

    #[test]
    fn test_decode_sei_gps_null_island() {
        // lat=0, lon=0 should be rejected
        let mut data = Vec::new();
        data.push(0x59); // field 11
        data.extend_from_slice(&0.0f64.to_le_bytes());
        data.push(0x61); // field 12
        data.extend_from_slice(&0.0f64.to_le_bytes());

        assert!(decode_sei_gps(&data).is_none());
    }
}
