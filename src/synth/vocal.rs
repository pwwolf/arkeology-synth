//! Vocal synth: a source-filter voice model. Each note is one or more
//! "singers" producing a glottal pulse (or a saw or buzz, for talkbox and
//! robot sounds) with aspiration noise, vibrato that arrives after a delay,
//! and slow pitch and level drift. The voices are summed and shaped by five
//! formant resonators in series (a cascade, as in Klatt's synthesizer, so
//! the fundamental passes below the first formant), set from
//! published soprano/alto/tenor/bass vowel tables and morphed continuously
//! a → e → i → o → u (and towards a closed-mouth hum). The mod wheel moves
//! the vowel, so a controller can make it talk.

use crate::dsp::{AdsrParams, Biquad, Env, Rng, midi_to_freq, poly_blep, sin_cycles};
use crate::params::{ParamDesc as P, Unit};

use super::{COMMON, Controls, GLIDE_PARAM, MAX_BLOCK, Poly, VOICES_PARAM, Voice, glide};

pub const VOICE_TYPES: [&str; 4] = ["Soprano", "Alto", "Tenor", "Bass"];
pub const SOURCES: [&str; 3] = ["Glottal", "Saw", "Buzz"];

pub const VOICES: usize = 0;
pub const GLIDE: usize = 1;
pub const VOICE_TYPE: usize = 2;
pub const VOWEL: usize = 3;
pub const WHEEL_VOWEL: usize = 4;
pub const FORMANT_SHIFT: usize = 5;
pub const HUM: usize = 6;
pub const SOURCE: usize = 7;
pub const BREATH: usize = 8;
pub const BRIGHTNESS: usize = 9;
pub const VEL_BRIGHT: usize = 10;
pub const VIBRATO: usize = 11;
pub const VIBRATO_RATE: usize = 12;
pub const VIBRATO_DELAY: usize = 13;
pub const DRIFT: usize = 14;
pub const SINGERS: usize = 15;
pub const DETUNE: usize = 16;
pub const WIDTH: usize = 17;
pub const ATTACK: usize = 18;
pub const RELEASE: usize = 19;

pub static PARAMS: [P; 20] = [
    VOICES_PARAM,
    GLIDE_PARAM,
    P::choice("voice_type", "Voice", "Voice", &VOICE_TYPES, 1),
    P::float("vowel", "Vowel", "Voice", 0.0, 4.0, 0.0, Unit::Vowel),
    P::float(
        "wheel_vowel",
        "Wheel>Vowel",
        "Voice",
        -4.0,
        4.0,
        2.0,
        Unit::None,
    ),
    P::float(
        "formant_shift",
        "Formant Shift",
        "Voice",
        -12.0,
        12.0,
        0.0,
        Unit::Semitones,
    ),
    P::float("hum", "Hum", "Voice", 0.0, 1.0, 0.0, Unit::Percent),
    P::choice("source", "Source", "Source", &SOURCES, 0),
    P::float("breath", "Breath", "Source", 0.0, 1.0, 0.15, Unit::Percent),
    P::float(
        "brightness",
        "Brightness",
        "Source",
        0.0,
        1.0,
        0.5,
        Unit::Percent,
    ),
    P::float(
        "vel_bright",
        "Vel>Bright",
        "Source",
        0.0,
        1.0,
        0.5,
        Unit::Percent,
    ),
    P::float(
        "vibrato",
        "Vibrato",
        "Expression",
        0.0,
        100.0,
        25.0,
        Unit::Cents,
    )
    .step(1.0),
    P::float(
        "vibrato_rate",
        "Vib Rate",
        "Expression",
        2.0,
        9.0,
        5.5,
        Unit::Hz,
    ),
    P::float(
        "vibrato_delay",
        "Vib Delay",
        "Expression",
        0.0,
        2.0,
        0.35,
        Unit::Seconds,
    ),
    P::float("drift", "Drift", "Expression", 0.0, 1.0, 0.3, Unit::Percent),
    P::int(
        "singers",
        "Singers",
        "Ensemble",
        1,
        MAX_SINGERS as i32,
        1,
        Unit::None,
    ),
    P::float("detune", "Detune", "Ensemble", 0.0, 40.0, 12.0, Unit::Cents).step(1.0),
    P::float("width", "Width", "Ensemble", 0.0, 1.0, 0.6, Unit::Percent),
    P::float(
        "attack",
        "Attack",
        "Envelope",
        0.005,
        4.0,
        0.08,
        Unit::Seconds,
    )
    .exp(),
    P::float(
        "release",
        "Release",
        "Envelope",
        0.01,
        6.0,
        0.35,
        Unit::Seconds,
    )
    .exp(),
];

