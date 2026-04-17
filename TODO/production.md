# mergeFox — 프로덕션 레디니스 TODO

작성 기준: 2026-04-17. `features.md`는 **기능 갭(features)** 중심, 이 문서는
**프로덕션 인프라 · UX · 제어성 · 자율성** 중심. 두 문서는 상호보완이며
이 파일의 항목은 `features.md`의 기능 작업과 병렬 진행 가능한 횡단 과제.

## 최근 진행 (2026-04-17 세션)

**Phase 1/2/3a**: A1 · A6 · B2 · B3 · F4 · E5 · E8
**Settings & MCP 추가(이전 세션)**: D1 · D2 · D4 · D5 · D6 · D7 · D8 · D9 · D14 · H1
**Sprint 2 (UX 기초)**: I1 · I2 · I4 · I6 · C1 · C2 · C3 · C4 · C8 · G4
**부분**:
- A2 — 문서+훅만, 인증서 미보유
- D3 — mergeFox 설정 범위엔 연결됐지만 git identity/remote 자체는 유지
- E3 — dispatcher만, multi-select UI 미배선
- E6 — rename 추가, 나머지는 이전부터 존재
- E12 — timeout env + lock 감지; 재시도/동시성 미추가
- G1 — ahead/behind 배지 완료, auto-fetch 타이머는 유예
- H2 — risk 분류/confirmation preview 추가됐으나 MCP tier/journal 정책 미연결
- H3 — source 모델/헬퍼만
- H4 — stdio transport 추가, 세션 토큰 미구현

**부수**:
- Settings 검색/변경 배지/JSON export-import, About/Diagnostics 섹션
- build commit 메타데이터 노출, Settings 리포 컨텍스트 바/모달 크기 영속화
- config schema migration(backup/rollback), `mergefox --mcp-stdio` read-only server
- `src/git/graph.rs` ref 라벨링 버그 수정 (`refs/stash` 등이 Delete 메뉴에 노출되던 문제)
- `CONTRIBUTING.md` / `SECURITY.md` / `ARCHITECTURE.md` / `CHANGELOG.md` 초판

## 범례

**우선순위**
- **P0** — 릴리스 차단 (알파→베타 승격 불가)
- **P1** — 베타 단계 필수 (정식 릴리스 차단)
- **P2** — 정식 릴리스 완성도
- **P3** — nice-to-have / 차별화

**상태**
- ⛔ 미착수 · 🟡 부분 · 🟢 완료 · ❓ 확인 필요

---

## A. 배포 / 인프라

### A1. CI/CD 파이프라인  🟢  **P0**
- ✅ `.github/workflows/ci.yml` — fmt(advisory) + clippy(advisory) + build + test + deny + audit(weekly)
- ✅ 3-OS 매트릭스 (ubuntu-latest, macos-latest, windows-latest)
- ✅ `.github/workflows/release.yml` — tag push 시 빌드 + 서명 훅 + draft release
- ⚠️ 알파 단계는 advisory 모드 (fmt/clippy 엄격 모드는 §6.2 dead-code 해소 후)
- ⚠️ PR clippy diff 코멘트는 미구현 (선택적)
- ⚠️ macOS arm64+x86_64 빌드는 release 워크플로우에만 있고 ci에는 macos-latest만

### A2. 코드 서명 / 공증  🟡  **P0**
- ✅ `RELEASE.md` — 서명/공증 절차 문서 + 시크릿 목록
- ✅ release 워크플로우에 조건부 서명/공증 훅 (secrets 존재 시에만 실행)
- ⛔ macOS: Developer ID 인증서 조달 필요 ($99/년)
- ⛔ Windows: OV/EV 인증서 조달 필요
- ⛔ Linux: AppImage GPG 서명은 워크플로우에 미포함 (베타 이후)

### A3. 자동 업데이트  ⛔  **P1**
- 최소: "새 버전 있음" 알림 + 다운로드 페이지 링크
- 본격: `self_update` 또는 Sparkle 스타일 delta 업데이트
- 채널 분리 (stable/beta/nightly)

### A4. 크래시 리포팅  ⛔  **P1**
- `panic = "abort"` 환경이라 `minidump-writer` 또는 sentry 필요
- opt-in, 개인정보(경로/브랜치명) 해시 처리
- `panic_recovery.rs`와 연동: UI 복구 + dump 저장 병행

