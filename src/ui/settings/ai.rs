//! "AI" settings section — configure the LLM endpoint used by the
//! commit-message / explain / conflict / composer tasks.
//!
//! Unlike General / Repository, the AI section uses **staged draft state**
//! rather than immediate-save. Reason: an endpoint is only useful if
//! protocol + base URL + model + key are all consistent, so applying a
//! half-edited protocol change would be actively harmful (requests go
//! out, fail, user confuses themselves). The draft lives in the modal
//! (`SettingsModal::ai`), and the `Save` and `Test` buttons both read
//! from it.
//!
//! API keys go to the OS keyring — `config.json` only stores the
//! endpoint's public fields. When the draft is saved, the key is written
//! to the keyring under the endpoint's `name`; when the modal is opened,
//! the key is loaded back from the keyring into the draft buffer.

use egui::{ComboBox, RichText, TextEdit};
use secrecy::{ExposeSecret, SecretString};

use super::{persist_config, Feedback};
use crate::ai::{self, anthropic_preset, ollama_preset, openai_preset, Endpoint, Protocol};
use crate::app::MergeFoxApp;
use crate::config::{Config, UiLanguage};

/// Working copy of an endpoint + API key for the settings UI.
///
/// API key is carried as a plain `String` here so `TextEdit` can bind to
/// it; we wrap it in `SecretString` only at save/test time. The draft is
/// discarded when the modal closes without saving.
pub struct AiDraft {
    pub preset: AiPreset,
    pub protocol: Protocol,
    pub name: String,
    pub base_url: String,
    pub model_id: String,
    pub api_key: String,
    pub context_window: u32,
    pub max_output: u32,
    pub supports_grammar: bool,
    pub supports_streaming: bool,
    /// Result of the last "Test" click — shown inline.
    pub test_status: TestStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestStatus {
    Idle,
    InProgress,
    Ok(String),
    Err(String),
}

impl Default for TestStatus {
    fn default() -> Self {
        Self::Idle
    }
}

/// Quick-start templates. Selecting a preset fills the base URL / model
/// / grammar flag with sensible defaults; user can still edit any field
/// afterward — the preset is purely a starting point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiPreset {
    Custom,
    OpenAI,
    Anthropic,
    Ollama,
}

impl AiPreset {
    fn label(self) -> &'static str {
        match self {
            Self::Custom => "Custom",
            Self::OpenAI => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Ollama => "Ollama (local)",
        }
    }

    fn all() -> &'static [Self] {
        &[Self::Ollama, Self::OpenAI, Self::Anthropic, Self::Custom]
    }

    /// Detect which preset an existing endpoint was built from. We pick
    /// the matching preset if both base URL and protocol match; otherwise
    /// fall back to `Custom` so the UI doesn't pretend a hand-edited
    /// config came from a preset.
    fn detect(endpoint: &Endpoint) -> Self {
        let base = endpoint.base_url.trim_end_matches('/');
        match endpoint.protocol {
            Protocol::OpenAICompatible if base == "https://api.openai.com/v1" => Self::OpenAI,
            Protocol::OpenAICompatible if base == "http://localhost:11434/v1" => Self::Ollama,
            Protocol::Anthropic if base == "https://api.anthropic.com" => Self::Anthropic,
            _ => Self::Custom,
        }
    }

    /// Apply preset defaults to the draft, preserving anything the user
    /// has already typed where it still makes sense (model id, key).
    fn apply(self, draft: &mut AiDraft) {
        match self {
            Self::OpenAI => {
                let mut ep = openai_preset(
                    if draft.model_id.is_empty() {
                        "gpt-4o-mini"
                    } else {
                        &draft.model_id
                    },
                    SecretString::new(draft.api_key.clone()),
                );
                // Preserve user-chosen name so their keyring entry is stable.
                if !draft.name.is_empty() {
                    ep.name = draft.name.clone();
                }
                draft.overwrite_from(&ep);
            }
            Self::Anthropic => {
                let mut ep = anthropic_preset(
                    if draft.model_id.is_empty() {
                        "claude-3-5-haiku-latest"
                    } else {
                        &draft.model_id
                    },
                    SecretString::new(draft.api_key.clone()),
                );
                if !draft.name.is_empty() {
                    ep.name = draft.name.clone();
                }
                draft.overwrite_from(&ep);
            }
            Self::Ollama => {
                let mut ep = ollama_preset(if draft.model_id.is_empty() {
                    "qwen2.5:0.5b"
                } else {
                    &draft.model_id
                });
                if !draft.name.is_empty() {
                    ep.name = draft.name.clone();
                }
                draft.overwrite_from(&ep);
                draft.api_key.clear(); // Ollama takes no key.
            }
            Self::Custom => {
                // Don't touch fields — user is in full-manual mode.
            }
        }
    }
}

