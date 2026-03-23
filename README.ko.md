# tmux-claude-unofficial-rate-limit

Claude 구독의 rate limit 잔여량을 tmux, 터미널, 또는 JSON 출력으로 확인하는 단일 바이너리 도구. Max에서 확인됨, Pro/Team은 미확인.

[English](README.md)

```
5h:24%(1h18m) 1w:40%
```

- **5h:24%** — 5시간 윈도우 잔여 24%
- **(1h18m)** — 리셋까지 남은 시간
- **1w:40%** — 주간 잔여 40%

## 요구사항

- macOS (CommonCrypto, Keychain 사용)
- Claude Code 또는 Claude Desktop 로그인 상태
- Rust 툴체인 (빌드 시)
- tmux (상태바 표시 시)

## FAQ

**Q: Pro 플랜에서도 되나요?**

**A:** rate limit 헤더를 반환하는 플랜이면 작동합니다. Max에서 확인됨, Pro/Team은 미확인.

## 설치

```bash
git clone https://github.com/lastrites2018/tmux-claude-unofficial-rate-limit.git
cd tmux-claude-unofficial-rate-limit
cargo build --release
cp target/release/rate-limit ~/.local/bin/claude-rate-limit
```

`~/.local/bin`이 `PATH`에 없으면 해당 경로를 추가하거나, `~/.local/bin/claude-rate-limit`처럼 전체 경로로 실행하세요.

## 토큰 설정

`~/.claude/.credentials.json`이 이미 있으면 이 단계는 건너뛰어도 됩니다. 없을 때만 Claude Desktop의 암호화 저장소에서 OAuth 토큰을 추출하세요. macOS 키체인 접근 팝업이 뜨면 **허용**을 누르세요.

```bash
claude-rate-limit extract-token
```

이 명령은 `~/.claude/.credentials.json`에 토큰을 원자적으로 저장하며, 파일 권한은 `0600`(소유자만 읽기/쓰기)입니다.

## 빠른 시작

대부분은 아래 명령만 알면 바로 사용할 수 있습니다:

```bash
# 터미널에서 현재 rate limit 확인
claude-rate-limit

# ~/.claude/.credentials.json 이 아직 없을 때만 실행
claude-rate-limit extract-token

# tmux 상태바용 출력
claude-rate-limit tmux
```

## 사용법

```bash
# tmux 상태바용 (색상 없음)
claude-rate-limit tmux

# 터미널 직접 확인 (ANSI 색상)
claude-rate-limit

# 스크립트용 구조화 출력
claude-rate-limit --json

# 캐시 무시하고 강제 갱신
claude-rate-limit --refresh

# 캐시 TTL 지정 (1..60분, 기본값: 15)
claude-rate-limit --ttl-minutes 5

# HTTP 타임아웃 지정 (1..30초, 기본값: 10)
claude-rate-limit --http-timeout-seconds 3

# reset 힌트 숨기기 (기본값은 표시)
claude-rate-limit --hide-reset-dates

# 토큰 추출/갱신
claude-rate-limit extract-token
```

`--json`은 `tmux`와 함께 사용할 수 없습니다. `--refresh`, `--hide-reset-dates`, `--ttl-minutes`, `--http-timeout-seconds`는 표시 모드에서만 적용되며 `extract-token`에는 사용할 수 없습니다.

reset 힌트는 기본적으로 켜져 있지만, 실제 표시는 잔여가 30% 이하일 때만 나타납니다. 5시간 reset은 같은 로컬 날짜 안에서는 상대 시간으로 보이고, 날짜를 넘기면 `M/D H[:MM]` 형식으로 바뀝니다. 1주 reset은 `M/D H[:MM]` 형식으로 표시됩니다. 이 힌트를 숨기려면 `--hide-reset-dates`를 사용하세요. `tmux` 출력에서는 줄 길이를 줄이기 위해 `[Xm ago]`가 보일 때 1주 reset 날짜는 숨깁니다.

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
3. 결과를 `~/.claude/rate-limit-cache.json`에 캐시 (기본 15분 TTL, `--ttl-minutes 1..60`으로 조정 가능)
4. 캐시 유효 시 API 호출 없이 즉시 출력

