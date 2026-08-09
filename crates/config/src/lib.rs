use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};

/// Configuration variables as key-value pairs.
pub type SetupConfig = HashMap<String, String>;

/// Standard location for the setup variables file.
pub const DEFAULT_CONFIG_PATH: &str = "/root/sentryusb.conf";

/// Location on the boot partition.
pub const BOOT_CONFIG_PATH: &str = "/boot/firmware/sentryusb.conf";

/// Legacy boot partition path.
const LEGACY_BOOT_PATH: &str = "/boot/sentryusb.conf";

/// Returns the first existing config file path.
///
/// `SENTRYUSB_CONFIG_PATH` overrides the on-Pi search chain entirely so
/// the daemon can run off-Pi (local development) against a config file
/// in a writable location. The override is read once; with it unset the
/// probe loop below runs live on every call, exactly as before.
pub fn find_config_path() -> &'static str {
    static ENV_OVERRIDE: OnceLock<Option<&'static str>> = OnceLock::new();
    let ov = ENV_OVERRIDE.get_or_init(|| {
        std::env::var("SENTRYUSB_CONFIG_PATH")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| &*Box::leak(s.into_boxed_str()))
    });
    if let Some(p) = ov {
        return p;
    }
    for p in [DEFAULT_CONFIG_PATH, BOOT_CONFIG_PATH, LEGACY_BOOT_PATH] {
        if Path::new(p).exists() {
            return p;
        }
    }
    DEFAULT_CONFIG_PATH
}

/// Base directory for runtime state that lives under `/mutable` on the
/// Pi (preferences store, GPS fix cache). `SENTRYUSB_MUTABLE_DIR`
/// redirects it for off-Pi development; unset means `/mutable`.
pub fn mutable_dir() -> &'static str {
    static DIR: OnceLock<&'static str> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var("SENTRYUSB_MUTABLE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| &*Box::leak(s.trim_end_matches('/').to_owned().into_boxed_str()))
            .unwrap_or("/mutable")
    })
}

/// Config key holding the SSH port of the rsync archive server.
pub const RSYNC_SSH_PORT_KEY: &str = "RSYNC_SSH_PORT";

/// The rsync server's SSH port, rejecting a value that is not a port.
///
/// `Ok(None)` is "not configured", which callers must render as no port
/// argument at all rather than an explicit 22, so existing command lines
/// are untouched for everyone on the default port.
///
/// An unparseable value is an error instead of a fallback to 22: the
/// fallback would connect somewhere the user never asked for and only
/// surface much later, as "Connection refused" in the archive log.
pub fn validate_rsync_ssh_port(config: &SetupConfig) -> std::result::Result<Option<u16>, String> {
    let raw = config.get(RSYNC_SSH_PORT_KEY).map_or("", |v| v.trim());
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<u16>() {
        Ok(port) if port > 0 => Ok(Some(port)),
        _ => Err(format!(
            "{RSYNC_SSH_PORT_KEY} must be a port number between 1 and 65535, got {raw:?}"
        )),
    }
}

/// The rsync server's SSH port, treating a malformed value as unset.
///
/// For runtime paths (host-key scan, backup upload) that have no way to
/// report a config error to anyone. Setup and the wizard's connection test
/// both call [`validate_rsync_ssh_port`], so a bad value is rejected where
/// the user can still see it.
pub fn rsync_ssh_port(config: &SetupConfig) -> Option<u16> {
    validate_rsync_ssh_port(config).ok().flatten()
}

/// Reads a sentryusb.conf file and returns both active (exported) and
/// commented-out variables.
pub fn parse_file(path: &str) -> Result<(SetupConfig, SetupConfig)> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to open config file: {}", path))?;

    let mut active = SetupConfig::new();
    let mut commented = SetupConfig::new();

    for line in content.lines() {
        if let Some((key, val)) = parse_export_line(line) {
            active.insert(key, val);
        } else if let Some((key, val)) = parse_commented_export_line(line) {
            commented.insert(key, val);
        }
    }

    Ok((active, commented))
}