impl AiDraft {
    /// Build a draft from the current Config. If an endpoint is configured
    /// we preload its API key from the keyring so the user can see the key
    /// is present (as a masked field) without re-entering it.
    pub fn from_config(
        config: &Config,
        secret_store: &crate::secrets::SecretStore,
    ) -> Self {
        if let Some(endpoint) = &config.ai_endpoint {
            // Key may be in either the OS keyring or the internal file
            // store, depending on user preference. `SecretStore` caches
            // the result so a subsequent Settings-open (or commit-modal
            // ✨ click) in the same session hits memory, not the OS
            // keychain consent daemon.
            let api_key = secret_store
                .load_api_key(&endpoint.name)
                .map(|s| s.expose_secret().to_string())
                .unwrap_or_default();
            let preset = AiPreset::detect(endpoint);
            Self {
                preset,
                protocol: endpoint.protocol,
                name: endpoint.name.clone(),
                base_url: endpoint.base_url.clone(),
                model_id: endpoint.model_id.clone(),
                api_key,
                context_window: endpoint.context_window,
                max_output: endpoint.max_output,
                supports_grammar: endpoint.supports_grammar,
                supports_streaming: endpoint.supports_streaming,
                test_status: TestStatus::Idle,
            }
        } else {
            // Default draft seeds Ollama because it needs no API key —
            // lowest-friction first-run experience.
            let preset = AiPreset::Ollama;
            let seed = ollama_preset("qwen2.5:0.5b");
            Self {
                preset,
                protocol: seed.protocol,
                name: seed.name,
                base_url: seed.base_url,
                model_id: seed.model_id,
                api_key: String::new(),
                context_window: seed.context_window,
                max_output: seed.max_output,
                supports_grammar: seed.supports_grammar,
                supports_streaming: seed.supports_streaming,
                test_status: TestStatus::Idle,
            }
        }
    }

    fn overwrite_from(&mut self, ep: &Endpoint) {
        self.protocol = ep.protocol;
        self.name = ep.name.clone();
        self.base_url = ep.base_url.clone();
        self.context_window = ep.context_window;
        self.max_output = ep.max_output;
        self.supports_grammar = ep.supports_grammar;
        self.supports_streaming = ep.supports_streaming;
        // Model + key are deliberately preserved — they're the fields
        // users care about most and presets shouldn't stomp on them.
    }

    fn to_endpoint(&self) -> Endpoint {
        Endpoint {
            name: self.name.clone(),
            protocol: self.protocol,
            base_url: self.base_url.clone(),
            api_key: SecretString::new(self.api_key.clone()),
            model_id: self.model_id.clone(),
            context_window: self.context_window.max(1024),
            max_output: self.max_output.max(32),
            supports_grammar: self.supports_grammar,
            supports_streaming: self.supports_streaming,
        }
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut MergeFoxApp) {
    let language = current_language(app);
    let labels = labels(language);

    ui.heading(labels.heading);
    ui.separator();
    ui.weak(labels.intro);
    ui.add_space(6.0);

    let mut intent: Option<Intent> = None;

    {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };
        let draft = &mut modal.ai;

        // ---- preset ----
        ui.horizontal(|ui| {
            ui.label(labels.preset);
            let before = draft.preset;
            ComboBox::from_id_salt("settings_ai_preset")
                .selected_text(draft.preset.label())
                .show_ui(ui, |ui| {
                    for p in AiPreset::all() {
                        ui.selectable_value(&mut draft.preset, *p, p.label());
                    }
                });
            if draft.preset != before {
                draft.preset.apply(draft);
                draft.test_status = TestStatus::Idle;
            }
        });

        ui.add_space(4.0);

        // ---- protocol ----
        ui.horizontal(|ui| {
            ui.label(labels.protocol);
            ComboBox::from_id_salt("settings_ai_protocol")
                .selected_text(match draft.protocol {
                    Protocol::OpenAICompatible => "OpenAI-compatible",
                    Protocol::Anthropic => "Anthropic",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut draft.protocol,
                        Protocol::OpenAICompatible,
                        "OpenAI-compatible",
                    );
                    ui.selectable_value(&mut draft.protocol, Protocol::Anthropic, "Anthropic");
                });
        });
        ui.weak(labels.protocol_hint);

