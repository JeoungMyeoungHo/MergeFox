<p align="center">
  <img src="./assets/icon.png" alt="mergeFox" width="180">
</p>

<h1 align="center">mergeFox</h1>

<p align="center">
  <b>가볍고 빠른 네이티브 Git GUI — Rust + egui + gix + 시스템 <code>git</code></b>
</p>

<p align="center">
  <b>알파 / Alpha</b> · <code>v0.1.0-alpha.2</code> · 🇰🇷 한국어 / <a href="./README.md">English</a>
</p>

<p align="center">
  <img src="./assets/screenshot-commit-modal.png" alt="mergeFox 커밋 모달 + 그래프" width="900">
</p>

---

## 한 줄 요약

Electron 없이 동작하는 **Rust 네이티브 Git GUI**입니다. 읽기 경로는
`gitoxide`(`gix`), 쓰기 경로는 시스템 `git` 바이너리를 그대로 호출합니다.
로컬의 pre-commit 훅, 서명 키, credential helper, mergetool 설정이
**터미널과 똑같이 동작**합니다.

## 주요 특징

- **Pure Rust UI** — Electron / WebView 없음. egui + eframe (기본
  `glow`, 옵션으로 `wgpu`)
- **gitoxide 읽기 / git CLI 쓰기** — 그래프 워크, blob 로딩, 커밋
  메타 등은 전부 in-process gix. commit / amend / rebase / merge /
  cherry-pick / revert / reset / stash / checkout / fetch / push /
  pull / clone 은 모두 시스템 `git`
- **Undo / Redo 저널** — 모든 상태 변화는 저널에 기록되어 `Cmd/Ctrl+Z`로
  되돌릴 수 있습니다 (dirty working tree는 자동으로 stash 후 복구)
- **멀티탭 워크스페이스** — 여러 저장소를 탭으로 동시에 열 수 있습니다

## 기능

**UI / 탐색**

- **단일 통합 상단 툴바** — Fetch / Pull / Push / Commit (primary
  accent) / Rebase / Undo / Redo / PR / Issue / Reflog / Settings 가
  전부 한 줄에 모았습니다. "뭔가를 한다"는 동작은 모두 여기서, 가운데
  패널은 view 컨트롤만 담당 (Graph ↔ Project segmented 스위처, Scope
  segmented 필터, Columns / Log).
- **커밋 그래프** — 브랜치별 채도 높은 색상, 노드 둘레의 패널-색 halo
  (lane 선이 노드를 가로지르지 않도록), pill 모양의 HEAD / 브랜치 chip,
  짝수 행 zebra striping, 선택된 커밋 좌측의 3 px accent 바. 호버는 테마
  액센트의 채도를 낮춘 톤다운된 색이라 "호버 중"과 "선택됨"이 hue 자체가
  다르게 보입니다.
- **Project 트리 탭** — segmented 스위처를 Project로 돌리면 워킹 트리를
  파일 트리로 탐색 (필터, 숨김 표시, 클릭 시 워킹-트리 diff)
- **커맨드 팔레트** (`⌘/Ctrl+K`) — 명령어와 최근 커밋을 fuzzy 검색
- **멀티탭 워크스페이스** — 여러 저장소를 탭으로 동시에
- **멀티-커밋 바스켓** — `⌘/Ctrl+클릭`으로 커밋 여러 개를 큐에 담아
  cherry-pick / revert를 한 번에

**커밋 / 히스토리 재작성**

- 커밋 창은 Unstaged / Staged 두 패널, 파일별 체크박스, 개별 ↑/↓ 화살표,
  일괄 "Stage selected / Unstage selected"
- **헝크 단위 스테이징** — `s` / `u` / `d` 키로 개별 hunk를 stage /
  unstage / discard
- **인터랙티브 리베이스** — Pick / Reword / Squash / Drop, ↑↓ 재정렬,
  backup branch 옵션, 충돌 발생 시 conflict 해결로 자동 전환
- **컨플릭트 해결기** — 상황별 Ours / Theirs 라벨 (merge / cherry-pick /
  rebase에 따라 의미 반전), conflict marker 색상 하이라이팅, Prev /
  Next, Take Both
- **Reword / Split** — 메시지만 안전하게 재작성, 파일 / 헝크 기준으로
  커밋 분할
- **Find / fix** — 풀 리베이스 없이 여러 커밋에 작은 수정을 일괄 적용
- **Bisect** 위저드
- **스테시** — 사이드바 `+ Stash`로 생성, 더블클릭 = pop, 우클릭 = Pop /
  Apply / Drop
