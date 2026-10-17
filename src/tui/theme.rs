//! The colors of the view. The selected row is bold, with nothing dim, on a
//! background a step from the terminal's own toward its text color: the
//! largest step, up to 14 %, that leaves every text color of the row at
//! 4.5:1 or more (WCAG AA). The terminal says its background, its text
//! color and its palette when asked (OSC 11, 10 and 4); one that does not
//! say its background gets no background, only the mark and bold.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};

/// How long to wait for the terminal's answer. A local terminal answers in
/// well under a millisecond; the device attributes asked after it end the
/// wait as soon as they arrive, so this only runs out on a terminal that
/// answers neither.
const ANSWER: Duration = Duration::from_millis(100);

type Rgb = (u8, u8, u8);

/// What the terminal said of its colors; `None` where it did not answer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Colors {
    pub bg: Option<Rgb>,
    pub fg: Option<Rgb>,
    /// Red and yellow: the colors of text the selected row may hold.
    pub red: Option<Rgb>,
    pub yellow: Option<Rgb>,
}

/// The least contrast between text and its background (WCAG AA).
const MIN_CONTRAST: f64 = 4.5;
/// The largest step from the background toward the text color, in percent.
const MAX_STEP: u8 = 14;
/// For a text color the terminal did not say, the share of its contrast the
/// selection keeps: one at 5:1 on the background stays at 4.5:1.
const KEPT_UNSAID: f64 = 0.9;

/// WCAG relative luminance.
fn luminance((r, g, b): Rgb) -> f64 {
    let linear = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// WCAG contrast ratio, from 1 to 21.
fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The color a cell shows, from the 256-color palette when indexed.
fn rgb(color: Color) -> Rgb {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i @ 16..=231) => {
            let i = usize::from(i - 16);
            (STEPS[i / 36], STEPS[i / 6 % 6], STEPS[i % 6])
        }
        Color::Indexed(i @ 232..) => {
            let level = 8 + 10 * (i - 232);
            (level, level, level)
        }
        other => unreachable!("selections are 24-bit or of the 256 colors, not {other:?}"),
    }
}

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
            selection: selection(&ask(), truecolor),
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

    /// Bold, never dim, on the selection's background; the text keeps its
    /// colors.
    pub fn selected(&self) -> Style {
        let style = Style::new()
            .add_modifier(Modifier::BOLD)
            .remove_modifier(Modifier::DIM);
        match self.selection {
            Some(bg) => style.bg(bg),
            None => style,
        }
    }
}