        ui.add_space(8.0);

        // ---- fields ----
        grid_row(ui, labels.name, |ui| {
            ui.add(
                TextEdit::singleline(&mut draft.name)
                    .desired_width(f32::INFINITY)
                    .hint_text("e.g. ollama:qwen2.5:0.5b"),
            );
        });
        ui.weak(labels.name_hint);

        grid_row(ui, labels.base_url, |ui| {
            ui.add(
                TextEdit::singleline(&mut draft.base_url)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://api.openai.com/v1"),
            );
        });

        grid_row(ui, labels.model, |ui| {
            ui.add(
                TextEdit::singleline(&mut draft.model_id)
                    .desired_width(f32::INFINITY)
                    .hint_text("qwen2.5:0.5b"),
            );
        });

        grid_row(ui, labels.api_key, |ui| {
            ui.add(
                TextEdit::singleline(&mut draft.api_key)
                    .password(true)
                    .desired_width(f32::INFINITY)
                    .hint_text(labels.api_key_placeholder),
            );
        });
        ui.weak(labels.api_key_hint);

        ui.add_space(8.0);
        ui.collapsing(labels.advanced, |ui| {
            grid_row(ui, labels.context_window, |ui| {
                ui.add(
                    egui::DragValue::new(&mut draft.context_window)
                        .range(1024u32..=1_000_000u32)
                        .speed(256.0),
                );
            });
            grid_row(ui, labels.max_output, |ui| {
                ui.add(
                    egui::DragValue::new(&mut draft.max_output)
                        .range(32u32..=8192u32)
                        .speed(16.0),
                );
            });
            ui.checkbox(&mut draft.supports_grammar, labels.supports_grammar);
            ui.weak(labels.supports_grammar_hint);
            ui.checkbox(&mut draft.supports_streaming, labels.supports_streaming);
        });

        ui.add_space(12.0);

        // ---- inline test status ----
        match &draft.test_status {
            TestStatus::Idle => {}
            TestStatus::InProgress => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(labels.testing);
                });
            }
            TestStatus::Ok(msg) => {
                ui.colored_label(egui::Color32::LIGHT_GREEN, format!("✓ {msg}"));
            }
            TestStatus::Err(msg) => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("✗ {msg}"));
            }
        }

        ui.add_space(4.0);

        // ---- action buttons ----
        ui.horizontal(|ui| {
            let testing = matches!(draft.test_status, TestStatus::InProgress);
            if ui
                .add_enabled(!testing, egui::Button::new(labels.test))
                .clicked()
            {
                intent = Some(Intent::Test);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(labels.save).clicked() {
                    intent = Some(Intent::Save);
                }
                if app.config.ai_endpoint.is_some()
                    && ui
                        .button(RichText::new(labels.clear).color(egui::Color32::LIGHT_RED))
                        .clicked()
                {
                    intent = Some(Intent::Clear);
                }
            });
        });
    }

    if let Some(intent) = intent {
        handle_intent(app, intent, &labels);
    }

    // While a test is running, poll its handle each frame.
    poll_test(app, &labels);
}

enum Intent {
    Save,
    Test,
    Clear,
}

fn handle_intent(app: &mut MergeFoxApp, intent: Intent, labels: &Labels) {
    match intent {
        Intent::Save => save_endpoint(app, labels),
        Intent::Test => start_test(app, labels),
        Intent::Clear => clear_endpoint(app, labels),
    }
}