/// Writes the configuration back to the file, preserving comments and structure.
/// Variables in `new_config` will be written as active exports. Variables not in
/// `new_config` that were previously active will be commented out.
pub fn write_file(path: &str, new_config: &SetupConfig) -> Result<()> {
    // Keys are written into `export KEY=...` lines unquoted. `quote()`
    // neutralizes hostile *values*, but a hostile *key* — e.g. one
    // containing a newline or `=` — would inject an extra export line
    // into the bash-sourced config. Reject the whole write if any key
    // isn't a plain shell identifier (the same rule parse_key_value
    // enforces on read), so a crafted key can't smuggle in (say) a
    // WEB_PASSWORD override through the pre-setup /api/setup/config PUT.
    if let Some(bad) = new_config.keys().find(|k| !is_valid_key(k)) {
        anyhow::bail!("refusing to write config: invalid key {:?}", bad);
    }
    // Values can't contain a newline: `quote()` would emit a literal
    // multi-line bash string, but `parse_file()` is line-based and reads
    // back only the first line — so the value silently truncates on the
    // next load (and the trailing lines become stray config). Reject the
    // write rather than persist something that won't round-trip. (A
    // textarea-backed field like NOTIFICATION_COMMAND_* is the realistic
    // source of a newline.)
    if let Some((k, _)) = new_config.iter().find(|(_, v)| v.contains(['\n', '\r'])) {
        anyhow::bail!("refusing to write config: value for {:?} contains a newline", k);
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to open config file: {}", path))?;

    let mut seen = HashMap::new();
    let mut output = Vec::new();

    for line in content.lines() {
        if let Some((key, _)) = parse_export_line(line) {
            seen.insert(key.clone(), true);
            if let Some(val) = new_config.get(&key) {
                output.push(format!("export {}={}", key, quote(val)));
            } else {
                // Comment out variables not in new_config
                output.push(format!("#{}", line));
            }
        } else if let Some((key, _)) = parse_commented_export_line(line) {
            seen.insert(key.clone(), true);
            if let Some(val) = new_config.get(&key) {
                output.push(format!("export {}={}", key, quote(val)));
            } else {
                output.push(line.to_string());
            }
        } else {
            output.push(line.to_string());
        }
    }

    // Append any new variables not in the original file
    for (key, val) in new_config {
        if !seen.contains_key(key) {
            output.push(format!("export {}={}", key, quote(val)));
        }
    }

    // Atomic write: tmp + fsync + rename. A direct `fs::File::create`
    // followed by streaming writes is vulnerable to a torn file on power
    // cut mid-write, which on a Pi that loses power the instant the
    // user's Tesla disconnects is a real scenario. Config corruption
    // means the next boot can't parse sentryusb.conf and setup defaults
    // to unset everything — including archive URLs, hostnames, WiFi AP
    // creds. Write to `<path>.tmp`, fsync, rename over.
    let tmp = format!("{}.tmp", path);
    {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("failed to write config tmp file: {}", tmp))?;
        {
            let mut writer = io::BufWriter::new(&mut file);
            for line in &output {
                writeln!(writer, "{}", line)?;
            }
            writer.flush()?;
        }
        // Drop the writer above, then fsync the underlying file so the
        // rename below doesn't expose an empty-but-renamed file after
        // a crash.
        let _ = file.sync_all();
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to rename config tmp into place: {}", path));
    }

    Ok(())
}

/// Tries to parse a line as `export KEY=VALUE`.
fn parse_export_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("export ")?;
    parse_key_value(rest)
}

/// Tries to parse a line as `# export KEY=VALUE`.
fn parse_commented_export_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('#')?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("export ")?;
    parse_key_value(rest)
}

/// True when `key` is a plain shell identifier: `[A-Za-z_][A-Za-z0-9_]*`.
/// The single source of truth for key validity, used on both the parse
/// (read) and write paths.
fn is_valid_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Parses `KEY=VALUE` from a string.
fn parse_key_value(s: &str) -> Option<(String, String)> {
    let eq_pos = s.find('=')?;
    let key = &s[..eq_pos];

    if !is_valid_key(key) {
        return None;
    }

    let val = unquote(&s[eq_pos + 1..]);
    Some((key.to_string(), val))
}

/// Removes surrounding single or double quotes from a value.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
        {
            return s[1..s.len() - 1].to_string();
        }
    }
    // Handle $'...' syntax
    if s.starts_with("$'") && s.ends_with('\'') && s.len() >= 3 {
        return s[2..s.len() - 1].to_string();
    }
    // Strip inline comments for unquoted values
    let bytes = s.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'#' && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
            return s[..i - 1].trim().to_string();
        }
    }
    s.to_string()
}

/// Wraps a value in single quotes for safe bash export.
fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // If value contains no special characters, leave it bare.
    // \n and \r are included so an embedded newline gets quoted — the file
    // stays valid bash for `source` consumers. (parse_file is line-based
    // and still can't round-trip multi-line values; see review ledger.)
    const SPECIAL: &str = " \t\n\r'\"\\$!#&|;(){}[]<>?*~`";
    if !s.chars().any(|c| SPECIAL.contains(c)) {
        return s.to_string();
    }
    // Use single quotes; escape any embedded single quotes
    let escaped = s.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

