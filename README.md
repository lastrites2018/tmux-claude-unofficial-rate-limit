# tmux-claude-unofficial-rate-limit

Display Claude Max (Pro/Team) subscription rate limit remaining percentage in your tmux status bar.

[한국어](README.ko.md)

```
5h:77%(4h07m) 1w:40%
```

- **5h:77%** — 5-hour window remaining 77%
- **(4h07m)** — time until reset
- **1w:40%** — weekly remaining 40%

## Requirements

- macOS (uses CommonCrypto, Keychain)
- Claude Code or Claude Desktop logged in
- Rust toolchain (for building)
- tmux (for status bar display)

## Install

```bash
git clone https://github.com/lastrites2018/tmux-claude-unofficial-rate-limit.git
cd tmux-claude-unofficial-rate-limit
cargo build --release
cp target/release/rate-limit ~/.local/bin/claude-rate-limit
```

## Initial Setup

Extract the OAuth token from Claude Desktop's encrypted storage. When the macOS Keychain popup appears, click **Allow**.

```bash
claude-rate-limit extract-token
```

This writes the token atomically to `~/.claude/.credentials.json` with owner-only permissions (`0600`).

## Usage

```bash
# tmux status bar (no color)
claude-rate-limit tmux

# terminal with ANSI colors
claude-rate-limit

# force refresh (ignore cache)
claude-rate-limit --refresh

# extract/refresh token
claude-rate-limit extract-token
```

## tmux Configuration

Add to `~/.tmux.conf`:

```tmux
set -g status-right '#(~/.local/bin/claude-rate-limit tmux) | %Y-%m-%d %H:%M '
```

Reload:

```bash
tmux source-file ~/.tmux.conf
```

## How It Works

1. Read OAuth token from `~/.claude/.credentials.json`
2. Make a minimal API request (Haiku, 1 token) to Anthropic → parse rate limit from response headers
3. Cache result in `~/.claude/rate-limit-cache.json` (15-min TTL)
4. If cache is valid, output immediately without API call

Concurrent calls are serialized with `flock` — only one process makes the API call, others use cache.

## Files

| File | Purpose |
|---|---|
| `~/.claude/.credentials.json` | OAuth token (atomically written, `0600`) |
| `~/.claude/rate-limit-cache.json` | API response cache (atomically written, `0600`) |
| `~/.claude/rate-limit.lock` | Concurrent access lock (`0600`) |

## Token Expiry

If `[err]` appears, re-extract the token:

```bash
claude-rate-limit extract-token
```

## Security

**Why this is relatively safe:**

- **Application network traffic is narrow** — the application code sends its API request to `api.anthropic.com` and does not contain telemetry, analytics, or other application-level network destinations.
- **Token stays local** — `~/.claude/.credentials.json` is written atomically with owner-only permissions (`0600`).
- **Minimal API surface** — the API call sends a 1-token Haiku request solely to read response headers. The response body is discarded.
- **Small runtime surface** — the main logic runs in a single Rust binary. `extract-token` also invokes macOS's built-in `/usr/bin/security` and CommonCrypto.
- **Auditable implementation** — the core logic is concentrated in a single `src/main.rs` file, without build-time code generation or hidden background services.
- **No write access to Claude config** — the binary never modifies Claude Desktop or Claude Code configuration. `extract-token` only reads from Claude's config and writes to its own credential file.

## Stability of Token Extraction

`extract-token` depends on Claude Desktop's (Electron app) internal storage implementation.

**Unofficial implementation details relied upon:**

- Claude Desktop stores an encrypted token at `~/Library/Application Support/Claude/config.json` under the `oauth:tokenCache` key
- Electron's safeStorage uses `v10` prefix + AES-128-CBC (PBKDF2-SHA1, salt=`saltysalt`, iterations=1003) on macOS
- The encryption key is stored in macOS Keychain as `Claude Safe Storage` / `Claude Key`

**When this may break:**

- Claude Desktop update changes the token storage location or key name
- Electron changes the safeStorage encryption scheme (v10 → v11, etc.)
- Claude Code officially supports `.credentials.json`, making `extract-token` unnecessary

**Why breakage is safe:**

- Rate limit display (`tmux`/`--refresh`) only reads `~/.claude/.credentials.json`, independent of `extract-token`
- If `extract-token` fails, existing token continues to work until it expires
- No need to re-run `extract-token` if the token file already exists

## Platform

macOS only (Keychain, CommonCrypto)

## Disclaimer

This is an **unofficial, community-built tool** and is not affiliated with, endorsed by, or supported by Anthropic.

- This tool accesses undocumented internal storage of Claude Desktop (Electron safeStorage). This behavior may break at any time without notice.
- The OAuth token extracted by `extract-token` grants API access to your Anthropic account. Treat `~/.claude/.credentials.json` with the same care as SSH private keys or API secrets.
- Use this tool at your own risk. The author assumes no liability for account suspension, token leakage, rate limit miscalculation, unexpected API charges, or any other damages arising from the use of this tool.
- By using this tool, you accept full responsibility for its operation and any consequences.

## License

MIT
