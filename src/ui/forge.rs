use egui::{RichText, TextEdit};

use crate::app::{MergeFoxApp, View};
use crate::config::UiLanguage;
use crate::forge::{ForgeSelection, ForgeState, IssueModalState, PullRequestModalState};

#[derive(Debug, Clone)]
pub enum SidebarAction {
    Refresh,
    NewPullRequest,
    NewIssue,
    Select(ForgeSelection),
}

pub fn show_sidebar(
    ui: &mut egui::Ui,
    language: UiLanguage,
    forge: &ForgeState,
    action: &mut Option<SidebarAction>,
) {
    let labels = labels(language.resolved());

    ui.separator();
    ui.heading(labels.heading);

    if let Some(repo) = forge.repo.as_ref() {
        ui.small(format!("{}/{}", repo.owner, repo.repo));
        ui.weak(repo.remote_name.as_str());
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.small_button(labels.refresh).clicked() {
                *action = Some(SidebarAction::Refresh);
            }
            if ui.small_button(labels.new_pr).clicked() {
                *action = Some(SidebarAction::NewPullRequest);
            }
            if ui.small_button(labels.new_issue).clicked() {
                *action = Some(SidebarAction::NewIssue);
            }
        });
    } else {
        ui.weak(labels.connect_hint);
    }

    if forge.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak(labels.loading);
        });
    }

    if let Some(err) = &forge.last_error {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
    }

    ui.add_space(6.0);
    ui.collapsing(
        format!("{} ({})", labels.pull_requests, forge.pull_requests.len()),
        |ui| {
            if forge.pull_requests.is_empty() {
                ui.weak(labels.empty_prs);
            } else {
                for pr in &forge.pull_requests {
                    let selected = forge.selected == Some(ForgeSelection::PullRequest(pr.number));
                    let mut title = format!("#{} {}", pr.number, pr.title);
                    if pr.is_draft {
                        title.push_str(" · draft");
                    }
                    let resp = ui.selectable_label(selected, title);
                    if resp.clicked() {
                        *action = Some(SidebarAction::Select(ForgeSelection::PullRequest(
                            pr.number,
                        )));
                    }
                    resp.on_hover_text(pr.body.clone().unwrap_or_default());
                    ui.small(format!("{} → {} · {}", pr.head_ref, pr.base_ref, pr.author));
                    ui.add_space(4.0);
                }
            }
        },
    );

    ui.collapsing(
        format!("{} ({})", labels.issues, forge.issues.len()),
        |ui| {
            if forge.issues.is_empty() {
                ui.weak(labels.empty_issues);
            } else {
                for issue in &forge.issues {
                    let selected = forge.selected == Some(ForgeSelection::Issue(issue.number));
                    let resp =
                        ui.selectable_label(selected, format!("#{} {}", issue.number, issue.title));
                    if resp.clicked() {
                        *action = Some(SidebarAction::Select(ForgeSelection::Issue(issue.number)));
                    }
                    resp.on_hover_text(issue.body.clone().unwrap_or_default());
                    let meta = if issue.labels.is_empty() {
                        issue.author.clone()
                    } else {
                        format!("{} · {}", issue.author, issue.labels.join(", "))
                    };
                    ui.small(meta);
                    ui.add_space(4.0);
                }
            }
        },
    );
}

pub fn show(ctx: &egui::Context, app: &mut MergeFoxApp) {
    show_selected_detail(ctx, app);
    show_pull_request_modal(ctx, app);
    show_issue_modal(ctx, app);
}

