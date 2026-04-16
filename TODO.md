# mergeFox — 남은 작업 & 의도

작성 기준: 2026-04-15. `cargo build` 통과 (0 errors, 153 warnings).
경고의 대부분은 여기 적힌 "작성됐지만 미배선" 모듈에서 나오는 dead-code임.

## 범례

- **상태**: ⛔ 미착수 · 🟡 코드만 있고 UI 미배선 · 🟢 부분 동작
- **의도**: 이 기능이 왜 필요한지 — 타협하면 안 되는 핵심
- **범위**: 어디까지를 "done"으로 볼지

---

## 1. 기본 워크플로우 모달 (Z5 / Z6 / Z7)

### 1.1 Push 모달  ⛔

**의도**
`app.start_push(remote, branch, force)` 자체는 구현돼 있지만 지금은 컨텍스트
메뉴의 "Push"를 누르면 옵션 없이 즉시 실행됨. push는 remote가 2개 이상이거나
upstream이 없거나 force가 필요한 경우가 일상적이라, 무모달 실행은 실수 유발.

**범위**
- remote 드롭다운 (레포의 모든 remote 열거, 기본값은 현재 브랜치 upstream 또는 `origin`)
- branch 표시 (현재 HEAD 또는 선택된 브랜치 tip)
- `☐ Set upstream` 체크박스 (upstream 없을 때 기본 체크)
- `☐ Force with lease` 체크박스 — 일반 force는 제공하지 않음
  ("lease"는 원격 tip이 내가 본 값과 같을 때만 force 허용 → 덮어쓰기 사고 방지)
- 예상 push 범위 프리뷰 (ahead 커밋 N개 요약)
- `Push` 버튼 → 기존 `start_push` 호출, 백그라운드 잡
- 진행률은 기존 top_bar의 spinner + 라벨에 이미 연결돼 있음

**주의**
force-with-lease는 git의 push refspec에서 refspec 앞에 `+` 붙이는 것만으로는 안 됨.
원격 tip을 먼저 ls-remote로 받아와 비교 후 조건부 refspec 조립 필요.

---

### 1.2 Pull 모달  ⛔

**의도**
현재 `start_pull`은 `PullStrategy::Merge` 하드코딩. 사용자가 rebase 선호형이면
매번 터미널로 가야 함. strategy는 레포 성향에 따라 고정되는 경우가 많으니
"레포별 기본값" 기억 기능까지 포함해야 실용적.

**범위**
- strategy 라디오: `Merge` · `Rebase` · `Fast-forward only`
- ff-only는 divergence 감지 시 실패하는 게 옳은 동작 — 에러 메시지에 "rebase 또는 merge 필요" 문구
- `☑ Auto-stash dirty changes` (기본 on, policy B 유지)
- 레포별 마지막 선택을 `.git/mergefox/pull_prefs.json`에 저장 후 다음 호출 시 복원
- 진행 중에는 top_bar 스피너 사용 (기존 인프라)

**주의**
`start_pull`의 `PullStrategy::Rebase` 경로는 `git/jobs.rs`에서 아직 실제 rebase를
수행하지 않고 merge로 떨어질 수 있음 — UI 모달 붙이기 전에 jobs.rs 검증 필수.

---

### 1.3 Stash 모달 & 사이드바 리스트  ⛔

**의도**
`git/ops.rs`에 `stash_push` / `stash_pop` / `stash_list`까지 전부 있는데 UI 진입점이
전혀 없음. "실험적 변경 임시 보관"은 기본 워크플로우라 commit 모달 옆에 상시
버튼 또는 사이드바 섹션이 필요.

**범위**
- 사이드바에 `Stashes` 섹션 (접이식) — stash_list 결과를 간단히 메시지 + 시간으로 표시
- stash 항목 우클릭 메뉴: `Pop` · `Apply` · `Drop` · `Show diff`
- 툴바 `💾 Stash…` 버튼 → 메시지 입력 모달 → `stash_push`
- pop/apply 후 conflict 발생 시 상태를 명시적으로 사용자에게 알림
  (지금 구조에선 조용히 실패할 수 있음)
- journal에 `Operation::StashPush` / `StashPop` 기록 (undo 대상 포함)

---

## 2. Rebase / Worktree 마일스톤

### 2.1 Drop commit  ⛔

**의도**
`CommitAction::DropCommitPrompt(oid)`는 enum에 정의만 돼 있고 dispatcher에서
"wire 안 됨" 토스트만 띄움 (`main_panel.rs:456`). "실수로 올라간 커밋 하나만
지우기"는 빈번한 요구라 interactive rebase 없이도 되게 만들어야 함.

