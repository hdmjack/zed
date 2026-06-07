use crate::review_provider::ReviewComment;
use gpui::{Entity, TextStyleRefinement, px};
use markdown::{HeadingLevelStyles, Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use ui::{Color, IntoElement, Label, LabelSize, div, h_flex, prelude::*, v_flex};

#[derive(IntoElement)]
pub struct CommentCard {
    comment: ReviewComment,
    /// The comment body, pre-parsed as markdown.
    body: Entity<Markdown>,
}

impl CommentCard {
    pub fn new(comment: ReviewComment, body: Entity<Markdown>) -> Self {
        Self { comment, body }
    }
}

/// A one-line, formatted preview of a comment body (markdown rendered, clipped
/// to a single line) for collapsed rows.
#[derive(IntoElement)]
pub struct CommentPreview {
    body: Entity<Markdown>,
}

impl CommentPreview {
    pub fn new(body: Entity<Markdown>) -> Self {
        Self { body }
    }
}

impl RenderOnce for CommentPreview {
    fn render(self, window: &mut gpui::Window, cx: &mut gpui::App) -> impl IntoElement {
        let mut style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
        style.base_text_style.color = cx.theme().colors().text_muted;
        // Skip the paragraph's bottom margin / tall line-height so the single
        // line sits flush in the clipped row.
        style.height_is_multiple_of_line_height = true;
        div()
            .h(px(18.0))
            .min_w_0()
            .overflow_hidden()
            .text_size(px(12.0))
            .child(MarkdownElement::new(self.body, style))
    }
}

impl RenderOnce for CommentCard {
    fn render(self, window: &mut gpui::Window, cx: &mut gpui::App) -> impl IntoElement {
        let is_reply = self.comment.reply_to.is_some();
        let mut markdown_style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
        // Comments render denser than editor body text, with restrained headings
        // so bot comments (which lean on markdown headings heavily) stay compact.
        markdown_style.base_text_style.font_size = px(11.0).into();
        markdown_style.base_text_style.line_height = px(16.0).into();
        markdown_style.inline_code.font_size = Some(px(11.0).into());
        let heading = |size: f32| TextStyleRefinement {
            font_size: Some(px(size).into()),
            line_height: Some(px(size + 4.0).into()),
            ..Default::default()
        };
        markdown_style.heading_level_styles = Some(HeadingLevelStyles {
            h1: Some(heading(13.0)),
            h2: Some(heading(12.0)),
            h3: Some(heading(12.0)),
            h4: Some(heading(11.0)),
            h5: Some(heading(11.0)),
            h6: Some(heading(11.0)),
        });

        let mut card = v_flex()
            .w_full()
            // Allow the card to shrink below its content's intrinsic width so long
            // URLs / code spans wrap instead of overflowing the panel. Horizontal
            // placement is owned by the call site so margins stay symmetric.
            .min_w_0()
            .mb_1()
            .p_2()
            .gap_1()
            .overflow_x_hidden()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .when(is_reply, |el| {
                el.ml_4()
                    .border_l_2()
                    .border_color(cx.theme().colors().border_focused)
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(self.comment.author.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Default),
                    )
                    .when_some(self.comment.line, |el, line| {
                        el.child(
                            Label::new(format!("L{}", line))
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    })
                    .child(
                        Label::new(self.comment.created_at.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );

        if let Some(hunk) = &self.comment.diff_hunk {
            card = card.child(
                v_flex()
                    .rounded_md()
                    .bg(cx.theme().colors().surface_background)
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .overflow_x_hidden()
                    .py_1()
                    .children(hunk.lines().map(|line| {
                        let (line_color, line_bg) = if line.starts_with('+') {
                            (Color::Created, Some(cx.theme().status().created.alpha(0.1)))
                        } else if line.starts_with('-') {
                            (Color::Deleted, Some(cx.theme().status().deleted.alpha(0.1)))
                        } else if line.starts_with("@@") {
                            (Color::Muted, None)
                        } else {
                            (Color::Default, None)
                        };
                        let mut row = div().px_2().child(
                            Label::new(line.to_string())
                                .size(LabelSize::XSmall)
                                .color(line_color),
                        );
                        if let Some(bg) = line_bg {
                            row = row.bg(bg);
                        }
                        row
                    })),
            );
        }

        card = card.child(
            div()
                .w_full()
                .min_w_0()
                .overflow_x_hidden()
                // Markdown text runs inherit the ambient text size (TextRun carries
                // no font size of its own), so set it on the container.
                .text_size(px(12.0))
                .child(MarkdownElement::new(self.body, markdown_style)),
        );

        if let Some(bar) = crate::inline_comment::reaction_bar(&self.comment, "card", cx) {
            card = card.child(bar);
        }

        card
    }
}
