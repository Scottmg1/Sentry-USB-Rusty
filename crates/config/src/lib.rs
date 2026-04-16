use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

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
pub fn find_config_path() -> &'static str {
    for p in [DEFAULT_CONFIG_PATH, BOOT_CONFIG_PATH, LEGACY_BOOT_PATH] {
        if Path::new(p).exists() {
            // Leak the string so we can return a static reference.
            // This is called once at startup, so it's fine.
            return p;
        }
    }
    DEFAULT_CONFIG_PATH
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

    let mut file = fs::File::create(path)
        .with_context(|| format!("failed to write config file: {}", path))?;
    let mut writer = io::BufWriter::new(&mut file);
    for line in &output {
        writeln!(writer, "{}", line)?;
    }
    writer.flush()?;

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

/// Parses `KEY=VALUE` from a string.
fn parse_key_value(s: &str) -> Option<(String, String)> {
    let eq_pos = s.find('=')?;
    let key = &s[..eq_pos];

    // Validate key: must be [A-Za-z_][A-Za-z0-9_]*
    if key.is_empty() {
        return None;
    }
    let first = key.as_bytes()[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return None;
    }
    if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
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
    // If value contains no special characters, leave it bare
    const SPECIAL: &str = " \t'\"\\$!#&|;(){}[]<>?*~`";
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
}
