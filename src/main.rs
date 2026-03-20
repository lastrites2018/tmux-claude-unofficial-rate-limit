use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::Zeroize;

const CACHE_TTL: u64 = 900; // 15분

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

fn load_cache_from(path: &PathBuf, respect_ttl: bool, now_ts: f64) -> Option<Cache> {
    let s = fs::read_to_string(path).ok()?;
    let c: Cache = serde_json::from_str(&s).ok()?;
    if respect_ttl && (now_ts - c.fetched_at) >= CACHE_TTL as f64 {
        return None;
    }
    Some(c)
}

fn load_cache(respect_ttl: bool) -> Option<Cache> {
    let path = cache_path().ok()?;
    load_cache_from(&path, respect_ttl, now())
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

fn fetch(token: &str) -> Result<Cache, String> {
    let body = serde_json::to_vec(&serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "."}]
    }))
    .unwrap();

    let resp = ureq::post("https://api.anthropic.com/v1/messages")
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
    let h = |name: &str| -> f64 {
        hdrs.get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };

    Ok(Cache {
        fetched_at: now(),
        util_5h: h("anthropic-ratelimit-unified-5h-utilization"),
        util_1w: h("anthropic-ratelimit-unified-7d-utilization"),
        reset_5h: hdrs
            .get("anthropic-ratelimit-unified-5h-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0),
    })
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

fn fetch_with_lock(token: &str) -> Option<Cache> {
    use std::os::unix::fs::PermissionsExt;

    let path = lock_path().ok()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok()?;
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .ok()?;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .ok()?;

    // LOCK_EX | LOCK_NB = 6
    if unsafe { flock(file.as_raw_fd(), 6) } != 0 {
        return None;
    }

    // 락 후 캐시 재확인
    if let Some(c) = load_cache(true) {
        return Some(c);
    }

    let data = fetch(token).ok()?;
    if let Err(e) = save_cache(&data) {
        eprintln!("[warn] cache save failed: {e}");
    }
    Some(data)
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

fn remaining(util: f64) -> f64 {
    (100.0 - util * 100.0).max(0.0)
}

fn format_tmux(d: &Cache, now_ts: f64) -> String {
    let r5 = remaining(d.util_5h);
    let rw = remaining(d.util_1w);
    let reset = format_reset_at(d.reset_5h, now_ts);
    let age = (now_ts - d.fetched_at) as u64;
    let stale = if age > 120 {
        format!(" [{}m ago]", age / 60)
    } else {
        String::new()
    };

    let mut o = format!("5h:{r5:.0}%");
    if !reset.is_empty() {
        o.push_str(&format!("({reset})"));
    }
    o.push_str(&format!(" 1w:{rw:.0}%{stale}"));
    o
}

fn format_ansi(d: &Cache, now_ts: f64) -> String {
    let r5 = remaining(d.util_5h);
    let rw = remaining(d.util_1w);
    let reset = format_reset_at(d.reset_5h, now_ts);
    let age = (now_ts - d.fetched_at) as u64;
    let stale = if age > 120 {
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
    if !stale.is_empty() {
        o.push_str(&format!(" {s}{stale}{r}"));
    }
    o
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

fn main() {
    let args: Vec<String> = env::args().collect();

    // extract-token subcommand
    if args.iter().any(|a| a == "extract-token") {
        match extract_token() {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[err] {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let force = args.iter().any(|a| a == "--refresh");
    let tmux = args.iter().any(|a| a == "tmux");

    if let Err(e) = home() {
        if tmux {
            println!("[err]");
        } else {
            eprintln!("[err] {e}");
        }
        return;
    }

    let mut data = if force { None } else { load_cache(true) };

    if data.is_none() {
        let mut token = match get_token() {
            Some(t) => t,
            None => {
                eprintln!("[err] no token");
                if tmux {
                    println!("[err]");
                }
                return;
            }
        };

        if force {
            match fetch(&token) {
                Ok(d) => {
                    if let Err(e) = save_cache(&d) {
                        eprintln!("[warn] cache save failed: {e}");
                    }
                    data = Some(d);
                }
                Err(_) => {
                    data = load_cache(false);
                }
            }
        } else {
            data = fetch_with_lock(&token);
        }
        token.zeroize();

        if data.is_none() {
            data = load_cache(false);
        }
        if data.is_none() {
            if tmux {
                println!("[err]");
            } else {
                eprintln!("\x1b[38;2;243;139;168m[err] no data\x1b[0m");
            }
            return;
        }
    }

    let d = data.unwrap();
    let now_ts = now();
    if tmux {
        println!("{}", format_tmux(&d, now_ts));
    } else {
        println!("{}", format_ansi(&d, now_ts));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        };
        assert_eq!(format_tmux(&d, 1000.0), "5h:77%(30m) 1w:40%");
    }

    #[test]
    fn tmux_no_reset() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.5,
            util_1w: 0.8,
            reset_5h: 0,
        };
        assert_eq!(format_tmux(&d, 1000.0), "5h:50% 1w:20%");
    }

    #[test]
    fn tmux_stale_cache() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
        };
        // 5분 후 = 300초
        assert_eq!(format_tmux(&d, 1300.0), "5h:90% 1w:80% [5m ago]");
    }

    #[test]
    fn tmux_not_stale_within_2min() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.1,
            util_1w: 0.2,
            reset_5h: 0,
        };
        // 119초 후 → stale 아님
        assert_eq!(format_tmux(&d, 1119.0), "5h:90% 1w:80%");
    }

    #[test]
    fn tmux_over_limit() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 1.2,
            util_1w: 1.5,
            reset_5h: 1000 + 600,
        };
        assert_eq!(format_tmux(&d, 1000.0), "5h:0%(10m) 1w:0%");
    }

    #[test]
    fn tmux_reset_past() {
        let d = Cache {
            fetched_at: 1000.0,
            util_5h: 0.3,
            util_1w: 0.4,
            reset_5h: 500, // 과거
        };
        assert_eq!(format_tmux(&d, 1000.0), "5h:70% 1w:60%");
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
        };
        save_cache_to(&path, &data).unwrap();
        let loaded = load_cache_from(&path, false, 1000.0).unwrap();
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
        };
        save_cache_to(&path, &data).unwrap();
        // 899초 후 → TTL(900) 미만 → 유효
        assert!(load_cache_from(&path, true, 1899.0).is_some());
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
        };
        save_cache_to(&path, &data).unwrap();
        // 900초 후 → TTL(900) 이상 → 만료
        assert!(load_cache_from(&path, true, 1900.0).is_none());
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
        };
        save_cache_to(&path, &data).unwrap();
        // 만료여도 respect_ttl=false면 반환
        assert!(load_cache_from(&path, false, 99999.0).is_some());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cache_missing_file() {
        let path = tmp_path("nonexistent.json");
        assert!(load_cache_from(&path, false, 1000.0).is_none());
    }

    #[test]
    fn cache_invalid_json() {
        let path = tmp_path("invalid.json");
        fs::write(&path, "not json").unwrap();
        assert!(load_cache_from(&path, false, 1000.0).is_none());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cache_partial_json() {
        let path = tmp_path("partial.json");
        fs::write(&path, r#"{"fetched_at": 1000.0}"#).unwrap();
        assert!(load_cache_from(&path, false, 1000.0).is_none());
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
        };
        save_cache_to(&path, &data).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_file(&path);
    }
}
