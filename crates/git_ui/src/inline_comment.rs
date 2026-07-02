use pull_request::{ReactionContent, ReactionGroup, ReviewComment};
use editor::display_map::BlockContext;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ImageSource, Render, SharedString, Window, px,
};
use regex::Regex;
use std::sync::LazyLock;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownOptions, MarkdownStyle};
use ui::{
    Avatar, Button, ButtonStyle, Color, FluentBuilder, IconButton, IconName, IconSize, IntoElement,
    Label, LabelSize, PopoverMenu, Tooltip, div, h_flex, prelude::*, v_flex,
};

#[derive(Clone, Debug)]
pub struct SuggestionBlock {
    pub suggested_code: String,
}

/// Build a `Markdown` entity for a comment body with HTML parsing enabled, so
/// GitHub's `<details>`/`<summary>` and other inline HTML render instead of
/// showing as literal tags.
pub fn comment_markdown(body: SharedString, cx: &mut App) -> Entity<Markdown> {
    let cleaned = SharedString::from(sanitize_comment_html(&body));
    cx.new(|cx| {
        Markdown::new_with_options(
            cleaned,
            None,
            None,
            MarkdownOptions {
                parse_html: true,
                ..Default::default()
            },
            cx,
        )
    })
}

/// Resolve a comment image URL to a remote image source. Only http(s) URLs are
/// loaded (data: images are decoded by the markdown renderer itself); relative
/// or other schemes are skipped. Used as the `image_resolver` for comment
/// markdown so inline `<img>` and `<a><img></a>` badges render.
pub fn resolve_comment_image(url: &str) -> Option<ImageSource> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Some(ImageSource::from(url.to_string()))
    } else {
        None
    }
}

/// Strip HTML noise the markdown renderer can't display inline: HTML comments
/// (`<!-- ... -->`, often used by bots to stash metadata) and `<sub>`/`<sup>`
/// wrappers (unwrapped to their inner text).
fn sanitize_comment_html(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        if let Some(end) = rest[start..].find("-->") {
            rest = &rest[start + end + 3..];
        } else {
            rest = "";
            break;
        }
    }
    out.push_str(rest);

    let out = out
        .replace("<sub>", "")
        .replace("</sub>", "")
        .replace("<sup>", "")
        .replace("</sup>", "");

    rewrite_html_images(&out)
}

/// Convert HTML images/badges to markdown so they render even when they appear
/// inline (pulldown otherwise passes inline `<img>`/`<a>` through as literal
/// text). Handles `<picture>` wrappers, bare `<img>` → `![alt](src)`, and
/// `<a href><img></a>` → `[![alt](src)](href)`.
///
/// Runs only outside code: fenced (``` / ~~~) blocks and inline `code` spans are
/// passed through untouched so HTML in code examples isn't rewritten.
fn rewrite_html_images(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<char> = None;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = trimmed.as_bytes()[0] as char;
            match fence {
                None => fence = Some(marker),
                Some(open) if open == marker => fence = None,
                _ => {}
            }
            out.push_str(line);
        } else if fence.is_some() {
            out.push_str(line);
        } else {
            out.push_str(&rewrite_outside_inline_code(line));
        }
    }
    out
}

/// Rewrite HTML in a line, leaving inline `code` spans verbatim.
fn rewrite_outside_inline_code(line: &str) -> String {
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`\n]*`").unwrap());
    let mut out = String::with_capacity(line.len());
    let mut last = 0;
    for span in CODE.find_iter(line) {
        out.push_str(&rewrite_html_chunk(&line[last..span.start()]));
        out.push_str(span.as_str());
        last = span.end();
    }
    out.push_str(&rewrite_html_chunk(&line[last..]));
    out
}

fn rewrite_html_chunk(text: &str) -> String {
    // Tag bodies allow `>` inside quoted attribute values.
    static PICTURE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)</?picture\b(?:[^>"']|"[^"]*"|'[^']*')*>|<source\b(?:[^>"']|"[^"]*"|'[^']*')*>"#).unwrap()
    });
    static IMG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)<img\b(?:[^>"']|"[^"]*"|'[^']*')*>"#).unwrap()
    });
    static ANCHOR_IMG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)(<a\b(?:[^>"']|"[^"]*"|'[^']*')*>)\s*(!\[[^\]]*\]\([^)]*\))\s*</a>"#)
            .unwrap()
    });

    // Drop <picture>/<source> wrappers, keeping the inner <img>.
    let text = PICTURE.replace_all(text, "");

    // Bare <img …> → ![alt](src). Leave the tag untouched if it has no src.
    let text = IMG.replace_all(&text, |caps: &regex::Captures| {
        let tag = &caps[0];
        match html_attr(tag, "src") {
            Some(src) => format!("![{}]({})", html_attr(tag, "alt").unwrap_or_default(), src),
            None => tag.to_string(),
        }
    });

    // <a href><img></a> (now <a href>![alt](src)</a>) → [![alt](src)](href).
    ANCHOR_IMG
        .replace_all(&text, |caps: &regex::Captures| match html_attr(&caps[1], "href") {
            Some(href) => format!("[{}]({})", &caps[2], href),
            None => caps[2].to_string(),
        })
        .into_owned()
}