**범위**
- 확인 모달 (영향받는 descendant 커밋 수 표시)
- 백업 ref 자동 생성 (`<branch>.backup-<ts>`, 기존 undo 인프라와 동일 규약)
- `git` CLI 직접 호출: drop할 커밋의 parent 위에 이후 커밋들을 cherry-pick으로 재생
- 충돌 시 mid-rebase state 진입 — 충돌 해결 UI(있음)로 이어짐
- journal에 `Operation::Rebase { dropped: vec![oid] }` 기록

**주의**
gix / libgit2 계열 rebase API 대신 수동으로
branch tip을 움직이는 게 제어 편함. 기존 `Repo::cherry_pick_commit`을 반복
호출하는 방식 추천.

---

### 2.2 Move up / Move down  ⛔

**의도**
Drop과 동일한 rebase 엔진 필요. 순서 재배치는 drop의 일반화라 drop 구현 후
추가 공수 적음.

**범위**
- 선택한 커밋과 인접 커밋 순서 교환
- journal `Operation::Rebase { reordered: [(old_idx, new_idx)] }`
- 충돌 시 처리 동일

---

### 2.3 Create worktree from here  ⛔

**의도**
"이 커밋에서 격리된 작업 공간 만들기" — 현재 브랜치 건드리지 않고 실험할 때
필수. git worktree는 UI가 거의 없어서 CLI 의존하는 사람이 대부분인데, 이
도구의 차별화 포인트가 될 수 있음.

**범위**
- 경로 선택 (rfd 다이얼로그, 기본값은 `<repo>/../<repo>-<shortsha>`)
- 브랜치 선택: `새 브랜치` (이름 입력) 또는 `detached HEAD`
- `git worktree add` 호출
- 생성된 worktree를 탭으로 자동 열기 (기존 `WorkspaceTabs`에 append)
- 메인 레포의 worktree 목록을 사이드바에 표시 (선택적 확장)

**주의**
worktree는 `.git` 파일이 심볼릭 참조라 일부 경로 계산 로직이 깨질 수 있음.
`ws.repo.path()`가 `.git` 디렉토리를 주는지 workdir을 주는지 케이스별 검증.

---

## 3. Provider (원격 계정) 시스템

### 3.1 전체 Provider 스택  🟡

**의도**
"원격 레포 연결이 편리" — 이 프로젝트의 핵심 요구. 현재 `src/providers/*`에
GitHub / GitLab / Bitbucket / Azure DevOps / Codeberg / Gitea / Generic 7종의
REST 클라이언트와 OAuth device flow, PAT, SSH keygen 모듈이 전부 작성돼 있음.
그러나 UI 진입점이 0개 — 웰컴 화면 Providers 섹션이 비어 있음.

**범위**
- `Settings` 또는 웰컴 화면의 `Providers` 섹션:
  - `+ Add provider` 버튼 → 모달
  - 등록된 provider 카드 (호스트/라벨/사용자명/마지막 fetch 시각)
  - `Test` · `Remove` · `Change auth`
- Add Provider 모달 탭 3개:
  1. **PAT** — 토큰 붙여넣기 + `Test` 버튼 (`providers::pat::verify`)
  2. **OAuth** — 디바이스 코드 표시 + 브라우저 열기 + polling
     (`providers::oauth::start_device_flow`)
  3. **SSH Key** — 기존 키 스캔 or 새 키 생성
     (`providers::ssh::generate_ed25519`, agent 등록)
- 토큰은 `keyring` (이미 deps에 있음)에만 저장, `config.json`에는 키 ref만
- 웰컴 화면의 통합 입력창에서 내 레포 검색 (`provider.list_repos` — 추가 필요)

**주의**
1. `provider trait`에는 아직 `discover_repo`만 있고 `list_repos` / `create_pr`
   메서드가 없음. 트레이트 확장 필요.
2. OAuth는 Azure / Bitbucket 구형 계정에서 device flow 미지원 — 각 provider의
   `supports_device_flow()` 추가 후 UI에서 탭 disable.
3. SSH key를 OAuth 토큰으로 자동 업로드하는 플로우 (`POST /user/keys` 등)는 각
   provider별로 다름. 최초 릴리스는 "키 생성 + 공개키 복사 버튼" 까지만.

---

## 4. AI 하네스

### 4.1 AI 배선 진행 상황

**완료된 부분** ✅
- Config에 `ai_endpoint: Option<Endpoint>` 추가 (API key는 serde-skip)
- Keyring 헬퍼: `ai::save_api_key` / `ai::load_api_key` / `ai::config::delete_api_key`
  (service = `mergefox-ai`, account = endpoint name)
