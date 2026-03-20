# tmux-claude-unofficial-rate-limit

Claude Max (Pro/Team) 구독의 rate limit 잔여량을 tmux 상태바에 표시하는 단일 바이너리 도구.

[English](README.md)

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
git clone https://github.com/lastrites2018/tmux-claude-unofficial-rate-limit.git
cd tmux-claude-unofficial-rate-limit
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

**안전한 이유:**

- **외부 전송 없음** — 바이너리는 `api.anthropic.com`하고만 통신합니다. 제3자 서버, 텔레메트리, 분석 없음. `src/main.rs` 단일 파일을 감사해서 확인할 수 있습니다.
- **토큰은 로컬에만 저장** — `~/.claude/.credentials.json`은 chmod 600(소유자만 읽기/쓰기)으로 저장. `~/.ssh/id_rsa`와 동일한 보안 모델.
- **최소 API 호출** — Haiku 1토큰 요청으로 응답 헤더만 읽음. 응답 본문은 사용하지 않음.
- **런타임 의존성 없음** — 단일 정적 링크 바이너리. Python, Node.js, 셸 스크립트 없이 변조 가능성 최소화.
- **완전 감사 가능** — 도구 전체가 Rust 소스 1개 파일(테스트 포함 ~700줄). 빌드 타임 코드 생성이나 동작을 숨기는 매크로 없음.
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

- rate limit 표시 자체(`tmux`/`--refresh`)는 `~/.claude/.credentials.json`만 읽으므로 `extract-token`과 독립적
- `extract-token`이 실패해도 기존 토큰이 만료되기 전까지는 정상 작동
- 토큰 파일이 이미 있으면 `extract-token`을 다시 실행할 필요 없음

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
