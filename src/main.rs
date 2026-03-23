use pico_args::Arguments;
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zeroize::Zeroize;

const DEFAULT_CACHE_TTL_MINUTES: u64 = 15;
const MIN_CACHE_TTL_MINUTES: u64 = 1;
const MAX_CACHE_TTL_MINUTES: u64 = 60;
const DEFAULT_HTTP_TIMEOUT_SECONDS: u64 = 10;
const MIN_HTTP_TIMEOUT_SECONDS: u64 = 1;
const MAX_HTTP_TIMEOUT_SECONDS: u64 = 30;
const LOCK_RETRY_INTERVAL_MS: u64 = 200;
const LOCK_RETRY_ATTEMPTS: u32 = 15; // 약 3초
const WEEKLY_RESET_DISPLAY_THRESHOLD_PERCENT: u64 = 30;

fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or("HOME is not set".into())
}
fn credentials_path() -> Result<PathBuf, String> {
    Ok(home()?.join(".claude/.credentials.json"))
}
fn cache_path() -> Result<PathBuf, String> {
    Ok(home()?.join(".claude/rate-limit-cache.json"))
}
fn lock_path() -> Result<PathBuf, String> {
    Ok(home()?.join(".claude/rate-limit.lock"))
}
fn claude_config_path() -> Result<PathBuf, String> {
    Ok(home()?.join("Library/Application Support/Claude/config.json"))
}

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuthData>,
}
#[derive(Deserialize)]
struct OAuthData {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Cache {
    fetched_at: f64,
    util_5h: f64,
    util_1w: f64,
    reset_5h: u64,
    reset_1w: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandMode {
    Show,
    ExtractToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Ansi,
    Tmux,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cli {
    command: CommandMode,
    output_mode: OutputMode,
    force_refresh: bool,
    show_reset_dates: bool,
    cache_ttl: Duration,
    http_timeout: Duration,
}

#[derive(Serialize)]
struct JsonStatus {
    fetched_at: f64,
    cache_age_seconds: u64,
    stale: bool,
    utilization_5h: f64,
    utilization_1w: f64,
    remaining_5h_percent: f64,
    remaining_1w_percent: f64,
    reset_5h_unix: u64,
    reset_5h_in: String,
    reset_5h_at: String,
    reset_1w_unix: u64,
    reset_1w_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalTimeParts {
    year: i32,
    yday: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
}

fn format_arg_list(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_bounded_u64(
    name: &str,
    value: Option<u64>,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let parsed = value.unwrap_or(default);
    if !(min..=max).contains(&parsed) {
        return Err(format!("{name} must be in range {min}..={max}"));
    }
    Ok(parsed)
}

fn parse_cli_from(args: Vec<OsString>) -> Result<Cli, String> {
    let mut pargs = Arguments::from_vec(args);
    let force_refresh = pargs.contains("--refresh");
    let json = pargs.contains("--json");
    let show_reset_dates = pargs.contains("--show-reset-dates");
    let ttl_minutes_arg: Option<u64> = pargs
        .opt_value_from_str("--ttl-minutes")
        .map_err(|e| e.to_string())?;
    let http_timeout_seconds_arg: Option<u64> = pargs
        .opt_value_from_str("--http-timeout-seconds")
        .map_err(|e| e.to_string())?;
    let subcommand = pargs.subcommand().map_err(|e| e.to_string())?;
    let remaining = pargs.finish();
    if !remaining.is_empty() {
        return Err(format!(
            "unexpected arguments: {}",
            format_arg_list(&remaining)
        ));
    }

    let cache_ttl_minutes = parse_bounded_u64(
        "--ttl-minutes",
        ttl_minutes_arg,
        DEFAULT_CACHE_TTL_MINUTES,
        MIN_CACHE_TTL_MINUTES,
        MAX_CACHE_TTL_MINUTES,
    )?;
    let http_timeout_seconds = parse_bounded_u64(
        "--http-timeout-seconds",
        http_timeout_seconds_arg,
        DEFAULT_HTTP_TIMEOUT_SECONDS,
        MIN_HTTP_TIMEOUT_SECONDS,
        MAX_HTTP_TIMEOUT_SECONDS,
    )?;

    let command = match subcommand.as_deref() {
        Some("extract-token") => CommandMode::ExtractToken,
        Some("tmux") | None => CommandMode::Show,
        Some(other) => return Err(format!("unknown command: {other}")),
    };

    let output_mode = match (subcommand.as_deref(), json) {
        (Some("extract-token"), false) => OutputMode::Ansi,
        (Some("extract-token"), true) => {
            return Err("--json cannot be used with extract-token".into());
        }
        (Some("tmux"), false) => OutputMode::Tmux,
        (Some("tmux"), true) => return Err("--json cannot be used with tmux".into()),
        (None, true) => OutputMode::Json,
        (None, false) => OutputMode::Ansi,
        _ => OutputMode::Ansi,
    };

    if command == CommandMode::ExtractToken
        && (force_refresh
            || show_reset_dates
            || ttl_minutes_arg.is_some()
            || http_timeout_seconds_arg.is_some())
    {
        return Err("extract-token does not accept display/runtime options".into());
    }

    Ok(Cli {
        command,
        output_mode,
        force_refresh,
        show_reset_dates,
        cache_ttl: Duration::from_secs(cache_ttl_minutes * 60),
        http_timeout: Duration::from_secs(http_timeout_seconds),
    })
}

fn parse_cli() -> Result<Cli, String> {
    parse_cli_from(env::args_os().skip(1).collect())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn get_token() -> Option<String> {
    let path = credentials_path().ok()?;
    let mut s = fs::read_to_string(path).ok()?;
    let parsed = serde_json::from_str(&s).ok();
    s.zeroize();
    let c: Credentials = parsed?;
    c.oauth?.access_token
}

fn load_cache_from(
    path: &Path,
    respect_ttl: bool,
    now_ts: f64,
    cache_ttl: Duration,
) -> Option<Cache> {
    let s = fs::read_to_string(path).ok()?;
    let c: Cache = serde_json::from_str(&s).ok()?;
    if respect_ttl && (now_ts - c.fetched_at) >= cache_ttl.as_secs_f64() {
        return None;
    }
    Some(c)
}

fn load_cache(respect_ttl: bool, cache_ttl: Duration) -> Option<Cache> {
    let path = cache_path().ok()?;
    load_cache_from(&path, respect_ttl, now(), cache_ttl)
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("create parent dir: {e}"))?;

    let mut file = tempfile::Builder::new()
        .prefix(".rate-limit.")
        .tempfile_in(parent)
        .map_err(|e| format!("create temp file: {e}"))?;

    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| format!("set temp permissions: {e}"))?;

    file.as_file_mut()
        .write_all(bytes)
        .map_err(|e| format!("write temp file: {e}"))?;
    file.as_file_mut()
        .sync_all()
        .map_err(|e| format!("sync temp file: {e}"))?;

    file.persist(path)
        .map_err(|e| format!("rename: {}", e.error))?;

    Ok(())
}

fn save_cache_to(path: &Path, data: &Cache) -> Result<(), String> {
    let json = serde_json::to_string(data).unwrap();
    write_atomic(path, json.as_bytes(), 0o600)
}

fn save_cache(data: &Cache) -> Result<(), String> {
    let path = cache_path()?;
    save_cache_to(&path, data)
}

fn parse_required_f64_header(name: &str, value: Option<&str>) -> Result<f64, String> {
    let raw = value.ok_or_else(|| format!("missing rate-limit header: {name}"))?;
    raw.parse::<f64>()
        .map_err(|e| format!("invalid rate-limit header {name}: {e}"))
}

fn parse_optional_u64_header(name: &str, value: Option<&str>) -> Result<u64, String> {
    match value {
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|e| format!("invalid rate-limit header {name}: {e}")),
        None => Ok(0),
    }
}

fn cache_from_rate_limit_headers(
    util_5h: Option<&str>,
    util_1w: Option<&str>,
    reset_5h: Option<&str>,
    reset_1w: Option<&str>,
    fetched_at: f64,
) -> Result<Cache, String> {
    Ok(Cache {
        fetched_at,
        util_5h: parse_required_f64_header("anthropic-ratelimit-unified-5h-utilization", util_5h)?,
        util_1w: parse_required_f64_header("anthropic-ratelimit-unified-7d-utilization", util_1w)?,
        reset_5h: parse_optional_u64_header("anthropic-ratelimit-unified-5h-reset", reset_5h)?,
        reset_1w: parse_optional_u64_header("anthropic-ratelimit-unified-7d-reset", reset_1w)?,
    })
}

fn fetch(token: &str, http_timeout: Duration) -> Result<Cache, String> {
    let body = serde_json::to_vec(&serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "."}]
    }))
    .unwrap();

    let config = ureq::Agent::config_builder()
        .timeout_global(Some(http_timeout))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let resp = agent
        .post("https://api.anthropic.com/v1/messages")
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .send(&body)
        .map_err(|e| match &e {
            ureq::Error::StatusCode(401) => {
                "token expired — run: claude-rate-limit extract-token".into()
            }
            _ => e.to_string(),
        })?;

    let hdrs = resp.headers();
    cache_from_rate_limit_headers(
        hdrs.get("anthropic-ratelimit-unified-5h-utilization")
            .and_then(|v| v.to_str().ok()),
        hdrs.get("anthropic-ratelimit-unified-7d-utilization")
            .and_then(|v| v.to_str().ok()),
        hdrs.get("anthropic-ratelimit-unified-5h-reset")
            .and_then(|v| v.to_str().ok()),
        hdrs.get("anthropic-ratelimit-unified-7d-reset")
            .and_then(|v| v.to_str().ok()),
        now(),
    )
}