fn save_endpoint(app: &mut MergeFoxApp, labels: &Labels) {
    let endpoint = {
        let Some(modal) = app.settings_modal.as_ref() else {
            return;
        };
        if modal.ai.base_url.trim().is_empty() {
            app.settings_modal.as_mut().unwrap().feedback =
                Some(Feedback::err(labels.err_base_url));
            return;
        }
        if modal.ai.model_id.trim().is_empty() {
            app.settings_modal.as_mut().unwrap().feedback = Some(Feedback::err(labels.err_model));
            return;
        }
        if modal.ai.name.trim().is_empty() {
            app.settings_modal.as_mut().unwrap().feedback = Some(Feedback::err(labels.err_name));
            return;
        }
        modal.ai.to_endpoint()
    };

    // Persist the secret via the unified SecretStore (keyring or file,
    // depending on user preference). We do this first so a config file
    // pointing at an endpoint whose key failed to save never reaches
    // disk — better to reject the save than leave orphan config.
    if let Err(e) = app
        .secret_store
        .save_api_key(&endpoint.name, &endpoint.api_key)
    {
        if let Some(modal) = app.settings_modal.as_mut() {
            modal.feedback = Some(Feedback::err(format!("secret store: {e:#}")));
        }
        return;
    }

    app.config.ai_endpoint = Some(endpoint);
    persist_config(app, labels.saved);
}

fn clear_endpoint(app: &mut MergeFoxApp, labels: &Labels) {
    // Remove the secret before clearing config, same ordering logic as
    // save — we'd rather leave a stale config field than a stale secret.
    if let Some(endpoint) = app.config.ai_endpoint.as_ref() {
        let _ = app.secret_store.delete_api_key(&endpoint.name);
    }
    app.config.ai_endpoint = None;
    persist_config(app, labels.cleared);
    if let Some(modal) = app.settings_modal.as_mut() {
        modal.ai = AiDraft::from_config(&Config::default(), &app.secret_store);
    }
}

fn start_test(app: &mut MergeFoxApp, labels: &Labels) {
    let endpoint = {
        let Some(modal) = app.settings_modal.as_mut() else {
            return;
        };
        modal.ai.test_status = TestStatus::InProgress;
        modal.ai.to_endpoint()
    };

    let task = ai::AiTask::spawn(async move {
        use crate::ai::{build_client, CompletionRequest, Msg, Role};
        let client = build_client(endpoint);
        let req = CompletionRequest {
            system: "Reply with the single word OK.".to_string(),
            messages: vec![Msg {
                role: Role::User,
                content: "ping".to_string(),
            }],
            max_tokens: 8,
            temperature: 0.0,
            grammar: None,
            json_schema: None,
            stop: vec![],
        };
        client.complete(req).await
    });

    // Store the running handle in app state for `poll_test` to observe.
    app.ai_test_task = Some(task);
    let _ = labels; // labels are re-derived inside poll_test
}

/// Called every frame from `show`. Picks up a running test and updates
/// the draft's inline status.
fn poll_test(app: &mut MergeFoxApp, labels: &Labels) {
    let Some(task) = app.ai_test_task.as_mut() else {
        return;
    };
    let Some(result) = task.poll() else {
        return;
    };
    app.ai_test_task = None;

    let Some(modal) = app.settings_modal.as_mut() else {
        return;
    };
    match result {
        Ok(resp) => {
            let preview = resp.text.trim();
            let preview = if preview.is_empty() {
                labels.test_empty_reply.to_string()
            } else {
                let truncated: String = preview.chars().take(60).collect();
                format!("{} · {} tokens in", truncated, resp.tokens_in)
            };
            modal.ai.test_status = TestStatus::Ok(preview);
        }
        Err(err) => {
            modal.ai.test_status = TestStatus::Err(format!("{err}"));
        }
    }
}

fn grid_row<R>(ui: &mut egui::Ui, label: &str, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        // Fixed-width label so aligned fields line up.
        ui.add_sized([140.0, 20.0], egui::Label::new(label));
        contents(ui)
    })
    .inner
}

fn current_language(app: &MergeFoxApp) -> UiLanguage {
    app.settings_modal
        .as_ref()
        .map(|m| m.language.resolved())
        .unwrap_or_else(|| app.config.ui_language.resolved())
}