fn show_selected_detail(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let snapshot = {
        let View::Workspace(tabs) = &app.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        tabs.current().forge.clone()
    };
    let Some(selected) = snapshot.selected else {
        return;
    };
    let labels = labels(app.config.ui_language.resolved());

    let mut copy_url: Option<String> = None;
    let mut copy_mcp = false;
    let mut clear_selection = false;
    let mut open = true;

    match selected {
        ForgeSelection::PullRequest(number) => {
            let Some(pr) = snapshot.pull_requests.iter().find(|pr| pr.number == number) else {
                return;
            };
            let repo_label = snapshot
                .repo
                .as_ref()
                .map(|repo| format!("{}/{}", repo.owner, repo.repo))
                .unwrap_or_default();

            egui::Window::new(format!("{} #{}", labels.pull_request, pr.number))
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(620.0)
                .default_height(460.0)
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.heading(&pr.title);
                        if pr.is_draft {
                            ui.label(RichText::new("draft").color(egui::Color32::LIGHT_BLUE));
                        }
                    });
                    if !repo_label.is_empty() {
                        ui.weak(repo_label);
                    }
                    ui.small(format!(
                        "{} · {} → {} · {}",
                        pr.author,
                        pr.head_ref,
                        pr.base_ref,
                        pr.state.as_api_str()
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(labels.copy_url).clicked() {
                            copy_url = Some(pr.url.clone());
                        }
                        if ui.button(labels.copy_mcp).clicked() {
                            copy_mcp = true;
                        }
                        ui.hyperlink_to(labels.open_link, &pr.url);
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if let Some(body) = &pr.body {
                            if body.trim().is_empty() {
                                ui.weak(labels.empty_body);
                            } else {
                                ui.label(body);
                            }
                        } else {
                            ui.weak(labels.empty_body);
                        }
                    });
                });
        }
        ForgeSelection::Issue(number) => {
            let Some(issue) = snapshot.issues.iter().find(|issue| issue.number == number) else {
                return;
            };
            let repo_label = snapshot
                .repo
                .as_ref()
                .map(|repo| format!("{}/{}", repo.owner, repo.repo))
                .unwrap_or_default();

            egui::Window::new(format!("{} #{}", labels.issue, issue.number))
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(620.0)
                .default_height(460.0)
                .show(ctx, |ui| {
                    ui.heading(&issue.title);
                    if !repo_label.is_empty() {
                        ui.weak(repo_label);
                    }
                    ui.small(format!("{} · {}", issue.author, issue.state.as_api_str()));
                    if !issue.labels.is_empty() {
                        ui.small(format!("labels: {}", issue.labels.join(", ")));
                    }
                    if !issue.assignees.is_empty() {
                        ui.small(format!("assignees: {}", issue.assignees.join(", ")));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(labels.copy_url).clicked() {
                            copy_url = Some(issue.url.clone());
                        }
                        if ui.button(labels.copy_mcp).clicked() {
                            copy_mcp = true;
                        }
                        ui.hyperlink_to(labels.open_link, &issue.url);
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if let Some(body) = &issue.body {
                            if body.trim().is_empty() {
                                ui.weak(labels.empty_body);
                            } else {
                                ui.label(body);
                            }
                        } else {
                            ui.weak(labels.empty_body);
                        }
                    });
                });
        }
    }

    if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        clear_selection = true;
    }
    if let Some(url) = copy_url {
        ctx.copy_text(url);
        app.hud = Some(crate::app::Hud::new(labels.copied_url, 1600));
    }
    if copy_mcp {
        let json = serde_json::to_string_pretty(&crate::mcp::forge_view_for_state(&snapshot))
            .unwrap_or_else(|_| "{}".into());
        ctx.copy_text(json);
        app.hud = Some(crate::app::Hud::new(labels.copied_mcp, 1800));
    }
    if clear_selection {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        if !tabs.launcher_active {
            tabs.current_mut().forge.selected = None;
        }
    }
}

fn show_pull_request_modal(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let labels = labels(app.config.ui_language.resolved());
    let mut submit = false;
    let mut close = false;
    let mut open = true;

    {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        let ws = tabs.current_mut();
        let Some(modal) = ws.forge.pr_modal.as_mut() else {
            return;
        };
        let scope_text = ws
            .forge
            .repo
            .as_ref()
            .map(|repo| crate::forge::pull_request_scope_text(repo.private));

        egui::Window::new(labels.new_pr)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .default_height(520.0)
            .show(ctx, |ui| {
                render_pull_request_modal(ui, modal, scope_text, &labels);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(labels.cancel).clicked() {
                        close = true;
                    }
                    if ui
                        .add_enabled(modal.head_ready, egui::Button::new(labels.create_pr))
                        .clicked()
                    {
                        submit = true;
                    }
                });
            });
    }

    if !open {
        close = true;
    }
    if close {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        if !tabs.launcher_active {
            tabs.current_mut().forge.pr_modal = None;
        }
    } else if submit {
        app.submit_pull_request();
    }
}

fn show_issue_modal(ctx: &egui::Context, app: &mut MergeFoxApp) {
    let labels = labels(app.config.ui_language.resolved());
    let mut submit = false;
    let mut close = false;
    let mut open = true;

    {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        if tabs.launcher_active {
            return;
        }
        let ws = tabs.current_mut();
        let Some(modal) = ws.forge.issue_modal.as_mut() else {
            return;
        };
        let scope_text = ws
            .forge
            .repo
            .as_ref()
            .map(|_| crate::forge::issue_scope_text());

        egui::Window::new(labels.new_issue)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(640.0)
            .default_height(500.0)
            .show(ctx, |ui| {
                render_issue_modal(ui, modal, scope_text, &labels);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(labels.cancel).clicked() {
                        close = true;
                    }
                    if ui.button(labels.create_issue).clicked() {
                        submit = true;
                    }
                });
            });
    }

    if !open {
        close = true;
    }
    if close {
        let View::Workspace(tabs) = &mut app.view else {
            return;
        };
        if !tabs.launcher_active {
            tabs.current_mut().forge.issue_modal = None;
        }
    } else if submit {
        app.submit_issue();
    }
}