const MAX_SINGERS: usize = 6;
const FORMANTS: usize = 5;
const OUTPUT_GAIN: f32 = 1.5;

/// One vowel: formant frequencies (Hz), levels (dB) and bandwidths (Hz).
/// The cascade derives the relative formant levels from frequencies and
/// bandwidths, so the level column is kept for reference.
type Vowel = ([f32; FORMANTS], [f32; FORMANTS], [f32; FORMANTS]);

/// Vowel formants per voice type, in a, e, i, o, u order (the classic
/// singing-voice tables used by IRCAM's CHANT and Csound).
const VOWEL_TABLE: [[Vowel; 5]; 4] = [
    // Soprano
    [
        (
            [800.0, 1150.0, 2900.0, 3900.0, 4950.0],
            [0.0, -6.0, -32.0, -20.0, -50.0],
            [80.0, 90.0, 120.0, 130.0, 140.0],
        ),
        (
            [350.0, 2000.0, 2800.0, 3600.0, 4950.0],
            [0.0, -20.0, -15.0, -40.0, -56.0],
            [60.0, 100.0, 120.0, 150.0, 200.0],
        ),
        (
            [270.0, 2140.0, 2950.0, 3900.0, 4950.0],
            [0.0, -12.0, -26.0, -26.0, -44.0],
            [60.0, 90.0, 100.0, 120.0, 120.0],
        ),
        (
            [450.0, 800.0, 2830.0, 3800.0, 4950.0],
            [0.0, -11.0, -22.0, -22.0, -50.0],
            [70.0, 80.0, 100.0, 130.0, 135.0],
        ),
        (
            [325.0, 700.0, 2700.0, 3800.0, 4950.0],
            [0.0, -16.0, -35.0, -40.0, -60.0],
            [50.0, 60.0, 170.0, 180.0, 200.0],
        ),
    ],
    // Alto
    [
        (
            [800.0, 1150.0, 2800.0, 3500.0, 4950.0],
            [0.0, -4.0, -20.0, -36.0, -60.0],
            [80.0, 90.0, 120.0, 130.0, 140.0],
        ),
        (
            [400.0, 1600.0, 2700.0, 3300.0, 4950.0],
            [0.0, -24.0, -30.0, -35.0, -60.0],
            [60.0, 80.0, 120.0, 150.0, 200.0],
        ),
        (
            [350.0, 1700.0, 2700.0, 3700.0, 4950.0],
            [0.0, -20.0, -30.0, -36.0, -60.0],
            [50.0, 100.0, 120.0, 150.0, 200.0],
        ),
        (
            [450.0, 800.0, 2830.0, 3500.0, 4950.0],
            [0.0, -9.0, -16.0, -28.0, -55.0],
            [70.0, 80.0, 100.0, 130.0, 135.0],
        ),
        (
            [325.0, 700.0, 2530.0, 3500.0, 4950.0],
            [0.0, -12.0, -30.0, -40.0, -64.0],
            [50.0, 60.0, 170.0, 180.0, 200.0],
        ),
    ],
    // Tenor
    [
        (
            [650.0, 1080.0, 2650.0, 2900.0, 3250.0],
            [0.0, -6.0, -7.0, -8.0, -22.0],
            [80.0, 90.0, 120.0, 130.0, 140.0],
        ),
        (
            [400.0, 1700.0, 2600.0, 3200.0, 3580.0],
            [0.0, -14.0, -12.0, -14.0, -20.0],
            [70.0, 80.0, 100.0, 120.0, 120.0],
        ),
        (
            [290.0, 1870.0, 2800.0, 3250.0, 3540.0],
            [0.0, -15.0, -18.0, -20.0, -30.0],
            [40.0, 90.0, 100.0, 120.0, 120.0],
        ),
        (
            [400.0, 800.0, 2600.0, 2800.0, 3000.0],
            [0.0, -10.0, -12.0, -12.0, -26.0],
            [40.0, 80.0, 100.0, 120.0, 120.0],
        ),
        (
            [350.0, 600.0, 2700.0, 2900.0, 3300.0],
            [0.0, -20.0, -17.0, -14.0, -26.0],
            [40.0, 60.0, 100.0, 120.0, 120.0],
        ),
    ],
    // Bass
    [
        (
            [600.0, 1040.0, 2250.0, 2450.0, 2750.0],
            [0.0, -7.0, -9.0, -9.0, -20.0],
            [60.0, 70.0, 110.0, 120.0, 130.0],
        ),
        (
            [400.0, 1620.0, 2400.0, 2800.0, 3100.0],
            [0.0, -12.0, -9.0, -12.0, -18.0],
            [40.0, 80.0, 100.0, 120.0, 120.0],
        ),
        (
            [250.0, 1750.0, 2600.0, 3050.0, 3340.0],
            [0.0, -30.0, -16.0, -22.0, -28.0],
            [60.0, 90.0, 100.0, 120.0, 120.0],
        ),
        (
            [400.0, 750.0, 2400.0, 2600.0, 2900.0],
            [0.0, -11.0, -21.0, -20.0, -40.0],
            [40.0, 80.0, 100.0, 120.0, 120.0],
        ),
        (
            [350.0, 600.0, 2400.0, 2675.0, 2950.0],
            [0.0, -20.0, -32.0, -28.0, -36.0],
            [40.0, 80.0, 100.0, 120.0, 120.0],
        ),
    ],
];