### A5. 패키징 자동화  ⛔  **P1**
- `cargo-dist` 또는 `cargo-bundle` 표준화
- Homebrew cask, winget, Flathub (베타 이후)
- `Dockerfile.build`는 재현 빌드용으로 유지

### A6. `Cargo.toml` 메타데이터 정리  🟢  **P0**
- ✅ `repository`, `homepage`, `documentation` 실제 URL
- ✅ `keywords`, `categories`, `rust-version = "1.76"`
- ✅ `exclude = [...]` — TODO/, .github/, screenshots 등 배포 제외

---

## B. 품질 · 보안

### B1. 테스트 커버리지 확충  🟡  **P1**
- 현재 39 tests / 8 files / 23k LOC
- 우선 보강: `git_url.rs`, `secrets.rs`, `git/ops.rs`, `git/cli.rs`, `providers/*`
- fixture 레포 기반 통합 테스트 디렉토리 신설
- UI 스냅샷 테스트 (egui `ImageBuilder` 또는 insta)

### B2. `tracing` 기반 로깅  🟢  **P0**
- ✅ `src/logging.rs` — tracing-subscriber + env-filter + rolling daily file
- ✅ OS별 로그 경로 (macOS `~/Library/Logs/mergefox/`, Linux XDG_STATE, Windows LOCALAPPDATA)
- ✅ `MERGEFOX_LOG` env-filter, `MERGEFOX_LOG_FORMAT=json`, `MERGEFOX_LOG_STDERR=0`
- ✅ 10건 전부 `tracing::warn!`/`debug!`/`trace!`로 마이그레이션
- ⛔ release에서 `RUST_BACKTRACE=full` 강제는 미설정

### B3. 의존성 공급망  🟡  **P0**
- ✅ `deny.toml` — advisories / licenses / bans / sources
- ✅ CI에 `cargo-deny-action` + 주간 `cargo-audit`
- ⛔ `LICENSES-THIRD-PARTY.md` 자동 생성 (cargo-about) 미착수

### B4. 보안 강화  🟡  **P1**
- Windows에서 `secrets.json` ACL 미적용 추정 — 검증 + 필요 시 DPAPI
- `reqwest` timeout 전역 명시 (기본값 무한)
- Git URL 파싱 시 `file://`, `ext::` 프로토콜 필터
- `providers/*`의 token 취급 재감사 (평문 보관 경로 없는지)

### B5. 패닉 경로 감사  🟡  **P1**
- 33개 `unwrap/expect` 전수 조사, 특히 `secrets.rs` 4건
- `anyhow::Result` 또는 UI 폴백으로 치환
- 핫 경로(메인 루프)에서 `expect`와 `unwrap_or_default` 구분

### B6. `cargo clippy` warnings 153건  ⛔  **P2**
- `features.md` 6.2와 중복 — 본질은 dead-code 배선 이슈
- 전역 `#![warn(clippy::pedantic)]` 일부 도입

### B7. egui 0.29 → 0.30 업그레이드  ⛔  **P2**
- `id_source` → `id_salt` (`features.md` 6.1)
- 다른 breaking 변경 대응 체크리스트

---

## C. UI/UX 기초

### C1. 커맨드 팔레트 (⌘K)  🟢  **P1**
- ✅ `src/ui/palette.rs` — fuzzy subsequence matcher (유닛테스트 3건)
- ✅ ⌘/Ctrl+K 토글(텍스트 필드 포커스 중에도 동작)
- ✅ 노출 커맨드: Settings 섹션 점프, Reflog, Shortcuts, Activity log, Commit modal, Undo/Redo, Panic recovery, Graph scope 3종, 로컬 브랜치 checkout, Fetch
- ✅ ↑↓ 선택/Enter 실행/Esc 닫기, 선택된 행 자동 스크롤
- ⛔ 파괴적 액션(reset, drop, force push)은 의도적으로 미노출
- fuzzy match, 모든 `CommitAction` + 설정 점프 + 브랜치/커밋 검색
- 키보드 유저 흡수의 핵심. Fork/Sublime Merge에 모두 있음