/// Helper to get a config value, trying active first then commented.
pub fn get_config_value(active: &SetupConfig, commented: &SetupConfig, key: &str) -> Option<String> {
    active.get(key).or_else(|| commented.get(key)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unquote_single_quotes() {
        assert_eq!(unquote("'hello world'"), "hello world");
    }

    #[test]
    fn test_unquote_double_quotes() {
        assert_eq!(unquote("\"hello world\""), "hello world");
    }

    #[test]
    fn test_unquote_dollar_quotes() {
        assert_eq!(unquote("$'hello world'"), "hello world");
    }

    #[test]
    fn test_unquote_inline_comment() {
        assert_eq!(unquote("3480 # this number is in seconds"), "3480");
    }

    #[test]
    fn test_unquote_bare() {
        assert_eq!(unquote("hello"), "hello");
    }

    #[test]
    fn test_quote_empty() {
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn test_quote_bare() {
        assert_eq!(quote("hello"), "hello");
    }

    #[test]
    fn test_quote_special() {
        assert_eq!(quote("hello world"), "'hello world'");
    }

    #[test]
    fn test_quote_embedded_single_quote() {
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_parse_export_line() {
        assert_eq!(
            parse_export_line("export WIFI_SSID='MyNetwork'"),
            Some(("WIFI_SSID".to_string(), "MyNetwork".to_string()))
        );
    }

    #[test]
    fn test_parse_commented_export_line() {
        assert_eq!(
            parse_commented_export_line("# export WIFI_SSID='MyNetwork'"),
            Some(("WIFI_SSID".to_string(), "MyNetwork".to_string()))
        );
    }

    #[test]
    fn test_parse_invalid_key() {
        assert_eq!(parse_export_line("export 123=bad"), None);
    }

    #[test]
    fn write_file_rejects_injection_key() {
        // A key carrying a newline + a second export would inject an
        // arbitrary variable (e.g. WEB_PASSWORD) into the bash-sourced
        // config. write_file must refuse the whole write.
        let dir = std::env::temp_dir().join(format!(
            "sentryusb-cfg-inject-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sentryusb.conf");
        std::fs::write(&path, "export GOOD=1\n").unwrap();

        let mut cfg = SetupConfig::new();
        cfg.insert("EVIL\nexport WEB_PASSWORD".to_string(), "x".to_string());
        let r = write_file(path.to_str().unwrap(), &cfg);
        assert!(r.is_err(), "injection key must be rejected");
        // The file must be untouched (no WEB_PASSWORD smuggled in).
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("WEB_PASSWORD"), "config must be unchanged");

        let mut ok = SetupConfig::new();
        ok.insert("GOOD".to_string(), "2".to_string());
        assert!(write_file(path.to_str().unwrap(), &ok).is_ok());

        // A newline in a VALUE can't round-trip the line-based parser, so
        // write_file must reject it rather than silently truncate on reload.
        let mut nl = SetupConfig::new();
        nl.insert("NOTIFICATION_COMMAND_START".to_string(), "line1\nline2".to_string());
        assert!(
            write_file(path.to_str().unwrap(), &nl).is_err(),
            "newline value must be rejected"
        );
        let after2 = std::fs::read_to_string(&path).unwrap();
        assert!(!after2.contains("line2"), "config must be unchanged on rejected value");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    // ── rsync SSH port ───────────────────────────────────────────────────

    fn cfg_with_port(raw: &str) -> SetupConfig {
        let mut cfg = SetupConfig::new();
        cfg.insert(RSYNC_SSH_PORT_KEY.to_string(), raw.to_string());
        cfg
    }

    /// Strict lookup against a config holding just this one key.
    fn strict(raw: &str) -> std::result::Result<Option<u16>, String> {
        validate_rsync_ssh_port(&cfg_with_port(raw))
    }

    #[test]
    fn ssh_port_unset_means_default() {
        // None rather than Some(22), so callers emit no port argument and
        // the existing command lines stay as they were.
        assert_eq!(validate_rsync_ssh_port(&SetupConfig::new()), Ok(None));
        // The wizard writes an empty export when the field is cleared.
        assert_eq!(strict(""), Ok(None));
        assert_eq!(strict("   "), Ok(None));
    }

    #[test]
    fn ssh_port_parses_custom_port() {
        assert_eq!(strict("23232"), Ok(Some(23232)));
        assert_eq!(strict("65535"), Ok(Some(65535)));
        // Values come from a bash-sourced file, so tolerate stray spaces.
        assert_eq!(strict(" 2222 "), Ok(Some(2222)));
    }

    #[test]
    fn ssh_port_rejects_non_ports() {
        for bad in ["0", "-1", "70000", "22/tcp", "twenty-two"] {
            assert!(
                strict(bad).is_err(),
                "{bad:?} must be rejected, not silently treated as port 22"
            );
        }
    }

    #[test]
    fn runtime_lookup_is_lenient_on_garbage() {
        // Runtime callers have nobody to report a config error to, so they
        // fall back to the default instead of failing the transfer.
        assert_eq!(rsync_ssh_port(&cfg_with_port("not-a-port")), None);
        assert_eq!(rsync_ssh_port(&cfg_with_port("23232")), Some(23232));
    }
}
