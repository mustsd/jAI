//! The screen: layout, wrapping and scrolling.
//!
//! Split out from [`crate::tui`] so it can be tested without a terminal — the
//! tests in this module draw into ratatui's `TestBackend` and assert on the
//! resulting buffer, which is the only reliable way to catch "the newest message
//! is off the bottom of the screen" bugs.
//!
//! ## Why jAI wraps text itself
//!
//! `Paragraph::scroll` scrolls by *rendered* lines, so the caller has to know how
//! many rendered lines the text produces. Letting `Paragraph` wrap and asking it
//! for `line_count()` is fragile: the count depends on the wrap mode, the gutter
//! is included in the width, and getting it wrong silently pushes the newest
//! lines past the bottom of the screen. jAI therefore wraps to exactly the width
//! it will render, and scrolls by its own line count — the arithmetic cannot
//! disagree with the drawing.

use crate::transcript::{Kind, Transcript};
use jai_core::usage::UsageLine;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Columns reserved for the speaker label (`You`, `jAI`, `tool`, ...).
pub const GUTTER: usize = 9;

/// How long a transient message stays on screen.
const MESSAGE_TTL: Duration = Duration::from_secs(5);

/// Rows the idle footer needs: a rule separating it from the transcript, then
/// the input line itself.
///
/// Both rows are required. The input line used to be given whatever was left
/// over after the rule, which was nothing — the row the user types into was
/// allocated zero height and never drawn.
const INPUT_ROWS: u16 = 2;

/// Rows a confirmation needs beyond its detail lines: the question and the
/// `y = allow, n/Esc = deny` hint.
const PROMPT_EXTRA_ROWS: usize = 2;

/// What the user is currently being asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub question: String,
    /// Extra lines describing what would happen.
    pub details: Vec<String>,
}

/// Everything the screen needs, independent of the engine.
pub struct View {
    pub transcript: Transcript,
    /// Provider and model shown in the header.
    pub title: String,
    /// Mode label shown in the header.
    pub mode: String,
    pub input: String,
    /// Cursor position inside `input`, in characters.
    pub cursor: usize,
    pub prompt: Option<Prompt>,
    /// Transient hint shown next to the prompt.
    pub message: Option<(String, Instant)>,
    /// Lines scrolled up from the bottom; 0 means "newest line is visible".
    pub scroll: u16,
    /// Rendered line count of the last draw, for tests and scroll clamping.
    last_total: usize,
    last_visible: usize,
}

impl View {
    pub fn new(transcript: Transcript, title: impl Into<String>, mode: impl Into<String>) -> Self {
        Self {
            transcript,
            title: title.into(),
            mode: mode.into(),
            input: String::new(),
            cursor: 0,
            prompt: None,
            message: None,
            scroll: 0,
            last_total: 0,
            last_visible: 0,
        }
    }

    /// Total rendered lines of the transcript (set by the last [`View::draw`]).
    pub fn rendered_lines(&self) -> usize {
        self.last_total
    }

    /// Lines of the transcript that fit on screen (set by the last draw).
    pub fn visible_lines(&self) -> usize {
        self.last_visible
    }

    /// Scroll up by `delta` lines.
    pub fn scroll_up(&mut self, delta: u16) {
        self.scroll = self.scroll.saturating_add(delta);
    }