- 비차단 AI 태스크 러너: `ai::AiTask<T>` — `spawn` + 프레임마다 `poll()`
  (tokio `oneshot` 기반, 드롭 시 결과 폐기)
- `Repo::staged_diff_text(max_bytes)` — staged 없으면 workdir 폴백, 바이트 캡
- `Settings → AI` 섹션 — 프리셋(Ollama/OpenAI/Anthropic/Custom), 프로토콜,
  Base URL, 모델, API key(keyring 즉시 저장), 고급(context/max_output/
  grammar/streaming), Test 버튼(백그라운드 ping + 인라인 결과)
- Commit 모달 ✨ Generate 버튼 — 스테이지된 diff를 `gen_commit_message`에
  태우고 결과를 메시지 필드에 주입 (이미 작성 중이면 append). 실패 시
  인라인 에러. 실행 중 스피너 + 버튼 비활성화.

**아직 미배선**
- **Stash 메시지** — stash 모달 자체가 없음 (1.3 참조). 모달 만들 때 같이.
- **Explain change** — 커밋 컨텍스트 메뉴에 `Explain`, diff 뷰어 하단 패널에
  markdown 결과. `ai::tasks::explain_change::explain_change` 호출.
- **Commit composer** — commit 모달에 "Split into logical commits" → composer
  모달. Rust 클러스터링 + 그룹별 메시지 생성 (`compose_commits`).
- **PR 충돌 제안** — conflict 해결 UI에 `Suggest merge` 버튼. 제안은 자동
  적용 금지, hallucination 검증은 task 내부에 이미 있음.
- **`Record AI outputs in journal`** 체크 (diff 노출 우려로 기본 off)

**주의**
- Endpoint `name`이 keyring account 키이므로, 저장 후 이름을 바꾸면 기존
  키가 고아가 됨. Settings UI에 힌트 문구로 안내 중이지만, 이름 변경 시
  자동 마이그레이션(delete old → save new)을 추가하면 더 안전.
- Ollama 기본값 선택 의도적: 로컬 0-키 경험이 최저 마찰.
- AI 에러 타입은 `AiError` — UI에선 그냥 `format!("{e}")` 로 표시 중.
  Retry/Context-overflow는 이미 task 내부에서 처리되니 UI는 단순 텍스트.

### 4.2 기존 범위에서 옮겨 온 항목 (요약용)
  하단의 markdown 패널에
- **PR 충돌 제안** — conflict 해결 UI가 별도로 필요 (아직 없음). 병합 중
  `REBASE_HEAD`/`MERGE_HEAD` 감지해서 "충돌 해결" 뷰로 전환. 이 뷰에 `Suggest`
  버튼 추가
- **Commit composer** — commit 모달에 `Split into logical commits` 옵션,
  composer 모달 오픈 → `compose_commits` 결과로 그룹화 편집 가능하게

**주의**
1. AI 출력은 절대 자동 적용 금지. 특히 conflict 제안은 원본 토큰만 사용되는지
   검증 후 사용자 승인. 이 검증 로직은 `tasks::pr_conflict` 안에 이미 있음 —
   UI에서 "Apply" 버튼을 누를 때만 실제 파일에 쓰기.
