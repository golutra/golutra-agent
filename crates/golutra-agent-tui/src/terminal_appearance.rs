use ratatui::style::Color;
use std::{sync::OnceLock, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorDepth {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TerminalAppearance {
    pub(crate) light: bool,
    pub(crate) depth: ColorDepth,
}

static APPEARANCE: OnceLock<TerminalAppearance> = OnceLock::new();

pub(crate) fn initialize() {
    APPEARANCE.get_or_init(|| {
        let depth = match supports_color::on(supports_color::Stream::Stdout) {
            None => ColorDepth::None,
            Some(level) if level.has_16m => ColorDepth::TrueColor,
            Some(level) if level.has_256 => ColorDepth::Ansi256,
            Some(_) => ColorDepth::Ansi16,
        };
        let light = depth != ColorDepth::None
            && matches!(
                termbg::theme(Duration::from_millis(50)),
                Ok(termbg::Theme::Light)
            );
        TerminalAppearance { light, depth }
    });
}

pub(crate) fn current() -> TerminalAppearance {
    *APPEARANCE.get().unwrap_or(&TerminalAppearance {
        light: false,
        depth: ColorDepth::TrueColor,
    })
}

impl TerminalAppearance {
    pub(crate) fn diff_background(self, added: bool) -> Color {
        match (self.depth, self.light, added) {
            (ColorDepth::None | ColorDepth::Ansi16, _, _) => Color::Reset,
            (ColorDepth::Ansi256, false, true) => Color::Indexed(22),
            (ColorDepth::Ansi256, false, false) => Color::Indexed(52),
            (ColorDepth::Ansi256, true, true) => Color::Indexed(194),
            (ColorDepth::Ansi256, true, false) => Color::Indexed(224),
            (ColorDepth::TrueColor, false, true) => Color::Rgb(33, 41, 34),
            (ColorDepth::TrueColor, false, false) => Color::Rgb(60, 23, 15),
            (ColorDepth::TrueColor, true, true) => Color::Rgb(218, 251, 225),
            (ColorDepth::TrueColor, true, false) => Color::Rgb(255, 235, 233),
        }
    }

    pub(crate) fn color(self, color: Color) -> Color {
        if self.depth == ColorDepth::None {
            return Color::Reset;
        }
        let Color::Rgb(r, g, b) = color else {
            return color;
        };
        match self.depth {
            ColorDepth::TrueColor => color,
            ColorDepth::Ansi256 => {
                let quantize = |v: u8| ((u16::from(v) * 5 + 127) / 255) as u8;
                Color::Indexed(16 + 36 * quantize(r) + 6 * quantize(g) + quantize(b))
            }
            ColorDepth::Ansi16 => Color::Reset,
            ColorDepth::None => Color::Reset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_backgrounds_adapt_to_lightness_and_color_depth() {
        for depth in [
            ColorDepth::None,
            ColorDepth::Ansi16,
            ColorDepth::Ansi256,
            ColorDepth::TrueColor,
        ] {
            for light in [false, true] {
                let appearance = TerminalAppearance { depth, light };
                if matches!(depth, ColorDepth::None | ColorDepth::Ansi16) {
                    assert_eq!(appearance.diff_background(true), Color::Reset);
                    assert_eq!(appearance.diff_background(false), Color::Reset);
                } else {
                    assert_ne!(
                        appearance.diff_background(true),
                        appearance.diff_background(false)
                    );
                    assert_ne!(
                        appearance.diff_background(true),
                        TerminalAppearance {
                            light: !light,
                            depth
                        }
                        .diff_background(true)
                    );
                }
            }
        }
    }
}