### C2. 토스트 / 노티피케이션 센터 통합  🟢  **P1**
- ✅ `src/ui/notifications.rs` — 심각도 태깅(Info/Success/Warning/Error)
- ✅ Info/Success 자동 소멸, Warning/Error는 수동 닫기까지 유지
- ✅ Queue(최대 8개) — 오래된 항목 자동 evict
- ✅ `app.notify_ok/info/warn/err` / `notify_err_with_detail` 진입점
- ⛔ 기존 `Hud` / `last_error` / Settings `Feedback` 호출부 전면 마이그레이션은 점진적 (신규 code 먼저)
- 현재 `Hud`, 섹션별 `Feedback`, 에러 산발 → 단일 큐로 통합
- 자동 소멸 시간 일관화, 에러는 수동 닫기까지 유지
- "전체 히스토리 보기" 패널

### C3. 빈 상태(empty state) 카피 전면 감사  🟡  **P1**
- ✅ Sidebar: Local 브랜치 없음 / Remote-tracking 없음 / Stash 없음 카피
- ✅ Settings → Repository: 원격 / 워크트리 빈 상태에 다음 액션 포함
- ⛔ Welcome 0-accounts, Graph 0-commits(init 직후), AI 미설정 등은 추후 감사
- 브랜치 0개, stash 0개, 커밋 0개(init 직후), AI 미설정, provider 0개
- 각 진입점에 "왜 비어있나 + 다음 행동 버튼"

### C4. 키보드 내비게이션  🟢  **P1**
- ✅ `?` / `Shift+/` — 치트시트 모달 (`src/ui/shortcuts.rs`), 텍스트 필드 포커스 중엔 suppress
- ✅ ⌘K — 커맨드 팔레트(C1과 연결)
- ✅ 치트시트 문서화: 모든 전역 숏컷 노출
- 현재: `Ctrl+Tab`, `Cmd+W`, `Cmd+Z` 등 몇 개
- 필요: `/`(검색), `?`(치트시트), `Esc` 일관성, 모달 Enter=primary
- 포커스 트랩 검증, 커스텀 포커스 하이라이트

### C5. 반응형 breakpoint  ⛔  **P2**
- 3-패널 → 2-패널 → 1-패널 자동 전환 규칙
- 최소 너비 700 재검토

### C6. 밀도 토글 (Compact/Comfortable/Spacious)  ⛔  **P2**
- Settings → General에 3단계 + 폰트 크기 별도

### C7. 애니메이션 기본값  ⛔  **P3**
- 탭/패널/토스트/모달 12~16ms easing
- `prefers-reduced-motion` 상응 토글

### C8. 마이크로카피 감사  🟡  **P1**
- ✅ `ConfirmKind::body()` — preflight 라인과 중복 제거, Reflog 복구 힌트 통합
- ✅ 빈 상태 카피(C3)에 다음 행동 포함
- ⛔ 전체 UI 문자열의 영문/한글 혼재 전면 감사는 미착수
- 영문/한글 하드코딩 혼재 점검
- Git 원어 유지 + 툴팁으로 설명 (force push with lease, FF-only, detached HEAD)
- 파괴적 액션 확인 모달에 구체 숫자("N개 커밋 소실")

### C9. 툴팁 정책 표준화  ⛔  **P2**
- 아이콘 버튼 전부 툴팁 + 단축키 병기
- 300~500ms 지연

### C10. Onboarding  ⛔  **P2**
- "Try with sample repo" 버튼 welcome에
- 선택적 3단계 투어

### C11. 접근성(a11y)  ⛔  **P2**
- WCAG AA 대비비 검증
- 스크린리더 제한 상황에서 키보드 온리 사용 보장
- 고대비 테마 프리셋

### C12. 국제화 완성도  🟡  **P2**
- `fluent-rs` 도입 또는 `labels` 테이블 단일화
- 현재 `settings/mod.rs`의 `match (section, lang)` 카테시안 제거
- `sys-locale` 배선은 완료 상태

---

## D. 설정 메뉴 (Settings modal)

### D1. 섹션 검색 필드  🟢  **P2**
- ✅ 사이드바 상단 검색 필드
- ✅ 섹션 라벨 + 설정 키워드 기반 fuzzy 매칭
- ✅ 검색 결과가 없을 때 empty state 표시

### D2. "변경됨" 배지  🟢  **P2**
- ✅ 사이드바 섹션별 `Changed` 배지
- ✅ 기본값과 다른 섹션은 라벨 강조
- 범위: General / Repository / Integrations / AI (About 제외)

### D3. Reset 버튼 일관화  🟡  **P2**
- ✅ footer에 `Reset section` / `Reset all`
- ✅ 공통 확인 모달 동반
- ✅ General / Repository / Integrations / AI reset 연결
- ⛔ Git identity / remote 자체는 mergeFox 설정이 아니라 reset 대상에서 제외