unsafe extern "C" {
    fn flock(fd: i32, op: i32) -> i32;
    // CommonCrypto AES
    fn CCCrypt(
        op: u32,
        alg: u32,
        options: u32,
        key: *const u8,
        key_len: usize,
        iv: *const u8,
        data_in: *const u8,
        data_in_len: usize,
        data_out: *mut u8,
        data_out_avail: usize,
        data_out_moved: *mut usize,
    ) -> i32;
}

// === extract-token ===

fn keychain_password() -> Result<String, String> {
    let mut out = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Safe Storage",
            "-a",
            "Claude Key",
            "-w",
        ])
        .output()
        .map_err(|e| format!("security command failed: {e}"))?;
    if !out.status.success() {
        out.stdout.zeroize();
        out.stderr.zeroize();
        return Err("keychain access denied — run from interactive terminal".into());
    }

    let stdout = std::mem::take(&mut out.stdout);
    out.stderr.zeroize();

    let mut password = match String::from_utf8(stdout) {
        Ok(s) => s,
        Err(e) => {
            let msg = e.utf8_error().to_string();
            let mut bytes = e.into_bytes();
            bytes.zeroize();
            return Err(format!("security output UTF-8 decode: {msg}"));
        }
    };

    while matches!(password.chars().last(), Some('\n' | '\r')) {
        password.pop();
    }

    Ok(password)
}