struct Labels {
    heading: &'static str,
    intro: &'static str,
    preset: &'static str,
    protocol: &'static str,
    protocol_hint: &'static str,
    name: &'static str,
    name_hint: &'static str,
    base_url: &'static str,
    model: &'static str,
    api_key: &'static str,
    api_key_placeholder: &'static str,
    api_key_hint: &'static str,
    advanced: &'static str,
    context_window: &'static str,
    max_output: &'static str,
    supports_grammar: &'static str,
    supports_grammar_hint: &'static str,
    supports_streaming: &'static str,
    test: &'static str,
    save: &'static str,
    clear: &'static str,
    testing: &'static str,
    test_empty_reply: &'static str,
    saved: &'static str,
    cleared: &'static str,
    err_base_url: &'static str,
    err_model: &'static str,
    err_name: &'static str,
}

fn labels(lang: UiLanguage) -> Labels {
    match lang {
        UiLanguage::Korean => Labels {
            heading: "AI 엔드포인트",
            intro:
                "커밋 메시지 생성이나 변경 설명 같은 AI 기능이 사용하는 LLM 엔드포인트를 설정합니다. API 키는 OS 키체인에 저장됩니다.",
            preset: "프리셋",
            protocol: "프로토콜",
            protocol_hint: "프리셋을 고르면 자동 선택되지만 수동으로 바꿀 수도 있습니다.",
            name: "식별 이름",
            name_hint: "키체인 항목 이름으로 사용됩니다. 한 번 저장한 뒤에는 바꾸지 않는 편이 좋습니다.",
            base_url: "Base URL",
            model: "모델",
            api_key: "API 키",
            api_key_placeholder: "로컬 Ollama는 비워두세요",
            api_key_hint: "저장 시 OS 키체인에만 기록되며 설정 파일에는 저장되지 않습니다.",
            advanced: "고급",
            context_window: "컨텍스트 윈도우 (토큰)",
            max_output: "최대 출력 토큰",
            supports_grammar: "GBNF 문법 지원 (llama.cpp / Ollama)",
            supports_grammar_hint: "켜면 작은 로컬 모델도 엄격한 포맷(예: Conventional Commits)을 따르게 됩니다.",
            supports_streaming: "스트리밍 응답 지원",
            test: "테스트",
            save: "저장",
            clear: "엔드포인트 제거",
            testing: "엔드포인트를 확인하는 중…",
            test_empty_reply: "응답은 성공했지만 본문이 비어있습니다",
            saved: "AI 엔드포인트를 저장했습니다",
            cleared: "AI 엔드포인트를 제거했습니다",
            err_base_url: "Base URL이 비어있습니다.",
            err_model: "모델 ID가 비어있습니다.",
            err_name: "식별 이름이 비어있습니다.",
        },
        _ => Labels {
            heading: "AI Endpoint",
            intro:
                "Configure the LLM endpoint used by AI features (commit-message generation, explain change, etc.). API keys are stored in the OS keyring.",
            preset: "Preset",
            protocol: "Protocol",
            protocol_hint: "Presets set this automatically, but you can override it.",
            name: "Identifier",
            name_hint:
                "Used as the keyring account. Keep it stable once saved — changing it creates a new keyring entry.",
            base_url: "Base URL",
            model: "Model",
            api_key: "API key",
            api_key_placeholder: "Leave empty for local Ollama",
            api_key_hint: "Stored only in the OS keyring, never in the config file.",
            advanced: "Advanced",
            context_window: "Context window (tokens)",
            max_output: "Max output tokens",
            supports_grammar: "Supports GBNF grammar (llama.cpp / Ollama)",
            supports_grammar_hint:
                "When enabled, tiny local models can be forced into strict formats (e.g. Conventional Commits).",
            supports_streaming: "Supports streaming",
            test: "Test",
            save: "Save",
            clear: "Clear endpoint",
            testing: "Probing endpoint…",
            test_empty_reply: "Endpoint responded but returned no text",
            saved: "Saved AI endpoint",
            cleared: "Cleared AI endpoint",
            err_base_url: "Base URL cannot be empty.",
            err_model: "Model id cannot be empty.",
            err_name: "Identifier cannot be empty.",
        },
    }
}
