//! Small, allocation-free DSP building blocks shared by every synth.

use std::sync::LazyLock;

const SINE_SIZE: usize = 4096;

static SINE: LazyLock<Box<[f32]>> = LazyLock::new(|| {
    (0..SINE_SIZE + 2)
        .map(|i| (i as f64 / SINE_SIZE as f64 * std::f64::consts::TAU).sin() as f32)
        .collect()
});

/// Force the lookup tables to be built before the audio thread touches them.
pub fn init_tables() {
    LazyLock::force(&SINE);
}

/// Table-based sine where `phase` is measured in cycles (1.0 == one period).
#[inline]
pub fn sin_cycles(phase: f32) -> f32 {
    let p = phase - phase.floor();
    let x = p * SINE_SIZE as f32;
    let i = x as usize;
    let f = x - i as f32;
    let t = &*SINE;
    // `i` is at most SINE_SIZE thanks to the guard entries.
    let a = t[i];
    a + (t[i + 1] - a) * f
}

#[inline]
pub fn midi_to_freq(note: f32) -> f32 {
    440.0 * semitones_to_ratio(note - 69.0)
}

#[inline]
pub fn semitones_to_ratio(semis: f32) -> f32 {
    (semis * (1.0 / 12.0)).exp2()
}

/// Equal-power pan law; `pan` in -1..=1. Returns (left, right) gains.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let x = (pan.clamp(-1.0, 1.0) + 1.0) * 0.125; // 0..0.25 cycles
    (sin_cycles(x + 0.25), sin_cycles(x))
}

/// Tiny xorshift PRNG: deterministic, fast and safe to use on the audio thread.
#[derive(Clone, Copy)]
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Rng(seed.max(1))
    }

    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform in [0, 1).
    #[inline]
    pub fn unipolar(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    /// Uniform in [-1, 1).
    #[inline]
    pub fn bipolar(&mut self) -> f32 {
        self.unipolar() * 2.0 - 1.0
    }
}

/// One-pole parameter smoother to avoid zipper noise.
#[derive(Clone, Copy)]
pub struct Smooth {
    pub value: f32,
    coef: f32,
}

impl Smooth {
    pub fn new(value: f32, time_s: f32, sample_rate: f32) -> Self {
        Smooth {
            value,
            coef: (-1.0 / (time_s * sample_rate)).exp(),
        }
    }

    #[inline]
    pub fn next(&mut self, target: f32) -> f32 {
        self.value = target + (self.value - target) * self.coef;
        self.value
    }
}

// ---------------------------------------------------------------------------
// ADSR envelope
// ---------------------------------------------------------------------------

/// Pre-computed per-sample coefficients for an ADSR, shared by all voices.
#[derive(Clone, Copy, Debug)]
pub struct AdsrParams {
    attack_inc: f32,
    decay_coef: f32,
    sustain: f32,
    release_coef: f32,
}

/// Coefficient for an exponential segment that falls by 60 dB over `time_s`.
fn exp_coef(time_s: f32, sample_rate: f32) -> f32 {
    let samples = (time_s * sample_rate).max(1.0);
    (0.001f32.ln() / samples).exp()
}

impl AdsrParams {
    pub fn new(attack: f32, decay: f32, sustain: f32, release: f32, sample_rate: f32) -> Self {
        AdsrParams {
            attack_inc: 1.0 / (attack * sample_rate).max(1.0),
            decay_coef: exp_coef(decay, sample_rate),
            sustain: sustain.clamp(0.0, 1.0),
            release_coef: exp_coef(release, sample_rate),
        }
    }
}