### D4. Export / Import JSON  🟢  **P2**
- ✅ footer에 `Export JSON` / `Import JSON`
- ✅ `Config` JSON 내보내기/불러오기
- ✅ 토큰/크리덴셜은 원래 `config.json`에 저장되지 않으므로 export에서 자연스럽게 제외

### D5. "Open config folder" 버튼  🟢  **P2**
- ✅ Settings → About/Diagnostics에 `Open config folder` 버튼
- ✅ 설정 파일 경로를 함께 노출해서 열기 실패 시 수동 접근 가능

### D6. 모달 크기 영속화  🟢  **P3**
- ✅ Settings 창의 현재 width/height를 `Config`에 저장
- ✅ 다음 open 시 저장된 크기로 복원

### D7. About / Diagnostics 섹션  🟢  **P1**
- ✅ Settings → About/Diagnostics 섹션
- ✅ 버전 / 빌드 커밋 / Git 가용성 / 플랫폼 / 현재 저장소 / 설정 경로 / 로그 경로 표시
- ✅ `Copy diagnostics` 버튼으로 버그 리포트용 텍스트 원샷 복사
- ✅ `Open log folder` 보너스 진입점 추가

### D8. Settings deep link  🟢  **P2**
- ✅ `MergeFoxApp::open_settings_section(SettingsSection)` 추가
- ✅ Commit 모달에서 AI 미설정 시 `AI settings…` 버튼으로 바로 AI 섹션 진입
- ✅ AI 미설정 상태에서 Generate 클릭 시 actionable HUD로 AI 설정 점프 지원

### D9. 리포 컨텍스트 스위처  🟢  **P2**
- ✅ Settings 상단에 현재 리포 컨텍스트 바 + 열린 탭 전환 드롭다운
- ✅ 현재 활성 탭과 다른 리포를 보고 있을 때 "현재 탭으로 전환" 빠른 버튼
- ✅ Settings의 repo-bound 동작이 active tab이 아니라 선택된 repo context를 사용하도록 수정

### D10. 설정 종류 확장  ⛔  **P2**
각 설정 추가 (현재 없는 것만):
- `editor.commit_template` (`.gitmessage`)
- `editor.signoff_default` (DCO)
- `editor.gpg_sign` + signingkey 연동
- `diff.tool` / `merge.tool` 외부 도구
- `terminal.launch_command`
- `fetch.auto_interval_min` (0/5/15/30)
- `safety.confirm_destructive` 마스터 토글
- `safety.auto_backup_refs_ttl_days`
- `ui.keymap` 프리셋 + 커스텀
- `ui.font_family` / `ui.font_size` (UI/mono 각각)
- `log.level`

### D11. 팀 공유 설정 파일  ⛔  **P2**
- `.mergefox/repo.toml` 커밋 가능한 설정
- pull strategy / commit template / reviewer

### D12. 핫 리로드  ⛔  **P3**
- `notify` crate로 config 감시 + debounce

### D13. 환경변수 오버라이드 확장  ⛔  **P3**
- `MERGEFOX_CONFIG_DIR`, `MERGEFOX_NO_PROVIDERS`, `MERGEFOX_AI_DISABLE`

### D14. 스키마 마이그레이션 플레이북  🟢  **P1**
- ✅ `Config::load`에 schema migration 경로 실제 구현
- ✅ migration 전 원본 config backup 저장
- ✅ migration 저장 실패 시 원본 config로 롤백
- ✅ v2 schema로 Settings window state를 포함해 자동 승격

---

## E. Git 제어성

### E1. Interactive rebase UI 확장  ⛔  **P1**
- `features.md`의 Drop/Move에 더해 **squash / fixup / reword / edit**
- 세션 중 스텝 편집 가능

### E2. Bisect UI  ⛔  **P2**
- `good` / `bad` 마크, 자동 이진 탐색 진행
- Tower/JetBrains 수준

### E3. Cherry-pick 범위 / 다중 선택  🟡  **P1**
- ✅ `CommitAction::CherryPick(Vec<Oid>)` enum 리팩터
- ✅ Dispatcher 순차 실행 + 부분성공 리포트 ("Picked 2/5 — resolve conflicts…")
- ✅ Journal 엔트리는 실제 적용된 커밋만 기록
- ⛔ Multi-select UI(Shift-click / range 선택) 미배선 — 콜사이트는 현재 단일원소 vec