    /// Scroll down by `delta` lines, stopping at the newest line.
    pub fn scroll_down(&mut self, delta: u16) {
        self.scroll = self.scroll.saturating_sub(delta);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    pub fn set_message(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now()));
    }

    /// Insert a character at the cursor.
    pub fn insert_char(&mut self, ch: char) {
        let mut chars: Vec<char> = self.input.chars().collect();
        let at = self.cursor.min(chars.len());
        chars.insert(at, ch);
        self.cursor = at + 1;
        self.input = chars.into_iter().collect();
        // Typing is a request to look at the newest output.
        self.scroll_to_bottom();
    }

    pub fn delete_before_cursor(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut chars: Vec<char> = self.input.chars().collect();
        let at = self.cursor.min(chars.len());
        chars.remove(at - 1);
        self.cursor = at - 1;
        self.input = chars.into_iter().collect();
    }

    pub fn delete_at_cursor(&mut self) {
        let mut chars: Vec<char> = self.input.chars().collect();
        let at = self.cursor.min(chars.len());
        if at < chars.len() {
            chars.remove(at);
            self.input = chars.into_iter().collect();
        }
    }

    /// Lines of the transcript, already wrapped to `width`.
    pub fn body_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let body = width.saturating_sub(GUTTER).max(1);
        let show_thinking = self.transcript.show_thinking();
        let mut lines = Vec::new();
        for entry in self.transcript.entries() {
            if entry.kind == Kind::Reasoning && !show_thinking {
                continue;
            }
            let style = style_for(entry.kind);
            let mut first = true;
            for source in entry.text.split('\n') {
                for chunk in wrap(source, body) {
                    let gutter = if first {
                        format!("{:>width$} ", entry.kind.label(), width = GUTTER - 1)
                    } else {
                        " ".repeat(GUTTER)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(gutter, Style::default().fg(Color::DarkGray)),
                        Span::styled(chunk, style),
                    ]));
                    first = false;
                }
                if first {
                    // An empty source line still occupies one row.
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{:>width$} ", entry.kind.label(), width = GUTTER - 1),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(String::new(), style),
                    ]));
                    first = false;
                }
            }
            if !entry.kind.is_meta() && entry.kind != Kind::Reasoning {
                lines.push(Line::from(""));
            }
        }
        if self.transcript.is_running() && !self.transcript.entries().is_empty() {
            lines.push(Line::from(Span::styled(
                format!("{}· working…", " ".repeat(GUTTER)),
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines
    }

    /// Rows the footer needs for what is currently on screen.
    fn footer_height(&self) -> u16 {
        match &self.prompt {
            Some(prompt) => match prompt.details.len() + PROMPT_EXTRA_ROWS {
                needed if needed > u16::MAX as usize => u16::MAX,
                needed => needed as u16,
            },
            None => INPUT_ROWS,
        }
    }

    /// Draw the whole screen and update the scroll bookkeeping.
    ///
    /// Rows are handed out by hand, in priority order, instead of by `Layout` +
    /// `Constraint`: the input line and the status line must never be squeezed
    /// out by a transcript that fills the screen, and a manual layout is the only
    /// way to guarantee that (and to test it).
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.size();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let header = Rect::new(area.x, area.y, area.width, 1.min(area.height));
        let free = area.height - header.height;
        let (status_height, footer_height) = self.footer_allocation(free);
        let body = Rect::new(
            area.x,
            header.bottom(),
            area.width,
            free - status_height - footer_height,
        );
        let status = Rect::new(area.x, body.bottom(), area.width, status_height);
        let footer = Rect::new(area.x, status.bottom(), area.width, footer_height);

        self.draw_header(frame, header);
        self.draw_body(frame, body);
        self.draw_status(frame, status);
        self.draw_footer(frame, footer);
    }

    /// How many rows the status line and the footer get out of `free`.
    ///
    /// An idle footer needs two rows (rule + input) and a confirmation needs one
    /// per detail plus the question and the key hint. On a cramped terminal the
    /// ranking is: the question the keyboard is waiting on, then the input line,
    /// then the status line, and the separating rule only when there is a row to
    /// spare.
    fn footer_allocation(&self, free: u16) -> (u16, u16) {
        if free == 0 {
            return (0, 0);
        }
        if self.prompt.is_some() {
            let needed = self.footer_height().min(free);
            // A confirmation already uses every row it can get; otherwise the
            // status line keeps its own row.
            let status = u16::from(free > needed);
            (status, needed.min(free - status))
        } else {
            let input = u16::from(free > 0);
            let rule = u16::from(free >= 3);
            let status = u16::from(free >= 2);
            (status, input + rule)
        }
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let hint = "Ctrl-C/Esc cancel · Ctrl-R thinking · Ctrl-D quit";
        let room = area.width as usize;
        let left = format!(" jAI {} · {} ", self.title, self.mode);
        // Only show the key hints when they fit next to the title; a wrapped
        // header would eat a transcript row.
        let show_hint = UnicodeWidthStr::width(left.as_str()) + UnicodeWidthStr::width(hint) < room;
        // Either way the header is one row: a long provider/model pair is cut
        // rather than allowed to wrap.
        let title = if show_hint {
            left
        } else {
            shorten(&left, room)
        };
        let mut spans = vec![Span::styled(title, Style::default().fg(Color::Cyan))];
        if show_hint {
            spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_body(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let lines = self.body_lines(area.width);
        self.last_total = lines.len();
        let visible = area.height as usize;
        self.last_visible = visible;
        if visible == 0 {
            return;
        }
        let max_scroll = self.last_total.saturating_sub(visible);
        self.scroll = self.scroll.min(max_scroll as u16);
        // `scroll` counts up from the newest line, ratatui counts down from the
        // oldest, so the offset is what remains above the visible window.
        let offset = (max_scroll - self.scroll as usize) as u16;

        if lines.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " ask something — /help lists the commands",
                    Style::default().fg(Color::DarkGray),
                ))),
                area,
            );
            return;
        }
        frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), area);
    }

    fn draw_status(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let UsageLine {
            task,
            context,
            elapsed,
        } = self.transcript.usage_line();
        let dim = Style::default().fg(Color::DarkGray);
        let mut spans: Vec<Span<'static>> = Vec::new();
        if let Some(task) = task {
            spans.push(Span::styled(
                format!(
                    " Tokens: {}{}",
                    if task.estimated { "~" } else { "" },
                    jai_core::usage::format_count(task.total_tokens)
                ),
                dim,
            ));
        }
        if let Some(context) = context {
            let style = if context.is_near_limit() {
                Style::default().fg(Color::Red)
            } else {
                dim
            };
            if !spans.is_empty() {
                spans.push(Span::styled(" | ", dim));
            } else {
                spans.push(Span::styled(" ", dim));
            }
            spans.push(Span::styled(format!("Cxt: {}", context.render()), style));
        }
        if let Some(elapsed) = elapsed {
            if spans.is_empty() {
                spans.push(Span::styled(" ", dim));
            } else {
                spans.push(Span::styled(" | ", dim));
            }
            spans.push(Span::styled(
                format!("Time Used: {}", jai_core::usage::format_duration(elapsed)),
                dim,
            ));
        }
        if self.scroll > 0 {
            if !spans.is_empty() {
                spans.push(Span::styled(" | ", dim));
            }
            spans.push(Span::styled(
                format!("scrolled ↑{}", self.scroll),
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_footer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let Some(prompt) = self.prompt.clone() else {
            self.draw_input(frame, area);
            return;
        };
        self.draw_prompt(frame, area, &prompt);
    }

    /// A confirmation: details, then the question, then the key hint.
    fn draw_prompt(&mut self, frame: &mut Frame<'_>, area: Rect, prompt: &Prompt) {
        let mut lines: Vec<Line<'static>> = prompt
            .details
            .iter()
            .map(|detail| {
                Line::from(Span::styled(
                    shorten(detail, area.width as usize),
                    Style::default().fg(Color::DarkGray),
                ))
            })
            .collect();
        lines.push(Line::from(vec![
            Span::styled(
                shorten(&prompt.question, area.width.saturating_sub(6) as usize),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(" [y/N]", Style::default().add_modifier(Modifier::BOLD)),
        ]));

        // The question is the point of the prompt: it is the last line, and the
        // rows above it are what gets dropped when the details do not fit.
        let hint_rows = u16::from(area.height >= 2);
        let question_rows = area.height - hint_rows;
        let start = lines.len().saturating_sub(question_rows as usize);
        frame.render_widget(
            Paragraph::new(lines[start..].to_vec()),
            Rect::new(area.x, area.y, area.width, question_rows),
        );
        if hint_rows == 1 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " y = allow, n/Esc = deny",
                    Style::default().fg(Color::DarkGray),
                ))),
                Rect::new(
                    area.x,
                    area.y + question_rows,
                    area.width,
                    hint_rows,
                ),
            );
        }
    }

    /// The idle footer: a rule, then the line the user types into.
    fn draw_input(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // In a one-row footer the prompt line wins over the rule.
        let (rule, prompt_row) = if area.height >= INPUT_ROWS {
            (
                Some(Rect::new(area.x, area.y, area.width, 1)),
                Rect::new(area.x, area.y + 1, area.width, 1),
            )
        } else {
            (None, Rect::new(area.x, area.y, area.width, area.height))
        };
        if let Some(rule) = rule {
            let dim = Style::default().fg(Color::DarkGray);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(rule.width as usize),
                    dim,
                ))),
                rule,
            );
        }

        // Long input scrolls horizontally so the cursor stays visible.
        let room = prompt_row.width.saturating_sub(6) as usize;
        let cursor = self.cursor;
        let chars: Vec<char> = self.input.chars().collect();
        let start = caret_offset(&chars, cursor, room);
        let shown: String = chars[start..].iter().take(room.max(1)).collect();

        let mut spans = vec![
            Span::styled("You> ", Style::default().fg(Color::Green)),
            Span::raw(shown),
        ];
        if let Some((message, at)) = &self.message {
            if at.elapsed() < MESSAGE_TTL {
                let text = format!("{message} ");
                let used = 5 + (cursor - start) + 1;
                if (used as u16) < prompt_row.width {
                    spans.push(Span::styled(
                        " ".repeat((prompt_row.width as usize).saturating_sub(text.len() + used)),
                        Style::default(),
                    ));
                    spans.push(Span::styled(text, Style::default().fg(Color::DarkGray)));
                }
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), prompt_row);

        let x = prompt_row.x + 5 + (cursor - start) as u16;
        let max_x = prompt_row.x + prompt_row.width.saturating_sub(1);
        frame.set_cursor(x.min(max_x), prompt_row.y);
    }

    /// Answer the pending prompt; the caller applies the decision.
    pub fn take_prompt(&mut self) -> Option<Prompt> {
        self.prompt.take()
    }
}

