//! Probe a Tesla dashcam clip's SEI stream: raw frame count, gear/flag
//! RLEs (raw frame space), and SEI speed extremes. Handy for verifying
//! the extractor against real footage without a full server run.
//!
//! Usage:
//!     cargo run -p sentryusb-drives --example probe_clip -- <clip.mp4> [more.mp4 ...]

use sentryusb_drives::extract::extract_gps_from_file;

fn gear_name(g: u8) -> &'static str {
    match g {
        0 => "Park",
        1 => "Drive",
        2 => "Reverse",
        3 => "Neutral",
        _ => "?",
    }
}

fn main() -> anyhow::Result<()> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: probe_clip <clip.mp4> [more.mp4 ...]");
        std::process::exit(2);
    }
    for path in paths {
        let gps = extract_gps_from_file(&path)?;
        println!("{path}");
        println!("  raw_frame_count:  {}", gps.raw_frame_count);
        println!("  raw_park_count:   {}", gps.raw_park_count);
        println!("  points (deduped): {}", gps.points.len());
        let gears: Vec<String> = gps
            .gear_runs
            .iter()
            .map(|r| format!("{} x{}", gear_name(r.gear), r.frames))
            .collect();
        println!("  gearRuns:  [{}]", gears.join(", "));
        // Flag bits: 1=blinker L, 2=blinker R, 4=brake, 8=accel — shown
        // as ABRL binary so hazards read as 0011.
        let flags: Vec<String> = gps
            .flag_runs
            .iter()
            .map(|r| {
                let max = r.max_mps.unwrap_or(f64::NAN);
                format!("{:04b} x{} (|v|max {max:.1})", r.flags, r.frames)
            })
            .collect();
        println!("  flagRuns (ABRL bits): [{}]", flags.join(", "));
        let max_abs = gps.speeds.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        println!("  max |sei speed|:  {max_abs:.2} m/s");
    }
    Ok(())
}
