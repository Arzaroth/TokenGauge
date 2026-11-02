//! Colour for text printed to a terminal: none when stdout is not one, or
//! when `NO_COLOR` asks for none.

use std::io::IsTerminal;

use crate::panel::Tone;

pub fn styled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// The escapes a printout uses, all empty when it is not styled.
#[derive(Debug, Clone, Copy)]
pub struct Ink {
    pub dim: &'static str,
    pub good: &'static str,
    pub warn: &'static str,
    pub bad: &'static str,
    pub accent: &'static str,
    pub reset: &'static str,
}

impl Ink {
    pub fn new(styled: bool) -> Self {
        if styled {
            Ink {
                dim: "\x1b[2m",
                good: "\x1b[32m",
                warn: "\x1b[33m",
                bad: "\x1b[31m",
                accent: "\x1b[36m",
                reset: "\x1b[0m",
            }
        } else {
            Ink {
                dim: "",
                good: "",
                warn: "",
                bad: "",
                accent: "",
                reset: "",
            }
        }
    }

    pub fn tone(&self, tone: Tone) -> &'static str {
        match tone {
            Tone::Normal => "",
            Tone::Dim => self.dim,
            Tone::Good => self.good,
            Tone::Warn => self.warn,
            Tone::Critical => self.bad,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unstyled_ink_is_empty() {
        let ink = Ink::new(false);
        assert_eq!(
            [ink.dim, ink.good, ink.warn, ink.bad, ink.accent, ink.reset].concat(),
            ""
        );
        assert_eq!(Ink::new(true).tone(Tone::Critical), "\x1b[31m");
    }
}