/// Closed-mouth hum ("mmm"): a strong low nasal resonance, little above it.
const HUM_VOWEL: Vowel = (
    [250.0, 1100.0, 2300.0, 3200.0, 4200.0],
    [0.0, -26.0, -38.0, -46.0, -56.0],
    [60.0, 150.0, 200.0, 250.0, 300.0],
);

/// Formants at a vowel position (0..=4) for a voice type, morphing
/// frequencies on a log scale and levels in dB.
pub fn vowel_at(voice_type: usize, pos: f32, hum: f32) -> Vowel {
    let table = &VOWEL_TABLE[voice_type.min(3)];
    let pos = pos.clamp(0.0, 4.0);
    let i = (pos as usize).min(3);
    let t = pos - i as f32;
    let mix = |a: &Vowel, b: &Vowel, t: f32| -> Vowel {
        let mut v = *a;
        for k in 0..FORMANTS {
            v.0[k] = a.0[k] * (b.0[k] / a.0[k]).powf(t);
            v.1[k] = a.1[k] + (b.1[k] - a.1[k]) * t;
            v.2[k] = a.2[k] + (b.2[k] - a.2[k]) * t;
        }
        v
    };
    mix(
        &mix(&table[i], &table[i + 1], t),
        &HUM_VOWEL,
        hum.clamp(0.0, 1.0),
    )
}

/// Magnitude at `at` Hz of `Biquad::resonator(f, bw, sr)`.
fn resonator_gain(f: f32, bw: f32, at: f32, sr: f32) -> f32 {
    let r = (-std::f64::consts::PI * bw as f64 / sr as f64).exp();
    let a1 = -2.0 * r * (std::f64::consts::TAU * f as f64 / sr as f64).cos();
    let a2 = r * r;
    let w = std::f64::consts::TAU * at as f64 / sr as f64;
    let (re, im) = (
        1.0 + a1 * w.cos() + a2 * (2.0 * w).cos(),
        -a1 * w.sin() - a2 * (2.0 * w).sin(),
    );
    ((1.0 + a1 + a2) / (re * re + im * im).sqrt()) as f32
}

