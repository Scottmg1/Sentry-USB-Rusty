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

use crate::types::{
    ExtractedGps, FlagRun, GearRun, GpsPoint, FLAG_ACCEL, FLAG_BLINKER_LEFT,
    FLAG_BLINKER_RIGHT, FLAG_BRAKE,
};

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

    let (points, gears, ap_states, speeds, accel_positions, flags, accel_x, accel_y) =
        extract_from_mdat(&mut f, mdat_offset, mdat_size)?;
    Ok(assemble_extracted(
        points,
        gears,
        ap_states,
        speeds,
        accel_positions,
        flags,
        accel_x,
        accel_y,
    ))
}

/// Assemble the final [`ExtractedGps`] from raw per-frame arrays: raw
/// counts and BOTH RLEs first, GPS dedup last.
///
/// Order matters: gear/flag runs must RLE every raw SEI frame. Computing
/// gear runs after dedup silently dropped any run whose frames shared a
/// GPS fix with the frame before it — a car parking is stationary, so the
/// short trailing Park run collapsed into the last Drive-gear point and
/// vanished, and the park-gap splitter then fused the summon segment with
/// the drive that followed it. The final clip of a session can also carry
/// far fewer real frames than its 60s container claims (recording stops
/// early); trailing frames are real data, never junk to trim.
fn assemble_extracted(
    mut points: Vec<GpsPoint>,
    mut gears: Vec<u8>,
    mut ap_states: Vec<u8>,
    mut speeds: Vec<f32>,
    mut accel_positions: Vec<f32>,
    flags: Vec<u8>,
    mut accel_x: Vec<f32>,
    mut accel_y: Vec<f32>,
) -> ExtractedGps {
    // Raw-frame-space values BEFORE dedup — raw_frame_count is the true
    // number of SEI frames in the video, needed for correct t = index/FPS
    // computation, and the runs must cover every raw frame (see above).
    let raw_frame_count = gears.len() as u32;
    let raw_park_count = gears.iter().filter(|&&g| g == GEAR_PARK).count() as u32;
    let gear_runs = compute_gear_runs(&gears);
    let flag_runs = compute_flag_runs(&flags, Some(&speeds));

    // An all-zero IMU channel means the firmware doesn't emit fields
    // 14/15 — store nothing so consumers can distinguish "no channel"
    // from "flat road, gentle drive".
    if accel_x.iter().all(|&v| v == 0.0) && accel_y.iter().all(|&v| v == 0.0) {
        accel_x = Vec::new();
        accel_y = Vec::new();
    }

    // Deduplicate consecutive identical GPS points
    dedup_consecutive(
        &mut points,
        &mut gears,
        &mut ap_states,
        &mut speeds,
        &mut accel_positions,
        &mut accel_x,
        &mut accel_y,
    );

    ExtractedGps {
        points,
        gear_states: gears,
        autopilot_states: ap_states,
        speeds,
        accel_positions,
        raw_park_count,
        raw_frame_count,
        gear_runs,
        flag_runs,
        accel_x,
        accel_y,
    }
}

/// Extract raw (non-deduplicated) GPS data from a Tesla dashcam MP4 file.
///
/// Returns the raw frame arrays with 1:1 index-to-frame correspondence.
/// Use when frame-accurate timestamps matter (e.g. telemetry overlay).
///
/// The trailing array is the per-frame flags byte (FLAG_BLINKER_LEFT etc.).
/// It was previously dropped here, which is why the clip telemetry endpoint
/// could not report turn signals or braking despite both being decoded.
pub fn extract_gps_from_file_raw(path: &str) -> Result<RawFrameArrays> {
    let mut f = File::open(path)
        .with_context(|| format!("failed to open MP4 file: {}", path))?;
    let (mdat_offset, mdat_size) = find_mdat_box(&mut f)?;
    if mdat_size == 0 {
        return Ok(Default::default());
    }
    extract_from_mdat(&mut f, mdat_offset, mdat_size)
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

        // A box smaller than its own header is corrupt; without this
        // guard `box_size - header_size` underflows for an mdat (huge
        // bogus size → the NAL scanner chews garbage to EOF) and the
        // skip loop stalls or wraps for other boxes.
        if box_size < header_size {
            break;
        }
        if is_mdat {
            return Ok((pos + header_size, box_size - header_size));
        }
        pos += box_size;
    }

    Ok((0, 0))
}

