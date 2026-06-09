use crate::review_provider::{ReactionContent, ReviewComment};
use editor::display_map::BlockContext;
use gpui::{AnyElement, App, Entity, ImageSource, SharedString};
use regex::Regex;
use std::sync::LazyLock;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownOptions, MarkdownStyle};
use ui::{
    Button, ButtonStyle, Color, ContextMenu, FluentBuilder, IconName, IntoElement, Label,
    LabelSize, PopoverMenu, div, h_flex, prelude::*, v_flex,
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
fn rewrite_html_images(body: &str) -> String {
    static PICTURE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)</?picture[^>]*>|<source\b[^>]*>").unwrap());
    static IMG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<img\b[^>]*>").unwrap());
    static ANCHOR_IMG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)(<a\b[^>]*>)\s*(!\[[^\]]*\]\([^)]*\))\s*</a>").unwrap()
    });

    // Drop <picture>/<source> wrappers, keeping the inner <img>.
    let body = PICTURE.replace_all(body, "");

    // Bare <img …> → ![alt](src). Leave the tag untouched if it has no src.
    let body = IMG.replace_all(&body, |caps: &regex::Captures| {
        let tag = &caps[0];
        match html_attr(tag, "src") {
            Some(src) => format!("![{}]({})", html_attr(tag, "alt").unwrap_or_default(), src),
            None => tag.to_string(),
        }
    });

    // <a href><img></a> (now <a href>![alt](src)</a>) → [![alt](src)](href).
    ANCHOR_IMG
        .replace_all(&body, |caps: &regex::Captures| match html_attr(&caps[1], "href") {
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
    let style = MarkdownStyle::themed(MarkdownFont::Editor, cx.window, cx.app);

    let mut container = v_flex()
        .w_full()
        .max_w(cx.max_width - cx.anchor_x)
        .overflow_x_hidden()
        .pl(cx.anchor_x)
        .pr_2()
        .py_1()
        .gap_1()
        .border_t_1()
        .border_b_1()
        .border_color(colors.border)
        .bg(colors.editor_background);

    for (comment, markdown_entity, suggestions) in &comments {
        let is_reply = comment.reply_to.is_some();

        let mut row = v_flex()
            .px_2()
            .py_1()
            .gap_0p5()
            .rounded_md()
            .when(is_reply, |el| {
                el.ml_4()
                    .border_l_2()
                    .border_color(colors.border_focused)
                    .pl_2()
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(comment.author.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Default),
                    )
                    .child(
                        Label::new(comment.created_at.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                MarkdownElement::new(markdown_entity.clone(), style.clone())
                    .image_resolver(resolve_comment_image),
            );

        for suggestion in suggestions {
            row = row.child(render_suggestion_block(suggestion, comment.id, cx));
        }

        if let Some(bar) = reaction_bar(comment, "inline", cx.app) {
            row = row.child(bar);
        }

        container = container.child(row);
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
        .p_2()
        .rounded_md()
        .bg(colors.surface_background)
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
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
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
                .p_1()
                .rounded_sm()
                .bg(colors.editor_background)
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

/// A row of reaction pills (one per non-zero reaction) plus a "+" picker, shared
/// by the sidebar comment card and the inline diff comment block. Both surfaces
/// drive reactions by dispatching `ReactToComment`, so this needs no entity
/// handle. Returns `None` for comments without a node id (e.g. a just-posted
/// comment that hasn't been reloaded), which can't be reacted to yet.
pub fn reaction_bar(comment: &ReviewComment, surface: &str, cx: &mut App) -> Option<AnyElement> {
    if comment.node_id.is_empty() {
        return None;
    }
    let comment_id = comment.id;
    let colors = cx.theme().colors().clone();

    let mut bar = h_flex().flex_wrap().gap_1().items_center();
    for group in &comment.reactions {
        if group.count == 0 {
            continue;
        }
        let content = group.content;
        let add = !group.viewer_reacted;
        let (background, border) = if group.viewer_reacted {
            (colors.element_selected, colors.border_focused)
        } else {
            (colors.element_background, colors.border)
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

    let reactions = comment.reactions.clone();
    let picker = PopoverMenu::new(SharedString::from(format!(
        "react-picker-{surface}-{comment_id}"
    )))
        .trigger(
            Button::new(
                SharedString::from(format!("react-add-{surface}-{comment_id}")),
                "Add reaction",
            )
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .style(ButtonStyle::Subtle),
        )
        .menu(move |window, cx| {
            let reactions = reactions.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                for content in ReactionContent::ALL {
                    let already = reactions
                        .iter()
                        .any(|group| group.content == content && group.viewer_reacted);
                    let add = !already;
                    menu = menu.entry(
                        format!("{}  {}", content.emoji(), content.label()),
                        None,
                        move |window, cx| {
                            window.dispatch_action(
                                Box::new(ReactToComment {
                                    comment_id,
                                    content: content.graphql().to_string(),
                                    add,
                                }),
                                cx,
                            );
                        },
                    );
                }
                menu
            }))
        });

    Some(bar.child(picker).into_any_element())
}