#[derive(Default)]
pub struct VocalShared {
    sample_rate: f32,
    source: usize,
    breath: f32,
    brightness: f32,
    vel_bright: f32,
    vibrato: f32,
    vibrato_rate: f32,
    vibrato_delay: f32,
    drift: f32,
    singers: usize,
    detune: f32,
    width: f32,
    env: AdsrParams,
}

#[derive(Clone, Copy, Default)]
struct Singer {
    phase: f32,
    vib_phase: f32,
    vib_rate: f32,
    /// Fixed detune (cents) and stereo gains for this singer.
    detune: f32,
    pan: (f32, f32),
    /// Slow random pitch (cents) and level drift, and their targets.
    jitter: f32,
    jitter_to: f32,
    shimmer: f32,
    shimmer_to: f32,
    /// Samples before this singer comes in (ensemble entries are ragged).
    delay: u32,
}

pub struct VocalVoice {
    active: bool,
    note: u8,
    pitch: f32,
    vel: f32,
    env: Env,
    age: u32,
    singers: [Singer; MAX_SINGERS],
    count: usize,
    rng: Rng,
}

impl VocalVoice {
    fn new(seed: u32) -> Self {
        VocalVoice {
            active: false,
            note: 60,
            pitch: 60.0,
            vel: 1.0,
            env: Env::default(),
            age: 0,
            singers: [Singer::default(); MAX_SINGERS],
            count: 1,
            rng: Rng::new(0x5106_0000 ^ seed.wrapping_mul(0x9E37_79B9)),
        }
    }
}

/// Rosenberg glottal pulse, as its time derivative (what the lips radiate):
/// a smooth opening, a sharper closing (shorter = brighter), then closed.
/// The step at closure is band-limited with PolyBLEP. Normalised so the
/// closing peak is -1.
#[inline]
fn glottal(phase: f32, dt: f32, open: f32, close: f32) -> f32 {
    use std::f32::consts::PI;
    let edge = open + close;
    let v = if phase < open {
        0.5 * PI / open * (PI * phase / open).sin()
    } else if phase < edge {
        -0.5 * PI / close * (0.5 * PI * (phase - open) / close).sin()
    } else {
        0.0
    };
    let jump = 0.5 * PI / close;
    let t = (phase - edge).rem_euclid(1.0);
    (v + 0.5 * jump * poly_blep(t, dt)) / jump
}

impl Voice for VocalVoice {
    type Shared = VocalShared;

    fn start(&mut self, note: u8, velocity: f32, from_note: Option<f32>, s: &VocalShared) {
        if !self.active {
            self.pitch = from_note.unwrap_or(note as f32);
            self.age = 0;
            let sr = s.sample_rate;
            self.count = s.singers.clamp(1, MAX_SINGERS);
            const SPREAD: [f32; MAX_SINGERS] = [0.0, -1.0, 1.0, -0.5, 0.5, -0.8];
            for (k, g) in self.singers.iter_mut().enumerate().take(self.count) {
                let solo = self.count == 1;
                let pan = if solo {
                    0.0
                } else {
                    s.width * (2.0 * k as f32 / (self.count - 1) as f32 - 1.0)
                };
                let (l, r) = crate::dsp::pan_gains(pan);
                *g = Singer {
                    phase: self.rng.unipolar(),
                    vib_phase: self.rng.unipolar(),
                    vib_rate: 1.0 + 0.08 * self.rng.bipolar(),
                    detune: if solo {
                        0.0
                    } else {
                        s.detune * (SPREAD[k] + 0.2 * self.rng.bipolar())
                    },
                    pan: (l * std::f32::consts::SQRT_2, r * std::f32::consts::SQRT_2),
                    delay: if k == 0 {
                        0
                    } else {
                        (self.rng.unipolar() * 0.035 * sr) as u32
                    },
                    ..Singer::default()
                };
            }
        } else if from_note.is_none() {
            self.pitch = note as f32;
        }
        self.note = note;
        self.vel = velocity;
        self.active = true;
        self.env.trigger();
    }

