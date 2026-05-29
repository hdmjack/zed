use crate::review_provider::ReviewComment;
use ui::{Color, IntoElement, Label, LabelSize, div, h_flex, prelude::*, v_flex};

#[derive(IntoElement)]
pub struct CommentCard {
    comment: ReviewComment,
}

impl CommentCard {
    pub fn new(comment: ReviewComment) -> Self {
        Self { comment }
    }
}

impl RenderOnce for CommentCard {
    fn render(self, _window: &mut gpui::Window, cx: &mut gpui::App) -> impl IntoElement {
        let is_reply = self.comment.reply_to.is_some();

        let mut card = v_flex()
            .mx_2()
            .mb_1()
            .p_2()
            .gap_1()
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
            Label::new(self.comment.body)
                .size(LabelSize::XSmall)
                .color(Color::Default),
        );

        card
    }
}