- **Author identicon** — GitHub 스타일 5×5 대칭 블록, 이메일 기반 로컬
  생성 (Gravatar 외부 호출 없음)

**Diff 뷰어**

- 파일 목록 + 패치 라인 모두 virtualize (kernel 크기 머지도 부드러움)
- **이미지 diff** (PNG / JPG / GIF / WEBP / BMP …) 지원
- **Diff 미니맵** — 긴 패치 안에서 위치 추적
- **Blame 뷰** — 파일 컨텍스트 메뉴에서 열기

**안전장치**

- **Undo / Redo 저널** — 모든 상태 변화는 저널에 기록되어 `⌘/Ctrl+Z`로
  되돌릴 수 있습니다 (dirty working tree는 자동으로 stash 후 복구)
- **패닉 복구** (`⌘/Ctrl+Shift+Esc`) — 과거 저널 스냅샷 중 아무거나
  골라서 `recovery-<sha>` 브랜치로 복원 (망한 rebase도 복구)
- **Reflog 복구** (`⌘/Ctrl+Shift+R`) — 최근 HEAD 이동을 diff 미리보기와
  함께 보고 안전한 브랜치로 복구
- **Pre-flight 모달** — destructive 작업(hard reset, force push, branch
  삭제, commit drop)에 구체 숫자("`main` 위 커밋 3개가 사라집니다")를
  먼저 보여줌

**원격 / Forge**

- **Push / Pull / Fetch** 분할 버튼 — Pull은 merge / rebase /
  fast-forward 전략 선택, Push는 일반 push / force push (확인 다이얼로그)
- **저장소별 기본 remote** + 선호 pull 전략
- **PR / 이슈 생성·브라우징** — GitHub / GitLab / Bitbucket / Gitea /
  Codeberg / Azure DevOps. PAT 또는 OAuth device flow
- **CI 상태 배지** — 커밋 행에 forge check-run 결과 표시
- **Git LFS** — 10 MB 이상 커밋된 바이너리 사이드바 경고 + 옵션으로
  **LFS lock** 컨트롤(lock / unlock / steal)
- **워크트리 관리** — Settings → Repository에서 list / add / lock /
  remove
- **Publish 모달** — 새 remote에 처음 push할 때

**AI (선택)**

- 커밋 메시지 생성 — OpenAI 호환 엔드포인트(OpenAI / Anthropic /
  Ollama / 자체 호스팅) 무엇이든
- Diff 요약, 저장소 컨벤션 학습, 변경 시그널 추출

**MCP 서버 (선택)**

- **Model Context Protocol stdio 서버** — LLM 에이전트가 stdio로 붙어
  읽기 전용 저장소 조사 + 옵트인 mutation 도구를 사용. 토큰 인증; 아래
  `MERGEFOX_MCP_TOKEN` 참고.

## 상태

**두 번째 알파**(`v0.1.0-alpha.2`)입니다. 일상 Git 작업에는 쓸 만한 수준이고,
주변 UI와 일부 네트워크 흐름은 계속 빠르게 다듬는 중입니다.
전체 변경사항은 [RELEASE_NOTES.md](./RELEASE_NOTES.md),
기능 갭은 [TODO/features.md](./TODO/features.md),
프로덕션 레디니스 로드맵은 [TODO/production.md](./TODO/production.md)에서 확인하세요.

알파 시점 한계:

- GPG 서명은 로컬 `user.signingkey` 설정을 그대로 따르지만 커밋별 UI
  토글은 아직 없음
- LFS lock 컨트롤은 있지만 오브젝트 단위 LFS 인스펙터는 없음
- 바이너리는 코드사인 / 공증되지 않음 (macOS Gatekeeper 경고 — 처음
  실행 시 우클릭 → 열기 한 번)
- 일부 Forge UI 흐름(Bitbucket / Azure DevOps PR review thread)은 아직
  읽기 전용

## 설치 / 실행

### 소스에서 빌드

```bash
git clone https://github.com/JeoungMyeoungHo/MergeFox
cd MergeFox
cargo run --release
```

릴리스 빌드는 `target/release/mergefox` (Windows는 `.exe`)에 생성됩니다.
앱은 실행 시 `PATH`의 시스템 `git`을 그대로 사용합니다.

### 요구 사항

- 최신 stable Rust toolchain
- 시스템 `git` 바이너리 (2.x 이상)
- Transitive native 의존성을 위한 C/C++ toolchain

플랫폼별 힌트:

- **macOS** — `xcode-select --install`
- **Linux** — `build-essential`, `pkg-config`, 데스크톱 라이브러리
  (`libxkbcommon`, `libwayland`, `libx11`, …)
