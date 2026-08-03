//! The permission dialog: a centered modal blocking on the user's decision
//! about one pending tool call.
//!
//! Spec: upstream `packages/tui/src/routes/session/permission.tsx`, trimmed to
//! the one-shot shape [`ganja_core::PermissionReply`] offers today — no
//! "always" confirmation stage and no "reject with a message" stage, both of
//! which upstream's richer protocol supports and ganja's does not yet.

use ganja_core::PermissionId;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Widget as _, Wrap},
};

use crate::theme::Theme;

/// Lines of pretty-printed JSON shown before the rest is clamped.
const ARGS_PREVIEW_LINES: usize = 8;

/// A tool call waiting on the user's decision, and what to show about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Permission {
    id: PermissionId,
    tool: String,
    title: String,
    args: serde_json::Value,
}

impl Permission {
    /// Builds the dialog state for one `PermissionRequested` event.
    #[must_use]
    pub fn new(id: PermissionId, tool: String, title: String, args: serde_json::Value) -> Self {
        Self {
            id,
            tool,
            title,
            args,
        }
    }

    /// The request this dialog is showing, so a caller can tell whether an
    /// incoming `PermissionReplied` names it.
    #[must_use]
    pub fn id(&self) -> &PermissionId {
        &self.id
    }

    /// Draws the modal centered over `area`.
    pub fn render(&self, area: Rect, buffer: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(1, 76);
        let height = area.height.saturating_sub(2).clamp(1, 20);
        let popup = area.centered(Constraint::Length(width), Constraint::Length(height));

        Clear.render(popup, buffer);

        let mut lines = vec![
            Line::styled(format!("tool: {}", self.tool), theme.accent),
            Line::styled(self.title.clone(), theme.fg),
            Line::raw(""),
        ];
        lines.extend(self.args_preview(theme));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "[y] allow once   [a] always allow   [n]/[Esc] reject",
            theme.dim,
        ));

        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(" permission "))
            .style(theme.fg)
            .wrap(Wrap { trim: false })
            .render(popup, buffer);
    }

    /// The call's arguments, pretty-printed and clamped to a few lines: the
    /// dialog needs enough to recognize the call, not the whole payload.
    fn args_preview(&self, theme: &Theme) -> Vec<Line<'static>> {
        let pretty = serde_json::to_string_pretty(&self.args).unwrap_or_default();
        let mut shown: Vec<&str> = pretty.lines().collect();
        let clamped = shown.len() > ARGS_PREVIEW_LINES;
        shown.truncate(ARGS_PREVIEW_LINES);

        let mut preview: Vec<Line<'static>> = shown
            .into_iter()
            .map(|line| Line::styled(line.to_owned(), theme.dim))
            .collect();
        if clamped {
            preview.push(Line::styled("...".to_owned(), theme.dim));
        }

        preview
    }
}

#[cfg(test)]
mod tests {
    use ganja_core::PermissionId;
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::Permission;
    use crate::theme::Theme;

    fn permission() -> Permission {
        Permission::new(
            PermissionId::from("perm_1".to_owned()),
            "shell".to_owned(),
            "cargo test".to_owned(),
            serde_json::json!({"command": "cargo test"}),
        )
    }

    fn rendered(permission: &Permission, area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        permission.render(area, &mut buffer, &Theme::default());

        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_dialog_shows_the_tool_the_title_and_the_options() {
        let screen = rendered(&permission(), Rect::new(0, 0, 60, 18));

        assert!(screen.contains("shell"), "got:\n{screen}");
        assert!(screen.contains("cargo test"), "got:\n{screen}");
        assert!(screen.contains("allow once"), "got:\n{screen}");
        assert!(screen.contains("always allow"), "got:\n{screen}");
        assert!(screen.contains("reject"), "got:\n{screen}");
    }

    #[test]
    fn the_args_preview_is_pretty_printed_json() {
        let screen = rendered(&permission(), Rect::new(0, 0, 60, 18));

        assert!(screen.contains("\"command\""), "got:\n{screen}");
    }

    #[test]
    fn a_long_args_object_is_clamped_with_a_marker() {
        let mut object = serde_json::Map::new();
        for index in 0..40 {
            object.insert(format!("key_{index}"), serde_json::json!(index));
        }
        let permission = Permission::new(
            PermissionId::from("perm_1".to_owned()),
            "shell".to_owned(),
            "many args".to_owned(),
            serde_json::Value::Object(object),
        );

        let screen = rendered(&permission, Rect::new(0, 0, 60, 18));

        assert!(
            screen.contains("..."),
            "a clamped preview should say so:\n{screen}"
        );
    }

    #[test]
    fn a_zero_area_draws_nothing_and_does_not_panic() {
        rendered(&permission(), Rect::new(0, 0, 0, 0));
    }

    /// The dialog renders text the model chose: `title` is built from the tool
    /// call's own arguments. A title carrying a literal escape sequence must
    /// never reach the terminal, where it could clear the screen, move the
    /// cursor, or repaint the very prompt the user is about to answer — the one
    /// moment in the session where what is on screen has to be trustworthy.
    ///
    /// Nothing in this crate strips it. `ratatui-core` filters control
    /// characters twice, in `Span::styled_graphemes` (`text/span.rs`) and again
    /// in `Buffer::set_stringn` (`buffer/buffer.rs`), both as
    /// `filter(|g| !g.contains(char::is_control))`. That protection is
    /// inherited, so it would vanish silently the day a component writes to the
    /// backend or calls `Cell::set_symbol` itself. Pinned here so that day
    /// fails a test instead of shipping.
    #[test]
    fn an_escape_sequence_in_a_title_never_reaches_the_buffer() {
        let permission = Permission::new(
            PermissionId::from("perm_1".to_owned()),
            "shell".to_owned(),
            "\u{1b}[2J\u{1b}[31mrm -rf /\u{7}".to_owned(),
            serde_json::json!({ "command": "\u{1b}[2Jrm -rf /" }),
        );

        let screen = rendered(&permission, Rect::new(0, 0, 60, 18));

        // `rendered` joins rows with a newline of its own; any other control
        // character in the string got there from the dialog's own text.
        let leaked: Vec<char> = screen
            .chars()
            .filter(|character| *character != '\n' && character.is_control())
            .collect();

        assert!(
            leaked.is_empty(),
            "control characters reached the buffer: {leaked:?}\n{screen}"
        );
        // Without this the assertion above would also pass on a blank screen.
        assert!(
            screen.contains("rm -rf /"),
            "the printable remainder still has to render:\n{screen}"
        );
    }
}