fn style_for(kind: Kind) -> Style {
    match kind {
        Kind::User => Style::default().fg(Color::Green),
        Kind::Assistant => Style::default(),
        Kind::Reasoning => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        Kind::Tool => Style::default().fg(Color::Blue),
        Kind::Notice => Style::default().fg(Color::DarkGray),
        Kind::Error => Style::default().fg(Color::Red),
    }
}

/// Wrap `text` to at most `width` display columns.
///
/// Breaks on whitespace when there is any, and inside a word when there is not,
/// so a long URL or a base64 blob cannot run off the edge. Never returns an empty
/// vector: a blank source line is one blank rendered line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    // `pending_space` defers a break opportunity: trailing whitespace is dropped
    // at a break instead of starting the next line with it.
    let mut pending_space = false;

    for word in split_words(text) {
        let word_width = UnicodeWidthStr::width(word.as_str());
        if word.trim().is_empty() {
            if !current.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if current_width > 0 && current_width + usize::from(pending_space) + word_width > width {
            out.push(std::mem::take(&mut current));
            current_width = 0;
            pending_space = false;
        }
        if pending_space && !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        pending_space = false;
        // A word wider than the line is broken across lines.
        let mut rest = word.as_str();
        while !rest.is_empty() {
            let room = width.saturating_sub(current_width).max(1);
            let taken = take_width(rest, room);
            current.push_str(taken);
            current_width += UnicodeWidthStr::width(taken);
            rest = &rest[taken.len()..];
            if !rest.is_empty() {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Split on spaces and tabs while keeping the separators, so `wrap` can decide
/// where a space is worth keeping.
fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_space = false;
    for ch in text.chars() {
        let space = ch == ' ' || ch == '\t';
        if space != in_space && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        in_space = space;
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Longest prefix of `text` that fits in `width` display columns.
fn take_width(text: &str, width: usize) -> &str {
    let mut used = 0usize;
    for (index, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > width {
            return &text[..index];
        }
        used += ch_width;
        if used == width {
            return &text[..index + ch.len_utf8()];
        }
    }
    text
}

/// First character index to show so that `cursor` stays inside `room` columns.
fn caret_offset(chars: &[char], cursor: usize, room: usize) -> usize {
    let cursor = cursor.min(chars.len());
    let room = room.max(1);
    let mut used = 0usize;
    let mut start = cursor;
    while start > 0 {
        let ch_width = chars[start - 1].width().unwrap_or(0);
        if used + ch_width > room {
            break;
        }
        used += ch_width;
        start -= 1;
    }
    start
}

/// Truncate `text` to fit `width` columns, adding an ellipsis when cut.
fn shorten(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let taken = take_width(text, width.saturating_sub(1));
    format!("{taken}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use jai_core::ContextUsage;
    use jai_core::engine::EngineEvent;
    use jai_core::message::TokenUsage;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn buffer_lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn render(view: &mut View, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        buffer_lines(&terminal)
    }

    fn view() -> View {
        let mut transcript = Transcript::new(128_000, true);
        transcript.push_user("hello");
        View::new(transcript, "deepseek / deepseek-chat", "safe")
    }

    /// Display width of one rendered row.
    ///
    /// ratatui stores a double-width grapheme in one cell and blanks in the cell
    /// it covers, so summing cell widths over-counts; a wide symbol means its
    /// following cell is a placeholder and must not be counted again.
    fn row_width(row: &str) -> usize {
        let mut used = 0usize;
        let mut skip = 0usize;
        for ch in row.chars() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let width = ch.width().unwrap_or(0);
            used += width;
            skip = width.saturating_sub(1);
        }
        used
    }

    /// Everything drawn on screen, with whitespace removed — so a message that
    /// wrapped across rows can be matched as one string.
    fn visible_text(lines: &[String]) -> String {
        lines
            .iter()
            .flat_map(|line| line.chars())
            .filter(|ch| !ch.is_whitespace())
            .collect()
    }

    /// Index of the last transcript row (the row above the status line).
    fn body_last_row(height: u16) -> usize {
        height as usize - 1 /* header */ - 1 /* status */ - INPUT_ROWS as usize
    }

    #[test]
    fn wrapping_breaks_on_spaces_and_long_words() {
        assert_eq!(wrap("", 10), vec![""]);
        assert_eq!(wrap("short", 10), vec!["short"]);
        assert_eq!(wrap("one two three", 8), vec!["one two", "three"]);
        // A single long token is hard-broken.
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a bbbbbbbbbb c", 5), vec!["a", "bbbbb", "bbbbb", "c"]);
        // Trailing spaces never start a line.
        assert_eq!(wrap("ab   cd", 2), vec!["ab", "cd"]);
    }

    #[test]
    fn wrapping_counts_wide_characters_as_two_columns() {
        // Six CJK characters are twelve columns: two lines of four characters.
        assert_eq!(wrap("你好世界你好", 8), vec!["你好世界", "你好"]);
        // Mixed content still fits the budget.
        for line in wrap("实现一个 agent 的运行", 10) {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 10, "{line:?}");
        }
    }

    #[test]
    fn the_newest_message_is_visible_at_the_bottom() {
        let mut view = view();
        for index in 0..40 {
            view.transcript.push_user(&format!("message {index}"));
        }
        let lines = render(&mut view, 40, 12);
        // The bottom of the screen shows the newest message, not the oldest.
        assert!(
            lines.iter().any(|line| line.contains("message 39")),
            "newest line missing:\n{}",
            lines.join("\n")
        );
        assert!(
            !lines.iter().any(|line| line.contains("message 0 ")),
            "oldest line should have scrolled away:\n{}",
            lines.join("\n")
        );
        assert_eq!(view.scroll, 0);
    }

    #[test]
    fn a_tall_transcript_ends_exactly_on_the_newest_line() {
        let mut view = view();
        for index in 0..60 {
            view.transcript.push_user(&format!("line {index}"));
        }
        let height = 10;
        let lines = render(&mut view, 30, height);
        // The newest line is the last row of the body: the transcript fills its
        // area exactly, with no gap left above the status line.
        let last = body_last_row(height);
        assert!(
            lines[last].contains("line 59"),
            "expected the newest line on the last body row, got:\n{}",
            lines.join("\n")
        );
        // And nothing transcript-shaped leaked into the status or footer rows.
        for line in &lines[last + 1..] {
            assert!(
                !line.contains("line "),
                "transcript content below the body: {line:?}"
            );
        }
    }

    #[test]
    fn wrapped_lines_do_not_hide_the_end_of_the_transcript() {
        // Long paragraphs wrap into many rendered lines; scrolling must be
        // counted in those rendered lines.
        let mut view = view();
        for index in 0..8 {
            let body = format!("{index} {}", "word ".repeat(60));
            view.transcript.push_user(&body.trim());
        }
        view.transcript.push_user("THE VERY LAST MESSAGE");
        let lines = render(&mut view, 50, 14);
        assert!(
            lines.iter().any(|line| line.contains("THE VERY LAST MESSAGE")),
            "the end of a wrapped transcript must be visible:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn scrolling_up_shows_older_lines_and_scrolling_back_returns() {
        let mut view = view();
        for index in 0..40 {
            view.transcript.push_user(&format!("message {index}"));
        }
        render(&mut view, 40, 12);
        let total = view.rendered_lines();
        let visible = view.visible_lines();
        assert!(total > visible, "the transcript must overflow");

        view.scroll_up(5);
        let lines = render(&mut view, 40, 12);
        assert!(lines.iter().any(|line| line.contains("scrolled")));
        assert!(
            !lines.iter().any(|line| line.contains("message 39")),
            "scrolling up must move away from the newest line"
        );

        view.scroll_to_bottom();
        let lines = render(&mut view, 40, 12);
        assert!(lines.iter().any(|line| line.contains("message 39")));
    }

    #[test]
    fn the_scroll_offset_cannot_go_past_the_oldest_line() {
        let mut view = view();
        for index in 0..5 {
            view.transcript.push_user(&format!("message {index}"));
        }
        render(&mut view, 40, 12);
        view.scroll_up(1000);
        let lines = render(&mut view, 40, 12);
        assert!(
            lines.iter().any(|line| line.contains("message 0")),
            "the oldest line must be reachable:\n{}",
            lines.join("\n")
        );
        assert_eq!(view.scroll as usize, view.rendered_lines() - view.visible_lines());
    }

    #[test]
    fn the_status_line_matches_the_documented_shape() {
        let mut view = view();
        view.transcript.apply(&EngineEvent::TurnStarted {
            session_id: "s".into(),
        });
        view.transcript.apply(&EngineEvent::Usage {
            usage: TokenUsage {
                prompt_tokens: 12_000,
                completion_tokens: 400,
                total_tokens: 12_400,
            },
        });
        view.transcript.apply(&EngineEvent::Context {
            usage: ContextUsage::new(345_000, 1_000_000, false),
        });
        let lines = render(&mut view, 120, 10);
        let status = lines
            .iter()
            .find(|line| line.contains("Tokens:"))
            .expect("a status line");
        assert!(status.contains("Tokens: 12,400"), "{status}");
        assert!(status.contains("Cxt: 345,000 / 1,000,000 (34.5%)"), "{status}");
        assert!(status.contains("Time Used:"), "{status}");
    }

    #[test]
    fn the_input_line_is_the_last_row_and_shows_the_cursor() {
        let mut view = view();
        view.input = "hello".to_string();
        view.cursor = 5;
        let lines = render(&mut view, 30, 8);
        let last = lines.last().expect("a last row");
        assert!(last.starts_with("You> hello"), "{last:?}");
    }

    /// Regression: the input line was handed whatever was left after the
    /// separating rule, which was nothing — the row the user types into was
    /// allocated zero height and never appeared at all.
    #[test]
    fn the_input_line_is_visible_even_when_the_transcript_fills_the_screen() {
        for height in [2u16, 3, 4, 8, 24] {
            let mut view = view();
            for index in 0..200 {
                view.transcript.push_user(&format!("a very long message {index}"));
            }
            view.input = "typed".to_string();
            view.cursor = 5;
            let lines = render(&mut view, 40, height);
            let prompt_row = lines
                .iter()
                .find(|line| line.contains("You>"))
                .unwrap_or_else(|| {
                    panic!(
                        "no input row at height {height}:\n{}",
                        lines.join("\n")
                    )
                });
            assert!(prompt_row.contains("typed"), "{prompt_row:?}");
        }
    }

    /// Regression: the status line had no room of its own and could be pushed
    /// off screen entirely.
    #[test]
    fn the_status_line_is_visible_even_on_a_short_terminal() {
        let mut view = view();
        for index in 0..200 {
            view.transcript.push_user(&format!("filler {index}"));
        }
        for height in [3u16, 4, 24] {
            let lines = render(&mut view, 40, height);
            assert!(
                lines.iter().any(|line| line.contains("Cxt:")),
                "no status line at height {height}:\n{}",
                lines.join("\n")
            );
        }
        // At two rows there is only room for the header and the prompt, and the
        // prompt is the one that must survive.
        let lines = render(&mut view, 40, 2);
        assert!(lines.iter().any(|line| line.contains("You>")));
    }

    /// The status line is populated from the moment the agent starts, so the
    /// figure is never a blank row.
    #[test]
    fn the_context_figure_is_shown_before_the_first_turn() {
        let mut transcript = Transcript::new(128_000, true);
        transcript.apply(&EngineEvent::Context {
            usage: ContextUsage::estimate(0, 128_000),
        });
        let mut view = View::new(transcript, "p / m", "safe");
        let lines = render(&mut view, 60, 8);
        assert!(
            lines.iter().any(|line| line.contains("Cxt: ~0 / 128,000")),
            "{}",
            lines.join("\n")
        );
    }

    /// A confirmation must never be clipped away either: the question is what
    /// the keyboard is waiting on.
    #[test]
    fn a_long_confirmation_keeps_its_question_on_screen() {
        let mut view = view();
        view.prompt = Some(Prompt {
            question: "run shell_execute?".to_string(),
            details: (0..20).map(|i| format!("detail {i}")).collect(),
        });
        let lines = render(&mut view, 50, 10);
        assert!(
            lines.iter().any(|line| line.contains("run shell_execute?")),
            "{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|line| line.contains("[y/N]")),
            "{}",
            lines.join("\n")
        );
        assert!(!lines.iter().any(|line| line.contains("You>")));
    }

    #[test]
    fn long_input_scrolls_horizontally_so_the_cursor_stays_visible() {
        let mut view = view();
        for _ in 0..40 {
            view.insert_char('x');
        }
        let lines = render(&mut view, 20, 8);
        let last = lines.last().unwrap();
        assert!(last.starts_with("You> "), "{last:?}");
        assert_eq!(last.chars().filter(|ch| *ch == 'x').count(), 14);
    }

    #[test]
    fn a_confirmation_replaces_the_input_line_and_shows_the_details() {
        let mut view = view();
        view.prompt = Some(Prompt {
            question: "run shell_execute (execute)?".to_string(),
            details: vec![
                "summary:   run `cargo test`".to_string(),
                "directory: /work/project".to_string(),
            ],
        });
        let lines = render(&mut view, 60, 10);
        assert!(
            lines.iter().any(|line| line.contains("run `cargo test`")),
            "{}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|line| line.contains("[y/N]")),
            "{}",
            lines.join("\n")
        );
        assert!(!lines.iter().any(|line| line.contains("You>")));
    }

    #[test]
    fn reasoning_is_hidden_until_it_is_enabled() {
        let mut view = view();
        view.transcript.set_show_thinking(false);
        view.transcript.apply(&EngineEvent::ReasoningDelta {
            text: "secret thinking".into(),
        });
        let lines = render(&mut view, 40, 10);
        assert!(!lines.iter().any(|line| line.contains("secret")));

        view.transcript.set_show_thinking(true);
        view.transcript.apply(&EngineEvent::ReasoningDelta {
            text: "visible thinking".into(),
        });
        let lines = render(&mut view, 40, 10);
        assert!(
            lines.iter().any(|line| line.contains("visible thinking")),
            "{}",
            lines.join("\n")
        );
    }

    #[test]
    fn wide_characters_do_not_overflow_the_body_width() {
        let mut view = view();
        let text = "实现一个智能体".repeat(6);
        view.transcript.push_user(&text);
        let lines = render(&mut view, 32, 12);
        for line in &lines {
            assert!(row_width(line) <= 32, "{line:?} is too wide");
        }
        // Nothing was clipped or split down the middle of a character: the whole
        // message is still on screen.
        assert!(
            visible_text(&lines).contains(&text),
            "the message was clipped:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn a_tiny_terminal_does_not_panic() {
        let mut view = view();
        view.transcript.push_user("hello");
        view.prompt = Some(Prompt {
            question: "run it?".to_string(),
            details: vec!["a long detail line that will not fit".to_string()],
        });
        for (width, height) in [(1u16, 1u16), (2, 3), (5, 2), (10, 40)] {
            let _ = render(&mut view, width, height);
        }
    }

    #[test]
    fn an_empty_transcript_shows_a_hint() {
        let mut transcript = Transcript::new(128_000, true);
        transcript.reset();
        let mut view = View::new(transcript, "p / m", "safe");
        let lines = render(&mut view, 40, 8);
        assert!(
            lines.iter().any(|line| line.contains("/help")),
            "{}",
            lines.join("\n")
        );
    }

    #[test]
    fn the_newest_entry_is_visible_while_it_streams() {
        let mut view = view();
        for index in 0..30 {
            view.transcript.push_notice(&format!("note {index}"));
        }
        view.transcript.apply(&EngineEvent::TurnStarted {
            session_id: "s".into(),
        });
        for chunk in ["streaming ", "answer ", "in ", "progress"] {
            view.transcript.apply(&EngineEvent::Delta {
                text: chunk.to_string(),
            });
        }
        let lines = render(&mut view, 40, 12);
        assert!(
            lines.iter().any(|line| line.contains("progress")),
            "the streaming tail must stay on screen:\n{}",
            lines.join("\n")
        );
    }
}