fn render_pull_request_modal(
    ui: &mut egui::Ui,
    modal: &mut PullRequestModalState,
    scope_text: Option<&str>,
    labels: &Labels,
) {
    if let Some(scope) = scope_text {
        ui.weak(format!("{}: {}", labels.scope_hint, scope));
    }
    if let Some(hint) = &modal.head_hint {
        let color = if modal.head_ready {
            egui::Color32::GRAY
        } else {
            egui::Color32::from_rgb(240, 180, 90)
        };
        ui.colored_label(color, hint);
    }
    field(ui, labels.head_branch, |ui| {
        ui.add(TextEdit::singleline(&mut modal.head).desired_width(f32::INFINITY));
    });
    field(ui, labels.base_branch, |ui| {
        ui.add(TextEdit::singleline(&mut modal.base).desired_width(f32::INFINITY));
    });
    field(ui, labels.title, |ui| {
        ui.add(TextEdit::singleline(&mut modal.title).desired_width(f32::INFINITY));
    });
    ui.checkbox(&mut modal.draft, labels.draft_pr);
    ui.label(labels.body);
    ui.add(
        TextEdit::multiline(&mut modal.body)
            .desired_width(f32::INFINITY)
            .desired_rows(14),
    );
    if let Some(err) = &modal.last_error {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
    }
}

fn render_issue_modal(
    ui: &mut egui::Ui,
    modal: &mut IssueModalState,
    scope_text: Option<&str>,
    labels: &Labels,
) {
    if let Some(scope) = scope_text {
        ui.weak(format!("{}: {}", labels.scope_hint, scope));
    }
    field(ui, labels.title, |ui| {
        ui.add(TextEdit::singleline(&mut modal.title).desired_width(f32::INFINITY));
    });
    ui.label(labels.body);
    ui.add(
        TextEdit::multiline(&mut modal.body)
            .desired_width(f32::INFINITY)
            .desired_rows(14),
    );
    if let Some(err) = &modal.last_error {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
    }
}

fn field(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(label);
        add(ui);
    });
}

struct Labels {
    heading: &'static str,
    loading: &'static str,
    connect_hint: &'static str,
    refresh: &'static str,
    new_pr: &'static str,
    new_issue: &'static str,
    pull_requests: &'static str,
    issues: &'static str,
    pull_request: &'static str,
    issue: &'static str,
    empty_prs: &'static str,
    empty_issues: &'static str,
    empty_body: &'static str,
    copy_url: &'static str,
    copy_mcp: &'static str,
    open_link: &'static str,
    copied_url: &'static str,
    copied_mcp: &'static str,
    scope_hint: &'static str,
    head_branch: &'static str,
    base_branch: &'static str,
    title: &'static str,
    body: &'static str,
    draft_pr: &'static str,
    cancel: &'static str,
    create_pr: &'static str,
    create_issue: &'static str,
}

fn labels(language: UiLanguage) -> Labels {
    match language {
        UiLanguage::Korean => Labels {
            heading: "원격 협업",
            loading: "PR / 이슈를 불러오는 중...",
            connect_hint: "GitHub 계정을 설정의 연동 섹션에 연결하면 이 저장소의 PR과 이슈를 바로 볼 수 있습니다.",
            refresh: "새로고침",
            new_pr: "PR 만들기",
            new_issue: "이슈 만들기",
            pull_requests: "풀 리퀘스트",
            issues: "이슈",
            pull_request: "풀 리퀘스트",
            issue: "이슈",
            empty_prs: "열린 PR이 없습니다.",
            empty_issues: "열린 이슈가 없습니다.",
            empty_body: "본문이 없습니다.",
            copy_url: "URL 복사",
            copy_mcp: "MCP JSON 복사",
            open_link: "브라우저에서 열기",
            copied_url: "URL을 복사했습니다",
            copied_mcp: "MCP JSON을 복사했습니다",
            scope_hint: "필요한 scope",
            head_branch: "From",
            base_branch: "To",
            title: "제목",
            body: "본문",
            draft_pr: "Draft PR",
            cancel: "취소",
            create_pr: "PR 생성",
            create_issue: "이슈 생성",
        },
        _ => Labels {
            heading: "Forge",
            loading: "Loading pull requests and issues...",
            connect_hint: "Connect GitHub in Settings → Integrations to view pull requests and issues for this repo.",
            refresh: "Refresh",
            new_pr: "New PR",
            new_issue: "New Issue",
            pull_requests: "Pull Requests",
            issues: "Issues",
            pull_request: "Pull Request",
            issue: "Issue",
            empty_prs: "No open pull requests.",
            empty_issues: "No open issues.",
            empty_body: "No description provided.",
            copy_url: "Copy URL",
            copy_mcp: "Copy MCP JSON",
            open_link: "Open in browser",
            copied_url: "Copied URL",
            copied_mcp: "Copied MCP JSON",
            scope_hint: "Required scope",
            head_branch: "From",
            base_branch: "To",
            title: "Title",
            body: "Body",
            draft_pr: "Draft pull request",
            cancel: "Cancel",
            create_pr: "Create PR",
            create_issue: "Create issue",
        },
    }
}
