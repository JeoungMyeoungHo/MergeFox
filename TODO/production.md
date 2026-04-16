# mergeFox — 프로덕션 레디니스 TODO

작성 기준: 2026-04-17. `features.md`는 **기능 갭(features)** 중심, 이 문서는
**프로덕션 인프라 · UX · 제어성 · 자율성** 중심. 두 문서는 상호보완이며
이 파일의 항목은 `features.md`의 기능 작업과 병렬 진행 가능한 횡단 과제.

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

### A1. CI/CD 파이프라인  ⛔  **P0**
- GitHub Actions: `cargo build --release` + `test` + `clippy -D warnings` + `fmt --check`
- 3-매트릭스(macOS-arm64/x86_64, Windows, Linux-x86_64/arm64)
- 태그 푸시 시 릴리스 아티팩트 자동 빌드 (dmg/msi/AppImage/deb)
- `cargo-deny` + `cargo-audit` 단계 포함
- PR에 clippy diff 코멘트

### A2. 코드 서명 / 공증  ⛔  **P0**
- macOS: Developer ID Application + `notarytool` 파이프라인
- Windows: OV 또는 EV 코드 서명 인증서 (EV는 SmartScreen 즉시 통과)
- Linux: AppImage에 GPG 서명 + `.sig` 아티팩트 공개

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

### A6. `Cargo.toml` 메타데이터 정리  🟡  **P0**
- `repository = "https://github.com/yourname/mergefox"` **플레이스홀더**
- `homepage`, `documentation`, `keywords`, `categories` 추가

---

## B. 품질 · 보안

### B1. 테스트 커버리지 확충  🟡  **P1**
- 현재 39 tests / 8 files / 23k LOC
- 우선 보강: `git_url.rs`, `secrets.rs`, `git/ops.rs`, `git/cli.rs`, `providers/*`
- fixture 레포 기반 통합 테스트 디렉토리 신설
- UI 스냅샷 테스트 (egui `ImageBuilder` 또는 insta)

### B2. `tracing` 기반 로깅  ⛔  **P0**
- `println!` / `eprintln!` 10건 제거
- `~/Library/Logs/mergefox/` (OS별 경로) + 파일 로테이션
- `MERGEFOX_LOG` 레벨 제어
- release에서 `RUST_BACKTRACE=full` 강제

### B3. 의존성 공급망  ⛔  **P0**
- `cargo-deny.toml` 작성: 라이선스 허용 목록 + advisory
- `LICENSES-THIRD-PARTY.md` 자동 생성 (cargo-about)

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

### C1. 커맨드 팔레트 (⌘K)  ⛔  **P1**
- fuzzy match, 모든 `CommitAction` + 설정 점프 + 브랜치/커밋 검색
- 키보드 유저 흡수의 핵심. Fork/Sublime Merge에 모두 있음

### C2. 토스트 / 노티피케이션 센터 통합  ⛔  **P1**
- 현재 `Hud`, 섹션별 `Feedback`, 에러 산발 → 단일 큐로 통합
- 자동 소멸 시간 일관화, 에러는 수동 닫기까지 유지
- "전체 히스토리 보기" 패널

### C3. 빈 상태(empty state) 카피 전면 감사  ⛔  **P1**
- 브랜치 0개, stash 0개, 커밋 0개(init 직후), AI 미설정, provider 0개
- 각 진입점에 "왜 비어있나 + 다음 행동 버튼"

### C4. 키보드 내비게이션  🟡  **P1**
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

### C8. 마이크로카피 감사  ⛔  **P1**
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

### D1. 섹션 검색 필드  ⛔  **P2**
- 설정 키/라벨 기반 fuzzy. System Prefs / VS Code 패턴

### D2. "변경됨" 배지  ⛔  **P2**
- 기본값과 다른 항목에 dot/글자 강조

### D3. Reset 버튼 일관화  ⛔  **P2**
- 섹션별 + 전역 Reset to defaults
- 확인 모달 동반

### D4. Export / Import JSON  ⛔  **P2**
- 팀/머신 간 동기화
- 토큰/크리덴셜 제외 옵션

### D5. "Open config folder" 버튼  ⛔  **P2**
- 파워유저 탈출구

### D6. 모달 크기 영속화  ⛔  **P3**
- 사용자 resize 결과 다음 오픈 시 복원

### D7. About / Diagnostics 섹션  ⛔  **P1**
- 버전, 빌드 커밋, 로그 경로, "Copy diagnostics" 버튼
- 버그 리포트 필수 정보 원샷 복사

### D8. Settings deep link  ⛔  **P2**
- `open_settings(SettingsSection::Ai)` 외부 API
- "AI를 설정하세요" 토스트 → 클릭 → AI 섹션으로 점프

### D9. 리포 컨텍스트 스위처  ⛔  **P2**
- 탭 바뀌면 에러 대신 상단에 현재 리포 + 전환 드롭다운

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

### D14. 스키마 마이그레이션 플레이북  🟡  **P1**
- `Config::load`의 migration 자리(주석만 있음) 실제 구현
- 백업 → 마이그 → 실패 롤백

---

## E. Git 제어성

### E1. Interactive rebase UI 확장  ⛔  **P1**
- `features.md`의 Drop/Move에 더해 **squash / fixup / reword / edit**
- 세션 중 스텝 편집 가능

### E2. Bisect UI  ⛔  **P2**
- `good` / `bad` 마크, 자동 이진 탐색 진행
- Tower/JetBrains 수준

