//! Client-facing active vehicle identity, camera layout, and recording format.

use axum::http::StatusCode;
use axum::Json;

use sentryusb_vehicle_profile::Profile;

pub async fn get_profile() -> (StatusCode, Json<serde_json::Value>) {
    let p = Profile::active();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": p.profile.id,
            "display_name": p.profile.display_name,
            "brand": p.profile.brand,
            "cameras": p.cameras,
            "grid": p.viewer.grid,
            "filename_regex": p.recording.filename_regex,
            "segment_seconds": p.recording.segment_seconds,
            "rolling_window_minutes": p.recording.rolling_window_minutes,
        })),
    )
}
