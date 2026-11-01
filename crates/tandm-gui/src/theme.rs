use gpui::{Hsla, WindowAppearance, rgb};
use tandm_core::Tone;

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Hsla,
    pub surface: Hsla,
    pub surface2: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub good: Hsla,
    pub busy: Hsla,
    pub warn: Hsla,
    pub bad: Hsla,
    pub selected: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn alpha(c: Hsla, a: f32) -> Hsla {
    Hsla { a, ..c }
}

impl Palette {
    pub fn for_appearance(a: WindowAppearance) -> Palette {
        match a {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Palette::light(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Palette::dark(),
        }
    }

    pub fn dark() -> Palette {
        Palette {
            bg: c(0x14161b),
            surface: c(0x1c1f26),
            surface2: c(0x252932),
            border: c(0x313641),
            text: c(0xe6e8ec),
            muted: c(0x8b93a1),
            accent: c(0x8b9cff),
            good: c(0x3fb950),
            busy: c(0x58a6ff),
            warn: c(0xd29922),
            bad: c(0xf85149),
            selected: c(0x8b9cff),
        }
    }

    pub fn light() -> Palette {
        Palette {
            bg: c(0xf4f5f7),
            surface: c(0xffffff),
            surface2: c(0xeceef2),
            border: c(0xd5d9e0),
            text: c(0x1c2128),
            muted: c(0x5f6b7a),
            accent: c(0x4f5bd5),
            good: c(0x1a7f37),
            busy: c(0x0969da),
            warn: c(0x9a6700),
            bad: c(0xcf222e),
            selected: c(0x4f5bd5),
        }
    }

    pub fn tone(&self, t: Tone) -> Hsla {
        match t {
            Tone::Good => self.good,
            Tone::Busy => self.busy,
            Tone::Warn => self.warn,
            Tone::Bad => self.bad,
            Tone::Muted => self.muted,
        }
    }
}