2. AI 기능은 설정이 없을 때 버튼 자체를 disable (tooltip: "Settings → AI에서
   설정하세요"). 기능이 있는지 없는지 헷갈리는 게 최악.
3. 로컬 모델(Qwen2.5-0.5B)에선 explain이 느림 (hunk 단위 여러 호출). UI는
   스트리밍 또는 "in progress" 상태 표시 필수.

---

## 5. MCP 전송 계층

### 5.1 MCP 서버 (stdio / socket)  🟡

**의도**
`src/mcp/` 에는 read-only `ActivityLogView` 구조체와 JSON 직렬화가 있음. 내부
UI 인스펙터는 이것을 그대로 사용 중. 그러나 **외부 프로세스(에이전트)가 접근할
방법이 없음** — 애초에 "에이전트 게이트웨이" 포지셔닝이었는데 현재는 혼자
만들고 혼자 보는 상태.

**범위 (v1 — 최소)**
- stdio JSON-RPC 서버를 백그라운드 태스크로 실행 옵션 (`Settings → MCP → Enable`)
- 노출 툴 (read-only부터):
  - `activity.list(limit, offset)` → `ActivityLogView`
  - `activity.get(id)` → `ActivityEntry`
  - `repo.status()` → 현재 HEAD / 브랜치 / dirty 여부
  - `branch.list()` → 로컬 + 원격
  - `log.read(branch, limit)` → 커밋 요약
  - `diff.show(from, to)` → 텍스트 diff
- 세션 토큰 인증: 서버 시작 시 uuid 토큰 생성, Settings에 표시, 에이전트는
  `Authorization: Bearer <token>` 필요

**범위 (v2 — 쓰기)**
- Tier 2 (auto-confirm on clean tree): `branch.create`, `branch.checkout`, `fetch`
- Tier 3 (반드시 모달): `commit.create`, `push`, `pull`, `reset.hard`, `rebase`
- 각 쓰기 호출 → UI 승인 큐에 enqueue → 사용자 Approve/Deny → 결과 반환
- 프로토콜은 표준 MCP (rmcp crate 또는 직접 JSON-RPC) 로 구현해서
  Claude Code, Cursor 등에서 바로 붙도록

**주의**
1. 토큰은 절대 노출 금지 tool (`credential.*`) 와 hook 설치 금지 (`hook.install`)는
   기본 disable로 시작. "에이전트 게이트웨이"의 의의는 사용자가 통제권을 가진
   것이므로, 안전 기본값이 전부.
2. 승인 모달 UI는 이미 `pending_prompt` 인프라가 있으니 MCP 승인도 같은 큐에
   밀어넣는 방식이 자연스러움.
3. 외부 접근 가능해지면 **포트 노출 주의** — localhost 바인딩 고정, 0.0.0.0
   옵션은 제공하지 않음 (원하는 사용자는 SSH 터널링).

---

## 6. 품질 / 기술부채

### 6.1 egui 0.29 deprecation  ⛔

**의도**
`cargo build`에서 deprecation 2건 ( `src/ui/diff_view.rs:79, 97` 의
`ScrollArea::id_source`). 0.30에서 제거 예정이라 다음 egui 업그레이드 시
깨짐. 지금 고치면 5초.

**범위**
- `id_source(...)` → `id_salt(...)` 치환

---

### 6.2 dead-code 경고 153건  ⛔

**의도**
Provider / AI / 일부 MCP 심볼들이 전부 dead-code. 각 기능이 UI에 배선되면
자연히 사라지지만, 그 전까지는 "진짜 미사용 코드"와 "미배선 코드"가 섞여
있어서 실제 버그 놓치기 쉬움.

**범위**
- Provider / AI를 UI에 연결하면 대부분 해소
- 그래도 남는 것 있으면 `#[allow(dead_code)]` 파일 단위로 일괄 붙이고 TODO 주석

---

### 6.3 Journal I/O 에러 처리  🟢

**의도**
현재 `journal.record` 실패 시 사용자에게 보여줄 에러 처리가 미흡할 수 있음
(디스크 풀, 권한 등). undo/redo는 이 기능의 핵심 안전망이라 침묵 실패가
최악의 시나리오.

**범위**
- `append_line` 실패 시 명시적 토스트 에러
- 더 나아가 sync-write + fsync 정책 검토 (현재는 append 후 자동 flush에 의존)

---

## 완료된 것 (참고용)

- ✅ 웰컴 화면 / Repo 열기 / Clone (with HTTPS TLS)
- ✅ 멀티 탭 레포 (Ctrl+Tab, Cmd+W)
- ✅ 커밋 그래프 (레인 렌더, 컬럼 선택기, HEAD/refs 칩)
- ✅ 컨텍스트 메뉴 (checkout / revert / cherry-pick / reset / branch CRUD / tag / amend)
- ✅ Commit 모달 (스테이지 + 메시지 + amend)
- ✅ Undo/Redo (HUD + Panic Recovery + auto-stash, Cmd+Z/Cmd+Shift+Z 엄격 구분)
- ✅ Diff 뷰어 (텍스트 + 이미지 side-by-side, 파일 리스트, 5000줄/2MB 캡)
- ✅ MCP 액티비티 로그 (in-process, JSON export, 힌트 휴리스틱)
- ✅ Fetch (백그라운드 잡 + 진행률)

---

## 추천 진행 순서

현실적 순서 (의존성 + 체감 가치):

1. **Z5 Push 모달** — 매일 쓰는 기본 기능 구멍
2. **Z7 Stash 사이드바** — ops가 이미 있어서 UI만
3. **Z6 Pull 모달** (단, `jobs.rs`의 Rebase 경로 먼저 검증)
4. **Provider PAT만 우선** — OAuth는 복잡하니 토큰 붙여넣기부터
5. **AI 커밋 메시지 1개** — Settings + commit 모달에 ✨ 하나
6. **Drop commit** — 자주 요청되는 rebase 기능 1순위
7. AI 나머지 4개 태스크
8. Provider OAuth + SSH
9. MCP stdio 서버 (v1 read-only)
10. Worktree / Move commit / MCP v2 쓰기