### E4. Submodule 관리  ⛔  **P2**
- `update --init`, `foreach`, status 뷰
- 사이드바 섹션

### E5. Worktree list/remove/lock  🟢  **P1**
- ✅ `Repo::list_worktrees` / `remove_worktree` / `lock_worktree` / `unlock_worktree`
- ✅ `WorktreeInfo` + `--porcelain` 파서 (유닛테스트 2건)
- ✅ Settings → Repository 섹션에 리스트 + 메인/잠금/정리대상 뱃지 + remove/force remove 버튼
- ⛔ `add`(features.md 2.3)는 별도 작업 — worktree 생성 UI는 아직

### E6. Remote CRUD  🟡  **P1**
- ✅ add / remove / set-url — 이전 세션에 존재
- ✅ `Repo::rename_remote` + Settings UI "Rename to" 인라인 행 (기본 원격 자동 마이그레이션)
- ⛔ 원격별 `prune` 토글, `fetch --tags` 정책 등 세부 옵션 미추가

### E7. Blame 뷰어  ❓  **P1**
- diff_view 통합 여부 확인 필요
- 커밋 호핑 + 파일별 timeline

### E8. Reflog HUD 접근성  🟢  **P2**
- ✅ ⌘/Ctrl+Shift+R 글로벌 숏컷 — 워크스페이스 뷰 안에서만 활성
- ✅ 탑바 ↺ 버튼 툴팁에 숏컷 표기
- ⛔ HUD(하단 스트립)에는 상시 표시 안 함 — 숏컷 + 탑바로 충분 판단

### E9. Maintenance 메뉴  ⛔  **P2**
- `fsck`, `gc`, `repack`
- Settings → Repository → Maintenance

### E10. Sparse checkout / partial clone  ⛔  **P2**
- 모노레포 대응

### E11. Notes  ⛔  **P3**
- code review 워크플로

### E12. Job 시스템 확장  🟡  **P1**
- ✅ 타임아웃 설정화 (`MERGEFOX_GIT_TIMEOUT_SECS`, 기본 300s)
- ✅ `.git/index.lock`·`HEAD.lock` 사전 감지 (신선/stale 구분 메시지, push·pull에 장착)
- ✅ Locale 강제 (`LC_ALL=C.UTF-8`) — `GitCommand::run_raw_controlled`에 이미 있었음 (이전 세션)
- ⛔ `GitJobKind::Custom { label, command }` 추가
- ⛔ 동시성 충돌 감지 (push+pull 동시)
- ⛔ 재시도 정책 (exponential backoff)
- ⛔ Activity 기반 타임아웃 연장

---

## F. 인터랙션 · 액션 모델

### F1. Action framework 리팩터  ⛔  **P1**
- 현재 flat `CommitAction` enum → command trait + registry
- `trait Command { preconditions, preview, execute, undo, affected_refs }`
- 커맨드 팔레트 · MCP · 단축키 단일 소스

### F2. Multi-select 지원  ⛔  **P1**
- 단일 Oid 기반 액션 → `Vec<Oid>`
- "3개 커밋 cherry-pick / 2개 drop" 일괄 실행

### F3. 드래그 & 드롭  ⛔  **P1**
- 브랜치 → 브랜치: merge/rebase 모달
- 커밋 → 브랜치: cherry-pick
- 파일 스테이지/언스테이지 drag
- stash → 브랜치: apply

### F4. Pre-flight info 주입  🟢  **P0**
- ✅ `src/preflight.rs` — `PreflightInfo`, `Severity` (Info/Warning/Critical)
- ✅ `hard_reset`: 드롭 커밋 수 + 더티 워킹트리 감지 + reflog 참고 라인
- ✅ `delete_branch`: `--not` 기반 unreachable 커밋 수 (remote/local 분기)
- ✅ `force_push`: 덮어쓰는 원격 커밋 수 + force-with-lease 권유 + 로컬 ahead 수
- ✅ `drop_commit`: 재생 대상 descendant 수 + 백업 ref 안내
- ✅ 모달에 Severity별 색상 글리프(⛔/⚠/ℹ) 렌더링
- ⛔ Rebase / Revert / Amend(pushed) pre-flight은 아직 미장착

### F5. Dry-run / Preview  ⛔  **P1**
- Push/Pull/Rebase/Reset 실행 전 결과 프리뷰
- 모달 안에 "이 버튼을 누르면 이런 일이 일어남" 명시

