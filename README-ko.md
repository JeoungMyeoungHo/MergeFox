<p align="center">
  <img src="./assets/icon.png" alt="mergeFox" width="180">
</p>

<h1 align="center">mergeFox</h1>

<p align="center">
  <b>가볍고 빠른 네이티브 Git GUI — Rust + egui + gix + 시스템 <code>git</code></b>
</p>

<p align="center">
  <b>알파 / Alpha</b> · <code>v0.1.0-alpha.1</code> · 🇰🇷 한국어 / <a href="./README.md">English</a>
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

- 브랜치 / 태그 / SHA / author / date / 메시지를 나란히 보는 **커밋 그래프**
- **Author identicon** (GitHub 스타일 5×5 대칭 블록, 이메일에서 로컬
  생성 — Gravatar 외부 호출 없음)
- **인터랙티브 리베이스** — Pick / Reword / Squash / Drop, ↑↓ 재정렬,
  backup branch 옵션, 진행 중 conflict 자동 전환
- **컨플릭트 해결** — 상황별 Ours / Theirs 라벨 (merge / cherry-pick /
  rebase에 따라 의미를 반전 설명), 에디터에 conflict marker
  색상 하이라이팅, Prev / Next 네비게이션, Take Both 버튼
- **커밋 창** — Unstaged / Staged 두 패널, 파일별 체크박스, 개별 ↓/↑
  화살표, 일괄 "Stage selected / Unstage selected"
- **스테시** — 사이드바 `+ Stash`로 만들기, 더블클릭 = pop, 우클릭 =
  Pop / Apply / Drop
- **Diff 뷰어** — 파일 목록 + 패치 라인 모두 virtualize (kernel 크기의
  머지 커밋도 부드러움), 이미지 diff 지원
- **대용량 파일 경고** — 10 MB 이상 커밋된 바이너리를 사이드바에
  표시해 LFS로의 이관을 유도
- **패닉 복구** — 과거 저널 스냅샷 중 아무거나 골라서 `recovery-<sha>`
  브랜치로 복원 (망한 rebase도 복구 가능)
- **Reflog 복구 창** — 최근 HEAD 이동을 보고 안전하게 분기
- **AI 커밋 메시지** (선택) — OpenAI 호환 엔드포인트 아무거나 연결
  (OpenAI / Anthropic / Ollama / 자체 호스팅)
- **Forge 연동** — GitHub / GitLab / Bitbucket / Gitea / Codeberg.
  PR / 이슈 생성, 사이드바에서 목록 브라우징

## 상태

**첫 알파**(`v0.1.0-alpha.1`)입니다. 일상 Git 작업에는 쓸 만한 수준이고,
주변 UI와 일부 네트워크 흐름은 계속 빠르게 다듬는 중입니다.
전체 변경사항은 [RELEASE_NOTES.md](./RELEASE_NOTES.md),
기능 갭은 [TODO/features.md](./TODO/features.md),
프로덕션 레디니스 로드맵은 [TODO/production.md](./TODO/production.md)에서 확인하세요.

알파 시점 한계:

- Blame 뷰 없음
- 라인 / 헝크 단위 stage / unstage 아직 없음 (파일 단위만 지원)
- GPG 서명은 로컬 `user.signingkey` 설정을 그대로 따르지만 UI 스위치
  없음
- 전용 Git LFS 인스펙터 없음 (경고 배지만 표시)
- worktree 생성은 프롬프트만 연결되어 있고 실제 구현은 스텁

## 설치 / 실행

### 소스에서 빌드