- **Windows** — MSVC Build Tools

`gix`는 pure-Rust라 **`libgit2` 설치가 필요 없습니다**.

## 사용법

### 1. 저장소 열기

Welcome 또는 `+` 탭에서:

- **Open** — 로컬 저장소 경로
- **Clone** — URL + 저장 위치
- 최근 목록을 더블클릭해서 빠르게 재오픈

### 2. 그래프 탐색

가운데 패널이 커밋 그래프입니다. 그래프 위 view-controls 줄에는
`Graph | Project` segmented 스위처, `Scope: Current | All local |
All refs` 필터, Columns / Log 토글이 있습니다. 커밋을 클릭하면
오른쪽에 diff가 로드되며, 선택된 행은 액센트 채움 + 좌측 3 px
바로 표시되어 스크롤 중에도 위치가 보입니다. 우클릭 시 체크아웃
/ 여기서 브랜치 / 태그 / cherry-pick / revert / reset / drop /
복사 같은 메뉴가 나옵니다. `Cmd/Ctrl + 클릭`은 멀티-커밋 바스켓
토글 — 여러 커밋을 한 번에 cherry-pick / revert 할 때 편합니다.

### 3. 커밋

상단 툴바의 **📝 Commit** (액센트 채움 primary 버튼):

- **Unstaged** 패널에서 파일 체크 → `⬇ Stage selected`, 행별 `⬇`,
  또는 `⬇ Stage all`
- **Staged** 패널에서 `⬆ Unstage selected` / `⬆` / `⬆ Unstage all`
- 메시지를 입력 (또는 AI 엔드포인트가 설정되어 있다면 `✨ Generate`)
- `▸ Commit staged` / `Amend last` / `Stage all & commit` 중 선택

### 4. 리베이스

상단 툴바의 **⎇ Rebase…** 버튼으로 인터랙티브 리베이스 플래너를 엽니다.

- ↑ / ↓ 로 순서 변경
- Pick / Reword / Squash / Drop 중 선택
- `Backup current state with tag` 체크 후 **Rebase**
- Conflict 발생 시 컨플릭트 해결 창으로 자동 전환 → 각 파일 해결 →
  **Continue**

### 5. 스테시

사이드바 **Stashes** 섹션:

- `+ Stash` — 메시지 입력 후 생성 (working tree + index + untracked
  포함)
- 더블클릭 = pop
- 우클릭 = Pop / Apply / Drop

### 6. 워킹 트리 탐색 (Project 탭)

가운데 패널 위 segmented `Graph | Project`를 **Project**로 돌리면
워킹 트리가 파일 트리로 보입니다. 부분 문자열 필터(매치된 파일의
조상 노드를 자동으로 펼침), `Show hidden` 토글, 파일 클릭 시 HEAD
대비 워킹-트리 diff 가 지원됩니다.

### 7. 커맨드 팔레트 & Undo

`⌘/Ctrl+K` 로 명령어와 최근 커밋을 fuzzy 검색할 수 있습니다.
상태를 변경하는 모든 동작(commit, stage, checkout, rebase, …)은
저널에 남아 `⌘/Ctrl+Z` 로 되돌릴 수 있습니다 — dirty working tree
는 되돌리기 직전에 자동 stash 되었다가 복구 후 다시 풀립니다.

### 8. 언어 설정

Settings → General에서 언어를 선택합니다 (한국어 / 영어 / 일본어 /
중국어 / 프랑스어 / 스페인어 / …).
시스템에 한중일 폰트가 있으면 자동으로 fallback 되며, 없으면 egui
기본 폰트로 떨어집니다.

## 설정

우측 상단 **설정** 아이콘(⚙)에서:

- 언어
- 테마 (내장 팔레트 + 커스텀 accent / contrast / translucent)
- 저장소별 기본 remote, pull 전략 (merge / rebase / ff-only)
- Git provider 계정 (GitHub / GitLab / Bitbucket / Gitea / Codeberg /
  Azure DevOps) — PAT 또는 OAuth device flow
- SSH 키 생성 / 가져오기 / 공개키 복사
- AI 엔드포인트 (OpenAI 호환 URL)
- 활성 저장소의 워크트리 관리 (list / add / lock / remove)
- 워크스페이스 프로파일 — 저장소 프로파일에 따라 Settings UI는 관련 있는
  컨트롤만 노출 (General vs LFS-heavy / large-binary / regulated)
- MCP 서버 토큰 — stdio로 붙는 외부 LLM 에이전트가 사용