### F6. Inspector 패널  ⛔  **P2**
- 선택 객체의 모든 가능 액션을 버튼으로 노출
- 우클릭 전용 탐색 문제 해결

### F7. Prompt/Execute 분리  ⛔  **P2**
- `...Prompt` suffix 네이밍 → 구조적 분리
- Phase 1(모달) / Phase 2(실행) 명확화

---

## G. 편의성 (파워유저 retention)

### G1. 자동 fetch + ahead/behind 배지  🟡  **P1**
- ✅ `BranchInfo.ahead` / `behind` 필드 + `Repo::populate_tracking_counts`
- ✅ Sidebar에 `↑N / ↓N` 색상 뱃지(ahead=녹색, behind=주황) + diverged 툴팁
- ⛔ 주기 fetch 타이머(설정값 `fetch.auto_interval_min` 0/5/15/30) 미구현
- N분마다 백그라운드 fetch
- 브랜치에 "ahead 2 / behind 3" pill
- Git GUI의 "살아있는 느낌" 핵심

### G2. 원격 변경 감지 알림  ⛔  **P2**
- 같은 브랜치에 타인 푸시 시 푸시 전 경고

### G3. Branch graveyard  ⛔  **P2**
- 삭제 브랜치 30일 보관 (reflog 기반)
- 실수 복구 UI

### G4. Amend 안전장치  🟢  **P1**
- ✅ `preflight::amend_head` — `git branch --remotes --contains HEAD`로 이미 푸시된 커밋 감지
- ✅ AmendMessage 프롬프트에 인라인 Warning 라인 (force-with-lease 권장)
- ✅ 원격에 없을 때는 조용히 무시(정상 amend 경로 소음 없음)
- 푸시된 커밋 amend 시 force-with-lease 경고 모달

### G5. Commit message 히스토리  ⛔  **P2**
- 최근 N개 메시지 recall (↑ 키)

### G6. Conventional Commits 어시스트  ⛔  **P2**
- feat/fix/chore 드롭다운
- scope 자동 제안 (변경된 디렉토리 기반)

### G7. Auto-stash 범위 확장  ⛔  **P2**
- 현재 pull에만 → 브랜치 전환에도 (옵션)

### G8. Branch 이름 자동 완성  ⛔  **P3**
- 이슈 번호 + 슬러그
- Provider 연동 시 열린 이슈 목록에서 선택

### G9. .gitignore 템플릿  ⛔  **P3**
- 로컬 번들 템플릿

### G10. 외부 터미널 런칭  ⛔  **P2**
- 리포 경로에 터미널 열기 버튼
- 플랫폼별 기본 (iTerm/WT/gnome-terminal)

### G11. 작업 큐 시각화  ⛔  **P2**
- spinner 대신 "3 jobs running" pill + 상세 패널

### G12. Drag & Drop 파일 → Commit modal  ⛔  **P3**
- 파일 드롭으로 스테이지

---

## H. 자율성 / Agent 지원 (MCP)

### H1. Dry-run API  🟢  **P1**
- ✅ `src/mcp/action_preview.rs` — 현재 mergeFox action들을 JSON dry-run preview로 노출
- ✅ `mergefox_action_preview` MCP tool에서 실행 없이 label/effect/risk/preflight 반환
- ✅ destructive action은 기존 pre-flight 계산을 재사용

### H2. Action 위험도 분류  🟡  **P1**
- Safe / Recoverable / Destructive 3단계
- ✅ action preview에 `risk` + `confirmation_required` 필드 추가
- ⛔ MCP tier(auto-approve / modal policy)와 아직 직접 연결 안 됨
- ⛔ Journal 기록 범위 정책과도 아직 미연결

### H3. Journal의 source 태깅  🟡  **P1**
- ✅ `JournalEntry`에 `source: OpSource` 필드 존재 (`Ui` / `Mcp { agent }` / `External`)
- ✅ Activity log / MCP 뷰가 `source`를 그대로 노출
- ✅ `MergeFoxApp::journal_record_with_source` / `journal_record_mcp` 헬퍼 추가
- ⛔ MCP write 경로 실제 연결 전이라 현재 기록은 대부분 `Ui`
- ⛔ "Undo last 5 agent actions"는 아직

