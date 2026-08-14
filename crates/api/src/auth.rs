use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json;
use tracing::{info, warn};

const SESSION_COOKIE_NAME: &str = "sentryusb_session";
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour

/// Authentication state shared across the application.
#[derive(Clone)]
pub struct AuthState {
    inner: std::sync::Arc<AuthInner>,
}

struct AuthInner {
    username: String,
    password: String,
    sessions: RwLock<HashMap<String, SystemTime>>,
    sessions_file: PathBuf,
}

impl AuthState {
    /// Create authentication state with no credentials.
    pub fn disabled() -> Self {
        AuthState {
            inner: std::sync::Arc::new(AuthInner {
                username: String::new(),
                password: String::new(),
                sessions: RwLock::new(HashMap::new()),
                sessions_file: PathBuf::new(),
            }),
        }
    }

    /// Whether authentication is required.
    pub fn auth_required(&self) -> bool {
        // Partial credentials cannot authenticate anyone, so either blank field
        // disables the gate.
        !self.inner.username.is_empty() && !self.inner.password.is_empty()
    }

    /// Create a new session token.
    pub fn create_session(&self) -> Option<String> {
        let rng = SystemRandom::new();
        let mut bytes = [0u8; 32];
        if rng.fill(&mut bytes).is_err() {
            warn!("[auth] crypto random failed");
            return None;
        }
        let token = hex::encode(bytes);

        let expiry = SystemTime::now() + SESSION_TTL;
        if let Ok(mut sessions) = self.inner.sessions.write() {
            sessions.insert(token.clone(), expiry);
        }
        self.save_to_disk();
        Some(token)
    }

    /// Validate a session token.
    pub fn validate_session(&self, token: &str) -> bool {
        if let Ok(sessions) = self.inner.sessions.read() {
            if let Some(expiry) = sessions.get(token) {
                return SystemTime::now() < *expiry;
            }
        }
        false
    }

    /// Remove a session.
    pub fn remove_session(&self, token: &str) {
        if let Ok(mut sessions) = self.inner.sessions.write() {
            sessions.remove(token);
        }
        self.save_to_disk();
    }

    /// Constant-time credential comparison.
    pub fn check_credentials(&self, username: &str, password: &str) -> bool {
        let u_match = constant_time_eq(username.as_bytes(), self.inner.username.as_bytes());
        let p_match = constant_time_eq(password.as_bytes(), self.inner.password.as_bytes());
        u_match && p_match
    }

    /// Start the background cleanup task.
    pub fn start_cleanup_task(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CLEANUP_INTERVAL).await;
                let mut removed = 0;
                if let Ok(mut sessions) = state.inner.sessions.write() {
                    let now = SystemTime::now();
                    sessions.retain(|_, expiry| {
                        if now >= *expiry {
                            removed += 1;
                            false
                        } else {
                            true
                        }
                    });
                }
                if removed > 0 {
                    state.save_to_disk();
                }
            }
        });
    }

    fn load_from_disk(&self) {
        let path = &self.inner.sessions_file;
        if !path.exists() {
            return;
        }
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return,
        };
        let stored: HashMap<String, i64> = match serde_json::from_str(&data) {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut loaded = 0;
        if let Ok(mut sessions) = self.inner.sessions.write() {
            let now = SystemTime::now();
            for (token, unix) in stored {
                let expiry = UNIX_EPOCH + Duration::from_secs(unix as u64);
                if now < expiry {
                    sessions.insert(token, expiry);
                    loaded += 1;
                }
            }
        }
        if loaded > 0 {
            info!("[auth] Restored {} active sessions from disk", loaded);
        }
    }

    fn save_to_disk(&self) {
        let path = &self.inner.sessions_file;
        if path.as_os_str().is_empty() {
            return;
        }
        let stored: HashMap<String, i64> = if let Ok(sessions) = self.inner.sessions.read() {
            sessions
                .iter()
                .filter_map(|(token, expiry)| {
                    expiry
                        .duration_since(UNIX_EPOCH)
                        .ok()
                        .map(|d| (token.clone(), d.as_secs() as i64))
                })
                .collect()
        } else {
            return;
        };

        if let Ok(data) = serde_json::to_vec(&stored) {
            let _ = std::fs::write(path, data);
            // Session tokens are bearer credentials; restrict the file to root.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
        }
    }
}

/// Initialize authentication from the config file.
pub fn init_auth() -> AuthState {
    let config_path = sentryusb_config::find_config_path();
    let sessions_file = Path::new(config_path)
        .parent()
        .unwrap_or(Path::new("/root"))
        .join(".sentryusb-sessions.json");

    let (active, _, _) = match sentryusb_config::parse_file(config_path) {
        Ok((a, c)) => (a, c, ()),
        Err(e) => {
            warn!("[auth] Could not read config for web auth: {}", e);
            return AuthState::disabled();
        }
    };

    let username = active.get("WEB_USERNAME").cloned().unwrap_or_default();
    let password = active.get("WEB_PASSWORD").cloned().unwrap_or_default();

    if !username.is_empty() {
        info!("[auth] Web authentication enabled for user {:?}", username);
    }

    let state = AuthState {
        inner: std::sync::Arc::new(AuthInner {
            username,
            password,
            sessions: RwLock::new(HashMap::new()),
            sessions_file,
        }),
    };

    state.load_from_disk();
    state.start_cleanup_task();
    state
}