/// Extract an HTML attribute value (double- or single-quoted) from a tag string.
fn html_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let key = format!("{name}=");
    let mut from = 0;
    while let Some(pos) = lower[from..].find(&key) {
        let after = from + pos + key.len();
        // Ensure the match is an attribute boundary, not a suffix of another attr.
        let preceded_by_space = from + pos == 0
            || tag[..from + pos]
                .chars()
                .last()
                .is_some_and(|c| c.is_whitespace());
        let quote = tag[after..].chars().next();
        if preceded_by_space && matches!(quote, Some('"') | Some('\'')) {
            let quote = quote.unwrap();
            let value_start = after + 1;
            if let Some(end) = tag[value_start..].find(quote) {
                return Some(tag[value_start..value_start + end].to_string());
            }
        }
        from = after;
    }
    None
}

/// Extracts ```suggestion fenced blocks from a comment body.
/// Returns the body with suggestion blocks removed and the extracted suggestions.
pub fn parse_suggestions(body: &str) -> (String, Vec<SuggestionBlock>) {
    let mut cleaned_lines: Vec<&str> = Vec::new();
    let mut suggestions: Vec<SuggestionBlock> = Vec::new();
    let mut inside_suggestion = false;
    let mut current_suggestion_lines: Vec<&str> = Vec::new();

    for line in body.lines() {
        if inside_suggestion {
            if line.trim() == "```" {
                suggestions.push(SuggestionBlock {
                    suggested_code: current_suggestion_lines.join("\n"),
                });
                current_suggestion_lines.clear();
                inside_suggestion = false;
            } else {
                current_suggestion_lines.push(line);
            }
        } else {
            let trimmed = line.trim();
            if trimmed.starts_with("```suggestion") {
                inside_suggestion = true;
            } else {
                cleaned_lines.push(line);
            }
        }
    }

    (cleaned_lines.join("\n"), suggestions)
}