/// Reads through the mdat box parsing NAL units and extracting GPS from SEI.
///
/// Streams the mdat sequentially through a `BufReader`: the access pattern
/// is forward-only (read each 4-byte length, peek the type byte, then
/// either read the small SEI payload or skip past the NAL), so a single
/// large buffer turns what used to be 2-3 `seek`+`read` syscalls *per NAL*
/// — thousands of tiny syscalls on a typical minute-long clip's unbuffered
/// File handle — into one read per ~256 KB. Non-SEI NALs (the bulk of the
/// bytes — encoded video) are skipped with `seek_relative`, which discards
/// the buffer only when the skip lands outside it, so large video frames
/// don't get pulled into memory.
/// Raw per-frame arrays out of the SEI scan, 1:1 with SEI frames:
/// (points, gears, autopilot states, speeds, accel positions, flag bytes,
/// IMU lateral accel m/s², IMU longitudinal accel m/s²).
type RawFrameArrays = (
    Vec<GpsPoint>,
    Vec<u8>,
    Vec<u8>,
    Vec<f32>,
    Vec<f32>,
    Vec<u8>,
    Vec<f32>,
    Vec<f32>,
);

fn extract_from_mdat(f: &mut File, offset: u64, size: u64) -> Result<RawFrameArrays> {
    use std::io::BufReader;
    const BUF_SIZE: u64 = 64 * 1024;

    let mut points = Vec::new();
    let mut gears = Vec::new();
    let mut ap_states = Vec::new();
    let mut speeds = Vec::new();
    let mut accel_positions = Vec::new();
    let mut flags = Vec::new();
    let mut accel_x = Vec::new();
    let mut accel_y = Vec::new();

    let end = offset + size;
    let mut cursor = offset;
    let mut size_buf = [0u8; 4];

    let mut reader = BufReader::with_capacity(256 * 1024, f);
    reader.seek(SeekFrom::Start(offset))?;

    while cursor + 4 <= end {
        // Read NAL size (4 bytes, big-endian). Sequential — no seek; the
        // reader is already positioned at `cursor` from the prior NAL.
        if reader.read_exact(&mut size_buf).is_err() {
            break;
        }
        cursor += 4;

        let nal_size = u32::from_be_bytes(size_buf) as u64;
        if nal_size < 2 || cursor + nal_size > end {
            break;
        }

        // Read the NAL type byte (first byte of the NAL).
        let mut type_buf = [0u8; 1];
        if reader.read_exact(&mut type_buf).is_err() {
            break;
        }
        let nal_type = type_buf[0] & 0x1F;

        // NAL type 6 = SEI. The type byte is the first byte of the NAL, so
        // read the remaining (nal_size - 1) bytes after it into the buffer.
        if nal_type == 6 && nal_size <= BUF_SIZE {
            let mut nal = vec![0u8; nal_size as usize];
            nal[0] = type_buf[0];
            if reader.read_exact(&mut nal[1..]).is_ok() {
                if let Some(frame) = parse_tesla_sei(&nal) {
                    points.push([
                        (frame.lat * 1e6).round() / 1e6,
                        (frame.lon * 1e6).round() / 1e6,
                    ]);
                    gears.push(frame.gear);
                    ap_states.push(frame.ap_state);
                    speeds.push(frame.speed);
                    accel_positions.push(frame.accel_pos);
                    accel_x.push(frame.accel_x);
                    accel_y.push(frame.accel_y);
                    flags.push(
                        (if frame.blinker_left { FLAG_BLINKER_LEFT } else { 0 })
                            | (if frame.blinker_right { FLAG_BLINKER_RIGHT } else { 0 })
                            | (if frame.brake { FLAG_BRAKE } else { 0 })
                            | (if frame.accel_pos > 0.0 { FLAG_ACCEL } else { 0 }),
                    );
                }
            } else {
                break;
            }
        } else {
            // Skip the rest of this NAL (the type byte was already read).
            if reader.seek_relative((nal_size - 1) as i64).is_err() {
                break;
            }
        }

        cursor += nal_size;
    }

    Ok((points, gears, ap_states, speeds, accel_positions, flags, accel_x, accel_y))
}

/// One decoded SEI frame's worth of Tesla metadata.
struct SeiFrame {
    lat: f64,
    lon: f64,
    gear: u8,
    ap_state: u8,
    speed: f32,
    accel_pos: f32,
    blinker_left: bool,
    blinker_right: bool,
    brake: bool,
    /// IMU linear acceleration, m/s² (proto fields 14/15). X is lateral
    /// (positive = rightward force), Y is longitudinal (negative =
    /// deceleration) — the axis convention Sentry-Six's G-force meter
    /// renders. proto3 omits zeros, so absent decodes as 0.0.
    accel_x: f32,
    accel_y: f32,
}