impl Default for AdsrParams {
    fn default() -> Self {
        AdsrParams::new(0.01, 0.3, 0.7, 0.3, 48_000.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Stage {
    #[default]
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Env {
    pub stage: Stage,
    pub level: f32,
}

const ENV_FLOOR: f32 = 1.0e-4;

impl Env {
    /// Start (or restart) the envelope. The level is kept so retriggers don't click.
    pub fn trigger(&mut self) {
        self.stage = Stage::Attack;
    }

    pub fn release(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    pub fn is_idle(&self) -> bool {
        self.stage == Stage::Idle
    }

    #[inline]
    pub fn next(&mut self, p: &AdsrParams) -> f32 {
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.level += p.attack_inc;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = p.sustain + (self.level - p.sustain) * p.decay_coef;
                if (self.level - p.sustain).abs() < ENV_FLOOR {
                    self.level = p.sustain;
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => {
                // Follow live sustain edits smoothly.
                self.level = p.sustain + (self.level - p.sustain) * 0.999;
                if self.level < ENV_FLOOR {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
            Stage::Release => {
                self.level *= p.release_coef;
                if self.level < ENV_FLOOR {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }
}

// ---------------------------------------------------------------------------
// State-variable filter (Simper / Cytomic TPT form)
// ---------------------------------------------------------------------------

#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FilterMode {
    #[default]
    LowPass,
    BandPass,
    HighPass,
}

#[derive(Clone, Copy, Debug)]
pub struct SvfCoefs {
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
    mode: FilterMode,
}

impl SvfCoefs {
    pub fn new(mode: FilterMode, cutoff: f32, resonance: f32, sample_rate: f32) -> Self {
        let fc = cutoff.clamp(10.0, sample_rate * 0.45);
        let g = (std::f32::consts::PI * fc / sample_rate).tan();
        let k = 2.0 - 1.96 * resonance.clamp(0.0, 1.0);
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        SvfCoefs { a1, a2, a3, k, mode }
    }
}

impl Default for SvfCoefs {
    fn default() -> Self {
        SvfCoefs::new(FilterMode::LowPass, 20_000.0, 0.0, 48_000.0)
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Svf {
    ic1: f32,
    ic2: f32,
}

impl Svf {
    #[inline]
    pub fn process(&mut self, c: &SvfCoefs, v0: f32) -> f32 {
        let v3 = v0 - self.ic2;
        let v1 = c.a1 * self.ic1 + c.a2 * v3;
        let v2 = self.ic2 + c.a2 * self.ic1 + c.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        match c.mode {
            FilterMode::LowPass => v2,
            FilterMode::BandPass => v1,
            FilterMode::HighPass => v0 - c.k * v1 - v2,
        }
    }
}

// ---------------------------------------------------------------------------
// Band-limited oscillator helper and ladder filter
// ---------------------------------------------------------------------------

/// PolyBLEP residual for a discontinuity at phase 0; `t` is the phase in
/// cycles and `dt` the per-sample phase increment.
#[inline]
pub fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

/// Zero-delay-feedback 4-pole (24 dB/oct) ladder low-pass with a saturating
/// feedback path. `g = tan(pi * fc / sr)`, `k` is resonance (0..~4).
#[derive(Clone, Copy, Default)]
pub struct Ladder {
    s: [f32; 4],
}

impl Ladder {
    #[inline]
    pub fn process(&mut self, x: f32, g: f32, k: f32) -> f32 {
        let big_g = g / (1.0 + g);
        let b = 1.0 / (1.0 + g);
        let g2 = big_g * big_g;
        let sigma = (g2 * big_g * self.s[0] + g2 * self.s[1] + big_g * self.s[2] + self.s[3]) * b;
        let u = ((x - k * sigma) / (1.0 + k * g2 * g2)).tanh();
        let mut y = u;
        for s in &mut self.s {
            let v = (y - *s) * big_g;
            y = v + *s;
            *s = y + v;
        }
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_table_matches_std() {
        for i in 0..1000 {
            let p = i as f32 / 997.0 - 0.3;
            assert!((sin_cycles(p) - (p * std::f32::consts::TAU).sin()).abs() < 1e-4);
        }
    }

    #[test]
    fn adsr_runs_to_idle() {
        let p = AdsrParams::new(0.001, 0.01, 0.5, 0.01, 48_000.0);
        let mut e = Env::default();
        e.trigger();
        for _ in 0..4800 {
            e.next(&p);
        }
        assert!((e.level - 0.5).abs() < 0.01);
        e.release();
        for _ in 0..4800 {
            e.next(&p);
        }
        assert!(e.is_idle());
    }
}