fn decrypt_safe_storage(encrypted_b64: &str, password: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encrypted_b64)
        .map_err(|e| format!("base64 decode: {e}"))?;

    if raw.len() < 3 || &raw[..3] != b"v10" {
        return Err("not v10 encrypted data".into());
    }
    let ciphertext = &raw[3..];

    // PBKDF2(password, "saltysalt", 1003, 16)
    let mut key = [0u8; 16];
    pbkdf2_sha1(password.as_bytes(), b"saltysalt", 1003, &mut key);

    let iv = [0x20u8; 16]; // space * 16

    let mut out = vec![0u8; ciphertext.len() + 16];
    let mut out_len: usize = 0;

    // kCCDecrypt=1, kCCAlgorithmAES128=0, kCCOptionPKCS7Padding=1
    let status = unsafe {
        CCCrypt(
            1,
            0,
            1,
            key.as_ptr(),
            key.len(),
            iv.as_ptr(),
            ciphertext.as_ptr(),
            ciphertext.len(),
            out.as_mut_ptr(),
            out.len(),
            &mut out_len,
        )
    };
    key.zeroize();

    if status != 0 {
        out.zeroize();
        return Err(format!("CCCrypt failed: {status}"));
    }
    out.truncate(out_len);
    Ok(out)
}

fn pbkdf2_sha1(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    // PBKDF2-HMAC-SHA1 (minimal, no external crate)
    use std::num::Wrapping;

    fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
        let mut k = [0u8; 64];
        if key.len() > 64 {
            k[..20].copy_from_slice(&sha1(key));
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for i in 0..64 {
            ipad[i] ^= k[i];
            opad[i] ^= k[i];
        }
        let mut inner = ipad.to_vec();
        inner.extend_from_slice(msg);
        let inner_hash = sha1(&inner);
        let mut outer = opad.to_vec();
        outer.extend_from_slice(&inner_hash);
        sha1(&outer)
    }

    fn sha1(data: &[u8]) -> [u8; 20] {
        let mut h0 = Wrapping(0x67452301u32);
        let mut h1 = Wrapping(0xEFCDAB89u32);
        let mut h2 = Wrapping(0x98BADCFEu32);
        let mut h3 = Wrapping(0x10325476u32);
        let mut h4 = Wrapping(0xC3D2E1F0u32);

        let bit_len = (data.len() as u64) * 8;
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());

        for chunk in msg.chunks(64) {
            let mut w = [0u32; 80];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ]);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
            for (i, wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | ((!b) & d), Wrapping(0x5A827999)),
                    20..=39 => (b ^ c ^ d, Wrapping(0x6ED9EBA1)),
                    40..=59 => ((b & c) | (b & d) | (c & d), Wrapping(0x8F1BBCDC)),
                    _ => (b ^ c ^ d, Wrapping(0xCA62C1D6)),
                };
                let temp = Wrapping(a.0.rotate_left(5)) + f + e + k + Wrapping(*wi);
                e = d;
                d = c;
                c = Wrapping(b.0.rotate_left(30));
                b = a;
                a = temp;
            }
            h0 += a;
            h1 += b;
            h2 += c;
            h3 += d;
            h4 += e;
        }
        let mut r = [0u8; 20];
        r[0..4].copy_from_slice(&h0.0.to_be_bytes());
        r[4..8].copy_from_slice(&h1.0.to_be_bytes());
        r[8..12].copy_from_slice(&h2.0.to_be_bytes());
        r[12..16].copy_from_slice(&h3.0.to_be_bytes());
        r[16..20].copy_from_slice(&h4.0.to_be_bytes());
        r
    }

    // PBKDF2 with HMAC-SHA1
    let mut salt_block = salt.to_vec();
    salt_block.extend_from_slice(&1u32.to_be_bytes()); // block index 1
    let mut u = hmac_sha1(password, &salt_block);
    let mut result = u;
    for _ in 1..iterations {
        u = hmac_sha1(password, &u);
        for j in 0..20 {
            result[j] ^= u[j];
        }
    }
    let copy_len = out.len().min(20);
    out[..copy_len].copy_from_slice(&result[..copy_len]);
}