    fn release(&mut self) {
        self.env.release();
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn render(&mut self, s: &VocalShared, ctl: &Controls, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let sr = s.sample_rate;
        self.pitch = glide(self.pitch, self.note as f32, ctl.glide_coef, n);
        let base = midi_to_freq(self.pitch + ctl.pitch);
        // Vibrato fades in over half a second once the delay has passed.
        let secs = self.age as f32 / sr;
        let vib = s.vibrato * ((secs - s.vibrato_delay) / 0.5).clamp(0.0, 1.0);
        // Velocity opens the voice up: a quicker glottal closure is brighter.
        let bright = (s.brightness + s.vel_bright * (self.vel - 0.7)).clamp(0.0, 1.0);
        let open = 0.42 - 0.12 * bright;
        let close = open * (0.45 - 0.33 * bright);
        let norm = 1.0 / (self.count as f32).sqrt();
        let drift = s.drift;

        // Per-block drift targets: jitter (cents) and shimmer (level).
        for g in &mut self.singers[..self.count] {
            if self.rng.unipolar() < 0.06 {
                g.jitter_to = self.rng.bipolar() * drift * 12.0;
                g.shimmer_to = self.rng.bipolar() * drift * 0.15;
            }
            g.jitter += (g.jitter_to - g.jitter) * 0.05;
            g.shimmer += (g.shimmer_to - g.shimmer) * 0.05;
        }

        let mut level = 0.0f32;
        for i in 0..n {
            let amp = self.env.next(&s.env) * self.vel * norm;
            level = level.max(amp);
            let (mut ol, mut or) = (0.0, 0.0);
            for g in &mut self.singers[..self.count] {
                if g.delay > 0 {
                    g.delay -= 1;
                    continue;
                }
                let cents = g.detune + g.jitter + vib * sin_cycles(g.vib_phase);
                let f = base * (cents / 1200.0).exp2();
                let dt = (f / sr).min(0.45);
                let p = g.phase;
                let src = match s.source {
                    0 => glottal(p, dt, open, close),
                    // A saw is far denser than a glottal pulse: level-match it.
                    1 => 0.4 * (2.0 * p - 1.0 - poly_blep(p, dt)),
                    _ => {
                        // Narrow pulse train (buzz), DC removed.
                        let w = 0.1;
                        let naive = if p < w { 1.0 } else { 0.0 };
                        2.0 * (naive + poly_blep(p, dt)
                            - poly_blep((p - w).rem_euclid(1.0), dt)
                            - w)
                    }
                };
                // Aspiration: noise, louder while the folds are open.
                let open_now = if p < open + close { 1.0 } else { 0.35 };
                let noise = self.rng.bipolar() * open_now;
                let x = (src * (1.0 - 0.5 * s.breath) + noise * s.breath * 0.6) * (1.0 + g.shimmer);
                ol += x * g.pan.0;
                or += x * g.pan.1;
                g.phase = (p + dt).fract();
                g.vib_phase = (g.vib_phase + s.vibrato_rate * g.vib_rate / sr).fract();
            }
            l[i] += ol * amp;
            r[i] += or * amp;
        }
        self.age = self.age.saturating_add(n as u32);
        if self.env.is_idle() || level < 1e-5 && self.env.stage == crate::dsp::Stage::Release {
            self.active = false;
        }
    }
}

pub struct VocalSynth {
    pub poly: Poly<VocalVoice>,
    pub shared: VocalShared,
    sample_rate: f32,
    voice_type: usize,
    vowel: f32,
    wheel_vowel: f32,
    formant_shift: f32,
    hum: f32,
    /// Smoothed vowel position and hum, so 7-bit wheel moves don't step.
    vowel_now: f32,
    hum_now: f32,
    /// What the formant filters are currently tuned to.
    tuned: (usize, f32, f32, f32),
    formants: [[Biquad; FORMANTS]; 2],
    /// Closed-mouth low-pass for Hum (one-pole coefficient and state).
    hum_lp: f32,
    /// 1 / the cascade's gain at the first formant, so every vowel peaks
    /// at the same level and morphing doesn't pump the volume.
    norm: f32,
    hum_z: [f32; 2],
    tmp_l: [f32; MAX_BLOCK],
    tmp_r: [f32; MAX_BLOCK],
}

impl VocalSynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = VocalSynth {
            poly: Poly::new(|i| VocalVoice::new(i as u32 + 1)),
            shared: VocalShared::default(),
            sample_rate,
            voice_type: 1,
            vowel: 0.0,
            wheel_vowel: 0.0,
            formant_shift: 0.0,
            hum: 0.0,
            vowel_now: f32::NAN,
            hum_now: 0.0,
            tuned: (usize::MAX, f32::NAN, f32::NAN, f32::NAN),
            formants: [[Biquad::resonator(500.0, 100.0, sample_rate); FORMANTS]; 2],
            hum_lp: 0.0,
            norm: 1.0,
            hum_z: [0.0; 2],
            tmp_l: [0.0; MAX_BLOCK],
            tmp_r: [0.0; MAX_BLOCK],
        };
        s.update(&super::SynthKind::Vocal.defaults());
        s
    }

    pub fn update(&mut self, params: &[f32]) {
        let sr = self.sample_rate;
        let p = &params[COMMON.len()..];
        self.poly.update_common(params, p[VOICES], p[GLIDE], sr);
        self.voice_type = (p[VOICE_TYPE].round().max(0.0) as usize).min(VOICE_TYPES.len() - 1);
        self.vowel = p[VOWEL];
        self.wheel_vowel = p[WHEEL_VOWEL];
        self.formant_shift = p[FORMANT_SHIFT];
        self.hum = p[HUM];
        let s = &mut self.shared;
        s.sample_rate = sr;
        s.source = (p[SOURCE].round().max(0.0) as usize).min(SOURCES.len() - 1);
        s.breath = p[BREATH];
        s.brightness = p[BRIGHTNESS];
        s.vel_bright = p[VEL_BRIGHT];
        s.vibrato = p[VIBRATO];
        s.vibrato_rate = p[VIBRATO_RATE];
        s.vibrato_delay = p[VIBRATO_DELAY];
        s.drift = p[DRIFT];
        s.singers = p[SINGERS].round().max(1.0) as usize;
        s.detune = p[DETUNE];
        s.width = p[WIDTH];
        s.env = AdsrParams::new(p[ATTACK], 0.3, 1.0, p[RELEASE], sr);
    }

    /// Where the vowel knob and mod wheel put the vowel (0 = a .. 4 = u).
    fn vowel_target(&self) -> f32 {
        (self.vowel + self.poly.ctl.modwheel * self.wheel_vowel).clamp(0.0, 4.0)
    }

    /// Retune the formant filters for the current vowel (keeping their
    /// state, so a moving vowel doesn't click).
    fn tune_formants(&mut self) {
        let target = self.vowel_target();
        if self.vowel_now.is_nan() {
            self.vowel_now = target;
            self.hum_now = self.hum;
        }
        self.vowel_now += (target - self.vowel_now) * 0.15;
        self.hum_now += (self.hum - self.hum_now) * 0.15;
        let key = (
            self.voice_type,
            self.vowel_now,
            self.hum_now,
            self.formant_shift,
        );
        if key == self.tuned {
            return;
        }
        self.tuned = key;
        let sr = self.sample_rate;
        let (freqs, _, widths) = vowel_at(self.voice_type, self.vowel_now, self.hum_now);
        let shift = (self.formant_shift / 12.0).exp2();
        let f1 = (freqs[0] * shift).clamp(80.0, 0.45 * sr);
        let mut gain_at_f1 = 1.0;
        for k in 0..FORMANTS {
            let f = (freqs[k] * shift).clamp(80.0, 0.45 * sr);
            let bw = widths[k] * shift.sqrt();
            let coefs = Biquad::resonator(f, bw, sr);
            gain_at_f1 *= resonator_gain(f, bw, f1, sr);
            for ch in &mut self.formants {
                ch[k].retune(coefs);
            }
        }
        self.norm = 1.0 / gain_at_f1.max(1e-3);
        // A closed mouth radiates little above the nasal murmur.
        let fc = 12_000.0 * (700.0f32 / 12_000.0).powf(self.hum_now);
        self.hum_lp = (-std::f32::consts::TAU * fc / sr).exp();
    }

    /// One sample through the formant cascade of channel `ch`.
    fn formant(&mut self, ch: usize, x: f32) -> f32 {
        let mut y = x;
        for b in &mut self.formants[ch] {
            y = b.process(y);
        }
        let z = &mut self.hum_z[ch];
        *z = y + (*z - y) * self.hum_lp;
        *z * self.norm * OUTPUT_GAIN
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        self.tune_formants();
        self.tmp_l[..n].fill(0.0);
        self.tmp_r[..n].fill(0.0);
        self.poly
            .render(&self.shared, &mut self.tmp_l[..n], &mut self.tmp_r[..n]);
        for i in 0..n {
            l[i] += self.formant(0, self.tmp_l[i]);
            r[i] += self.formant(1, self.tmp_r[i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::Sample;
    use crate::synth::SynthKind;

    fn synth(overrides: &[(&str, f32)]) -> VocalSynth {
        crate::dsp::init_tables();
        let mut s = VocalSynth::new(48_000.0);
        let mut p = SynthKind::Vocal.defaults();
        for (k, v) in overrides {
            p[SynthKind::Vocal.index_of(k).unwrap()] = *v;
        }
        s.update(&p);
        s
    }

    fn render(s: &mut VocalSynth, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut ol, mut or) = (Vec::new(), Vec::new());
        let (mut l, mut r) = ([0.0f32; MAX_BLOCK], [0.0f32; MAX_BLOCK]);
        while ol.len() < frames {
            l.fill(0.0);
            r.fill(0.0);
            s.render(&mut l, &mut r);
            ol.extend_from_slice(&l);
            or.extend_from_slice(&r);
        }
        (ol, or)
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// Magnitude response (dB) of the formant filters at `f`, measured on
    /// fresh copies so the synth's own filter state is untouched.
    fn response(s: &VocalSynth, f: f32) -> f32 {
        let mut bank: [Biquad; FORMANTS] = std::array::from_fn(|k| {
            let mut b = Biquad::default();
            b.retune(s.formants[0][k]);
            b
        });
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for n in 0..24_000 {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let y = bank.iter_mut().fold(x, |y, b| b.process(y));
            let ph = std::f64::consts::TAU * f as f64 * n as f64 / 48_000.0;
            re += y as f64 * ph.cos();
            im -= y as f64 * ph.sin();
        }
        20.0 * (re * re + im * im).sqrt().log10() as f32
    }

    /// Frequency of the strongest response between `lo` and `hi`.
    fn peak_between(s: &VocalSynth, lo: f32, hi: f32) -> f32 {
        let mut best = (lo, f32::MIN);
        let mut f = lo;
        while f <= hi {
            let db = response(s, f);
            if db > best.1 {
                best = (f, db);
            }
            f *= 1.01;
        }
        best.0
    }

    fn settle(s: &mut VocalSynth) {
        for _ in 0..200 {
            s.tune_formants();
        }
    }

    #[test]
    fn vowels_put_formants_where_the_tables_say() {
        for (vt, _) in VOICE_TYPES.iter().enumerate() {
            for (v, vowel) in VOWEL_TABLE[vt].iter().enumerate() {
                let mut s = synth(&[("voice_type", vt as f32), ("vowel", v as f32)]);
                settle(&mut s);
                let (f1, f2) = (vowel.0[0], vowel.0[1]);
                let p1 = peak_between(&s, f1 * 0.7, (f1 * f2).sqrt());
                assert!(
                    (p1 / f1 - 1.0).abs() < 0.08,
                    "{} {}: F1 {p1} vs {f1}",
                    VOICE_TYPES[vt],
                    v
                );
            }
        }
        // a vs i: open vowel high F1, close front vowel low F1 and high F2.
        let mut a = synth(&[("vowel", 0.0)]);
        let mut i = synth(&[("vowel", 2.0)]);
        settle(&mut a);
        settle(&mut i);
        assert!(response(&a, 800.0) > response(&i, 800.0) + 10.0);
        assert!(response(&i, 1_700.0) > response(&a, 1_700.0) + 6.0);
    }

    #[test]
    fn mod_wheel_moves_the_vowel() {
        let mut s = synth(&[("vowel", 0.0), ("wheel_vowel", 2.0)]);
        s.poly.set_modwheel(1.0);
        settle(&mut s);
        assert!((s.vowel_now - 2.0).abs() < 0.01, "{}", s.vowel_now);
        let f1 = VOWEL_TABLE[1][2].0[0];
        let p1 = peak_between(&s, 200.0, 600.0);
        assert!((p1 / f1 - 1.0).abs() < 0.08, "F1 {p1} vs i {f1}");
    }

    /// The singers' source is in tune for every source type. (Measured
    /// before the formants: a strong formant on the 2nd harmonic, as in an
    /// alto's "ah" on A4, fools a pitch detector though listeners still hear
    /// the fundamental, just as with a real voice.)
    #[test]
    fn sings_in_tune() {
        for source in 0..SOURCES.len() {
            for note in [43u8, 57, 69, 79] {
                let s = synth(&[
                    ("source", source as f32),
                    ("vibrato", 0.0),
                    ("drift", 0.0),
                    ("breath", 0.0),
                ]);
                let mut poly = s.poly;
                poly.note_on(note, 0.8, &s.shared);
                let mut out = Vec::new();
                let (mut l, mut r) = ([0.0f32; MAX_BLOCK], [0.0f32; MAX_BLOCK]);
                while out.len() < 24_000 {
                    l.fill(0.0);
                    r.fill(0.0);
                    poly.render(&s.shared, &mut l, &mut r);
                    out.extend_from_slice(&l);
                }
                let p = Sample::new("t", out[4_800..].to_vec(), None, 48_000.0)
                    .detect_pitch()
                    .unwrap_or(f32::NAN);
                assert!(
                    (p - note as f32).abs() < 0.05,
                    "source {source} note {note}: {p}"
                );
            }
        }
    }

    /// The cascade passes the fundamental below the first formant (parallel
    /// band-passes would cut it and make voices thin).
    #[test]
    fn formants_keep_the_fundamental() {
        for vt in 0..VOICE_TYPES.len() {
            for (v, vowel) in VOWEL_TABLE[vt].iter().enumerate() {
                let mut s = synth(&[("voice_type", vt as f32), ("vowel", v as f32)]);
                settle(&mut s);
                let f1 = vowel.0[0];
                let low = response(&s, 60.0);
                let below_f1 = response(&s, f1 * 0.5);
                assert!(
                    below_f1 > low - 1.0,
                    "{} vowel {v}: {below_f1:.1} vs {low:.1} dB",
                    VOICE_TYPES[vt]
                );
            }
        }
    }

    #[test]
    fn ensemble_is_wide_but_not_louder() {
        let level = |singers: f32| {
            let mut s = synth(&[("singers", singers), ("width", 0.8)]);
            s.poly.note_on(57, 0.8, &s.shared);
            render(&mut s, 48_000)
        };
        let (sl, sr) = level(1.0);
        let (el, er) = level(6.0);
        let db = 20.0 * ((rms(&el) + rms(&er)) / (rms(&sl) + rms(&sr))).log10();
        assert!(db.abs() < 3.0, "6 singers {db:.1} dB vs 1");
        let diff =
            |l: &[f32], r: &[f32]| rms(&l.iter().zip(r).map(|(a, b)| a - b).collect::<Vec<_>>());
        assert!(diff(&sl, &sr) < 1e-4, "a solo voice is centred");
        assert!(diff(&el, &er) > 0.1 * rms(&el), "an ensemble is wide");
    }

    #[test]
    fn notes_end_after_release() {
        let mut s = synth(&[("release", 0.1)]);
        s.poly.note_on(60, 0.8, &s.shared);
        render(&mut s, 9_600);
        s.poly.note_off(60);
        render(&mut s, 48_000);
        assert_eq!(s.poly.active_voices(), 0);
    }
}