/// True when setup has written a completion marker.
///
/// Both boot paths are supported across Pi OS layouts.
fn setup_is_finished() -> bool {
    const MARKERS: &[&str] = &[
        "/sentryusb/SENTRYUSB_SETUP_FINISHED",
        "/boot/firmware/SENTRYUSB_SETUP_FINISHED",
        "/boot/SENTRYUSB_SETUP_FINISHED",
    ];
    MARKERS.iter().any(|p| std::path::Path::new(p).exists())
}

/// Axum middleware for authentication.
pub async fn auth_middleware(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    req: Request,
    next: Next,
) -> Response {
    if !auth.auth_required() {
        return next.run(req).await;
    }

    let path = req.uri().path().to_string();

    if !path.starts_with("/api/") {
        return next.run(req).await;
    }

    if let Some(addr) = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>() {
        // Canonicalize IPv4-mapped IPv6 on dual-stack listeners.
        if addr.ip().to_canonical().is_loopback() {
            return next.run(req).await;
        }
    }

    // The frontend needs these endpoints before it can choose login or setup.
    const EXEMPT_ALWAYS: &[&str] = &[
        "/api/status",
        "/api/setup/status",
        "/api/auth/login",
        "/api/auth/logout",
        "/api/auth/check",
    ];
    if EXEMPT_ALWAYS.contains(&path.as_str()) {
        return next.run(req).await;
    }

    // Setup endpoints are unauthenticated only until provisioning completes.
    // Exempt only the setup log used by the wizard; all other logs stay gated.
    if !setup_is_finished()
        && (path.starts_with("/api/setup/") || path == "/api/logs/setup")
    {
        return next.run(req).await;
    }

    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let token = extract_cookie(cookie_header, SESSION_COOKIE_NAME);

    if let Some(token) = token {
        if auth.validate_session(token) {
            return next.run(req).await;
        }
    }

    let body = serde_json::json!({"error": "Authentication required"});
    let mut response = axum::response::Json(body).into_response();
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}

/// Extract a cookie value from a Cookie header string.
fn extract_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    for part in header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(name) {
            if let Some(value) = value.strip_prefix('=') {
                return Some(value);
            }
        }
    }
    None
}

/// Constant-time byte comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

use axum::Json;
use axum::response::IntoResponse;
use serde::Deserialize;


/// Lock an IP out after five failed logins within five minutes.
const LOGIN_RATE_WINDOW: Duration = Duration::from_secs(5 * 60);
const LOGIN_RATE_MAX_FAILS: usize = 5;

fn login_rate_store() -> &'static std::sync::Mutex<HashMap<String, Vec<std::time::Instant>>> {
    static STORE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, Vec<std::time::Instant>>>,
    > = std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn login_rate_limited(ip: &str) -> bool {
    let mut map = match login_rate_store().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let cutoff = std::time::Instant::now().checked_sub(LOGIN_RATE_WINDOW);
    if let Some(times) = map.get_mut(ip) {
        times.retain(|t| cutoff.map(|c| *t > c).unwrap_or(true));
        if times.is_empty() {
            map.remove(ip);
            return false;
        }
        return times.len() >= LOGIN_RATE_MAX_FAILS;
    }
    false
}

fn record_login_failure(ip: &str) {
    let mut map = match login_rate_store().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    map.entry(ip.to_string())
        .or_default()
        .push(std::time::Instant::now());
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

/// POST /api/auth/login
pub async fn handle_login(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Json(req): Json<LoginRequest>,
) -> Response {
    if !auth.auth_required() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Authentication is not configured"}))).into_response();
    }

    let ip = addr.ip().to_canonical().to_string();
    if login_rate_limited(&ip) {
        warn!("[auth] Login rate limit hit for {}", ip);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "Too many failed attempts — try again in a few minutes"
            })),
        )
            .into_response();
    }

    if !auth.check_credentials(&req.username, &req.password) {
        record_login_failure(&ip);
        warn!("[auth] Failed login attempt for user {:?} from {}", req.username, ip);
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Invalid username or password"}))).into_response();
    }

    let token = match auth.create_session() {
        Some(t) => t,
        None => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "Failed to create session"}))).into_response();
        }
    };

    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_COOKIE_NAME,
        token,
        SESSION_TTL.as_secs()
    );

    let mut response = Json(serde_json::json!({"success": true})).into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        cookie.parse().unwrap(),
    );
    response
}

/// POST /api/auth/logout
pub async fn handle_logout(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    req: Request,
) -> impl axum::response::IntoResponse {
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(token) = extract_cookie(cookie_header, SESSION_COOKIE_NAME) {
        auth.remove_session(token);
    }

    let clear_cookie = format!(
        "{}=; Path=/; HttpOnly; Max-Age=0",
        SESSION_COOKIE_NAME
    );

    let body = serde_json::json!({"success": true});
    let mut response = axum::response::Json(body).into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        clear_cookie.parse().unwrap(),
    );
    response
}

/// GET /api/auth/check
pub async fn handle_auth_check(
    axum::extract::State(auth): axum::extract::State<AuthState>,
    req: Request,
) -> (StatusCode, Json<serde_json::Value>) {
    let auth_required = auth.auth_required();
    let mut authenticated = !auth_required;

    if auth_required {
        let cookie_header = req
            .headers()
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if let Some(token) = extract_cookie(cookie_header, SESSION_COOKIE_NAME) {
            authenticated = auth.validate_session(token);
        }
    }

    (StatusCode::OK, Json(serde_json::json!({
        "authenticated": authenticated,
        "auth_required": auth_required,
    })))
}
