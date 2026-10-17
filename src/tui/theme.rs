//! The colors of the view. The selected row gets a background a little
//! lighter than a dark terminal's, or a little darker than a light one's,
//! so its text keeps its own colors and stays readable. The terminal says
//! its background when asked (OSC 11); one that does not answer gets no
//! background, only the mark and bold.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};

/// How long to wait for the terminal's answer. A local terminal answers in
/// well under a millisecond; the device attributes asked after it end the
/// wait as soon as they arrive, so this only runs out on a terminal that
/// answers neither.
const ANSWER: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub color: bool,
    /// The selected row's background, when the terminal's is known.
    pub selection: Option<Color>,
}

impl Theme {
    pub const PLAIN: Theme = Theme {
        color: false,
        selection: None,
    };

    /// Asks the terminal on stdin and stdout for its background. Call it in
    /// raw mode, before anything else reads the terminal.
    pub fn detect(color: bool) -> Self {
        if !color {
            return Self::PLAIN;
        }
        let truecolor =
            std::env::var("COLORTERM").is_ok_and(|v| matches!(v.as_str(), "truecolor" | "24bit"));
        Self {
            color,
            selection: background().map(|bg| selection(bg, truecolor)),
        }
    }

    pub fn fg(&self, color: Color) -> Style {
        if self.color {
            Style::new().fg(color)
        } else {
            Style::new()
        }
    }

    pub fn accent(&self) -> Style {
        self.fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }

    /// Bold on the selection's background; the text keeps its colors.
    pub fn selected(&self) -> Style {
        let style = Style::new().add_modifier(Modifier::BOLD);
        match self.selection {
            Some(bg) => style.bg(bg),
            None => style,
        }
    }
}

/// A background a step away from `bg`: lighter on a dark one, darker on a
/// light one.
fn selection((r, g, b): (u8, u8, u8), truecolor: bool) -> Color {
    let luma = 0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b);
    let (toward, amount) = if luma < 128.0 {
        (255.0, 0.14)
    } else {
        (0.0, 0.09)
    };
    let mix = |c: u8| (f64::from(c) + (toward - f64::from(c)) * amount).round() as u8;
    let (r, g, b) = (mix(r), mix(g), mix(b));
    if truecolor {
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(nearest_256(r, g, b))
    }
}

/// The closest color of the xterm 256-color palette, from its 6×6×6 cube or
/// its gray ramp.
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let step = |c: u8| {
        (0..6)
            .min_by_key(|&i| (i32::from(STEPS[i]) - i32::from(c)).abs())
            .unwrap()
    };
    let distance = |(x, y, z): (u8, u8, u8)| {
        [(x, r), (y, g), (z, b)]
            .iter()
            .map(|&(a, c)| (i32::from(a) - i32::from(c)).pow(2))
            .sum::<i32>()
    };
    let (ri, gi, bi) = (step(r), step(g), step(b));
    let cube = (16 + 36 * ri + 6 * gi + bi) as u8;
    let cube_rgb = (STEPS[ri], STEPS[gi], STEPS[bi]);
    let mean = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray = (mean.saturating_sub(3) / 10).min(23) as u8;
    let level = 8 + 10 * gray;
    if distance((level, level, level)) < distance(cube_rgb) {
        232 + gray
    } else {
        cube
    }
}

/// The terminal's background, asked with OSC 11 and followed by a request
/// for the device attributes, which every terminal answers: the second
/// answer ends the wait whether the first came or not.
fn background() -> Option<(u8, u8, u8)> {
    let mut out = io::stdout().lock();
    out.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    out.flush().ok()?;
    let deadline = Instant::now() + ANSWER;
    let mut answer = Vec::new();
    while !attributes_end(&answer) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut poll = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd and a timeout in milliseconds.
        let ready = unsafe { libc::poll(&mut poll, 1, left.as_millis() as libc::c_int) };
        if ready <= 0 {
            continue;
        }
        let mut buf = [0u8; 256];
        // SAFETY: reads into a local buffer of its length.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        answer.extend_from_slice(&buf[..n as usize]);
    }
    parse_background(&answer)
}

/// Whether `answer` holds the device attributes: `ESC [ ? … c`.
fn attributes_end(answer: &[u8]) -> bool {
    answer
        .windows(3)
        .position(|w| w == b"\x1b[?")
        .is_some_and(|i| answer[i..].contains(&b'c'))
}

/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` with 1 to 4 hex digits per channel.
fn parse_background(answer: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(answer).ok()?;
    let start = text.find("]11;rgb:")? + "]11;rgb:".len();
    let end = text[start..].find(|c: char| !c.is_ascii_hexdigit() && c != '/')?;
    let mut channels = text[start..start + end].split('/').map(|hex| {
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len() as u32)) - 1;
        (!hex.is_empty() && hex.len() <= 4).then(|| (value * 255 / max) as u8)
    });
    Some((channels.next()??, channels.next()??, channels.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_terminals_answer() {
        let dark = b"\x1b]11;rgb:1e1e/2121/2727\x1b\\\x1b[?62;22c";
        assert_eq!(parse_background(dark), Some((0x1e, 0x21, 0x27)));
        assert!(attributes_end(dark));
        assert_eq!(
            parse_background(b"\x1b]11;rgb:ff/ff/ff\x07"),
            Some((255, 255, 255))
        );
        assert_eq!(parse_background(b"\x1b[?1;2c"), None, "no answer to OSC 11");
        assert!(!attributes_end(b"\x1b]11;rgb:ff/ff/ff\x07"));
    }

    #[test]
    fn the_selection_is_a_step_from_the_background() {
        assert_eq!(selection((0x1e, 0x21, 0x27), true), Color::Rgb(62, 64, 69));
        assert_eq!(selection((255, 255, 255), true), Color::Rgb(232, 232, 232));
        // Without 24-bit color, the nearest gray of the 256 colors.
        assert_eq!(selection((0x1e, 0x21, 0x27), false), Color::Indexed(238));
        assert_eq!(selection((255, 255, 255), false), Color::Indexed(254));
    }
}