/// The selected row's background: the largest step from the terminal's
/// background toward its text color that keeps every text color of the row
/// readable, or none when the background is unknown or no step does.
fn selection(colors: &Colors, truecolor: bool) -> Option<Color> {
    let bg = colors.bg?;
    // Without its text color, the one that reads on that background.
    let fg = colors.fg.unwrap_or(if luminance(bg) < 0.18 {
        (255, 255, 255)
    } else {
        (0, 0, 0)
    });
    let texts = [Some(fg), colors.red, colors.yellow];
    let unsaid = texts.contains(&None);
    (1..=MAX_STEP).rev().find_map(|step| {
        let amount = f64::from(step) / 100.0;
        let mix =
            |c: u8, to: u8| (f64::from(c) + (f64::from(to) - f64::from(c)) * amount).round() as u8;
        let (r, g, b) = (mix(bg.0, fg.0), mix(bg.1, fg.1), mix(bg.2, fg.2));
        let color = if truecolor {
            Color::Rgb(r, g, b)
        } else {
            Color::Indexed(nearest_256(r, g, b))
        };
        let sel = rgb(color);
        // A color already under 4.5:1 on the background is the theme's.
        let readable = texts
            .iter()
            .flatten()
            .all(|&t| contrast(t, bg) < MIN_CONTRAST || contrast(t, sel) >= MIN_CONTRAST);
        let kept = !unsaid || 1.0 / contrast(bg, sel) >= KEPT_UNSAID;
        (sel != bg && readable && kept).then_some(color)
    })
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

/// The terminal's colors, asked with OSC 11, 10 and 4 and followed by a
/// request for the device attributes, which every terminal answers: that
/// answer ends the wait whether the others came or not.
fn ask() -> Colors {
    let mut out = io::stdout().lock();
    let asked = out
        .write_all(b"\x1b]11;?\x1b\\\x1b]10;?\x1b\\\x1b]4;1;?\x1b\\\x1b]4;3;?\x1b\\\x1b[c")
        .and_then(|()| out.flush());
    if asked.is_err() {
        return Colors::default();
    }
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
    Colors {
        bg: parse_color(&answer, "]11;"),
        fg: parse_color(&answer, "]10;"),
        red: parse_color(&answer, "]4;1;"),
        yellow: parse_color(&answer, "]4;3;"),
    }
}

/// Whether `answer` holds the device attributes: `ESC [ ? … c`.
fn attributes_end(answer: &[u8]) -> bool {
    answer
        .windows(3)
        .position(|w| w == b"\x1b[?")
        .is_some_and(|i| answer[i..].contains(&b'c'))
}

/// The color after `key` in `answer`, as in `ESC ] 11 ; rgb:RRRR/GGGG/BBBB`,
/// with 1 to 4 hex digits per channel.
fn parse_color(answer: &[u8], key: &str) -> Option<Rgb> {
    let text = std::str::from_utf8(answer).ok()?;
    let start = text.find(&format!("{key}rgb:"))? + key.len() + "rgb:".len();
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
        let herdr = b"\x1b]11;rgb:2828/2c2c/3434\x1b\\\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;22c";
        assert_eq!(parse_color(herdr, "]11;"), Some((0x28, 0x2c, 0x34)));
        assert_eq!(parse_color(herdr, "]10;"), Some((0xff, 0xff, 0xff)));
        assert_eq!(parse_color(herdr, "]4;1;"), None, "no palette");
        assert!(attributes_end(herdr));
        let palette = b"\x1b]4;1;rgb:cfcf/2222/2e2e\x07\x1b]4;3;rgb:4d/2d/00\x07";
        assert_eq!(parse_color(palette, "]4;1;"), Some((0xcf, 0x22, 0x2e)));
        assert_eq!(parse_color(palette, "]4;3;"), Some((0x4d, 0x2d, 0x00)));
        assert_eq!(parse_color(b"\x1b[?1;2c", "]11;"), None, "no answer");
        assert!(!attributes_end(b"\x1b]11;rgb:ff/ff/ff\x07"));
    }

    /// Herdr, as it answered: OSC 11 and OSC 10, no palette.
    const HERDR: Colors = Colors {
        bg: Some((0x28, 0x2c, 0x34)),
        fg: Some((0xff, 0xff, 0xff)),
        red: None,
        yellow: None,
    };
    /// The light theme of the screen captures (GitHub light).
    const LIGHT: Colors = Colors {
        bg: Some((0xff, 0xff, 0xff)),
        fg: Some((0x1f, 0x23, 0x28)),
        red: Some((0xcf, 0x22, 0x2e)),
        yellow: Some((0x4d, 0x2d, 0x00)),
    };
    /// The dark theme of the screen captures (GitHub dark).
    const DARK: Colors = Colors {
        bg: Some((0x0d, 0x11, 0x17)),
        fg: Some((0xe6, 0xed, 0xf3)),
        red: Some((0xff, 0x7b, 0x72)),
        yellow: Some((0xd2, 0x99, 0x22)),
    };

    #[test]
    fn every_text_of_the_selected_row_keeps_4_5_to_1() {
        for (name, colors) in [("herdr", HERDR), ("light", LIGHT), ("dark", DARK)] {
            for truecolor in [true, false] {
                let sel = rgb(selection(&colors, truecolor).expect("a background"));
                let bg = colors.bg.unwrap();
                assert_ne!(sel, bg, "{name}: a background that shows");
                for text in [colors.fg, colors.red, colors.yellow].into_iter().flatten() {
                    if contrast(text, bg) >= MIN_CONTRAST {
                        let c = contrast(text, sel);
                        assert!(
                            c >= MIN_CONTRAST,
                            "{name} {truecolor}: {text:?} on {sel:?} is {c:.2}"
                        );
                    }
                }
                if colors.red.is_none() {
                    // A color the terminal did not say, at 5:1 on its background,
                    // stays at 4.5:1 on the selection.
                    let kept = contrast(bg, (0, 0, 0)).min(contrast(sel, (0, 0, 0)))
                        / contrast(bg, (0, 0, 0)).max(contrast(sel, (0, 0, 0)));
                    assert!(
                        kept >= 0.9,
                        "{name} {truecolor}: keeps {kept:.3} of the contrast"
                    );
                }
            }
        }
        assert_eq!(
            selection(&Colors::default(), true),
            None,
            "no background, no selection"
        );
    }
}