fn extract_token() -> Result<(), String> {
    let mut password = keychain_password()?;

    let config_path = claude_config_path()?;
    let mut config_str =
        fs::read_to_string(config_path).map_err(|e| format!("read config.json: {e}"))?;
    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(e) => {
            config_str.zeroize();
            return Err(format!("parse config.json: {e}"));
        }
    };
    config_str.zeroize();

    let mut encrypted = config
        .get("oauth:tokenCache")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or("no oauth:tokenCache in config.json")?;

    let decrypted = decrypt_safe_storage(&encrypted, &password);
    encrypted.zeroize();
    password.zeroize();
    let decrypted = decrypted?;
    let mut decrypted_str = match String::from_utf8(decrypted) {
        Ok(s) => s,
        Err(e) => {
            let msg = e.utf8_error().to_string();
            let mut bytes = e.into_bytes();
            bytes.zeroize();
            return Err(format!("UTF-8 decode: {msg}"));
        }
    };

    let token_data: serde_json::Value = match serde_json::from_str(&decrypted_str) {
        Ok(v) => v,
        Err(e) => {
            decrypted_str.zeroize();
            return Err(format!("parse decrypted JSON: {e}"));
        }
    };
    decrypted_str.zeroize();

    let mut token = token_data
        .as_object()
        .and_then(|obj| {
            obj.values().find_map(|v| {
                v.as_object()
                    .and_then(|inner| inner.get("token"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
        })
        .ok_or("no token found in decrypted data")?;

    let creds = serde_json::json!({"claudeAiOauth": {"accessToken": token.as_str()}});
    let path = credentials_path()?;
    let mut json = serde_json::to_string(&creds).unwrap();
    token.zeroize();

    let write_result = write_atomic(&path, json.as_bytes(), 0o600);
    json.zeroize();
    write_result?;

    println!("saved to {}", path.display());
    Ok(())
}

enum LockAcquireResult {
    Acquired,
    Cache(Cache),
    TimedOut,
}

fn wait_for_lock_or_cache<F, C, S>(
    mut try_lock: F,
    mut load_cache: C,
    mut sleep: S,
) -> LockAcquireResult
where
    F: FnMut() -> bool,
    C: FnMut() -> Option<Cache>,
    S: FnMut(),
{
    for attempt in 0..=LOCK_RETRY_ATTEMPTS {
        if try_lock() {
            return LockAcquireResult::Acquired;
        }
        if let Some(cache) = load_cache() {
            return LockAcquireResult::Cache(cache);
        }
        if attempt == LOCK_RETRY_ATTEMPTS {
            return LockAcquireResult::TimedOut;
        }
        sleep();
    }
    LockAcquireResult::TimedOut
}

fn fetch_with_lock_at(
    lock_path: &Path,
    cache_path: &Path,
    token: &str,
    cache_ttl: Duration,
    http_timeout: Duration,
) -> Option<Cache> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).ok()?;
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .ok()?;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .ok()?;

    let lock_result = wait_for_lock_or_cache(
        || unsafe { flock(file.as_raw_fd(), 6) } == 0,
        || load_cache_from(cache_path, true, now(), cache_ttl),
        || std::thread::sleep(Duration::from_millis(LOCK_RETRY_INTERVAL_MS)),
    );

    match lock_result {
        LockAcquireResult::Cache(cache) => Some(cache),
        LockAcquireResult::TimedOut => None,
        LockAcquireResult::Acquired => {
            if let Some(cache) = load_cache_from(cache_path, true, now(), cache_ttl) {
                return Some(cache);
            }

            let data = fetch(token, http_timeout).ok()?;
            if let Err(e) = save_cache_to(cache_path, &data) {
                eprintln!("[warn] cache save failed: {e}");
            }
            Some(data)
        }
    }
}

fn fetch_with_lock(token: &str, cache_ttl: Duration, http_timeout: Duration) -> Option<Cache> {
    let cache = cache_path().ok()?;
    let lock = lock_path().ok()?;
    fetch_with_lock_at(&lock, &cache, token, cache_ttl, http_timeout)
}

fn format_reset_at(ts: u64, now_ts: f64) -> String {
    if ts == 0 {
        return String::new();
    }
    let s = ts as i64 - now_ts as i64;
    if s <= 0 {
        return String::new();
    }
    let (h, m) = (s / 3600, (s % 3600) / 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

fn local_time_parts(ts: u64) -> Option<LocalTimeParts> {
    let raw = ts as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let ptr = unsafe { libc::localtime_r(&raw, tm.as_mut_ptr()) };
    if ptr.is_null() {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    Some(LocalTimeParts {
        year: tm.tm_year + 1900,
        yday: tm.tm_yday,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    })
}

fn is_same_local_day(a: LocalTimeParts, b: LocalTimeParts) -> bool {
    a.year == b.year && a.yday == b.yday
}

fn format_month_day_time(parts: LocalTimeParts) -> String {
    if parts.minute == 0 {
        format!("{}/{} {}", parts.month, parts.day, parts.hour)
    } else {
        format!(
            "{}/{} {}:{:02}",
            parts.month, parts.day, parts.hour, parts.minute
        )
    }
}

fn format_absolute_reset(ts: u64) -> String {
    local_time_parts(ts)
        .map(format_month_day_time)
        .unwrap_or_default()
}

fn cache_age_seconds(fetched_at: f64, now_ts: f64) -> u64 {
    if now_ts <= fetched_at {
        0
    } else {
        (now_ts - fetched_at) as u64
    }
}

fn is_stale(age_seconds: u64, cache_ttl: Duration) -> bool {
    age_seconds >= cache_ttl.as_secs()
}

fn remaining(util: f64) -> f64 {
    (100.0 - util * 100.0).max(0.0)
}

fn rounded_percent(percent: f64) -> u64 {
    format!("{percent:.0}").parse().unwrap_or_default()
}

fn should_show_weekly_reset(
    remaining_1w_percent: f64,
    reset_1w: u64,
    show_reset_dates: bool,
) -> bool {
    show_reset_dates
        && reset_1w != 0
        && rounded_percent(remaining_1w_percent) <= WEEKLY_RESET_DISPLAY_THRESHOLD_PERCENT
}

fn format_5h_reset_display(reset_5h: u64, now_ts: f64, show_reset_dates: bool) -> String {
    let relative = format_reset_at(reset_5h, now_ts);
    if !show_reset_dates || relative.is_empty() {
        return relative;
    }

    let now_parts = match local_time_parts(now_ts as u64) {
        Some(parts) => parts,
        None => return relative,
    };
    let reset_parts = match local_time_parts(reset_5h) {
        Some(parts) => parts,
        None => return relative,
    };

    if is_same_local_day(now_parts, reset_parts) {
        relative
    } else {
        format_month_day_time(reset_parts)
    }
}

fn format_weekly_reset_suffix(rw: f64, reset_1w: u64, show_reset_dates: bool) -> String {
    if !should_show_weekly_reset(rw, reset_1w, show_reset_dates) {
        return String::new();
    }
    let absolute = format_absolute_reset(reset_1w);
    if absolute.is_empty() {
        String::new()
    } else {
        format!("({absolute})")
    }
}

fn format_tmux(d: &Cache, now_ts: f64, cache_ttl: Duration, show_reset_dates: bool) -> String {
    let r5 = remaining(d.util_5h);
    let rw = remaining(d.util_1w);
    let reset = format_5h_reset_display(d.reset_5h, now_ts, show_reset_dates);
    let age = cache_age_seconds(d.fetched_at, now_ts);
    let stale = if is_stale(age, cache_ttl) {
        format!(" [{}m ago]", age / 60)
    } else {
        String::new()
    };
    let weekly_reset = if stale.is_empty() {
        format_weekly_reset_suffix(rw, d.reset_1w, show_reset_dates)
    } else {
        String::new()
    };

    let mut o = format!("5h:{r5:.0}%");
    if !reset.is_empty() {
        o.push_str(&format!("({reset})"));
    }
    o.push_str(&format!(" 1w:{rw:.0}%{weekly_reset}{stale}"));
    o
}

fn format_ansi(d: &Cache, now_ts: f64, cache_ttl: Duration, show_reset_dates: bool) -> String {
    let r5 = remaining(d.util_5h);
    let rw = remaining(d.util_1w);
    let reset = format_5h_reset_display(d.reset_5h, now_ts, show_reset_dates);
    let weekly_reset = format_weekly_reset_suffix(rw, d.reset_1w, show_reset_dates);
    let age = cache_age_seconds(d.fetched_at, now_ts);
    let stale = if is_stale(age, cache_ttl) {
        format!(" [{}m ago]", age / 60)
    } else {
        String::new()
    };

    let (b, s, bd, r) = (
        "\x1b[38;2;137;180;250m",
        "\x1b[38;2;88;91;112m",
        "\x1b[1m",
        "\x1b[0m",
    );
    let mut o = format!("{b}{bd}5h{r} {}{r5:.0}%{r}", color(r5));
    if !reset.is_empty() {
        o.push_str(&format!("{s}({reset}){r}"));
    }
    o.push_str(&format!(" {s}·{r} {b}{bd}1w{r} {}{rw:.0}%{r}", color(rw)));
    if !weekly_reset.is_empty() {
        o.push_str(&format!("{s}{weekly_reset}{r}"));
    }
    if !stale.is_empty() {
        o.push_str(&format!(" {s}{stale}{r}"));
    }
    o
}

fn format_json(d: &Cache, now_ts: f64, cache_ttl: Duration) -> String {
    let age = cache_age_seconds(d.fetched_at, now_ts);
    serde_json::to_string(&JsonStatus {
        fetched_at: d.fetched_at,
        cache_age_seconds: age,
        stale: is_stale(age, cache_ttl),
        utilization_5h: d.util_5h,
        utilization_1w: d.util_1w,
        remaining_5h_percent: remaining(d.util_5h),
        remaining_1w_percent: remaining(d.util_1w),
        reset_5h_unix: d.reset_5h,
        reset_5h_in: format_reset_at(d.reset_5h, now_ts),
        reset_5h_at: format_absolute_reset(d.reset_5h),
        reset_1w_unix: d.reset_1w,
        reset_1w_at: format_absolute_reset(d.reset_1w),
    })
    .unwrap()
}

fn color(rem: f64) -> &'static str {
    if rem <= 20.0 {
        "\x1b[38;2;243;139;168m"
    } else if rem <= 50.0 {
        "\x1b[38;2;250;179;135m"
    } else {
        "\x1b[38;2;166;227;161m"
    }
}

fn print_show_error(output_mode: OutputMode, message: &str) {
    match output_mode {
        OutputMode::Tmux => println!("[err]"),
        OutputMode::Json => {
            println!("{}", serde_json::json!({ "error": message }));
        }
        OutputMode::Ansi => eprintln!("[err] {message}"),
    }
}

fn show_error_exit_code(output_mode: OutputMode) -> i32 {
    match output_mode {
        OutputMode::Tmux => 0,
        OutputMode::Json | OutputMode::Ansi => 1,
    }
}

fn run_show(cli: Cli) -> Result<String, String> {
    home()?;

    let mut data = if cli.force_refresh {
        None
    } else {
        load_cache(true, cli.cache_ttl)
    };

    if data.is_none() {
        let mut token = get_token().ok_or_else(|| "no token".to_string())?;

        if cli.force_refresh {
            match fetch(&token, cli.http_timeout) {
                Ok(d) => {
                    if let Err(e) = save_cache(&d) {
                        eprintln!("[warn] cache save failed: {e}");
                    }
                    data = Some(d);
                }
                Err(_) => {
                    data = load_cache(false, cli.cache_ttl);
                }
            }
        } else {
            data = fetch_with_lock(&token, cli.cache_ttl, cli.http_timeout);
        }
        token.zeroize();

        if data.is_none() {
            data = load_cache(false, cli.cache_ttl);
        }
    }

    let data = data.ok_or_else(|| "no data".to_string())?;
    let now_ts = now();
    let rendered = match cli.output_mode {
        OutputMode::Tmux => format_tmux(&data, now_ts, cli.cache_ttl, cli.show_reset_dates),
        OutputMode::Json => format_json(&data, now_ts, cli.cache_ttl),
        OutputMode::Ansi => format_ansi(&data, now_ts, cli.cache_ttl, cli.show_reset_dates),
    };
    Ok(rendered)
}

fn main() {
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("[err] {e}");
            std::process::exit(2);
        }
    };

    match cli.command {
        CommandMode::ExtractToken => {
            if let Err(e) = extract_token() {
                eprintln!("[err] {e}");
                std::process::exit(1);
            }
        }
        CommandMode::Show => match run_show(cli) {
            Ok(output) => println!("{output}"),
            Err(e) => {
                print_show_error(cli.output_mode, &e);
                let exit_code = show_error_exit_code(cli.output_mode);
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn default_cache_ttl() -> Duration {
        Duration::from_secs(DEFAULT_CACHE_TTL_MINUTES * 60)
    }

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = env::temp_dir();
        p.push(format!("rate-limit-test-{name}-{}", std::process::id()));
        p
    }

    // === remaining() ===

    #[test]
    fn remaining_normal() {
        assert_eq!(remaining(0.23), 77.0);
    }

    #[test]
    fn remaining_zero_usage() {
        assert_eq!(remaining(0.0), 100.0);
    }

    #[test]
    fn remaining_full_usage() {
        assert_eq!(remaining(1.0), 0.0);
    }

    #[test]
    fn remaining_over_limit_clamps_to_zero() {
        assert_eq!(remaining(1.5), 0.0);
    }

    #[test]
    fn remaining_small_fraction() {
        // 0.006 → 99.4%
        let r = remaining(0.006);
        assert!((r - 99.4).abs() < 0.01);
    }

    // === format_reset_at() ===

    #[test]
    fn reset_zero_timestamp() {
        assert_eq!(format_reset_at(0, 1000.0), "");
    }

    #[test]
    fn reset_past_timestamp() {
        assert_eq!(format_reset_at(900, 1000.0), "");
    }

    #[test]
    fn reset_30min_future() {
        assert_eq!(format_reset_at(1000 + 1800, 1000.0), "30m");
    }

    #[test]
    fn reset_2h30m_future() {
        // 2*3600 + 30*60 = 9000
        assert_eq!(format_reset_at(1000 + 9000, 1000.0), "2h30m");
    }

    #[test]
    fn reset_1sec_future() {
        assert_eq!(format_reset_at(1001, 1000.0), "0m");
    }

    #[test]
    fn reset_59sec_future() {
        assert_eq!(format_reset_at(1059, 1000.0), "0m");
    }

    #[test]
    fn reset_60sec_future() {
        assert_eq!(format_reset_at(1060, 1000.0), "1m");
    }

    #[test]
    fn reset_exactly_1h() {
        assert_eq!(format_reset_at(1000 + 3600, 1000.0), "1h00m");
    }

    // === format_tmux() ===

    #[test]
    fn tmux_normal_output() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.23,
            util_1w: 0.6,
            reset_5h: 1000 + 1800, // 30분 후
            reset_1w: 0,
        };
        assert_eq!(
            format_tmux(&d, 1000.0, default_cache_ttl(), false),
            "5h:77%(30m) 1w:40%"
        );
    }

    #[test]
    fn tmux_no_reset() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.5,
            util_1w: 0.8,
            reset_5h: 0,
            reset_1w: 0,
        };
        assert_eq!(
            format_tmux(&d, 1000.0, default_cache_ttl(), false),
            "5h:50% 1w:20%"
        );
    }

    #[test]
    fn tmux_stale_cache() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
            reset_1w: 0,
        };
        // 5분 후 = 300초, TTL 2분
        assert_eq!(
            format_tmux(&d, 1300.0, Duration::from_secs(120), false),
            "5h:90% 1w:80% [5m ago]"
        );
    }

    #[test]
    fn tmux_not_stale_within_ttl() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
            reset_1w: 0,
        };
        // 5분 TTL에서 119초 후 → stale 아님
        assert_eq!(
            format_tmux(&d, 1119.0, Duration::from_secs(300), false),
            "5h:90% 1w:80%"
        );
    }

    #[test]
    fn tmux_over_limit() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 1.2,
            util_1w: 1.5,
            reset_5h: 1000 + 600,
            reset_1w: 0,
        };
        assert_eq!(
            format_tmux(&d, 1000.0, default_cache_ttl(), false),
            "5h:0%(10m) 1w:0%"
        );
    }

    #[test]
    fn tmux_reset_past() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.3,
            util_1w: 0.4,
            reset_5h: 500, // 과거
            reset_1w: 0,
        };
        assert_eq!(
            format_tmux(&d, 1000.0, default_cache_ttl(), false),
            "5h:70% 1w:60%"
        );
    }

    // === cache round-trip ===

    #[test]
    fn cache_save_load_roundtrip() {
        let path = tmp_path("roundtrip.json");
        let data = Cache {
            fetched_at: 1000.0,
            util_5h: 0.23,
            util_1w: 0.6,
            reset_5h: 2000,
            reset_1w: 3000,
        };
        save_cache_to(&path, &data).unwrap();
        let loaded = load_cache_from(&path, false, 1000.0, default_cache_ttl()).unwrap();
        assert_eq!(data, loaded);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cache_ttl_fresh() {
        let path = tmp_path("fresh.json");
        let data = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
            reset_1w: 0,
        };
        save_cache_to(&path, &data).unwrap();
        // 899초 후 → TTL(900) 미만 → 유효
        assert!(load_cache_from(&path, true, 1899.0, default_cache_ttl()).is_some());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cache_ttl_expired() {
        let path = tmp_path("expired.json");
        let data = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
            reset_1w: 0,
        };
        save_cache_to(&path, &data).unwrap();
        // 900초 후 → TTL(900) 이상 → 만료
        assert!(load_cache_from(&path, true, 1900.0, default_cache_ttl()).is_none());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cache_ttl_ignored_when_false() {
        let path = tmp_path("ignore-ttl.json");
        let data = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
            reset_1w: 0,
        };
        save_cache_to(&path, &data).unwrap();
        // 만료여도 respect_ttl=false면 반환
        assert!(load_cache_from(&path, false, 99999.0, default_cache_ttl()).is_some());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cache_missing_file() {
        let path = tmp_path("nonexistent.json");
        assert!(load_cache_from(&path, false, 1000.0, default_cache_ttl()).is_none());
    }

    #[test]
    fn cache_invalid_json() {
        let path = tmp_path("invalid.json");
        fs::write(&path, "not json").unwrap();
        assert!(load_cache_from(&path, false, 1000.0, default_cache_ttl()).is_none());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cache_partial_json() {
        let path = tmp_path("partial.json");
        fs::write(&path, r#"{"fetched_at": 1000.0}"#).unwrap();
        assert!(load_cache_from(&path, false, 1000.0, default_cache_ttl()).is_none());
        let _ = fs::remove_file(&path);
    }

    // === color() ===

    #[test]
    fn color_thresholds() {
        let red = "\x1b[38;2;243;139;168m";
        let peach = "\x1b[38;2;250;179;135m";
        let green = "\x1b[38;2;166;227;161m";

        assert_eq!(color(0.0), red);
        assert_eq!(color(20.0), red);
        assert_eq!(color(20.1), peach);
        assert_eq!(color(50.0), peach);
        assert_eq!(color(50.1), green);
        assert_eq!(color(100.0), green);
    }

    // === credentials parsing ===

    #[test]
    fn parse_valid_credentials() {
        let json = r#"{"claudeAiOauth":{"accessToken":"sk-ant-test123"}}"#;
        let c: Credentials = serde_json::from_str(json).unwrap();
        assert_eq!(c.oauth.unwrap().access_token.unwrap(), "sk-ant-test123");
    }

    #[test]
    fn parse_missing_oauth() {
        let json = r#"{}"#;
        let c: Credentials = serde_json::from_str(json).unwrap();
        assert!(c.oauth.is_none());
    }

    #[test]
    fn parse_missing_token() {
        let json = r#"{"claudeAiOauth":{}}"#;
        let c: Credentials = serde_json::from_str(json).unwrap();
        assert!(c.oauth.unwrap().access_token.is_none());
    }

    // === atomic write ===

    #[test]
    fn save_cache_no_leftover_tmp() {
        let dir = tmp_path("atomic-dir");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("atomic.json");
        let data = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
            reset_1w: 0,
        };
        save_cache_to(&path, &data).unwrap();
        assert!(path.exists());
        let leftover = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .any(|name| name.starts_with(".rate-limit."));
        assert!(!leftover);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_cache_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = tmp_path("mode.json");
        let data = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
            reset_1w: 0,
        };
        save_cache_to(&path, &data).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_file(&path);
    }

    // === rate-limit headers ===

    #[test]
    fn parse_rate_limit_headers_requires_utilization_headers() {
        let err =
            cache_from_rate_limit_headers(None, Some("0.4"), Some("1234"), Some("4567"), 1000.0)
                .unwrap_err();
        assert!(err.contains("anthropic-ratelimit-unified-5h-utilization"));
    }

    #[test]
    fn parse_rate_limit_headers_rejects_invalid_utilization() {
        let err = cache_from_rate_limit_headers(
            Some("abc"),
            Some("0.4"),
            Some("1234"),
            Some("4567"),
            1000.0,
        )
        .unwrap_err();
        assert!(err.contains("anthropic-ratelimit-unified-5h-utilization"));
    }

    #[test]
    fn parse_rate_limit_headers_allows_missing_reset() {
        let cache =
            cache_from_rate_limit_headers(Some("0.23"), Some("0.6"), None, None, 1000.0).unwrap();
        assert_eq!(
            cache,
            Cache {
                fetched_at: 1000.0,
                util_5h: 0.23,
                util_1w: 0.6,
                reset_5h: 0,
                reset_1w: 0,
            }
        );
    }

    // === lock wait ===

    #[test]
    fn wait_for_lock_or_cache_returns_cache_after_retry() {
        let expected = Cache {
            fetched_at: 1000.0,
            util_5h: 0.23,
            util_1w: 0.6,
            reset_5h: 2000,
            reset_1w: 3000,
        };
        let try_count = Cell::new(0usize);
        let sleep_count = Cell::new(0usize);

        let result = wait_for_lock_or_cache(
            || {
                try_count.set(try_count.get() + 1);
                false
            },
            || {
                if try_count.get() >= 3 {
                    Some(expected.clone())
                } else {
                    None
                }
            },
            || {
                sleep_count.set(sleep_count.get() + 1);
            },
        );

        match result {
            LockAcquireResult::Cache(cache) => assert_eq!(cache, expected),
            _ => panic!("expected cache result"),
        }
        assert_eq!(try_count.get(), 3);
        assert_eq!(sleep_count.get(), 2);
    }

    #[test]
    fn wait_for_lock_or_cache_times_out_after_configured_retries() {
        let try_count = Cell::new(0usize);
        let sleep_count = Cell::new(0usize);

        let result = wait_for_lock_or_cache(
            || {
                try_count.set(try_count.get() + 1);
                false
            },
            || None,
            || {
                sleep_count.set(sleep_count.get() + 1);
            },
        );

        assert!(matches!(result, LockAcquireResult::TimedOut));
        assert_eq!(try_count.get(), (LOCK_RETRY_ATTEMPTS + 1) as usize);
        assert_eq!(sleep_count.get(), LOCK_RETRY_ATTEMPTS as usize);
    }

    // === cli parsing ===

    #[test]
    fn parse_cli_accepts_json_and_bounded_options() {
        let cli = parse_cli_from(vec![
            OsString::from("--json"),
            OsString::from("--ttl-minutes"),
            OsString::from("5"),
            OsString::from("--http-timeout-seconds"),
            OsString::from("9"),
        ])
        .unwrap();

        assert_eq!(cli.command, CommandMode::Show);
        assert_eq!(cli.output_mode, OutputMode::Json);
        assert!(!cli.show_reset_dates);
        assert_eq!(cli.cache_ttl, Duration::from_secs(5 * 60));
        assert_eq!(cli.http_timeout, Duration::from_secs(9));
    }

    #[test]
    fn parse_cli_accepts_show_reset_dates_flag() {
        let cli = parse_cli_from(vec![OsString::from("--show-reset-dates")]).unwrap();
        assert!(cli.show_reset_dates);
    }

    #[test]
    fn parse_cli_rejects_json_with_tmux() {
        let err =
            parse_cli_from(vec![OsString::from("tmux"), OsString::from("--json")]).unwrap_err();
        assert!(err.contains("--json cannot be used with tmux"));
    }

    #[test]
    fn parse_cli_rejects_out_of_range_ttl() {
        let err = parse_cli_from(vec![OsString::from("--ttl-minutes"), OsString::from("61")])
            .unwrap_err();
        assert!(err.contains("--ttl-minutes"));
    }

    #[test]
    fn parse_cli_rejects_runtime_options_for_extract_token() {
        let err = parse_cli_from(vec![
            OsString::from("extract-token"),
            OsString::from("--http-timeout-seconds"),
            OsString::from("3"),
        ])
        .unwrap_err();
        assert!(err.contains("extract-token does not accept"));
    }

    #[test]
    fn show_error_exit_code_is_nonzero_for_scriptable_modes() {
        assert_eq!(show_error_exit_code(OutputMode::Tmux), 0);
        assert_eq!(show_error_exit_code(OutputMode::Ansi), 1);
        assert_eq!(show_error_exit_code(OutputMode::Json), 1);
    }

    // === json output ===

    #[test]
    fn format_json_includes_structured_fields() {
        let cache = Cache {
            fetched_at: 1000.0,
            util_5h: 0.23,
            util_1w: 0.6,
            reset_5h: 2800,
            reset_1w: 4000,
        };
        let value: serde_json::Value =
            serde_json::from_str(&format_json(&cache, 1300.0, Duration::from_secs(240))).unwrap();

        assert_eq!(value["cache_age_seconds"], 300);
        assert_eq!(value["stale"], true);
        assert_eq!(value["remaining_5h_percent"], 77.0);
        assert_eq!(value["remaining_1w_percent"], 40.0);
        assert_eq!(value["reset_5h_in"], "25m");
        assert_eq!(value["reset_1w_unix"], 4000);
    }

    #[test]
    fn weekly_reset_suffix_hidden_above_threshold() {
        assert_eq!(format_weekly_reset_suffix(30.6, 1774846800, true), "");
    }

    #[test]
    fn weekly_reset_suffix_shown_at_or_below_threshold() {
        let suffix = format_weekly_reset_suffix(30.0, 1774846800, true);
        assert!(suffix.starts_with('('));
        assert!(suffix.ends_with(')'));
        assert!(suffix.contains('/'));
    }

    #[test]
    fn weekly_reset_suffix_uses_displayed_percent_threshold() {
        let suffix = format_weekly_reset_suffix(30.4, 1774846800, true);
        assert!(suffix.starts_with('('));
        assert!(suffix.ends_with(')'));
    }

    #[test]
    fn format_month_day_time_omits_minutes_when_zero() {
        let parts = LocalTimeParts {
            year: 2026,
            yday: 88,
            month: 3,
            day: 30,
            hour: 14,
            minute: 0,
        };
        assert_eq!(format_month_day_time(parts), "3/30 14");
    }

    #[test]
    fn tmux_hides_weekly_reset_when_stale_suffix_is_present() {
        let cache = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.8,
            reset_5h: 0,
            reset_1w: 1774846800,
        };
        assert_eq!(
            format_tmux(&cache, 1300.0, Duration::from_secs(120), true),
            "5h:90% 1w:20% [5m ago]"
        );
    }
}