동시 호출 시 `flock`으로 1개 프로세스만 API 호출하고, 나머지는 새 캐시가 생기길 잠시 기다린 뒤 사용할 수 있으면 캐시를 사용합니다.

## 파일 목록

| 파일 | 용도 |
|---|---|
| `~/.claude/.credentials.json` | OAuth 토큰 (원자적 기록, `0600`) |
| `~/.claude/rate-limit-cache.json` | API 응답 캐시 (원자적 기록, `0600`) |
| `~/.claude/rate-limit.lock` | 동시 호출 방지 락 (`0600`) |

## 문제 해결

표시 모드에서 `[err]`가 보이거나 `--json`이 에러 객체를 반환하면 원인을 먼저 확인하세요:

- 에러가 토큰 만료 또는 `401`을 가리키면 `claude-rate-limit extract-token`을 다시 실행하세요.
- `~/.claude/.credentials.json`이 이미 있고 정상이라면 `extract-token`을 다시 실행할 필요는 없습니다.
- `HOME` 미설정, 토큰 파일 없음, 네트워크 실패, rate-limit 헤더 누락 같은 경우는 환경을 고치거나 나중에 다시 시도해야 합니다.

## 보안

**상대적으로 안전한 이유:**

- **애플리케이션 레벨 네트워크 범위가 좁음** — 애플리케이션 코드가 보내는 API 요청 대상은 `api.anthropic.com`이며, 별도의 텔레메트리, 분석, 제3자 서버 호출 코드는 포함하지 않습니다.
- **토큰은 로컬에 저장됨** — `~/.claude/.credentials.json`은 원자적으로 기록되며 파일 권한은 `0600`(소유자만 읽기/쓰기)입니다. rate limit 조회 시 토큰은 `api.anthropic.com`으로만 전송됩니다.
- **최소 API 호출** — Haiku 1토큰 요청으로 응답 헤더만 읽음. 응답 본문은 사용하지 않음.
- **런타임 표면이 작음** — 주요 로직은 단일 Rust 바이너리에서 실행되며, `extract-token`은 macOS 기본 제공 `/usr/bin/security`와 CommonCrypto를 사용합니다.
- **감사 범위가 작음** — 핵심 런타임 로직이 `src/main.rs` 한 파일에 집중되어 있고, 별도 커스텀 빌드 스크립트나 숨겨진 백그라운드 서비스는 없습니다.
- **Claude 설정 변경 없음** — 바이너리는 Claude Desktop/Code 설정을 절대 수정하지 않음. `extract-token`은 Claude 설정을 읽기만 하고 자체 인증 파일에만 씀.

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

- 표시 명령은 `~/.claude/.credentials.json`만 읽으므로 `extract-token`과 독립적
- `extract-token`이 실패해도 기존 토큰이 만료되기 전까지는 정상 작동
- 토큰 파일이 이미 있고 토큰이 아직 유효하면 `extract-token`을 다시 실행할 필요 없음

## 플랫폼

macOS 전용 (Keychain, CommonCrypto 사용)

## 면책 조항

이 도구는 **비공식 커뮤니티 제작 도구**이며, Anthropic과 제휴, 보증, 지원 관계가 없습니다.

- 이 도구는 Claude Desktop(Electron safeStorage)의 문서화되지 않은 내부 저장 방식에 접근합니다. 이 동작은 예고 없이 중단될 수 있습니다.
- `extract-token`으로 추출한 OAuth 토큰은 Anthropic 계정에 대한 API 접근 권한을 부여합니다. `~/.claude/.credentials.json`을 SSH 개인키나 API 시크릿과 동일한 수준으로 관리하세요.
- 이 도구의 사용으로 인한 계정 정지, 토큰 유출, rate limit 오계산, 예상치 못한 API 과금 등 모든 손해에 대해 제작자는 책임을 지지 않습니다.
- 이 도구를 사용함으로써 그 운영과 결과에 대한 모든 책임을 수락하는 것으로 간주합니다.

## 라이선스

MIT