### H4. MCP v1 read-only 서버  🟡  **P1**
- ✅ `src/mcp/types.rs` / `activity_log.rs` — read-only JSON 스키마 + derived hints
- ✅ UI의 Activity Log inspector, Forge 패널 `Copy MCP JSON`로 in-process 소비 경로 존재
- ✅ `mergefox --mcp-stdio [--repo <path>]` stdio JSON-RPC transport 추가
- ✅ 외부 클라이언트가 `mergefox_activity_log` / `mergefox_action_preview` tool 호출 가능
- ⛔ 세션 토큰은 아직 없음

### H5. MCP v2 쓰기 티어  ⛔  **P2**
- Tier 2: auto-approve on clean tree
- Tier 3: 반드시 모달
- 승인 큐 UI는 pending_prompt 재사용

### H6. Auto-approve 정책 세분화  ⛔  **P2**
- 특정 브랜치 패턴만
- 특정 시간대만
- 특정 작업 타입만

### H7. Action 히스토리 export  ⛔  **P2**
- JSON replay 포맷 (audit / debug / demo)

### H8. Watch mode (subscribe)  ⛔  **P3**
- 리포 status 변경을 에이전트에 push

---

## I. 문서 / 커뮤니티

### I1. `CONTRIBUTING.md`  🟢  **P1**
- 개발 환경, 빌드 절차, 코드 스타일

### I2. `SECURITY.md`  🟢  **P1**
- 취약점 제보 창구 (Git 툴은 특히 중요)

### I3. `CODE_OF_CONDUCT.md`  ⛔  **P2**

### I4. `ARCHITECTURE.md`  🟢  **P1**
- 23k LOC면 필수
- 모듈 그래프, 데이터 흐름, 확장 포인트

### I5. 사용자 문서  ⛔  **P1**
- Getting started + troubleshooting
- 서명 안 된 빌드 열기 안내 (macOS Gatekeeper 등)

### I6. `CHANGELOG.md` (Keep a Changelog)  🟢  **P1**
- SemVer 정책 README 고지
- 1.0 전까지 breaking 규칙

---

## 추천 진행 순서

**Sprint 1 (P0 집중 · 릴리스 위생)** — 🟢 대부분 완료 (2026-04-17)
1. ✅ CI/CD 최소 파이프라인 (A1)
2. ✅ `tracing` 도입 + 로그 파일 (B2)
3. ✅ `Cargo.toml` 메타데이터 (A6)
4. ✅ `cargo-deny` / `audit` (B3)
5. ✅ Pre-flight info 주입 (F4)
6. ⛔ `reqwest` timeout + Windows ACL (B4) — 미착수

**Sprint 2 (UX 기초 · 알파→베타)**
7. 커맨드 팔레트 ⌘K (C1)
8. 토스트 통합 + 빈 상태 카피 (C2, C3)
9. About / Diagnostics 섹션 (D7)
10. 자동 fetch + ahead/behind (G1)
11. Amend/force-push 안전장치 (G4) — **F4 pre-flight으로 부분 커버됨**
12. CONTRIBUTING / SECURITY / ARCHITECTURE (I1/I2/I4)

**Sprint 3 (제어성 확장)** — 🟡 일부 완료
13. Action framework 리팩터 + multi-select (F1, F2)
14. Interactive rebase 확장 (E1)
15. ✅ Cherry-pick 다중 (E3 부분) — UI 미배선
16. ✅ Worktree list/remove (E5) · 🟡 Remote CRUD rename (E6)
17. 🟡 Job 시스템 견고화 (E12) — timeout+lock 완료, 재시도/동시성 미착수
18. ✅ E8 Reflog 숏컷 (⌘⇧R)

**Sprint 4 (자율성 · MCP)**
18. Dry-run API + 위험도 분류 (H1, H2)
19. Journal source 태깅 (H3)
20. MCP v1 read-only (H4)

**Sprint 5 (정식 릴리스)**
21. 코드 서명 + 공증 (A2)
22. 자동 업데이트 (A3)
23. 크래시 리포팅 (A4)
24. Settings 확장 (D1~D11)
25. DnD, 밀도 토글, 반응형 (F3, C5, C6)

**차후 (베타 이후)**
- Bisect, Submodule, Sparse checkout, Notes (E2, E4, E10, E11)
- MCP v2 쓰기 + 정책 (H5, H6)
- Branch graveyard, Conventional commits (G3, G6)
- i18n 완성 + a11y (C11, C12)
