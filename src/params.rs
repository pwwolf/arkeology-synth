//! Parameter descriptors. Every synth exposes a flat, static table of these;
//! the engine stores plain `f32` values indexed by position, and the UI uses
//! the descriptors to display, step and persist them.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Float,
    Int,
    Enum(&'static [&'static str]),
    Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scale {
    Linear,
    /// Logarithmic mapping between `min` and `max` (both must be > 0).
    Exp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unit {
    None,
    Percent,
    Seconds,
    Hz,
    Semitones,
    Cents,
    Ratio,
    Pan,
    /// MIDI note number, shown with its name.
    Note,
    Decibels,
}

#[derive(Clone, Copy, Debug)]
pub struct ParamDesc {
    /// Stable identifier used in patch files and MIDI mappings.
    pub key: &'static str,
    pub name: &'static str,
    pub group: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub kind: Kind,
    pub scale: Scale,
    pub unit: Unit,
    /// Step for a normal adjustment of a linear float; 0 means 1% of the range.
    pub step: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepSize {
    Fine,
    Normal,
    Coarse,
}

impl ParamDesc {
    pub const fn float(
        key: &'static str,
        name: &'static str,
        group: &'static str,
        min: f32,
        max: f32,
        default: f32,
        unit: Unit,
    ) -> Self {
        ParamDesc {
            key,
            name,
            group,
            min,
            max,
            default,
            kind: Kind::Float,
            scale: Scale::Linear,
            unit,
            step: 0.0,
        }
    }

    pub const fn int(
        key: &'static str,
        name: &'static str,
        group: &'static str,
        min: i32,
        max: i32,
        default: i32,
        unit: Unit,
    ) -> Self {
        let mut p = Self::float(
            key,
            name,
            group,
            min as f32,
            max as f32,
            default as f32,
            unit,
        );
        p.kind = Kind::Int;
        p
    }

    pub const fn choice(
        key: &'static str,
        name: &'static str,
        group: &'static str,
        options: &'static [&'static str],
        default: usize,
    ) -> Self {
        let mut p = Self::float(
            key,
            name,
            group,
            0.0,
            (options.len() - 1) as f32,
            default as f32,
            Unit::None,
        );
        p.kind = Kind::Enum(options);
        p
    }

    pub const fn toggle(
        key: &'static str,
        name: &'static str,
        group: &'static str,
        on: bool,
    ) -> Self {
        let mut p = Self::float(
            key,
            name,
            group,
            0.0,
            1.0,
            if on { 1.0 } else { 0.0 },
            Unit::None,
        );
        p.kind = Kind::Toggle;
        p
    }

    pub const fn exp(mut self) -> Self {
        self.scale = Scale::Exp;
        self
    }

    pub const fn step(mut self, step: f32) -> Self {
        self.step = step;
        self
    }

    pub fn is_discrete(&self) -> bool {
        !matches!(self.kind, Kind::Float)
    }

    pub fn clamp(&self, v: f32) -> f32 {
        let v = if v.is_finite() { v } else { self.default };
        let v = v.clamp(self.min, self.max);
        if self.is_discrete() { v.round() } else { v }
    }

    pub fn normalize(&self, v: f32) -> f32 {
        let v = self.clamp(v);
        if self.max <= self.min {
            return 0.0;
        }
        match self.scale {
            Scale::Linear => (v - self.min) / (self.max - self.min),
            Scale::Exp => (v / self.min).ln() / (self.max / self.min).ln(),
        }
    }

    pub fn denormalize(&self, n: f32) -> f32 {
        let n = n.clamp(0.0, 1.0);
        let v = match self.scale {
            Scale::Linear => self.min + n * (self.max - self.min),
            Scale::Exp => self.min * (self.max / self.min).powf(n),
        };
        self.clamp(v)
    }

    /// Nudge a value by one step in direction `dir` (+1 / -1).
    pub fn adjust(&self, v: f32, dir: f32, size: StepSize) -> f32 {
        if self.is_discrete() {
            let mult = if size == StepSize::Coarse && matches!(self.kind, Kind::Int) {
                ((self.max - self.min) / 8.0).round().max(1.0)
            } else {
                1.0
            };
            return self.clamp(v + dir * mult);
        }
        if self.scale == Scale::Linear && self.step > 0.0 {
            let step = match size {
                StepSize::Fine => self.step / 10.0,
                StepSize::Normal => self.step,
                StepSize::Coarse => self.step * 4.0,
            };
            // Snap to the step grid when using normal/coarse steps.
            let next = v + dir * step;
            let snapped = if size == StepSize::Fine {
                next
            } else {
                (next / step).round() * step
            };
            return self.clamp(snapped);
        }
        let step = match size {
            StepSize::Fine => 0.001,
            StepSize::Normal => 0.01,
            StepSize::Coarse => 0.1,
        };
        self.denormalize(self.normalize(v) + dir * step)
    }

    pub fn format(&self, v: f32) -> String {
        match self.kind {
            Kind::Enum(opts) => {
                let i = (v.round().max(0.0) as usize).min(opts.len() - 1);
                return opts[i].to_string();
            }
            Kind::Toggle => return if v >= 0.5 { "on".into() } else { "off".into() },
            Kind::Int => {
                let i = v.round() as i32;
                return match self.unit {
                    Unit::Note => format!("{i} {}", crate::midi::note_name(i.clamp(0, 127) as u8)),
                    Unit::Semitones if i > 0 => format!("+{i} st"),
                    Unit::Semitones => format!("{i} st"),
                    _ => format!("{i}"),
                };
            }
            Kind::Float => {}
        }
        match self.unit {
            Unit::None => format!("{v:.2}"),
            Unit::Percent => format!("{:.0}%", v * 100.0),
            Unit::Seconds if v < 1.0 => format!("{:.0} ms", v * 1000.0),
            Unit::Seconds => format!("{v:.2} s"),
            Unit::Hz if v >= 1000.0 => format!("{:.2} kHz", v / 1000.0),
            Unit::Hz if v < 10.0 => format!("{v:.2} Hz"),
            Unit::Hz => format!("{v:.0} Hz"),
            Unit::Semitones => format!("{v:+.2} st"),
            Unit::Cents => format!("{v:+.0} ct"),
            Unit::Ratio => format!("x{v:.3}"),
            Unit::Note => format!("{v:.0}"),
            Unit::Decibels => format!("{v:+.1} dB"),
            Unit::Pan if v.abs() < 0.005 => "C".into(),
            Unit::Pan if v < 0.0 => format!("L{:.0}", -v * 100.0),
            Unit::Pan => format!("R{:.0}", v * 100.0),
        }
    }

    /// Parse user-typed text into a value (accepts option names for enums).
    pub fn parse(&self, text: &str) -> Option<f32> {
        let t = text.trim().to_ascii_lowercase();
        match self.kind {
            Kind::Enum(opts) => {
                if let Some(i) = opts
                    .iter()
                    .position(|o| o.to_ascii_lowercase().starts_with(&t))
                {
                    return Some(i as f32);
                }
            }
            _ if self.unit == Unit::Note && t.starts_with(|c: char| c.is_ascii_alphabetic()) => {
                return parse_note_name(&t).map(|n| self.clamp(n as f32));
            }
            Kind::Toggle => match t.as_str() {
                "on" | "yes" | "true" => return Some(1.0),
                "off" | "no" | "false" => return Some(0.0),
                _ => {}
            },
            _ => {}
        }
        let numeric: String = t
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
            .collect();
        let mut v: f32 = numeric.parse().ok()?;
        let rest = t[numeric.len()..].trim();
        match self.unit {
            Unit::Percent => v /= 100.0,
            Unit::Seconds if rest.starts_with("ms") || (rest.is_empty() && v > 20.0) => v /= 1000.0,
            Unit::Hz if rest.starts_with('k') => v *= 1000.0,
            Unit::Pan => v /= 100.0,
            _ => {}
        }
        Some(self.clamp(v))
    }
}

/// Parse names like "c4", "f#2" or "bb1" (middle C = C4 = 60).
pub(crate) fn parse_note_name(t: &str) -> Option<i32> {
    let mut chars = t.chars().peekable();
    let base = match chars.next()? {
        'c' => 0,
        'd' => 2,
        'e' => 4,
        'f' => 5,
        'g' => 7,
        'a' => 9,
        'b' => 11,
        _ => return None,
    };
    let accidental = match chars.peek() {
        Some('#') => {
            chars.next();
            1
        }
        Some('b') => {
            chars.next();
            -1
        }
        _ => 0,
    };
    let octave: i32 = chars.collect::<String>().parse().ok()?;
    Some((octave + 1) * 12 + base + accidental)
}

pub fn index_of(table: &[ParamDesc], key: &str) -> Option<usize> {
    table.iter().position(|p| p.key == key)
}

// ---------------------------------------------------------------------------
// Master bus parameters
// ---------------------------------------------------------------------------

pub mod master {
    use super::{ParamDesc as P, Unit};

    pub const VOLUME: usize = 0;
    pub const REVERB_SIZE: usize = 1;
    pub const REVERB_DAMP: usize = 2;
    pub const REVERB_WIDTH: usize = 3;
    pub const REVERB_RETURN: usize = 4;
    pub const DRIVE: usize = 5;

    /// Where the master FX chain's parameters start.
    pub const FX_BASE: usize = 6;

    const BASE: [P; FX_BASE] = [
        P::float("volume", "Volume", "Master", 0.0, 1.0, 0.8, Unit::Percent),
        P::float(
            "reverb_size",
            "Size",
            "Reverb Send",
            0.0,
            1.0,
            0.75,
            Unit::Percent,
        ),
        P::float(
            "reverb_damp",
            "Damping",
            "Reverb Send",
            0.0,
            1.0,
            0.4,
            Unit::Percent,
        ),
        P::float(
            "reverb_width",
            "Width",
            "Reverb Send",
            0.0,
            1.0,
            1.0,
            Unit::Percent,
        ),
        P::float(
            "reverb_return",
            "Return",
            "Reverb Send",
            0.0,
            1.0,
            0.5,
            Unit::Percent,
        ),
        P::float("drive", "Drive", "Master", 0.0, 1.0, 0.0, Unit::Percent),
    ];

    const LEN: usize = FX_BASE + crate::fx::TABLE.len();

    const fn build() -> [P; LEN] {
        let mut out = [BASE[0]; LEN];
        let mut i = 0;
        while i < LEN {
            out[i] = if i < FX_BASE {
                BASE[i]
            } else {
                crate::fx::TABLE[i - FX_BASE]
            };
            i += 1;
        }
        out
    }

    /// Master bus parameters followed by the master FX chain.
    pub static PARAMS: [P; LEN] = build();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_round_trip() {
        let p = ParamDesc::float("x", "X", "G", 20.0, 20_000.0, 1000.0, Unit::Hz).exp();
        let n = p.normalize(1000.0);
        assert!((p.denormalize(n) - 1000.0).abs() < 0.1);
    }

    #[test]
    fn parse_units() {
        let t = ParamDesc::float("t", "T", "G", 0.001, 10.0, 0.1, Unit::Seconds).exp();
        assert!((t.parse("250ms").unwrap() - 0.25).abs() < 1e-6);
        assert!((t.parse("1.5").unwrap() - 1.5).abs() < 1e-6);
        let n = ParamDesc::int("n", "N", "G", 0, 127, 60, Unit::Note);
        assert_eq!(n.parse("c4"), Some(60.0));
        assert_eq!(n.parse("F#2"), Some(42.0));
        assert_eq!(n.parse("36"), Some(36.0));
        assert_eq!(n.format(36.0), "36 C2");
        let f = ParamDesc::float("f", "F", "G", 20.0, 20_000.0, 1000.0, Unit::Hz).exp();
        assert!((f.parse("2.5k").unwrap() - 2500.0).abs() < 1e-3);
    }
}
