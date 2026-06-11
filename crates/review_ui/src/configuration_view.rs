use anyhow::{Result, bail};
use credentials_provider::CredentialsProvider;
use editor::Editor;
use futures::AsyncReadExt as _;
use git_hosting_providers::{
    GithubTokenSource, clear_github_token, resolve_github_token_with_source, store_github_token,
};
use gpui::{AnyElement, Context, Entity, EventEmitter, Focusable, Render, SharedString, Window};
use http_client::{AsyncBody, HttpClient, HttpRequestExt, RedirectPolicy, Request};
use serde::Deserialize;
use std::sync::Arc;
use ui::{
    Button, ButtonStyle, Color, DynamicSpacing, Icon, IconButton, IconName, IconSize, IntoElement,
    Label, LabelSize, Tooltip, div, h_flex, prelude::*, rems, v_flex,
};

const GITHUB_API_URL: &str = "https://api.github.com";
const CREATE_TOKEN_URL: &str =
    "https://github.com/settings/tokens/new?scopes=repo&description=Zed%20review";

pub enum ConfigurationEvent {
    /// The stored GitHub credentials changed; the panel should rebuild its
    /// provider and reload.
    CredentialsChanged,
    /// Return to the pull request list.
    Back,
}

#[derive(Clone)]
enum Status {
    Idle,
    Working,
    Error(SharedString),
}

pub struct ConfigurationView {
    credentials: Arc<dyn CredentialsProvider>,
    http_client: Arc<dyn HttpClient>,
    owner: Option<String>,
    repo: Option<String>,
    token_editor: Entity<Editor>,
    source: GithubTokenSource,
    /// Login of the currently-resolved token, if validated.
    signed_in_login: Option<SharedString>,
    status: Status,
    /// Whether the token input is obscured (it's a secret). Toggled by the
    /// reveal eye button.
    token_masked: bool,
}

impl EventEmitter<ConfigurationEvent> for ConfigurationView {}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