### E3. Cherry-pick 범위 / 다중 선택  ⛔  **P1**
- `CommitAction::CherryPick(Oid)` → `CherryPick(Vec<Oid>)` or range

### E4. Submodule 관리  ⛔  **P2**
- `update --init`, `foreach`, status 뷰
- 사이드바 섹션

### E5. Worktree list/remove/lock  ⛔  **P1**
- `features.md` 2.3은 `add` 중심 — list/remove까지

### E6. Remote CRUD  ⛔  **P1**
- add/rename/remove/set-url 전부
- Settings → Repository에서

### E7. Blame 뷰어  ❓  **P1**
- diff_view 통합 여부 확인 필요
- 커밋 호핑 + 파일별 timeline

### E8. Reflog HUD 접근성  🟡  **P2**
- `reflog.rs` 있음 — reset 안전망이라 상시 접근 가능한 진입점

### E9. Maintenance 메뉴  ⛔  **P2**
- `fsck`, `gc`, `repack`
- Settings → Repository → Maintenance

### E10. Sparse checkout / partial clone  ⛔  **P2**
- 모노레포 대응

### E11. Notes  ⛔  **P3**
- code review 워크플로

### E12. Job 시스템 확장  🟡  **P1**
- `GitJobKind::Custom { label, command }` 추가
- 동시성 충돌 감지 (push+pull 동시)
- 재시도 정책 (exponential backoff)
- `.git/index.lock` 감지 + 복구 UI
- locale 강제 (`LC_ALL=C`)
- 타임아웃 300초 고정 → 설정 가능 + activity 기반 연장

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

### F4. Pre-flight info 주입  ⛔  **P0**
- Drop commit: "N개 descendant 리베이스, 충돌 확률, 파일 K개"
- Hard reset: "파일 N개 소실, 백업 ref X"
- Delete branch: unmerged 커밋 N개 경고
- Force push: 덮어쓰는 커밋 수/범위
- dispatcher에 `describe_effect() -> Effect` 추가

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

### G1. 자동 fetch + ahead/behind 배지  ⛔  **P1**
- N분마다 백그라운드 fetch
- 브랜치에 "ahead 2 / behind 3" pill
- Git GUI의 "살아있는 느낌" 핵심

### G2. 원격 변경 감지 알림  ⛔  **P2**
- 같은 브랜치에 타인 푸시 시 푸시 전 경고

### G3. Branch graveyard  ⛔  **P2**
- 삭제 브랜치 30일 보관 (reflog 기반)
- 실수 복구 UI

### G4. Amend 안전장치  ⛔  **P1**
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

### H1. Dry-run API  ⛔  **P1**
- 모든 action이 JSON으로 "뭐가 일어날지" 반환
- 에이전트가 승인 전 프리뷰 가능

### H2. Action 위험도 분류  ⛔  **P1**
- Safe / Recoverable / Destructive 3단계
- MCP 티어 + 확인 모달 정책과 연결
- Journal 기록 범위와 연동

### H3. Journal의 source 태깅  ⛔  **P1**
- `Operation { source: Agent(name) | User }`
- "Undo last 5 agent actions" 가능하게

### H4. MCP v1 read-only 서버  ⛔  **P1**
- `features.md` 5.1 참조
- stdio JSON-RPC + 세션 토큰
- Claude Code/Cursor에서 바로 연결

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

### I1. `CONTRIBUTING.md`  ⛔  **P1**
- 개발 환경, 빌드 절차, 코드 스타일

### I2. `SECURITY.md`  ⛔  **P1**
- 취약점 제보 창구 (Git 툴은 특히 중요)

### I3. `CODE_OF_CONDUCT.md`  ⛔  **P2**

### I4. `ARCHITECTURE.md`  ⛔  **P1**
- 23k LOC면 필수
- 모듈 그래프, 데이터 흐름, 확장 포인트

### I5. 사용자 문서  ⛔  **P1**
- Getting started + troubleshooting
- 서명 안 된 빌드 열기 안내 (macOS Gatekeeper 등)

### I6. `CHANGELOG.md` (Keep a Changelog)  ⛔  **P1**
- SemVer 정책 README 고지
- 1.0 전까지 breaking 규칙

---

## 추천 진행 순서

**Sprint 1 (P0 집중 · 릴리스 위생)**
1. CI/CD 최소 파이프라인 (A1)
2. `tracing` 도입 + 로그 파일 (B2)
3. `Cargo.toml` 메타데이터 (A6)
4. `cargo-deny` / `audit` (B3)
5. Pre-flight info 주입 (F4)
6. `reqwest` timeout + Windows ACL (B4)

**Sprint 2 (UX 기초 · 알파→베타)**
7. 커맨드 팔레트 ⌘K (C1)
8. 토스트 통합 + 빈 상태 카피 (C2, C3)
9. About / Diagnostics 섹션 (D7)
10. 자동 fetch + ahead/behind (G1)
11. Amend/force-push 안전장치 (G4)
12. CONTRIBUTING / SECURITY / ARCHITECTURE (I1/I2/I4)

**Sprint 3 (제어성 확장)**
13. Action framework 리팩터 + multi-select (F1, F2)
14. Interactive rebase 확장 (E1)
15. Cherry-pick 범위 (E3)
16. Remote CRUD / Worktree list (E5, E6)
17. Job 시스템 견고화 (E12)

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