/// Renders a PR comment thread (parent + replies) as an inline editor block.
/// Each tuple pairs a comment with its pre-created Markdown entity and extracted suggestions.
pub fn render_pr_comment_block(
    comments: Vec<(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)>,
    cx: &mut BlockContext,
) -> AnyElement {
    let colors = cx.theme().colors().clone();
    let anchor_x = cx.anchor_x;
    let max_width = cx.max_width;
    let mut style = MarkdownStyle::themed(MarkdownFont::Editor, cx.window, cx.app);
    // Flatten code/HTML blocks: drop the outlined-card border so a standalone
    // literal matches the flat inline-code fill (in-buffer content is low-chrome).
    // Long code lines scroll horizontally rather than clip.
    style.code_block.border_widths = gpui::EdgesRefinement::default();
    style.code_block.border_style = None;
    style.code_block.background = Some(colors.element_background.into());
    style.code_block_overflow_x_scroll = true;

    let mut container = v_flex()
        .w_full()
        .max_w(max_width - anchor_x)
        .overflow_x_hidden()
        .pr_2()
        .py_1()
        .gap_1()
        .border_t_1()
        .border_b_1()
        .border_color(colors.border_variant)
        .bg(colors.editor_background);

    for (comment, markdown_entity, suggestions) in &comments {
        let is_reply = comment.reply_to.is_some();
        // Reveal the reaction affordance in the left "gutter" cell on hover.
        let hover_group = SharedString::from(format!("inline-comment-{}", comment.id));
        let reaction_trigger = add_reaction_picker(comment, "inline", cx.app);

        let mut content = v_flex()
            .flex_1()
            .min_w_0()
            .py_1()
            .gap_0p5()
            // Replies indent under a muted hairline guide (the editor's own
            // indent-guide idiom), not a saturated accent rail.
            .when(is_reply, |el| {
                el.border_l_1().border_color(colors.border_variant).pl_2()
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Avatar::new(crate::review_view::avatar_url(&comment.author)).size(px(16.0)),
                    )
                    .child(
                        Label::new(comment.author.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Default),
                    )
                    .child(
                        div()
                            .id(("inline-comment-time", comment.id as usize))
                            .flex_none()
                            .tooltip(Tooltip::text(comment.created_at.clone()))
                            .child(
                                Label::new(crate::review_view::format_pr_date(&comment.created_at))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .child(
                MarkdownElement::new(markdown_entity.clone(), style.clone())
                    .image_resolver(resolve_comment_image),
            );

        for suggestion in suggestions {
            content = content.child(render_suggestion_block(suggestion, comment.id, cx));
        }

        // Existing reaction pills sit inline near the body (only when present).
        if let Some(pills) = reaction_pills(comment, "inline", cx.app) {
            content = content.child(pills);
        }

        // The "gutter" cell (aligned with the diff gutter, where the add-comment
        // "+" lives) holds the hover-revealed ThumbsUp reaction trigger.
        let gutter = div()
            .flex_none()
            .w(anchor_x)
            .flex()
            .justify_start()
            // Align with the gutter "+" add-comment button, which the editor
            // positions at ~git_gutter_width + 2px (≈ 0.275 * line_height + 2).
            .pl_2()
            .children(
                reaction_trigger
                    .map(|trigger| div().visible_on_hover(hover_group.clone()).child(trigger)),
            );

        container = container.child(
            h_flex()
                .group(hover_group)
                .w_full()
                .items_start()
                .child(gutter)
                .child(content),
        );
    }

    container.into_any_element()
}

fn render_suggestion_block(
    suggestion: &SuggestionBlock,
    comment_id: u64,
    cx: &mut BlockContext,
) -> impl IntoElement {
    let suggestion_id = SharedString::from(format!("suggestion_{}", comment_id));
    let colors = cx.theme().colors().clone();
    let status = cx.theme().status().clone();

    v_flex()
        .id(suggestion_id)
        .mt_1()
        .pl_2()
        .py_1()
        // Flat on the editor background; a left accent hairline marks it as a
        // suggestion (no rounded/tinted card).
        .border_l_2()
        .border_color(status.created)
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(
                    Label::new("Suggested change")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("apply_suggestion_{}", comment_id)),
                        "Apply",
                    )
                    .end_icon(ui::Icon::new(IconName::Check).size(ui::IconSize::XSmall))
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::XSmall)
                    .on_click({
                        move |_event, window, cx| {
                            window.dispatch_action(
                                Box::new(ApplySuggestion { comment_id }),
                                cx,
                            );
                        }
                    }),
                ),
        )
        .child(
            v_flex()
                .mt_1()
                .px_1()
                // Flat buffer-font code run on a faint fill — the one justified
                // fill (it's code), no outlined box.
                .bg(colors.element_background)
                .child(
                    Label::new(suggestion.suggested_code.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Default)
                        .buffer_font(cx.app),
                ),
        )
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
pub struct ApplySuggestion {
    pub comment_id: u64,
}

/// Dispatched (from either the sidebar card or an inline block) to add or remove
/// the current user's reaction on a comment. `content` is the GraphQL
/// `ReactionContent` value (e.g. `THUMBS_UP`).
#[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
pub struct ReactToComment {
    pub comment_id: u64,
    pub content: String,
    pub add: bool,
}