impl ConfigurationView {
    pub fn new(
        credentials: Arc<dyn CredentialsProvider>,
        http_client: Arc<dyn HttpClient>,
        owner: Option<String>,
        repo: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let token_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("GitHub personal access token", window, cx);
            // A token is a secret: obscure it by default.
            editor.set_masked(true, cx);
            editor
        });

        let view = Self {
            credentials,
            http_client,
            owner,
            repo,
            token_editor,
            source: GithubTokenSource::None,
            signed_in_login: None,
            status: Status::Idle,
            token_masked: true,
        };
        view.refresh_status(cx);
        view
    }

    fn toggle_token_mask(&mut self, cx: &mut Context<Self>) {
        self.token_masked = !self.token_masked;
        let masked = self.token_masked;
        self.token_editor
            .update(cx, |editor, cx| editor.set_masked(masked, cx));
        cx.notify();
    }

    /// Resolve the current token + source for display, and validate it to show
    /// the signed-in login.
    fn refresh_status(&self, cx: &mut Context<Self>) {
        let credentials = self.credentials.clone();
        let http_client = self.http_client.clone();
        cx.spawn(async move |this, cx| {
            let (token, source) = resolve_github_token_with_source(credentials, cx).await;
            let login = match &token {
                Some(token) => fetch_login(&http_client, token).await.ok(),
                None => None,
            };
            this.update(cx, |this, cx| {
                this.source = source;
                this.signed_in_login = login.map(SharedString::from);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let token = self.token_editor.read(cx).text(cx).trim().to_string();
        if token.is_empty() {
            self.status = Status::Error("Enter a personal access token.".into());
            cx.notify();
            return;
        }
        self.status = Status::Working;
        cx.notify();

        let credentials = self.credentials.clone();
        let http_client = self.http_client.clone();
        cx.spawn_in(window, async move |this, cx| {
            // Validate the token before storing it, so a typo surfaces here
            // rather than as a later rate-limit/auth failure.
            let login = match fetch_login(&http_client, &token).await {
                Ok(login) => login,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.status =
                            Status::Error(format!("Couldn't verify token: {error}").into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            if let Err(error) = store_github_token(credentials, &token, cx).await {
                this.update(cx, |this, cx| {
                    this.status = Status::Error(format!("Couldn't save token: {error}").into());
                    cx.notify();
                })
                .ok();
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.source = GithubTokenSource::Keychain;
                this.signed_in_login = Some(login.into());
                this.status = Status::Idle;
                this.token_editor
                    .update(cx, |editor, cx| editor.clear(window, cx));
                cx.emit(ConfigurationEvent::CredentialsChanged);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.status = Status::Working;
        cx.notify();
        let credentials = self.credentials.clone();
        let http_client = self.http_client.clone();
        cx.spawn(async move |this, cx| {
            if let Err(error) = clear_github_token(credentials.clone(), cx).await {
                this.update(cx, |this, cx| {
                    this.status = Status::Error(format!("Couldn't clear token: {error}").into());
                    cx.notify();
                })
                .ok();
                return;
            }
            // Re-resolve to show whatever source is next-best (env / gh / none).
            let (token, source) = resolve_github_token_with_source(credentials, cx).await;
            let login = match &token {
                Some(token) => fetch_login(&http_client, token).await.ok(),
                None => None,
            };
            this.update(cx, |this, cx| {
                this.source = source;
                this.signed_in_login = login.map(SharedString::from);
                this.status = Status::Idle;
                cx.emit(ConfigurationEvent::CredentialsChanged);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn source_label(&self) -> &'static str {
        match self.source {
            GithubTokenSource::Env => "GITHUB_TOKEN environment variable",
            GithubTokenSource::Keychain => "saved in this app",
            GithubTokenSource::GitCredential => "git credential helper",
            GithubTokenSource::GhCli => "gh CLI",
            GithubTokenSource::None => "not signed in",
        }
    }
}

/// Verify a token and return the authenticated user's login via `GET /user`.
async fn fetch_login(http_client: &Arc<dyn HttpClient>, token: &str) -> Result<String> {
    let request = Request::get(format!("{GITHUB_API_URL}/user"))
        .header("Accept", "application/vnd.github.v3+json")
        .follow_redirects(RedirectPolicy::FollowAll)
        .header("Authorization", format!("Bearer {token}"))
        .body(AsyncBody::default())?;
    let mut response = http_client.send(request).await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    if !response.status().is_success() {
        bail!("GitHub returned {}", response.status().as_u16());
    }
    let user: GhUser = serde_json::from_slice(&body)?;
    Ok(user.login)
}

impl Render for ConfigurationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let signed_in =
            self.signed_in_login.is_some() || !matches!(self.source, GithubTokenSource::None);

        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .id("config-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(self.render_account_section(signed_in, cx))
                    .child(self.render_token_section(signed_in, cx)),
            )
    }
}

impl ConfigurationView {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(ui::Tab::container_height(cx))
            .flex_none()
            .w_full()
            .items_center()
            .gap(DynamicSpacing::Base04.rems(cx))
            .px(DynamicSpacing::Base04.rems(cx))
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().tab_bar_background)
            .child(
                IconButton::new("config-back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Back to pull requests"))
                    .on_click(cx.listener(|_this, _, _window, cx| {
                        cx.emit(ConfigurationEvent::Back);
                    })),
            )
            .child(Label::new("GitHub Account"))
    }

    /// Settings-page section header: a `Headline` title with a muted description
    /// and an optional right-aligned action, mirroring Zed's agent settings.
    fn render_section_title(
        &self,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        action: Option<AnyElement>,
    ) -> impl IntoElement {
        h_flex().p_4().pb_0().mb_2p5().w_full().items_start().child(
            v_flex()
                .w_full()
                .gap_0p5()
                .child(
                    h_flex()
                        .pr_1()
                        .w_full()
                        .gap_2()
                        .justify_between()
                        .flex_wrap()
                        .child(Headline::new(title.into()))
                        .children(action),
                )
                .child(Label::new(description.into()).color(Color::Muted)),
        )
    }

    fn render_account_section(&self, signed_in: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let working = matches!(self.status, Status::Working);
        let from_keychain = matches!(self.source, GithubTokenSource::Keychain);

        // Sign out only acts on the app-stored (keychain) token.
        let sign_out = from_keychain.then(|| {
            Button::new("config-sign-out", "Sign out")
                .style(ButtonStyle::Outlined)
                .label_size(LabelSize::Small)
                .disabled(working)
                .on_click(cx.listener(|this, _, _window, cx| this.sign_out(cx)))
                .into_any_element()
        });

        let repo_line = match (&self.owner, &self.repo) {
            (Some(owner), Some(repo)) => Some(SharedString::from(format!("{owner}/{repo}"))),
            _ => None,
        };

        v_flex()
            .min_w_0()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(self.render_section_title(
                "Account",
                "Your sign-in for reviewing pull requests on this repository.",
                sign_out,
            ))
            .child(
                v_flex()
                    .p_4()
                    .pt_0()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Icon::new(if signed_in {
                                    IconName::Check
                                } else {
                                    IconName::Person
                                })
                                .size(IconSize::Small)
                                .color(if signed_in { Color::Success } else { Color::Muted }),
                            )
                            .child(match &self.signed_in_login {
                                Some(login) => Label::new(format!("Signed in as {login}")),
                                None if signed_in => Label::new("Signed in"),
                                None => Label::new("Not signed in"),
                            }),
                    )
                    // Muted key + default-color value, so the (immutable) label
                    // reads distinctly from the resolved value.
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new("Token source").color(Color::Muted))
                            .child(Label::new(self.source_label())),
                    )
                    .when_some(repo_line, |el, repo| {
                        el.child(
                            h_flex()
                                .gap_1()
                                .child(Label::new("Repository").color(Color::Muted))
                                .child(Label::new(repo)),
                        )
                    }),
            )
    }

    fn render_token_section(&self, signed_in: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let working = matches!(self.status, Status::Working);
        let reveal_icon = if self.token_masked {
            IconName::Eye
        } else {
            IconName::EyeOff
        };

        let create_token = Button::new("config-create-token", "Create token")
            .style(ButtonStyle::Outlined)
            .label_size(LabelSize::Small)
            .end_icon(
                Icon::new(IconName::ArrowUpRight)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .on_click(|_, _window, cx| cx.open_url(CREATE_TOKEN_URL))
            .into_any_element();

        let description = if signed_in {
            "Replace the token saved in your keychain."
        } else {
            "Sign in with a token (needs the `repo` scope), stored in your keychain."
        };

        v_flex()
            .min_w_0()
            .border_b_1()
            .border_color(colors.border)
            .child(self.render_section_title(
                "Personal Access Token",
                description,
                Some(create_token),
            ))
            .child(
                v_flex()
                    .p_4()
                    .pt_0()
                    .gap_2()
                    .child(
                        // Capped-width input with a reveal toggle for the secret.
                        h_flex()
                            .max_w(rems(28.))
                            .px_2()
                            .py_1()
                            .gap_1()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .child(
                                div()
                                    .id("config-token-input")
                                    .flex_1()
                                    .overflow_x_hidden()
                                    .cursor_text()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        window.focus(&this.token_editor.focus_handle(cx), cx);
                                    }))
                                    .child(self.token_editor.clone()),
                            )
                            .child(
                                IconButton::new("config-token-reveal", reveal_icon)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text(if self.token_masked {
                                        "Show token"
                                    } else {
                                        "Hide token"
                                    }))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.toggle_token_mask(cx);
                                    })),
                            ),
                    )
                    .when_some(
                        match &self.status {
                            Status::Error(message) => Some(message.clone()),
                            _ => None,
                        },
                        |el, message| {
                            el.child(
                                Label::new(message).size(LabelSize::Small).color(Color::Error),
                            )
                        },
                    )
                    .child(
                        // Neutral outline (not filled-accent): in Zed filled
                        // accent is reserved for upsell CTAs; a routine sign-in
                        // matches "Sign in to use GitHub Copilot" — a full-width
                        // outline button with a leading GitHub icon.
                        Button::new("config-sign-in", "Sign in")
                            .full_width()
                            .style(ButtonStyle::Outlined)
                            .start_icon(
                                Icon::new(IconName::Github)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .disabled(working)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.sign_in(window, cx);
                            })),
                    )
                    .child(
                        Label::new(
                            "GITHUB_TOKEN, the git credential helper, and gh are managed \
                             outside Zed. Panel preferences live in settings.json.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
    }
}
