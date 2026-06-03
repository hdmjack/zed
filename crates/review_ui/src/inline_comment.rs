use crate::review_provider::ReviewComment;
use editor::display_map::BlockContext;
use gpui::{AnyElement, App, Entity, SharedString};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownOptions, MarkdownStyle};
use ui::{
    Button, ButtonStyle, Color, FluentBuilder, IconName, IntoElement, Label, LabelSize, h_flex,
    prelude::*, v_flex,
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

    out.replace("<sub>", "")
        .replace("</sub>", "")
        .replace("<sup>", "")
        .replace("</sup>", "")
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
            .child(MarkdownElement::new(markdown_entity.clone(), style.clone()));

        for suggestion in suggestions {
            row = row.child(render_suggestion_block(suggestion, comment.id, cx));
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
