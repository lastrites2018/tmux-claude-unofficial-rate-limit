# claude-rate-limit

Claude Max (Pro/Team) 구독의 rate limit 잔여량을 tmux 상태바에 표시하는 단일 바이너리 도구.

```
5h:77%(4h07m) 1w:40%
```

- **5h:77%** — 5시간 윈도우 잔여 77%
- **(4h07m)** — 리셋까지 남은 시간
- **1w:40%** — 주간 잔여 40%

## 요구사항

- macOS (CommonCrypto, Keychain 사용)
- Claude Code 또는 Claude Desktop 로그인 상태
- Rust 툴체인 (빌드 시)
- tmux (상태바 표시 시)

## 설치

```bash
git clone <repo-url>
cd claude-rate-limit
cargo build --release
cp target/release/rate-limit ~/.local/bin/claude-rate-limit
```

## 초기 설정

OAuth 토큰을 Claude Desktop의 암호화 저장소에서 추출합니다. macOS 키체인 접근 팝업이 뜨면 **허용**을 누르세요.

```bash
claude-rate-limit extract-token
```

이 명령은 `~/.claude/.credentials.json` (chmod 600)에 토큰을 저장합니다.

## 사용법

```bash
# tmux 상태바용 (색상 없음)
claude-rate-limit tmux

# 터미널 직접 확인 (ANSI 색상)
claude-rate-limit

# 캐시 무시하고 강제 갱신
claude-rate-limit --refresh

# 토큰 추출/갱신
claude-rate-limit extract-token
```

## tmux 설정

`~/.tmux.conf`에 추가:

```tmux
set -g status-right '#(~/.local/bin/claude-rate-limit tmux) | %Y-%m-%d %H:%M '
```

리로드:

```bash
tmux source-file ~/.tmux.conf
```

## 동작 원리

1. `~/.claude/.credentials.json`에서 OAuth 토큰 읽기
2. Anthropic API에 최소 요청 (Haiku 1토큰) → 응답 헤더에서 rate limit 파싱
3. 결과를 `~/.claude/rate-limit-cache.json`에 캐시 (15분 TTL)
4. 캐시 유효 시 API 호출 없이 즉시 출력

동시 호출 시 `flock`으로 1개 프로세스만 API 호출, 나머지는 캐시 사용.

## 파일 목록

| 파일 | 용도 |
|---|---|
| `~/.claude/.credentials.json` | OAuth 토큰 (chmod 600) |
| `~/.claude/rate-limit-cache.json` | API 응답 캐시 |
| `~/.claude/rate-limit.lock` | 동시 호출 방지 락 |

## 토큰 만료 시

`[err]`가 표시되면 토큰을 재추출하세요:

```bash
claude-rate-limit extract-token
```

## 보안

- OAuth 토큰은 `~/.claude/.credentials.json`에 chmod 600으로 저장 (SSH 키와 동일 수준)
- Anthropic 공식 API(`api.anthropic.com`)만 호출, 제3자 서버 없음
- 서드파티 런타임 의존성 없음 (단일 바이너리)

## 토큰 추출의 안정성에 대하여

`extract-token`은 Claude Desktop(Electron 앱)의 내부 저장 방식에 의존합니다.

**의존하는 비공식 구현 세부사항:**

- Claude Desktop이 `~/Library/Application Support/Claude/config.json`에 `oauth:tokenCache` 키로 암호화된 토큰을 저장한다는 것
- Electron의 safeStorage가 macOS에서 `v10` 접두사 + AES-128-CBC (PBKDF2-SHA1, salt=`saltysalt`, iterations=1003)를 사용한다는 것
- macOS Keychain에 `Claude Safe Storage` / `Claude Key`로 암호화 키가 저장된다는 것

**이것이 깨질 수 있는 경우:**

- Claude Desktop 업데이트로 토큰 저장 위치나 키 이름이 변경될 때
- Electron이 safeStorage 암호화 방식을 변경할 때 (v10 → v11 등)
- Claude Code가 `.credentials.json`을 공식 지원하여 `extract-token`이 불필요해질 때

**깨지더라도 안전한 이유:**

- rate limit 표시 자체(`tmux`/`--refresh`)는 `~/.claude/.credentials.json`만 읽으므로 `extract-token`과 독립적
- `extract-token`이 실패해도 기존 토큰이 만료되기 전까지는 정상 작동
- 토큰 파일이 이미 있으면 `extract-token`을 다시 실행할 필요 없음

## 플랫폼

macOS 전용 (Keychain, CommonCrypto 사용)

## 라이선스

MIT
