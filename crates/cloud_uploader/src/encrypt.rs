

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::rand::{SecureRandom, SystemRandom};

use sentryusb_cloud_crypto::{aad, aead, ids};
use sentryusb_drives::types::Route;

#[derive(Debug, Clone)]
pub struct EncryptedRoute {
    pub route_id: String,
    pub route_blob_b64: String,
    pub wrapped_route_key_b64: String,

    pub source_file: String,
}

pub fn encrypt_route(
    route: &Route,
    pi_key: &[u8; 32],
    user_id: &str,
    pi_id: &str,
    cached_route_id: Option<&str>,
) -> Result<EncryptedRoute> {

    let route_id = match cached_route_id {
        Some(c) => c.to_string(),
        None => ids::route_id_from_path(&route.file),
    };

    let mut route_key_bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut route_key_bytes)
        .map_err(|_| anyhow::anyhow!("rng failure for route key"))?;

    let route_json = serde_json::to_vec(route).context("serialize Route to JSON")?;
    let blob_aad = aad::route_blob(user_id, pi_id, &route_id);
    let route_key = aead::Key::from_bytes(&route_key_bytes)?;
    let route_blob = aead::seal(&route_key, &blob_aad, &route_json)?;

    let wrap_aad = aad::route_key(user_id, pi_id, &route_id);
    let pi_key_obj = aead::Key::from_bytes(pi_key)?;
    let wrapped = aead::seal(&pi_key_obj, &wrap_aad, &route_key_bytes)?;

    route_key_bytes.fill(0);

    Ok(EncryptedRoute {
        route_id,
        route_blob_b64: B64.encode(&route_blob),
        wrapped_route_key_b64: B64.encode(&wrapped),
        source_file: route.file.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentryusb_drives::types::Route;

    fn sample_route() -> Route {
        Route {
            file: "2026-04-27/clip-front.mp4".to_string(),
            date: "2026-04-27_12-00-00".to_string(),
            points: vec![[40.7, -74.0]],
            gear_states: vec![0, 1, 0],
            autopilot_states: vec![0, 0, 1],
            speeds: vec![10.0, 12.0],
            accel_positions: vec![0.1, 0.2],
            raw_park_count: 1,
            raw_frame_count: 100,
            gear_runs: vec![],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
        }
    }

    #[test]
    fn encrypt_then_local_decrypt_roundtrip() {
        let pi_key = [7u8; 32];
        let user_id = "user-cuid-abc";
        let pi_id = "pi-cuid-xyz";
        let route = sample_route();
        let encrypted = encrypt_route(&route, &pi_key, user_id, pi_id, None).unwrap();

        assert_eq!(encrypted.route_id, ids::route_id_from_path(&route.file));
        assert_eq!(encrypted.route_id.len(), 64);

        let wrapped = B64.decode(&encrypted.wrapped_route_key_b64).unwrap();
        let blob = B64.decode(&encrypted.route_blob_b64).unwrap();

        let pi_key_obj = aead::Key::from_bytes(&pi_key).unwrap();
        let wrap_aad = aad::route_key(user_id, pi_id, &encrypted.route_id);
        let recovered_key_bytes = aead::open(&pi_key_obj, &wrap_aad, &wrapped).unwrap();
        let recovered_key: [u8; 32] = recovered_key_bytes.as_slice().try_into().unwrap();

        let blob_aad = aad::route_blob(user_id, pi_id, &encrypted.route_id);
        let route_key = aead::Key::from_bytes(&recovered_key).unwrap();
        let plaintext = aead::open(&route_key, &blob_aad, &blob).unwrap();
        let recovered_route: Route = serde_json::from_slice(&plaintext).unwrap();

        assert_eq!(recovered_route.file, route.file);
        assert_eq!(recovered_route.points, route.points);
        assert_eq!(recovered_route.speeds, route.speeds);
    }

    #[test]
    fn encrypt_different_routes_produces_distinct_blobs() {
        let pi_key = [9u8; 32];
        let mut a = sample_route();
        let mut b = sample_route();
        a.file = "a.mp4".to_string();
        b.file = "b.mp4".to_string();
        let ea = encrypt_route(&a, &pi_key, "u", "p", None).unwrap();
        let eb = encrypt_route(&b, &pi_key, "u", "p", None).unwrap();
        assert_ne!(ea.route_id, eb.route_id);
        assert_ne!(ea.route_blob_b64, eb.route_blob_b64);
        assert_ne!(ea.wrapped_route_key_b64, eb.wrapped_route_key_b64);
    }

    #[test]
    fn cached_route_id_is_used_verbatim() {
        let pi_key = [1u8; 32];
        let route = sample_route();
        let cached = "deadbeef".repeat(8);
        let e = encrypt_route(&route, &pi_key, "u", "p", Some(&cached)).unwrap();
        assert_eq!(e.route_id, cached);
    }
}