```bash
git clone https://github.com/your-org/mergefox
cd mergefox
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

가운데 패널이 커밋 그래프입니다. 커밋을 클릭하면 오른쪽에 diff가
로드됩니다. 컬럼 폭은 경계 드래그로 조절 가능하고, 커밋 우클릭 시
체크아웃 / 여기서 브랜치 / 태그 / cherry-pick / revert / reset /
drop / 복사 같은 메뉴가 나옵니다.

### 3. 커밋

상단 **Commit…** 버튼:

- **Unstaged** 패널에서 파일 체크 → `⬇ Stage selected`, 행별 `⬇`,
  또는 `⬇ Stage all`
- **Staged** 패널에서 `⬆ Unstage selected` / `⬆` / `⬆ Unstage all`
- 메시지를 입력 (또는 AI 엔드포인트가 설정되어 있다면 `✨ Generate`)
- `▸ Commit staged` / `Amend last` / `Stage all & commit` 중 선택

### 4. 리베이스

상단 **Rebase…**로 인터랙티브 리베이스 플래너를 엽니다.

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

### 6. 언어 설정

Settings → General에서 언어를 선택합니다 (한국어 / 영어 / 일본어 /
중국어 / 프랑스어 / 스페인어 / …).
시스템에 한중일 폰트가 있으면 자동으로 fallback 되며, 없으면 egui
기본 폰트로 떨어집니다.

## 설정

우측 상단 **설정** 아이콘(⚙)에서:

- 언어
- 테마 (내장 팔레트 + 커스텀 accent / contrast / translucent)
- 저장소별 기본 remote, pull 전략 (merge / rebase / ff-only)
- Git provider 계정 (GitHub / GitLab / Bitbucket / Gitea / Codeberg) —
  PAT 또는 OAuth
- SSH 키 생성 / 가져오기 / 공개키 복사
- AI 엔드포인트 (OpenAI 호환 URL)

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

| 단축키 | 동작 |
|---|---|
| `Cmd/Ctrl + Z` | Undo |
| `Cmd/Ctrl + Shift + Z` | Redo |
| `Cmd/Ctrl + Shift + Esc` | Panic recovery 열기 |
| `Ctrl + Tab` | 다음 탭 |
| `Ctrl + Shift + Tab` | 이전 탭 |
| `Cmd/Ctrl + W` | 현재 탭 닫기 |

## 환경변수

| 변수 | 효과 |
|---|---|
| `MERGEFOX_RENDERER=wgpu\|glow` | 렌더러 선택 (기본 `glow`) |
| `MERGEFOX_PROFILE_FRAMES=1` | 프레임별 시간 + 프레임 간 갭 로깅 |
| `MERGEFOX_PROFILE_DIFF=1` | `diff_for_commit` 단계별 타이밍 로깅 |
| `MERGEFOX_NO_AVATARS=1` | Author identicon 비활성화 |
| `MERGEFOX_STRAIGHT_LANES=1` | 그래프 곡선 → 직선 (성능 A/B) |
| `MERGEFOX_FORCE_CONTINUOUS=1` | 60 Hz 강제 렌더 |

## 프로젝트 구조

```text
src/
├── actions.rs        CommitAction (undoable user intents)
├── app.rs            앱 전체 상태 + 탭/모달/백그라운드 poller
├── clone.rs          Async clone (gix 우선, git CLI fallback)
├── config.rs         설정 / 테마 / AI 엔드포인트 영속화
├── forge/            GitHub / GitLab / … REST + PR / issue 모델
├── git/
│   ├── cli.rs        시스템 `git` 래퍼
│   ├── diff.rs       RepoDiff + unified-diff 파서
│   ├── graph.rs      CommitGraph + lane assignment
│   ├── jobs.rs       fetch / push / pull 배경 작업
│   ├── lfs.rs        LFS 후보 스캐너
│   ├── ops.rs        status / stage / commit / amend / stash
│   └── repo.rs       Repo 래퍼 (gix + CLI)
├── journal/          append-only 저널 + undo/redo
├── providers/        PAT / OAuth / SSH 키 관리
├── secrets.rs        2단계 자격증명 저장소 (OS 키체인 → 파일 폴백)
├── ai/               커밋 메시지 생성 + AI task runner
└── ui/               egui 뷰 (graph, sidebar, commit_modal, rebase,
                       conflicts, settings, prompt, hud, …)
```

## 라이선스

[Apache License 2.0](./LICENSE). 서드파티 표기는 [NOTICE](./NOTICE)를
참고하세요.