**자격 증명은 내 컴퓨터 밖으로 나가지 않습니다.** 2단계 저장소를 사용합니다:

1. **OS 키체인 우선** — macOS Keychain / Windows Credential Manager /
   Linux Secret Service가 사용 가능하면 거기에 저장.
2. **암호화되지 않은 파일 폴백** — 키체인 백엔드가 없을 때
   `~/Library/Application Support/mergefox/secrets.json`(macOS)
   또는 OS별 config 디렉토리. 파일은 사용자만 읽기 가능하게
   권한 `0600`이 걸리고, 파일 안에 경고 배너가 들어갑니다.
   홈 디렉토리에 접근할 수 있으면 토큰이 읽히므로
   일반적인 파일 권한 위생은 유지해 주세요.

`config.json`에는 **토큰 값이 절대 들어가지 않습니다** — 계정 핸들만
저장되고, 실제 값은 위 저장소에서 조회합니다.

## 성능

mergeFox는 대형 레포에서도 부드럽게 반응하도록 설계됐습니다:

- 커밋 그래프는 gix의 병렬 walker로 **백그라운드 스레드**에서 빌드,
  렌더는 virtualize (보이는 행만 그리기)
- 최근 32개 커밋 diff를 **LRU 캐시**에 유지 → 두 커밋 번갈아 보기는
  subprocess 0개
- 빠른 연속 클릭은 **coalesce** — worker가 돌고 있으면 중간 클릭은
  버리고 항상 최신 선택만 반영
- 매 프레임 git subprocess 호출 제거 — conflict 감지는 `.git/MERGE_HEAD`
  등 마커 파일만 확인 (기존엔 프레임당 `git` 3번 spawn)
- 테마 적용은 해시 기반 메모이즈 (매 프레임 egui style 리셋 방지)
- 백그라운드 워커가 작업 완료 시 `ctx.request_repaint()`로 메인 스레드를
  즉시 깨움 → 결과가 한 프레임 안에 반영

체감이 여전히 느리면 다음 환경변수로 프로파일링하세요:

```bash
MERGEFOX_PROFILE_FRAMES=1 MERGEFOX_PROFILE_DIFF=1 ./target/release/mergefox 2>profile.log
```

## 단축키

전역 (정식 목록은 `src/ui/shortcuts.rs`에 있고, 앱 안에서 `?`로 같은
치트시트를 열 수 있습니다):

| 단축키                       | 동작                                |
|------------------------------|-------------------------------------|
| `?` / `Shift + /`            | 단축키 치트시트 열기                |
| `Cmd/Ctrl + K`               | 커맨드 팔레트                       |
| `Ctrl + Tab`                 | 다음 워크스페이스 탭                |
| `Ctrl + Shift + Tab`         | 이전 워크스페이스 탭                |
| `Cmd/Ctrl + W`               | 현재 탭 닫기                        |
| `Cmd/Ctrl + Z`               | 마지막 mutation 되돌리기            |
| `Cmd/Ctrl + Shift + Z`       | Redo                                |
| `Cmd/Ctrl + Shift + R`       | Reflog 복구 열기                    |
| `Cmd/Ctrl + Shift + Esc`     | Panic recovery 열기                 |
| `Esc`                        | 최상위 모달 닫기                    |

Diff 패널 (헝크에 포커스가 있고, 텍스트 필드에 포커스가 없을 때):

| 단축키 | 동작                                |
|--------|-------------------------------------|
| `s`    | 현재 헝크 stage                     |
| `u`    | 현재 헝크 unstage                   |
| `d`    | 현재 헝크 discard                   |

## 환경변수

렌더링 / UI:

| 변수                            | 효과                                                  |
|---------------------------------|-------------------------------------------------------|
| `MERGEFOX_RENDERER=wgpu\|glow`  | 렌더러 선택 (기본 `glow`)                             |
| `MERGEFOX_FORCE_CONTINUOUS=1`   | 60 Hz 강제 렌더                                       |
| `MERGEFOX_NO_AVATARS=1`         | Author identicon 비활성화 (성능 A/B)                  |
| `MERGEFOX_STRAIGHT_LANES=1`     | 그래프 곡선 → 직선 (성능 A/B)                         |

로깅 / 프로파일링:

| 변수                                        | 효과                                                   |
|---------------------------------------------|--------------------------------------------------------|
| `MERGEFOX_LOG=mergefox::git::cli=debug`     | `tracing-subscriber` 필터 (`RUST_LOG`-스타일 spec)     |
| `MERGEFOX_LOG_FORMAT=json\|text`            | 로그 포맷 (기본: text)                                 |
| `MERGEFOX_LOG_STDERR=1`                     | 로그 파일 외에 stderr로도 함께 출력                    |
| `MERGEFOX_LOG_GIT=1`                        | 레거시 단축 — `mergefox::git::cli=debug`와 동일        |
| `MERGEFOX_LOG_AI_PROMPT=1`                  | AI 프롬프트 본문을 로그에 포함                         |
| `MERGEFOX_PROFILE_FRAMES=1`                 | 프레임별 시간 + 프레임 간 갭 로깅                      |
| `MERGEFOX_PROFILE_DIFF=1`                   | `diff_for_commit` 단계별 타이밍 로깅                   |

Git 작업 / 네트워크:

| 변수                              | 효과                                                  |
|-----------------------------------|-------------------------------------------------------|
| `MERGEFOX_GIT_TIMEOUT_SECS=300`   | fetch / push / pull 백그라운드 작업 타임아웃 (초)     |
| `MERGEFOX_HTTP_USER` / `HTTP_PASS`| 샌드박스 테스트 클론용 HTTPS 자격증명                 |

MCP (Model Context Protocol stdio 서버):

| 변수                                  | 효과                                                  |
|---------------------------------------|-------------------------------------------------------|
| `MERGEFOX_MCP_TOKEN=…`                | 클라이언트가 제시해야 하는 토큰 (없으면 비인증, 개발용)|
| `MERGEFOX_MCP_AUTO_APPROVE=1`         | mutation 도구를 자동 승인 (위험; 테스트 전용)         |
| `MERGEFOX_MCP_ALLOW_DESTRUCTIVE=1`    | 파괴적 변형 도구 허용 (force push, hard reset)        |

## 프로젝트 구조

```text
src/
├── actions.rs            CommitAction (undoable user intents)
├── app.rs                앱 전체 상태 + 탭/모달/백그라운드 poller
├── clone.rs              Async clone (gix 우선, git CLI fallback)
├── clone_auth.rs         clone 중 자격증명 프롬프트
├── config.rs             설정 / 테마 / AI 엔드포인트 영속화
├── forge.rs              Forge 상태 + provider dispatch (PR / 이슈 / CI)
├── git_url.rs            Git URL 파서 (https / ssh / scp / git 프로토콜)
├── gix_clone.rs          순수 Rust clone 경로
├── logging.rs            tracing 설정 + 일별 로테이션
├── preflight.rs          파괴적 작업 pre-flight 정보
├── secrets.rs            2단계 자격증명 저장소 (OS 키체인 → 파일 폴백)
├── workspace_profile.rs  저장소-프로파일 규칙 (Settings UI 노출 제어)
├── ai/                   커밋 메시지 생성 + AI task runner
├── git/
│   ├── basket_ops.rs     멀티-커밋 cherry-pick / revert / drop
│   ├── blame.rs          git blame 파서
│   ├── cli.rs            시스템 git 래퍼
│   ├── conflict_hunks.rs 충돌 마커 → 구조화된 hunk
│   ├── diff.rs           RepoDiff + unified-diff 파서
│   ├── find_fix_ops.rs   범위 전반에 작은 수정 일괄 적용
│   ├── graph.rs          CommitGraph + lane assignment
│   ├── hunk_staging.rs   hunk 단위 stage / unstage / discard
│   ├── jobs.rs           fetch / push / pull 배경 작업
│   ├── lfs.rs            LFS 후보 스캐너
│   ├── lfs_locks.rs      LFS lock list / acquire / release
│   ├── message_lint.rs   Conventional-Commits 린트
│   ├── project_templates.rs  .gitignore / 템플릿 헬퍼
│   ├── reflog_rewind.rs  reflog 탐색 + 복구 브랜치
│   ├── repo.rs           Repo 래퍼 (gix + CLI)
│   ├── reword_ops.rs     메시지만 안전하게 재작성
│   ├── split_ops.rs      파일 / hunk 기준 커밋 분할
│   └── ops.rs            status / stage / commit / amend / stash
├── journal/              append-only 저널 + undo / redo
├── mcp/                  Model Context Protocol stdio 서버
├── providers/            PAT / OAuth / SSH (GitHub, GitLab, Bitbucket,
│                         Gitea, Codeberg, Azure DevOps, generic)
└── ui/                   egui 뷰 — graph, sidebar, top_bar, main_panel,
                          commit_modal, rebase, conflicts, settings/, blame,
                          bisect, find_fix, palette, project_tree, …
```

## 라이선스

[Apache License 2.0](./LICENSE). 서드파티 표기는 [NOTICE](./NOTICE)를
참고하세요.