/// Finds the Tesla magic bytes (0x42...0x69) in a SEI NAL and decodes GPS + metadata.
fn parse_tesla_sei(nal: &[u8]) -> Option<SeiFrame> {
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
/// - blinker_on_left / blinker_on_right (fields 7/8, varint) — steady
///   switch state, not lamp flash
/// - brake_applied (field 9, varint) — pedal-only
/// - linear_acceleration_mps2_x / _y (fields 14/15, double) — IMU
///   lateral / longitudinal acceleration (Sentry-Six dashcam.proto)
///
/// proto3 omits false/zero fields from the wire, so absent simply
/// decodes as off. Hand-parses protobuf wire format to avoid external
/// dependencies.
fn decode_sei_gps(data: &[u8]) -> Option<SeiFrame> {
    let mut lat: f64 = 0.0;
    let mut lon: f64 = 0.0;
    let mut gear: u8 = 0;
    let mut ap_state: u8 = 0;
    let mut speed: f32 = 0.0;
    let mut accel_pos: f32 = 0.0;
    let mut blinker_left = false;
    let mut blinker_right = false;
    let mut brake = false;
    let mut accel_x: f32 = 0.0;
    let mut accel_y: f32 = 0.0;

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
                } else if field_num == 7 {
                    blinker_left = val != 0;
                } else if field_num == 8 {
                    blinker_right = val != 0;
                } else if field_num == 9 {
                    brake = val != 0;
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
                } else if field_num == 14 {
                    accel_x = val as f32;
                } else if field_num == 15 {
                    accel_y = val as f32;
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
        // NaN/inf IMU samples (never observed, but the field is a raw
        // double) degrade to 0.0 = "no data" rather than poisoning math.
        let clean = |v: f32| if v.is_finite() { v } else { 0.0 };
        Some(SeiFrame {
            lat,
            lon,
            gear,
            ap_state,
            speed,
            accel_pos,
            blinker_left,
            blinker_right,
            brake,
            accel_x: clean(accel_x),
            accel_y: clean(accel_y),
        })
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
///
/// The IMU accel arrays (when present) keep the PEAK |value| of each
/// collapsed run instead of the first frame's sample: GPS updates ~1 Hz
/// while SEI frames arrive at video rate, so a sub-second braking or
/// cornering spike between fixes would otherwise vanish. This biases
/// "time above threshold" up slightly for one-frame spikes — acceptable
/// against silently losing the very events the Safety Score exists to
/// catch.
fn dedup_consecutive(
    points: &mut Vec<GpsPoint>,
    gears: &mut Vec<u8>,
    ap_states: &mut Vec<u8>,
    speeds: &mut Vec<f32>,
    accel_positions: &mut Vec<f32>,
    accel_x: &mut Vec<f32>,
    accel_y: &mut Vec<f32>,
) {
    if points.len() <= 1 {
        return;
    }
    let has_imu = accel_x.len() == points.len() && accel_y.len() == points.len();

    let mut write = 0;
    for read in 1..points.len() {
        if points[read] != points[write] {
            write += 1;
            points[write] = points[read];
            gears[write] = gears[read];
            ap_states[write] = ap_states[read];
            speeds[write] = speeds[read];
            accel_positions[write] = accel_positions[read];
            if has_imu {
                accel_x[write] = accel_x[read];
                accel_y[write] = accel_y[read];
            }
        } else if has_imu {
            // Collapsed frame: fold its IMU sample into the retained
            // point, keeping the strongest magnitude (sign preserved).
            if accel_x[read].abs() > accel_x[write].abs() {
                accel_x[write] = accel_x[read];
            }
            if accel_y[read].abs() > accel_y[write].abs() {
                accel_y[write] = accel_y[read];
            }
        }
    }
    let new_len = write + 1;
    points.truncate(new_len);
    gears.truncate(new_len);
    ap_states.truncate(new_len);
    speeds.truncate(new_len);
    accel_positions.truncate(new_len);
    if has_imu {
        accel_x.truncate(new_len);
        accel_y.truncate(new_len);
    }
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

/// RLE the per-frame SEI flag bytes (blinkers/brake/accel bits) over RAW
/// frame indices — the same frame space as gear runs. Mirrors
/// Sentry-Drive's `computeFlagRuns(flags, speeds)` (drive-calc.cjs).
///
/// When `speeds` is given, each run carries `max_mps` — the max |SEI
/// speed| within its frames, rounded to 0.1 (SEI speed is signed;
/// Reverse counts via magnitude). This is the summon detector's
/// frame-space speed evidence: the park splitter's fraction→point slice
/// overshoots on deduped points, so a summon segment's stats can
/// inherit the following drive's speed samples — per-run maxima confine
/// the gate to the segment's own frames.
fn compute_flag_runs(flags: &[u8], speeds: Option<&[f32]>) -> Vec<FlagRun> {
    if flags.is_empty() {
        return Vec::new();
    }

    let r1 = |v: f64| (v * 10.0).round() / 10.0;
    let speed_at = |i: usize| -> f64 {
        speeds
            .and_then(|s| s.get(i))
            .map(|&v| (v as f64).abs())
            .unwrap_or(0.0)
    };

    let mut runs = Vec::new();
    let mut current_flags = flags[0];
    let mut count: u32 = 1;
    let mut max_abs = speed_at(0);

    for (i, &f) in flags.iter().enumerate().skip(1) {
        if f == current_flags {
            count += 1;
            let a = speed_at(i);
            if a > max_abs {
                max_abs = a;
            }
        } else {
            runs.push(FlagRun {
                flags: current_flags,
                frames: count,
                max_mps: speeds.map(|_| r1(max_abs)),
            });
            current_flags = f;
            count = 1;
            max_abs = speed_at(i);
        }
    }
    runs.push(FlagRun {
        flags: current_flags,
        frames: count,
        max_mps: speeds.map(|_| r1(max_abs)),
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
            flag_runs: Vec::new(),
            accel_x: Vec::new(),
            accel_y: Vec::new(),
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
        // IMU: a strong spike on a collapsed frame must survive into the
        // retained point (peak-preserving fold), sign intact.
        let mut ax = vec![0.1, -2.5, 0.2, 0.3, 0.0];
        let mut ay = vec![-0.5, -4.0, 0.1, -0.2, 0.0];

        dedup_consecutive(
            &mut points,
            &mut gears,
            &mut ap,
            &mut speeds,
            &mut accel,
            &mut ax,
            &mut ay,
        );
        assert_eq!(ax, vec![-2.5, 0.3, 0.0]);
        assert_eq!(ay, vec![-4.0, -0.2, 0.0]);

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
    fn test_compute_flag_runs() {
        // Mirrors Sentry-Drive drive-calc.test.js "computeFlagRuns RLEs
        // raw frames and round-trips totals".
        assert!(compute_flag_runs(&[], None).is_empty());
        let runs = compute_flag_runs(&[0, 0, 3, 3, 3, 1, 0], None);
        assert_eq!(
            runs,
            vec![
                FlagRun { flags: 0, frames: 2, max_mps: None },
                FlagRun { flags: 3, frames: 3, max_mps: None },
                FlagRun { flags: 1, frames: 1, max_mps: None },
                FlagRun { flags: 0, frames: 1, max_mps: None },
            ]
        );
        // Run totals must reconstruct the frame count — the summon
        // detector's segment bounds depend on flagRuns living in raw
        // frame space.
        let flags = [0u8, 8, 8, 4, 0, 0, 3, 3, 2, 1];
        let total: u32 = compute_flag_runs(&flags, None).iter().map(|r| r.frames).sum();
        assert_eq!(total as usize, flags.len());
    }

    #[test]
    fn test_compute_flag_runs_carries_per_run_max_speed() {
        // Mirrors drive-calc.test.js "computeFlagRuns carries per-run
        // max |SEI speed| when speeds are given".
        let flags = [0u8, 0, 3, 3, 8, 8, 8];
        let speeds = [0.0f32, -1.5, 0.4, 0.26, 2.0, 4.73, 3.1];
        assert_eq!(
            compute_flag_runs(&flags, Some(&speeds)),
            vec![
                // reverse counts via magnitude
                FlagRun { flags: 0, frames: 2, max_mps: Some(1.5) },
                FlagRun { flags: 3, frames: 2, max_mps: Some(0.4) },
                // rounded to 0.1
                FlagRun { flags: 8, frames: 3, max_mps: Some(4.7) },
            ]
        );
    }

    #[test]
    fn test_gear_runs_keep_trailing_park_despite_frozen_gps() {
        // Regression for the 2026-07-15_20-50-43 clip: the car stops
        // (GPS freezes) while still in Drive, then shifts to Park for the
        // final frames. Computing gear runs AFTER GPS dedup collapsed all
        // the same-fix frames into the last Drive point, so gearRuns
        // reported [Drive] only and the park splitter never separated the
        // summon from the following drive. Runs must RLE every RAW frame.
        let moving = 447; // distinct GPS fixes while driving
        let stopped_drive = 53; // still in Drive, GPS frozen
        let parked = 53; // trailing Park, GPS frozen
        let mut points = Vec::new();
        let mut gears = Vec::new();
        let mut flags = Vec::new();
        for i in 0..moving {
            points.push([37.0 + i as f64 * 1e-5, -122.0]);
            gears.push(GEAR_DRIVE);
            flags.push(0);
        }
        for _ in 0..(stopped_drive + parked) {
            points.push([37.0 + (moving - 1) as f64 * 1e-5, -122.0]);
            flags.push(3); // hazards held through the stop and D→P shift
        }
        gears.resize(moving + stopped_drive, GEAR_DRIVE);
        gears.resize(moving + stopped_drive + parked, GEAR_PARK);
        let n = points.len();
        // Crawl speed while in Drive (negative — this clip shape also
        // covers Reverse semantics), zero once parked: per-run maxima
        // must be |v| within the run's own frames, not whole-clip.
        let mut speeds = vec![-2.13f32; moving + stopped_drive];
        speeds.resize(n, 0.0);
        let out = assemble_extracted(
            points,
            gears,
            vec![0; n],
            speeds,
            vec![0.0; n],
            flags,
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(out.raw_frame_count, 553);
        assert_eq!(out.raw_park_count, 53);
        // Full raw-frame RLE: [Drive x500, Park x53] — the trailing Park
        // run survives even though its GPS fix never changed.
        assert_eq!(out.gear_runs.len(), 2);
        assert_eq!(out.gear_runs[0].gear, GEAR_DRIVE);
        assert_eq!(out.gear_runs[0].frames, 500);
        assert_eq!(out.gear_runs[1].gear, GEAR_PARK);
        assert_eq!(out.gear_runs[1].frames, 53);
        let gear_total: u32 = out.gear_runs.iter().map(|r| r.frames).sum();
        assert_eq!(gear_total, out.raw_frame_count);
        // Flag runs live in the same raw frame space, each carrying its
        // own max |SEI speed| (the -2.13 m/s crawl reads 2.1 absolute,
        // rounded to 0.1 — Sentry-Drive's r1).
        let flag_total: u32 = out.flag_runs.iter().map(|r| r.frames).sum();
        assert_eq!(flag_total, out.raw_frame_count);
        assert_eq!(
            out.flag_runs,
            vec![
                FlagRun { flags: 0, frames: 447, max_mps: Some(2.1) },
                FlagRun { flags: 3, frames: 106, max_mps: Some(2.1) },
            ]
        );
        // Dedup still applies to the point-domain arrays.
        assert_eq!(out.points.len(), moving);
        assert_eq!(out.gear_states.len(), moving);
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
        let frame = result.unwrap();
        assert!((frame.lat - 37.7749).abs() < 1e-10);
        assert!((frame.lon - (-122.4194)).abs() < 1e-10);
        assert_eq!(frame.gear, 1);
        // proto3 omits false fields — absent decodes as off.
        assert!(!frame.blinker_left);
        assert!(!frame.blinker_right);
        assert!(!frame.brake);
    }

    #[test]
    fn test_decode_sei_gps_flag_fields() {
        // Fields 7 (blinker_on_left), 8 (blinker_on_right), 9
        // (brake_applied) are varints; nonzero = on. Tags: (7<<3)|0 = 56,
        // (8<<3)|0 = 64, (9<<3)|0 = 72.
        let mut data = vec![0x38, 0x01, 0x40, 0x01, 0x48, 0x01];
        // Field 11 (lat), field 12 (lon) — required for a valid frame.
        data.push(0x59);
        data.extend_from_slice(&37.7749f64.to_le_bytes());
        data.push(0x61);
        data.extend_from_slice(&(-122.4194f64).to_le_bytes());

        let frame = decode_sei_gps(&data).expect("valid frame");
        assert!(frame.blinker_left);
        assert!(frame.blinker_right);
        assert!(frame.brake);
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