/// The reaction pills (one per non-zero reaction) for a comment. `None` when the
/// comment has no reactions to show. Clicking a pill toggles that reaction.
pub fn reaction_pills(comment: &ReviewComment, surface: &str, cx: &mut App) -> Option<AnyElement> {
    let comment_id = comment.id;
    let colors = cx.theme().colors().clone();

    let mut bar = h_flex().flex_wrap().gap_1().items_center();
    let mut any = false;
    for group in &comment.reactions {
        if group.count == 0 {
            continue;
        }
        any = true;
        let content = group.content;
        let add = !group.viewer_reacted;
        let (background, border) = if group.viewer_reacted {
            (colors.element_selected, colors.border_focused)
        } else {
            (colors.ghost_element_background, colors.border)
        };
        bar = bar.child(
            div()
                .id(SharedString::from(format!(
                    "reaction-{surface}-{comment_id}-{}",
                    content.graphql()
                )))
                .flex()
                .items_center()
                .gap_0p5()
                .px_1()
                .rounded_md()
                .border_1()
                .border_color(border)
                .bg(background)
                .cursor_pointer()
                .hover(|style| style.border_color(colors.border_focused))
                .child(Label::new(content.emoji()).size(LabelSize::XSmall))
                .child(
                    Label::new(group.count.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .on_click(move |_event, window, cx| {
                    window.dispatch_action(
                        Box::new(ReactToComment {
                            comment_id,
                            content: content.graphql().to_string(),
                            add,
                        }),
                        cx,
                    );
                }),
        );
    }

    any.then(|| bar.into_any_element())
}

/// The "Add reaction" picker: a muted `ThumbsUp` icon trigger opening the emoji
/// menu. `None` for comments without a node id (e.g. a just-posted comment that
/// hasn't been reloaded), which can't be reacted to yet.
pub fn add_reaction_picker(
    comment: &ReviewComment,
    surface: &str,
    _cx: &mut App,
) -> Option<AnyElement> {
    if comment.node_id.is_empty() {
        return None;
    }
    let comment_id = comment.id;
    let reactions = comment.reactions.clone();
    let picker = PopoverMenu::new(SharedString::from(format!(
        "react-picker-{surface}-{comment_id}"
    )))
    .trigger(
        IconButton::new(
            SharedString::from(format!("react-add-{surface}-{comment_id}")),
            IconName::ThumbsUp,
        )
        // XSmall + Transparent matches the gutter "+" add-comment button so the
        // two line up vertically in the gutter.
        .icon_size(IconSize::XSmall)
        .icon_color(Color::Muted)
        .style(ButtonStyle::Transparent)
        .tooltip(Tooltip::text("Add reaction")),
    )
    .menu(move |_window, cx| {
        let reactions = reactions.clone();
        Some(cx.new(|cx| ReactionPicker::new(comment_id, reactions, cx)))
    });
    Some(picker.into_any_element())
}

/// The emoji-grid popover shown by the "Add reaction" trigger: a compact grid of
/// the supported emojis (no labels). Clicking one toggles that reaction and
/// dismisses the popover.
struct ReactionPicker {
    comment_id: u64,
    reactions: Vec<ReactionGroup>,
    focus_handle: FocusHandle,
}

impl ReactionPicker {
    fn new(comment_id: u64, reactions: Vec<ReactionGroup>, cx: &mut App) -> Self {
        Self {
            comment_id,
            reactions,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Focusable for ReactionPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ReactionPicker {}

impl Render for ReactionPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let comment_id = self.comment_id;
        h_flex()
            // Capture focus + dismiss when the user clicks outside the popover.
            .track_focus(&self.focus_handle)
            .on_mouse_down_out(cx.listener(|_this, _event, _window, cx| {
                cx.emit(DismissEvent);
            }))
            // Popover surface chrome (background + border + rounding + shadow).
            .elevation_2(cx)
            .p_1()
            .gap_0p5()
            .flex_wrap()
            // Cap the width so the eight emojis wrap into a grid rather than a
            // single long row.
            .max_w(px(148.0))
            .children(ReactionContent::ALL.into_iter().map(|content| {
                let already = self
                    .reactions
                    .iter()
                    .any(|group| group.content == content && group.viewer_reacted);
                let add = !already;
                div()
                    .id(SharedString::from(format!("react-grid-{}", content.graphql())))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.0))
                    .rounded_md()
                    .cursor_pointer()
                    .when(already, |el| el.bg(colors.element_selected))
                    .tooltip(Tooltip::text(content.label()))
                    .child(Label::new(content.emoji()))
                    .on_click(cx.listener(move |_this, _event, window, cx| {
                        window.dispatch_action(
                            Box::new(ReactToComment {
                                comment_id,
                                content: content.graphql().to_string(),
                                add,
                            }),
                            cx,
                        );
                        cx.emit(DismissEvent);
                    }))
            }))
    }
}

/// Reaction pills followed by the "Add reaction" picker, on one row — used by the
/// sidebar comment card. (The inline thread renders the pills per-comment and the
/// picker in the thread footer next to "Reply" instead.)
pub fn reaction_bar(comment: &ReviewComment, surface: &str, cx: &mut App) -> Option<AnyElement> {
    let pills = reaction_pills(comment, surface, cx);
    let picker = add_reaction_picker(comment, surface, cx)?;
    Some(
        h_flex()
            .flex_wrap()
            .gap_1()
            .items_center()
            .children(pills)
            .child(picker)
            .into_any_element(),
    )
}
