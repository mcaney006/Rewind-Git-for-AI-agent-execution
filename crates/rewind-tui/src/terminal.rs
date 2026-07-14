use std::collections::VecDeque;

use ratatui::style::{Color, Modifier, Style};

const MAX_RENDERED_LINES: usize = 5_000;

#[derive(Clone, Debug, Default)]
pub(crate) struct TerminalDocument {
    pub(crate) lines: Vec<StyledLine>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StyledLine {
    pub(crate) spans: Vec<StyledSpan>,
}

#[derive(Clone, Debug)]
pub(crate) struct StyledSpan {
    pub(crate) text: String,
    pub(crate) style: Style,
}

impl TerminalDocument {
    pub(crate) fn parse(bytes: &[u8], truncated: bool) -> Self {
        let mut parser = Parser::default();
        parser.consume(bytes);
        parser.finish(truncated)
    }

    #[cfg(test)]
    fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Default)]
struct Parser {
    lines: VecDeque<StyledLine>,
    current: StyledLine,
    style: Style,
    text: Vec<u8>,
    discarded_lines: bool,
}

impl Parser {
    fn consume(&mut self, bytes: &[u8]) {
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                0x1b => {
                    self.flush_text();
                    index = self.escape(bytes, index);
                }
                b'\n' => {
                    self.flush_text();
                    self.newline();
                    index += 1;
                }
                b'\r' => {
                    self.flush_text();
                    self.current.spans.clear();
                    index += 1;
                }
                0x08 => {
                    self.flush_text();
                    self.backspace();
                    index += 1;
                }
                b'\t' => {
                    self.text.extend_from_slice(b"    ");
                    index += 1;
                }
                0x00..=0x1f | 0x7f => index += 1,
                _ => {
                    self.text.push(bytes[index]);
                    index += 1;
                }
            }
        }
    }

    fn escape(&mut self, bytes: &[u8], start: usize) -> usize {
        let Some(kind) = bytes.get(start + 1).copied() else {
            return bytes.len();
        };
        match kind {
            b'[' => {
                let mut end = start + 2;
                while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                    end += 1;
                }
                if end == bytes.len() {
                    return end;
                }
                if bytes[end] == b'm' {
                    self.sgr(&bytes[start + 2..end]);
                }
                end + 1
            }
            b']' => skip_osc(bytes, start + 2),
            _ => (start + 2).min(bytes.len()),
        }
    }

    fn sgr(&mut self, parameters: &[u8]) {
        let parameters = if parameters.is_empty() {
            vec![0]
        } else {
            parameters
                .split(|byte| *byte == b';')
                .filter_map(|part| std::str::from_utf8(part).ok()?.parse::<u16>().ok())
                .collect::<Vec<_>>()
        };
        let mut index = 0;
        while index < parameters.len() {
            let value = parameters[index];
            match value {
                0 => self.style = Style::default(),
                1 => self.style = self.style.add_modifier(Modifier::BOLD),
                2 => self.style = self.style.add_modifier(Modifier::DIM),
                3 => self.style = self.style.add_modifier(Modifier::ITALIC),
                4 => self.style = self.style.add_modifier(Modifier::UNDERLINED),
                7 => self.style = self.style.add_modifier(Modifier::REVERSED),
                22 => self.style = self.style.remove_modifier(Modifier::BOLD | Modifier::DIM),
                23 => self.style = self.style.remove_modifier(Modifier::ITALIC),
                24 => self.style = self.style.remove_modifier(Modifier::UNDERLINED),
                27 => self.style = self.style.remove_modifier(Modifier::REVERSED),
                30..=37 => self.style = self.style.fg(base_color(value - 30, false)),
                39 => self.style = self.style.fg(Color::Reset),
                40..=47 => self.style = self.style.bg(base_color(value - 40, false)),
                49 => self.style = self.style.bg(Color::Reset),
                90..=97 => self.style = self.style.fg(base_color(value - 90, true)),
                100..=107 => self.style = self.style.bg(base_color(value - 100, true)),
                38 | 48 => {
                    if let Some((color, consumed)) = extended_color(&parameters[index + 1..]) {
                        self.style = if value == 38 {
                            self.style.fg(color)
                        } else {
                            self.style.bg(color)
                        };
                        index += consumed;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.text)
            .chars()
            .map(|character| {
                if character.is_control() {
                    '\u{fffd}'
                } else {
                    character
                }
            })
            .collect::<String>();
        self.text.clear();
        if let Some(last) = self.current.spans.last_mut()
            && last.style == self.style
        {
            last.text.push_str(&text);
        } else {
            self.current.spans.push(StyledSpan {
                text,
                style: self.style,
            });
        }
    }

    fn newline(&mut self) {
        self.lines.push_back(std::mem::take(&mut self.current));
        if self.lines.len() > MAX_RENDERED_LINES {
            self.lines.pop_front();
            self.discarded_lines = true;
        }
    }

    fn backspace(&mut self) {
        while let Some(last) = self.current.spans.last_mut() {
            if last.text.pop().is_some() {
                if last.text.is_empty() {
                    self.current.spans.pop();
                }
                break;
            }
            self.current.spans.pop();
        }
    }

    fn finish(mut self, truncated: bool) -> TerminalDocument {
        self.flush_text();
        self.lines.push_back(self.current);
        if self.lines.len() > MAX_RENDERED_LINES {
            self.lines.pop_front();
            self.discarded_lines = true;
        }
        TerminalDocument {
            lines: self.lines.into_iter().collect(),
            truncated: truncated || self.discarded_lines,
        }
    }
}

fn skip_osc(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        if bytes[index] == 0x07 {
            return index + 1;
        }
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
            return index + 2;
        }
        index += 1;
    }
    index
}

fn base_color(value: u16, bright: bool) -> Color {
    match (value, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => Color::Reset,
    }
}

fn extended_color(parameters: &[u16]) -> Option<(Color, usize)> {
    match parameters {
        [5, index, ..] if *index <= u16::from(u8::MAX) => Some((Color::Indexed(*index as u8), 2)),
        [2, red, green, blue, ..]
            if *red <= u16::from(u8::MAX)
                && *green <= u16::from(u8::MAX)
                && *blue <= u16::from(u8::MAX) =>
        {
            Some((Color::Rgb(*red as u8, *green as u8, *blue as u8), 4))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_color_and_removes_terminal_control_sequences() {
        let document = TerminalDocument::parse(
            b"plain \x1b[31mred\x1b[0m\nprogress 1\rprogress 2\x1b]8;;https://invalid\x07link",
            false,
        );
        assert_eq!(document.plain_text(), "plain red\nprogress 2link");
        assert_eq!(document.lines[0].spans[1].style.fg, Some(Color::Red));
        assert!(!document.plain_text().contains('\x1b'));
    }

    #[test]
    fn invalid_utf8_is_visible_without_panicking() {
        assert_eq!(
            TerminalDocument::parse(b"a\xffb", false).plain_text(),
            "a\u{fffd}b"
        );
    }

    #[test]
    fn rendered_line_history_is_bounded() {
        let document = TerminalDocument::parse("x\n".repeat(6_000).as_bytes(), false);
        assert_eq!(document.lines.len(), MAX_RENDERED_LINES);
        assert!(document.truncated);
    }
}
