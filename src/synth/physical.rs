//! Physical-modelling synth with four models:
//!
//! - **String**: extended Karplus-Strong. A delay loop tuned to the note's
//!   period (with a first-order Thiran all-pass for the fractional part)
//!   contains a one-zero damping filter (brightness), an optional chain of
//!   dispersion all-passes (stiffness) and a loop gain set from the decay
//!   time. The phase delay of every loop element is measured at the
//!   fundamental and subtracted, so notes stay in tune at any setting. It is
//!   excited by filtered noise, comb-filtered by pick position.
//! - **Mallet**: modal synthesis. A short raised-cosine strike (its width set
//!   by hardness) rings a bank of two-pole resonators tuned to a material's
//!   partial ratios, with higher modes decaying faster.
//! - **Piano**: one to three slightly detuned stiff strings per note (as on a
//!   real piano), struck by a felt hammer whose contact time shortens with
//!   velocity, with register-dependent stiffness and decay, dampers that act
//!   on key release (none in the top octave and a half) and a hammer thump.
//! - **Bowed**: a bowed-string waveguide (McIntyre/Schumacher/Woodhouse,
//!   Smith). The string is split at the bow into neck and bridge segments and
//!   the bow drives it through a nonlinear stick-slip friction curve. Bow
//!   speed (velocity + mod wheel), pressure and position shape the tone;
//!   delayed vibrato and a violin-family body filter finish it.

use crate::dsp::{Biquad, FilterMode, Rng, Svf, SvfCoefs, midi_to_freq, pan_gains, sin_cycles};
use crate::params::{ParamDesc as P, Unit};

use super::{COMMON, Controls, MAX_BLOCK, Poly, VOICES_PARAM, Voice};

pub const MODELS: [&str; 4] = ["String", "Mallet", "Piano", "Bowed"];
pub const MATERIALS: [&str; 7] = [
    "Wood", "Metal", "Glass", "Free Bar", "Bell", "Membrane", "Tine",
];
pub const BOWED_BODIES: [&str; 4] = ["Violin", "Viola", "Cello", "Bass"];
/// "Box" is the original generic three-resonance body; the others are modal
/// guitar bodies.
pub const BODY_TYPES: [&str; 4] = ["Box", "Dreadnought", "Classical", "Parlor"];

pub const VOICES: usize = 0;
pub const MODEL: usize = 1;
pub const HARDNESS: usize = 2;
pub const POSITION: usize = 3;
pub const VEL_BRIGHT: usize = 4;
pub const BOW_PRESSURE: usize = 5;
pub const BOW_POSITION: usize = 6;
pub const BOW_ATTACK: usize = 7;
pub const VIBRATO: usize = 8;
pub const VIBRATO_RATE: usize = 9;
pub const BOWED_BODY: usize = 10;
pub const DECAY: usize = 11;
pub const HF_DAMP: usize = 12;
pub const STIFFNESS: usize = 13;
pub const UNISON: usize = 14;
pub const MATERIAL: usize = 15;
pub const BODY: usize = 16;
pub const RELEASE_DAMP: usize = 17;
pub const TONE: usize = 18;
pub const WIDTH: usize = 19;
pub const DECAY_TRACK: usize = 20;
pub const TWO_STAGE: usize = 21;
pub const BODY_TYPE: usize = 22;
pub const SYMPATHETIC: usize = 23;

pub static PARAMS: [P; 24] = [
    VOICES_PARAM,
    P::choice("model", "Model", "Model", &MODELS, 0),
    P::float(
        "hardness",
        "Hardness",
        "Exciter",
        0.0,
        1.0,
        0.5,
        Unit::Percent,
    ),
    P::float(
        "position",
        "Position",
        "Exciter",
        0.02,
        0.5,
        0.18,
        Unit::Percent,
    ),
    P::float(
        "vel_bright",
        "Vel>Bright",
        "Exciter",
        0.0,
        1.0,
        0.6,
        Unit::Percent,
    ),
    P::float(
        "bow_pressure",
        "Bow Pressure",
        "Bow",
        0.0,
        1.0,
        0.55,
        Unit::Percent,
    ),
    P::float(
        "bow_position",
        "Bow Position",
        "Bow",
        0.08,
        0.25,
        0.13,
        Unit::Percent,
    ),
    P::float(
        "bow_attack",
        "Bow Attack",
        "Bow",
        0.005,
        2.0,
        0.12,
        Unit::Seconds,
    )
    .exp(),
    P::float("vibrato", "Vibrato", "Bow", 0.0, 50.0, 18.0, Unit::Cents).step(1.0),
    P::float(
        "vibrato_rate",
        "Vibrato Rate",
        "Bow",
        3.0,
        8.0,
        5.5,
        Unit::Hz,
    ),
    P::choice("bowed_body", "Instrument", "Bow", &BOWED_BODIES, 0),
    P::float(
        "decay",
        "Decay",
        "Resonator",
        0.05,
        30.0,
        3.0,
        Unit::Seconds,
    )
    .exp(),
    P::float(
        "hf_damp",
        "HF Damping",
        "Resonator",
        0.0,
        1.0,
        0.4,
        Unit::Percent,
    ),
    P::float(
        "stiffness",
        "Stiffness",
        "Resonator",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
    P::float(
        "unison",
        "Unison Detune",
        "Resonator",
        0.0,
        12.0,
        1.2,
        Unit::Cents,
    )
    .step(0.1),
    P::choice("material", "Material", "Resonator", &MATERIALS, 0),
    P::float("body", "Body", "Response", 0.0, 1.0, 0.25, Unit::Percent),
    P::float(
        "release_damp",
        "Release Damp",
        "Response",
        0.0,
        1.0,
        0.6,
        Unit::Percent,
    ),
    P::float(
        "tone",
        "Tone",
        "Response",
        200.0,
        20_000.0,
        20_000.0,
        Unit::Hz,
    )
    .exp(),
    P::float(
        "width",
        "Stereo Width",
        "Response",
        0.0,
        1.0,
        0.4,
        Unit::Percent,
    ),
    // Appended so older patches keep their layout; the defaults reproduce
    // the earlier sound.
    P::float(
        "decay_track",
        "Decay Track",
        "Resonator",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
    P::float(
        "two_stage",
        "Two-Stage Decay",
        "Resonator",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
    P::choice("body_type", "Body Type", "Response", &BODY_TYPES, 0),
    P::float(
        "sympathetic",
        "Sympathetic",
        "Response",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
];

/// Which of this synth's own parameters apply to the selected model.
/// `p` is the synth-specific slice (after the common section).
pub fn param_visible(p: &[f32], local: usize) -> bool {
    let model = model_from(p[MODEL]);
    match local {
        HARDNESS | POSITION | VEL_BRIGHT | STIFFNESS | DECAY | RELEASE_DAMP => {
            model != Model::Bowed
        }
        BOW_PRESSURE | BOW_POSITION | BOW_ATTACK | VIBRATO | VIBRATO_RATE | BOWED_BODY => {
            model == Model::Bowed
        }
        UNISON => model == Model::Piano,
        DECAY_TRACK | TWO_STAGE | BODY_TYPE | SYMPATHETIC => model == Model::String,
        MATERIAL => model == Model::Mallet,
        _ => true,
    }
}

/// Partial ratios, relative amplitudes and a decay multiplier per material.
struct Material {
    ratios: &'static [f32],
    amps: &'static [f32],
    decay: f32,
}

const MATERIAL_TABLE: [Material; 7] = [
    // Tuned marimba bar: partials tuned to 1:4:10.
    Material {
        ratios: &[1.0, 3.99, 10.0],
        amps: &[1.0, 0.45, 0.2],
        decay: 0.5,
    },
    // Vibraphone-like metal bar: long, nearly harmonic ring.
    Material {
        ratios: &[1.0, 4.0, 10.0, 17.6],
        amps: &[1.0, 0.35, 0.18, 0.08],
        decay: 3.0,
    },
    Material {
        ratios: &[1.0, 2.32, 4.25, 6.63, 9.38],
        amps: &[1.0, 0.6, 0.4, 0.3, 0.2],
        decay: 1.5,
    },
    // Free (untuned) bar, xylophone-like.
    Material {
        ratios: &[1.0, 2.756, 5.404, 8.933, 13.344],
        amps: &[1.0, 0.5, 0.35, 0.25, 0.15],
        decay: 0.8,
    },
    // Church bell: hum, prime, tierce, quint, nominal and upper partials.
    Material {
        ratios: &[0.5, 1.0, 1.183, 1.506, 2.0, 2.514, 2.662, 3.011],
        amps: &[0.6, 1.0, 0.8, 0.5, 0.7, 0.3, 0.25, 0.2],
        decay: 4.0,
    },
    // Circular membrane (tuned drum).
    Material {
        ratios: &[1.0, 1.594, 2.136, 2.296, 2.653, 2.918, 3.156, 3.501],
        amps: &[1.0, 0.8, 0.6, 0.55, 0.45, 0.4, 0.3, 0.25],
        decay: 0.35,
    },
    // Kalimba / tine.
    Material {
        ratios: &[1.0, 5.9, 15.4],
        amps: &[1.0, 0.2, 0.08],
        decay: 2.0,
    },
];

const MAX_MODES: usize = 8;
const MAX_STRINGS: usize = 3;
const DELAY_LEN: usize = 4096;
const DISPERSION_STAGES: usize = 4;
const STRING_GAIN: f32 = 0.45;
/// How much velocity sets strike strength (see `dsp::velocity_gain`).
const VELOCITY_SENS: f32 = 0.65;
/// Extra level of the slow polarization: it starts quieter but outlasts the
/// first, which is what makes the decay two-stage.
const TWO_STAGE_TAIL: f32 = 1.4;
const PIANO_GAIN: f32 = 0.16;
const MALLET_GAIN: f32 = 0.32;
const BOWED_GAIN: f32 = 0.4;
/// Bowed strings need substantial losses for a stable Helmholtz motion
/// (with too little loss the bow locks onto higher modes): STK's value.
const BOWED_LOSS: f32 = 0.95;
/// Piano strings above this note have no dampers and ring after release.
const PIANO_UNDAMPED_FROM: u8 = 89;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Model {
    #[default]
    String,
    Mallet,
    Piano,
    Bowed,
}

fn model_from(v: f32) -> Model {
    match v.round() as usize {
        1 => Model::Mallet,
        2 => Model::Piano,
        3 => Model::Bowed,
        _ => Model::String,
    }
}

#[derive(Default)]
pub struct PhysicalShared {
    sample_rate: f32,
    model: Model,
    hardness: f32,
    position: f32,
    vel_bright: f32,
    bow_pressure: f32,
    bow_position: f32,
    bow_attack: f32,
    vibrato: f32,
    vibrato_rate: f32,
    bowed_body: usize,
    decay: f32,
    hf_damp: f32,
    stiffness: f32,
    unison: f32,
    material: usize,
    body: f32,
    release_damp: f32,
    width: f32,
    decay_track: f32,
    two_stage: f32,
}

/// Phase delay in samples of H(e^{jw}) given its value at `w`.
fn phase_delay(re: f64, im: f64, w: f64) -> f64 {
    -im.atan2(re) / w
}

/// First-order all-pass (a + z^-1)/(1 + a z^-1) evaluated at `w`.
fn allpass_at(a: f64, w: f64) -> (f64, f64) {
    let (c, s) = (w.cos(), w.sin());
    let (nr, ni) = (a + c, -s);
    let (dr, di) = (1.0 + a * c, -a * s);
    let den = dr * dr + di * di;
    ((nr * dr + ni * di) / den, (ni * dr - nr * di) / den)
}

// ---------------------------------------------------------------------------
// Karplus-Strong string loop (used by String and Piano)
// ---------------------------------------------------------------------------

struct WaveString {
    buf: Vec<f32>,
    w: usize,
    period: f32,
    n_int: usize,
    ap_a: f32,
    ap_x1: f32,
    ap_y1: f32,
    disp_a: f32,
    disp: [(f32, f32); DISPERSION_STAGES],
    damp_s: f32,
    lp_x1: f32,
    loop_gain: f32,
}

impl WaveString {
    fn new() -> Self {
        WaveString {
            buf: vec![0.0; DELAY_LEN],
            w: 0,
            period: 100.0,
            n_int: 100,
            ap_a: 0.0,
            ap_x1: 0.0,
            ap_y1: 0.0,
            disp_a: 0.0,
            disp: [(0.0, 0.0); DISPERSION_STAGES],
            damp_s: 0.25,
            lp_x1: 0.0,
            loop_gain: 0.99,
        }
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
        self.ap_x1 = 0.0;
        self.ap_y1 = 0.0;
        self.lp_x1 = 0.0;
        self.disp = [(0.0, 0.0); DISPERSION_STAGES];
    }

    /// Tune the loop to `f0` with the given damping (0..1), stiffness (0..1)
    /// and 60 dB decay time.
    fn tune(&mut self, sr: f32, f0: f32, hf_damp: f32, stiffness: f32, t60: f32) {
        let f0 = f0.clamp(sr / (DELAY_LEN as f32 - 8.0), sr * 0.2);
        let w = std::f64::consts::TAU * f0 as f64 / sr as f64;
        let period = (sr / f0) as f64;

        // The damping filter loses a little at the fundamental every pass and
        // the loop gain can't exceed 1 (DC would grow), so cap the damping at
        // what still allows the requested decay; otherwise treble notes, which
        // pass through the loop thousands of times a second, die instantly.
        let per_period = 10f32.powf(-3.0 / (f0 * t60.max(0.005)));
        let wanted = 0.03 + 0.47 * hf_damp;
        let c = (1.0 - per_period * per_period) / (2.0 * (1.0 - w.cos() as f32));
        let max_s = if c >= 0.25 {
            0.5
        } else {
            (1.0 - (1.0 - 4.0 * c).sqrt()) / 2.0
        };
        self.damp_s = wanted.min(max_s * 0.98);
        let sd = self.damp_s as f64;
        let (lp_re, lp_im) = ((1.0 - sd) + sd * w.cos(), -sd * w.sin());
        let lp_delay = phase_delay(lp_re, lp_im, w);
        let lp_mag = (lp_re * lp_re + lp_im * lp_im).sqrt() as f32;

        self.disp_a = -0.75 * stiffness.clamp(0.0, 1.0);
        let (dr, di) = allpass_at(self.disp_a as f64, w);
        let disp_delay = if stiffness > 0.0 {
            phase_delay(dr, di, w) * DISPERSION_STAGES as f64
        } else {
            0.0
        };

        let remaining = (period - lp_delay - disp_delay).max(2.5);
        // Keep the Thiran all-pass delay within 0.5..1.5 samples for accuracy.
        let n = (remaining - 0.5).floor().max(1.0);
        let d = remaining - n;
        self.n_int = n as usize;
        self.ap_a = ((1.0 - d) / (1.0 + d)) as f32;
        self.period = period as f32;

        // Loop gain for a 60 dB decay, compensating the damping filter's loss
        // at the fundamental.
        self.loop_gain = (per_period / lp_mag).min(0.9999);
    }

    /// Add an excitation into the samples the loop reads next, so re-strikes
    /// of a ringing string add to its vibration.
    fn inject(&mut self, exc: &[f32], amp: f32) {
        let mask = DELAY_LEN - 1;
        let start = self.w + DELAY_LEN - self.n_int;
        for (i, e) in exc.iter().take(self.n_int).enumerate() {
            self.buf[(start + i) & mask] += e * amp;
        }
    }

    #[inline]
    fn tick(&mut self) -> f32 {
        self.tick_in(0.0)
    }

    /// One sample, with `input` driving the string where the loop closes
    /// (the bridge): energy builds only at the string's own resonances.
    #[inline]
    fn tick_in(&mut self, input: f32) -> f32 {
        let mask = DELAY_LEN - 1;
        let x = self.buf[(self.w + DELAY_LEN - self.n_int) & mask];
        let a = self.ap_a;
        let mut y = a * x + self.ap_x1 - a * self.ap_y1;
        self.ap_x1 = x;
        self.ap_y1 = y;
        let da = self.disp_a;
        if da != 0.0 {
            for st in &mut self.disp {
                let out = da * y + st.0 - da * st.1;
                st.0 = y;
                st.1 = out;
                y = out;
            }
        }
        let sd = self.damp_s;
        let z = (1.0 - sd) * y + sd * self.lp_x1;
        self.lp_x1 = y;
        self.buf[self.w] = z * self.loop_gain + input;
        self.w = (self.w + 1) & mask;
        z
    }
}

// ---------------------------------------------------------------------------
// Bowed string waveguide
// ---------------------------------------------------------------------------

/// Stick-slip friction: reflection coefficient of the bow as a function of
/// the bow/string velocity difference (STK's "bow table").
#[inline]
fn bow_table(dv: f32, slope: f32) -> f32 {
    let x = (dv * slope).abs() + 0.75;
    let x2 = x * x;
    (1.0 / (x2 * x2)).clamp(0.01, 0.98)
}

/// String-loss filter pole, with its cutoff a fixed number of harmonics above
/// the note (16 bright .. 4 dark). Real string losses are relative to the
/// string's own harmonics; a fixed cutoff lets low notes fall into
/// multi-slip regimes (their upper harmonics barely damped) and over-damps
/// high notes towards a sine.
fn bowed_loss_pole(s: &PhysicalShared, f0: f32) -> f32 {
    let harmonics = 16.0 * 0.25f32.powf(s.hf_damp);
    (-std::f32::consts::TAU * f0 * harmonics / s.sample_rate)
        .exp()
        .clamp(0.05, 0.95)
}

/// Bow speed (velocity 0.8) at which `bow_slope` was measured.
const BOW_REF_SPEED: f32 = 0.194;

/// Friction-curve slope from bow pressure (plus mod wheel) and position.
///
/// Measured across G2-G6, slopes much above ~2.2 leave too little bow force
/// for Helmholtz motion on many notes (double slip, or the string sticking),
/// so pressure spans the stable window from light to heavy. The mod wheel
/// adds force, which stays stable (adding bow speed alone does not), and
/// force rises automatically for bow positions near the bridge, where the
/// minimum force grows steeply (Schelleng).
fn bow_slope(s: &PhysicalShared, modwheel: f32) -> f32 {
    let pressure = (s.bow_pressure + 0.5 * modwheel).min(1.0);
    let near = (s.bow_position / 0.13).min(1.0).powf(1.6);
    (2.0 - 0.9 * pressure) * near
}

struct BowedString {
    neck: Vec<f32>,
    bridge: Vec<f32>,
    w: usize,
    d_bridge: f32,
    d_neck: f32,
    d_neck_target: f32,
    lp_pole: f32,
    lp_y: f32,
    loss: f32,
    level: f32,
    slope: f32,
    speed: f32,
    contact: f32,
    attack_coef: f32,
    lift_coef: f32,
    vib_phase: f32,
    vib_time: f32,
    body: [Biquad; 6],
}

impl BowedString {
    fn new() -> Self {
        BowedString {
            neck: vec![0.0; DELAY_LEN],
            bridge: vec![0.0; DELAY_LEN],
            w: 0,
            d_bridge: 10.0,
            d_neck: 90.0,
            d_neck_target: 90.0,
            lp_pole: 0.6,
            lp_y: 0.0,
            loss: BOWED_LOSS,
            level: 1.0,
            slope: 3.0,
            speed: 0.0,
            contact: 0.0,
            attack_coef: 0.999,
            lift_coef: 0.99,
            vib_phase: 0.0,
            vib_time: 0.0,
            body: [Biquad::default(); 6],
        }
    }

    fn clear(&mut self) {
        self.neck.fill(0.0);
        self.bridge.fill(0.0);
        self.lp_y = 0.0;
        self.speed = 0.0;
        self.contact = 0.0;
        self.vib_time = 0.0;
        for b in &mut self.body {
            *b = Biquad::default();
        }
    }

    /// Total loop delay in samples for `f0`, minus the string filter's phase delay.
    fn loop_delay(&self, sr: f32, f0: f32) -> f32 {
        let w = std::f64::consts::TAU * f0 as f64 / sr as f64;
        // One-pole low-pass (1-p)/(1 - p z^-1).
        let p = self.lp_pole as f64;
        let (dr, di) = (1.0 - p * w.cos(), p * w.sin());
        let lp_delay = -(-di).atan2(dr) / w;
        (sr / f0) - lp_delay as f32
    }

    fn tune(&mut self, s: &PhysicalShared, f0: f32) {
        let sr = s.sample_rate;
        let f0 = f0.clamp(sr / (DELAY_LEN as f32 - 8.0), sr * 0.15);
        self.lp_pole = bowed_loss_pole(s, f0);
        let total = self.loop_delay(sr, f0);
        // Both segments are fractional (as in STK): rounding the short
        // bow-to-bridge segment would distort high notes badly.
        self.d_bridge = (total * s.bow_position).clamp(1.0, (DELAY_LEN / 2) as f32);
        self.d_neck_target = (total - self.d_bridge).max(2.0);
        self.loss = BOWED_LOSS;
        self.slope = bow_slope(s, 0.0);
        // A bow nearer the bridge drives a bigger string motion (~1/beta).
        self.level = s.bow_position / 0.13;
        self.attack_coef = (-1.0 / (s.bow_attack.max(0.005) * 0.3 * sr)).exp();
        self.lift_coef = (-1.0 / (0.04 * sr)).exp();
        self.set_body(s);
    }

    fn set_body(&mut self, s: &PhysicalShared) {
        let sr = s.sample_rate;
        // Violin-family body: air (A0) and main wood resonances, the "bridge
        // hill" and a high roll-off, scaled down for larger instruments.
        let scale = [1.0, 0.82, 0.45, 0.3][s.bowed_body.min(3)];
        let new = [
            Biquad::high_pass(180.0 * scale, 0.7, sr),
            Biquad::peaking(275.0 * scale, 9.0, 5.0, sr),
            Biquad::peaking(460.0 * scale, 9.0, 4.0, sr),
            Biquad::peaking(540.0 * scale, 6.0, 4.0, sr),
            Biquad::peaking(2_800.0 * scale.sqrt(), 6.0, 1.0, sr),
            Biquad::high_shelf(6_000.0, -10.0, sr),
        ];
        for (b, n) in self.body.iter_mut().zip(new) {
            b.retune(n);
        }
    }
}

// ---------------------------------------------------------------------------
// Voice
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Mode {
    b1: f32,
    b2: f32,
    gain: f32,
    y1: f32,
    y2: f32,
}

pub struct PhysicalVoice {
    active: bool,
    note: u8,
    vel: f32,
    model: Model,
    pending: bool,
    released: bool,
    killed: bool,
    tuned_for: f32,
    age: u32,
    quiet_blocks: u32,
    rng: Rng,
    pan: (f32, f32),
    exc: Vec<f32>,
    // String / Piano
    strings: [WaveString; MAX_STRINGS],
    string_count: usize,
    dc_x1: f32,
    dc_y1: f32,
    thump: f32,
    thump_coef: f32,
    thump_lp: f32,
    // Mallet
    modes: [Mode; MAX_MODES],
    mode_count: usize,
    mallet_f0: f32,
    pulse_len: u32,
    pulse_amp: f32,
    // Bowed
    bow: BowedString,
}

impl PhysicalVoice {
    fn new(seed: u32) -> Self {
        PhysicalVoice {
            active: false,
            note: 60,
            vel: 1.0,
            model: Model::String,
            pending: false,
            released: false,
            killed: false,
            tuned_for: f32::NAN,
            age: 0,
            quiet_blocks: 0,
            rng: Rng::new(0xB0D1_0000 ^ seed.wrapping_mul(0x9E37_79B9)),
            pan: (1.0, 1.0),
            exc: vec![0.0; DELAY_LEN],
            strings: std::array::from_fn(|_| WaveString::new()),
            string_count: 1,
            dc_x1: 0.0,
            dc_y1: 0.0,
            thump: 0.0,
            thump_coef: 0.0,
            thump_lp: 0.0,
            modes: [Mode::default(); MAX_MODES],
            mode_count: 0,
            mallet_f0: 440.0,
            pulse_len: 0,
            pulse_amp: 0.0,
            bow: BowedString::new(),
        }
    }

    /// Strike/pluck strength from velocity (brightness follows `vel` too,
    /// through `effective_hardness`).
    fn vel_gain(&self) -> f32 {
        crate::dsp::velocity_gain(self.vel, VELOCITY_SENS)
    }

    fn effective_hardness(&self, s: &PhysicalShared) -> f32 {
        (s.hardness + s.vel_bright * (self.vel - 0.7)).clamp(0.0, 1.0)
    }

    fn decay_time(&self, s: &PhysicalShared) -> f32 {
        let damped =
            self.released && !(self.model == Model::Piano && self.note >= PIANO_UNDAMPED_FROM);
        if self.killed {
            0.01
        } else if damped && s.release_damp > 0.0 {
            // Damping blends towards a quick 30 ms mute.
            let t = s.decay * (1.0 - s.release_damp) + 0.03 * s.release_damp;
            t.min(s.decay)
        } else {
            s.decay
        }
    }

    fn tune_string(&mut self, s: &PhysicalShared, f0: f32) {
        // Decay Track: high notes ring shorter, as on a real guitar (1 = half
        // as long per octave above middle C, twice as long per octave below).
        let track = ((60.0 - self.note as f32) / 12.0 * s.decay_track).exp2();
        let t60 = (self.decay_time(s) * track).clamp(0.01, 40.0);
        if s.two_stage <= 0.0 {
            self.string_count = 1;
            self.strings[0].tune(s.sample_rate, f0, s.hf_damp, s.stiffness, t60);
            return;
        }
        // Two polarizations: the plane that drives the bridge hard (and so
        // radiates most) loses energy quickly; the other, slightly detuned,
        // rings on. Together: a prompt attack, then a long, gently beating tail.
        let p = s.two_stage;
        self.string_count = 2;
        self.strings[0].tune(
            s.sample_rate,
            f0,
            s.hf_damp,
            s.stiffness,
            t60 * (1.0 - 0.45 * p),
        );
        let f1 = f0 * (0.6 * p / 1200.0).exp2();
        self.strings[1].tune(
            s.sample_rate,
            f1,
            s.hf_damp,
            s.stiffness,
            t60 * (1.0 + 0.6 * p),
        );
    }

    /// Strings per note and their detune (cents), like a real piano.
    fn piano_layout(&self, s: &PhysicalShared) -> (usize, [f32; MAX_STRINGS]) {
        let d = s.unison;
        match self.note {
            0..=32 => (1, [0.0; 3]),
            33..=44 => (2, [-d * 0.5, d * 0.5, 0.0]),
            _ => (3, [-d, 0.0, d]),
        }
    }

    fn tune_piano(&mut self, s: &PhysicalShared, f0: f32) {
        let (count, detune) = self.piano_layout(s);
        self.string_count = count;
        // Register: decay is long in the bass and short in the treble, and
        // strings get stiffer (more inharmonic) towards the top.
        let reg = ((self.note as f32 - 21.0) / 87.0).clamp(0.0, 1.0);
        let t60 =
            (self.decay_time(s) * ((60.0 - self.note as f32) / 18.0).exp2()).clamp(0.02, 40.0);
        let stiffness = (s.stiffness * (0.4 + 1.2 * reg)).min(1.0);
        // Slightly different decays per string give the two-stage decay.
        let spread = [1.0, 0.75, 1.3];
        for k in 0..count {
            let f = f0 * (detune[k] / 1200.0).exp2();
            self.strings[k].tune(s.sample_rate, f, s.hf_damp, stiffness, t60 * spread[k]);
        }
    }

    fn excite_string(&mut self, s: &PhysicalShared) {
        let sr = s.sample_rate;
        let n = (self.strings[0].period.round() as usize).clamp(2, DELAY_LEN - 1);
        let h = self.effective_hardness(s);
        let fc = 150.0 * 2f32.powf(h * 7.5);
        let c = (-std::f32::consts::TAU * fc / sr).exp();
        let mut lp = 0.0;
        let mut mean = 0.0;
        for e in &mut self.exc[..n] {
            lp = (1.0 - c) * self.rng.bipolar() + c * lp;
            *e = lp;
            mean += lp;
        }
        mean /= n as f32;
        let mut peak = 1e-9f32;
        for e in &mut self.exc[..n] {
            *e -= mean;
            peak = peak.max(e.abs());
        }
        // Pick position: comb filter (in place, back to front).
        let pick = ((s.position * n as f32).round() as usize).clamp(1, n - 1);
        for i in (pick..n).rev() {
            self.exc[i] -= self.exc[i - pick];
        }
        let amp = self.vel_gain() / (2.0 * peak);
        if self.string_count == 2 {
            // Split the pluck between the two planes (the sound is heard
            // mostly through the first, see `render`).
            let p = s.two_stage;
            let (a0, a1) = ((1.0 - 0.5 * p).sqrt(), (0.5 * p).sqrt() * TWO_STAGE_TAIL);
            // Both planes start in phase, so their outputs add: keep the
            // attack at the single-plane level and let only the tail change.
            let norm = amp / (a0 + a1);
            self.strings[0].inject(&self.exc[..n], norm * a0);
            self.strings[1].inject(&self.exc[..n], norm * a1);
        } else {
            let (exc, string) = (&self.exc[..n], &mut self.strings[0]);
            string.inject(exc, amp);
        }
    }

    fn excite_piano(&mut self, s: &PhysicalShared) {
        let sr = s.sample_rate;
        let period = self.strings[0].period;
        let h = self.effective_hardness(s);
        // Felt hammer: felt stiffens with force, so contact time shrinks
        // steeply as the hammer hits harder (~3.5 ms soft to ~0.4 ms hard),
        // capped to half a period so short treble strings still get a strike.
        let contact = ((0.0045 * (1.0 - h).powf(1.5) + 0.0003) * sr)
            .min(period * 0.5)
            .max(2.0);
        let width = contact as usize;
        let strike = ((s.position * period).round() as usize).max(1);
        let len = (width + strike)
            .min(period as usize)
            .max(width)
            .min(DELAY_LEN - 1);
        for (i, e) in self.exc[..len].iter_mut().enumerate() {
            *e = if i < width {
                0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / contact).cos()
            } else {
                0.0
            };
        }
        // Hammer position: notches the harmonics with a node at the strike
        // point. Only meaningful when the strike point is well clear of the
        // contact width; on short treble strings it would cancel the strike.
        if strike > 2 * width {
            for i in (strike..len).rev() {
                self.exc[i] -= self.exc[i - strike];
            }
        }
        let amp = self.vel_gain() * 1.2;
        for k in 0..self.string_count {
            self.strings[k].inject(&self.exc[..len], amp);
        }
        // Key and hammer "thump": a short low burst.
        self.thump = self.vel_gain() * 0.15;
        self.thump_coef = (-1.0 / (0.012 * sr)).exp();
    }

    fn tune_mallet(&mut self, s: &PhysicalShared, f0: f32, keep_state: bool) {
        let sr = s.sample_rate;
        let mat = &MATERIAL_TABLE[s.material.min(MATERIAL_TABLE.len() - 1)];
        let h = self.effective_hardness(s);
        let base_t60 = self.decay_time(s) * mat.decay;
        self.mallet_f0 = f0;
        // Materials with many strong partials would otherwise be much louder;
        // normalise by partial energy (relative to the 3-mode wooden bar).
        let energy = mat.amps.iter().map(|a| a * a).sum::<f32>().sqrt() / 1.1;
        let mut count = 0;
        for (k, (&ratio, &amp)) in mat.ratios.iter().zip(mat.amps).enumerate() {
            let f = f0 * ratio.powf(1.0 + s.stiffness * 0.15);
            if f >= sr * 0.45 || count == MAX_MODES {
                continue;
            }
            let t60 = base_t60 / (1.0 + s.hf_damp * 3.0 * (ratio - 1.0).max(0.0));
            let r = 0.001f32.powf(1.0 / (t60.max(0.005) * sr));
            let theta = std::f32::consts::TAU * f / sr;
            let pos = 0.25
                + 0.75
                    * (std::f32::consts::PI * (k + 1) as f32 * s.position)
                        .sin()
                        .abs();
            let soft = 1.0 / (1.0 + (1.0 - h) * 4.0 * (ratio - 1.0).max(0.0) / 3.0);
            let m = &mut self.modes[count];
            m.b1 = 2.0 * r * theta.cos();
            m.b2 = r * r;
            m.gain = amp / energy * pos * soft * theta.sin();
            if !keep_state {
                m.y1 = 0.0;
                m.y2 = 0.0;
            }
            count += 1;
        }
        self.mode_count = count;
    }

    fn excite_mallet(&mut self, s: &PhysicalShared) {
        let h = self.effective_hardness(s);
        // Contact time from hardness, capped at a fraction of the period so
        // the strike's spectrum always covers the fundamental.
        let contact = (0.0004 + (1.0 - h) * 0.004).min(0.6 / self.mallet_f0);
        let len = (s.sample_rate * contact).max(2.0);
        self.pulse_len = len as u32;
        // Unit-area pulse: low-frequency response is independent of hardness.
        self.pulse_amp = self.vel_gain() * 2.0 / len;
        self.age = 0;
    }

    fn reset(&mut self) {
        for st in &mut self.strings {
            st.clear();
        }
        self.bow.clear();
        self.modes = [Mode::default(); MAX_MODES];
        self.dc_x1 = 0.0;
        self.dc_y1 = 0.0;
        self.thump = 0.0;
        self.thump_lp = 0.0;
        self.tuned_for = f32::NAN;
    }

    #[inline]
    fn dc_block(&mut self, x: f32) -> f32 {
        let y = x - self.dc_x1 + 0.995 * self.dc_y1;
        self.dc_x1 = x;
        self.dc_y1 = y;
        y
    }

    fn render_bowed(
        &mut self,
        s: &PhysicalShared,
        ctl: &Controls,
        l: &mut [f32],
        r: &mut [f32],
    ) -> f32 {
        let sr = s.sample_rate;
        let n = l.len();
        // Delayed vibrato: fades in over ~0.4 s after a short pause.
        let b = &mut self.bow;
        b.vib_time += n as f32 / sr;
        b.vib_phase = (b.vib_phase + s.vibrato_rate * n as f32 / sr).fract();
        let onset = ((b.vib_time - 0.25) / 0.4).clamp(0.0, 1.0);
        let vib = s.vibrato * onset * sin_cycles(b.vib_phase) / 100.0;
        let f = midi_to_freq(self.note as f32 + ctl.pitch + vib);
        b.d_neck_target = (b.loop_delay(sr, f) - b.d_bridge).max(2.0);
        let step = (b.d_neck_target - b.d_neck) / n as f32;

        // Bow speed follows velocity. Bow force must scale with speed for the
        // same motion (Schelleng), so the friction slope scales inversely:
        // the regime is then independent of velocity and only the amplitude
        // follows it.
        let speed = 0.05 + 0.18 * self.vel;
        let target_speed = if self.released { 0.0 } else { speed };
        self.bow.slope = bow_slope(s, ctl.modwheel) * (BOW_REF_SPEED / speed);
        let mask = DELAY_LEN - 1;
        let mut peak = 0.0f32;
        for i in 0..n {
            let b = &mut self.bow;
            b.d_neck += step;
            // Bow speed ramps up over the attack with the bow fully on the
            // string from the start (as in STK); ramping contact instead
            // pushes many notes into the wrong oscillation regime.
            if self.released {
                b.speed *= b.lift_coef;
                b.contact *= b.lift_coef;
            } else {
                b.speed = target_speed + (b.speed - target_speed) * b.attack_coef;
                b.contact = 1.0;
            }
            let read = |buf: &[f32], delay: f32| {
                let pos = b.w as f32 + DELAY_LEN as f32 - delay;
                let k = pos as usize;
                let fr = pos - k as f32;
                let a0 = buf[k & mask];
                a0 + (buf[(k + 1) & mask] - a0) * fr
            };
            let bridge_out = read(&b.bridge, b.d_bridge);
            let neck_out = read(&b.neck, b.d_neck);
            // String losses at the bridge: one-pole low-pass and gain.
            b.lp_y = (1.0 - b.lp_pole) * bridge_out + b.lp_pole * b.lp_y;
            let bridge_refl = -b.lp_y * b.loss;
            let nut_refl = -neck_out;
            let string_vel = bridge_refl + nut_refl;
            let dv = b.speed - string_vel;
            let new_vel = dv * bow_table(dv, b.slope) * b.contact;
            b.neck[b.w] = bridge_refl + new_vel;
            b.bridge[b.w] = nut_refl + new_vel;
            b.w = (b.w + 1) & mask;

            let mut y = bridge_out;
            if s.body > 0.0 {
                let mut body = y;
                for f in &mut b.body {
                    body = f.process(body);
                }
                y = y * (1.0 - s.body) + body * s.body * 0.5;
            }
            let o = self.dc_block(y) * BOWED_GAIN * self.bow.level;
            peak = peak.max(o.abs());
            l[i] += o * self.pan.0;
            r[i] += o * self.pan.1;
        }
        peak
    }
}

impl Voice for PhysicalVoice {
    type Shared = PhysicalShared;

    fn start(&mut self, note: u8, velocity: f32, _from_note: Option<f32>, s: &PhysicalShared) {
        if !self.active || note != self.note || self.model != s.model {
            self.reset();
        }
        self.note = note;
        self.vel = velocity;
        self.model = s.model;
        self.pending = true;
        self.released = false;
        self.killed = false;
        self.active = true;
        self.quiet_blocks = 0;
        self.age = 0;
        let pan = ((note as f32 - 60.0) / 30.0 * s.width).clamp(-1.0, 1.0);
        let (l, r) = pan_gains(pan);
        self.pan = (l * std::f32::consts::SQRT_2, r * std::f32::consts::SQRT_2);
    }

    fn release(&mut self) {
        self.released = true;
        self.tuned_for = f32::NAN; // re-derive loop gain / mode decays
    }

    fn kill(&mut self) {
        // Damp hard rather than cutting off mid-cycle, which would click.
        self.killed = true;
        self.released = true;
        self.tuned_for = f32::NAN;
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn render(&mut self, s: &PhysicalShared, ctl: &Controls, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let pitch = self.note as f32 + ctl.pitch;
        let f0 = midi_to_freq(pitch);
        if self.tuned_for != pitch || self.pending {
            match self.model {
                Model::String => self.tune_string(s, f0),
                Model::Piano => self.tune_piano(s, f0),
                Model::Mallet => {
                    self.tune_mallet(s, f0, !self.pending || self.tuned_for.is_finite())
                }
                Model::Bowed => {
                    self.bow.tune(s, f0);
                    if self.pending {
                        self.bow.d_neck = self.bow.d_neck_target;
                    }
                }
            }
            self.tuned_for = pitch;
        }
        if self.pending {
            match self.model {
                Model::String => self.excite_string(s),
                Model::Piano => self.excite_piano(s),
                Model::Mallet => self.excite_mallet(s),
                Model::Bowed => {}
            }
            self.pending = false;
        }

        let mut peak = 0.0f32;
        match self.model {
            Model::String | Model::Piano => {
                let (gain, norm) = if self.model == Model::Piano {
                    (PIANO_GAIN, 1.0 / (self.string_count as f32).sqrt())
                } else {
                    (STRING_GAIN, 1.0)
                };
                let thump_c = (-std::f32::consts::TAU * 180.0 / s.sample_rate).exp();
                for i in 0..n {
                    let mut z = 0.0;
                    for st in &mut self.strings[..self.string_count] {
                        z += st.tick();
                    }
                    let mut o = self.dc_block(z * norm) * gain;
                    if self.thump > 1e-5 {
                        self.thump_lp =
                            (1.0 - thump_c) * self.rng.bipolar() + thump_c * self.thump_lp;
                        o += self.thump_lp * self.thump * 6.0;
                        self.thump *= self.thump_coef;
                    }
                    peak = peak.max(o.abs());
                    l[i] += o * self.pan.0;
                    r[i] += o * self.pan.1;
                }
            }
            Model::Mallet => {
                for i in 0..n {
                    let x = if self.age < self.pulse_len {
                        let t = self.age as f32 / self.pulse_len as f32;
                        self.pulse_amp * (0.5 - 0.5 * (std::f32::consts::TAU * t).cos())
                    } else {
                        0.0
                    };
                    self.age = self.age.saturating_add(1);
                    let mut sum = 0.0;
                    for m in &mut self.modes[..self.mode_count] {
                        let y = m.b1 * m.y1 - m.b2 * m.y2 + m.gain * x;
                        m.y2 = m.y1;
                        m.y1 = y;
                        sum += y;
                    }
                    let o = sum * MALLET_GAIN;
                    peak = peak.max(o.abs());
                    l[i] += o * self.pan.0;
                    r[i] += o * self.pan.1;
                }
            }
            Model::Bowed => {
                peak = self.render_bowed(s, ctl, &mut l[..n], &mut r[..n]);
            }
        }
        // Free the voice once it has been effectively silent for a while
        // (a bowed note is never silent while the bow is on the string).
        let bowing = self.model == Model::Bowed && !self.released;
        if peak < 1e-4 && !bowing {
            self.quiet_blocks += 1;
            if self.quiet_blocks > 8 {
                self.active = false;
            }
        } else {
            self.quiet_blocks = 0;
        }
    }
}

/// Open strings that ring in sympathy (standard guitar tuning, E2 to E4).
const OPEN_STRINGS: [u8; 6] = [40, 45, 50, 55, 59, 64];
/// How hard the played notes drive the open strings, and how loud they ring.
const SYMPATHETIC_DRIVE: f32 = 0.0025;
const SYMPATHETIC_LEVEL: f32 = 1.0;

const BODY_MODES: usize = 14;
/// Output level of the modal bodies relative to the dry string.
const MODAL_BODY_GAIN: f32 = 4.5;

/// Guitar body modes: (frequency Hz, Q, level). The lowest is the air
/// (Helmholtz) resonance of the sound hole, then the top plate's main
/// breathing mode and its couplings, cross-dipole and higher plate modes,
/// thinning out towards the treble. Values follow published modal analyses
/// of real instruments in outline, not any one guitar.
const GUITAR_BODIES: [[(f32, f32, f32); BODY_MODES]; 3] = [
    // Dreadnought (steel string): deep air resonance, strong low mids.
    [
        (98.0, 18.0, 1.0),
        (196.0, 24.0, 1.1),
        (238.0, 22.0, 0.55),
        (290.0, 28.0, 0.45),
        (372.0, 30.0, 0.5),
        (450.0, 30.0, 0.45),
        (560.0, 32.0, 0.35),
        (690.0, 34.0, 0.3),
        (840.0, 36.0, 0.28),
        (1_050.0, 38.0, 0.25),
        (1_320.0, 40.0, 0.2),
        (1_680.0, 40.0, 0.18),
        (2_150.0, 36.0, 0.15),
        (2_900.0, 30.0, 0.12),
    ],
    // Classical (fan-braced, lighter top): warm and round, softer treble.
    [
        (92.0, 16.0, 1.0),
        (185.0, 20.0, 1.2),
        (225.0, 20.0, 0.6),
        (270.0, 24.0, 0.5),
        (340.0, 26.0, 0.45),
        (420.0, 28.0, 0.4),
        (520.0, 30.0, 0.3),
        (640.0, 30.0, 0.25),
        (790.0, 32.0, 0.2),
        (980.0, 32.0, 0.16),
        (1_240.0, 34.0, 0.12),
        (1_600.0, 34.0, 0.1),
        (2_050.0, 30.0, 0.08),
        (2_700.0, 28.0, 0.06),
    ],
    // Parlor (small body): higher air resonance, less bass, focused mids.
    [
        (122.0, 16.0, 0.6),
        (232.0, 22.0, 1.1),
        (275.0, 22.0, 0.6),
        (340.0, 26.0, 0.55),
        (430.0, 28.0, 0.55),
        (520.0, 30.0, 0.45),
        (640.0, 32.0, 0.4),
        (780.0, 34.0, 0.32),
        (950.0, 36.0, 0.3),
        (1_180.0, 38.0, 0.25),
        (1_450.0, 38.0, 0.2),
        (1_850.0, 38.0, 0.17),
        (2_350.0, 34.0, 0.14),
        (3_100.0, 30.0, 0.11),
    ],
];

pub struct PhysicalSynth {
    pub poly: Poly<PhysicalVoice>,
    pub shared: PhysicalShared,
    sample_rate: f32,
    body: f32,
    body_model: Option<Model>,
    body_filters: [[Svf; 3]; 2],
    body_coefs: [SvfCoefs; 3],
    /// 0 = the generic box; otherwise an index into `GUITAR_BODIES` + 1.
    body_type: usize,
    body_modes: [[Biquad; BODY_MODES]; 2],
    sympathetic: f32,
    open_strings: [WaveString; OPEN_STRINGS.len()],
    /// (decay, hf damping, decay track) the open strings were tuned for.
    open_tuned_for: (f32, f32, f32),
    tone: SvfCoefs,
    tone_bypass: bool,
    tone_filters: [Svf; 2],
    tmp_l: [f32; MAX_BLOCK],
    tmp_r: [f32; MAX_BLOCK],
}

impl PhysicalSynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut synth = PhysicalSynth {
            poly: Poly::new(|i| PhysicalVoice::new(i as u32 + 1)),
            shared: PhysicalShared::default(),
            sample_rate,
            body: 0.0,
            body_model: None,
            body_filters: [[Svf::default(); 3]; 2],
            body_coefs: [SvfCoefs::default(); 3],
            body_type: 0,
            body_modes: [[Biquad::default(); BODY_MODES]; 2],
            sympathetic: 0.0,
            open_strings: std::array::from_fn(|_| WaveString::new()),
            open_tuned_for: (f32::NAN, f32::NAN, f32::NAN),
            tone: SvfCoefs::default(),
            tone_bypass: true,
            tone_filters: [Svf::default(); 2],
            tmp_l: [0.0; MAX_BLOCK],
            tmp_r: [0.0; MAX_BLOCK],
        };
        synth.update(&super::SynthKind::Physical.defaults());
        synth
    }

    pub fn update(&mut self, params: &[f32]) {
        let sr = self.sample_rate;
        let p = &params[COMMON.len()..];
        // Physical models don't glide: pitch comes from the resonator itself.
        self.poly.update_common(params, p[VOICES], 0.0, sr);
        let s = &mut self.shared;
        s.sample_rate = sr;
        s.model = model_from(p[MODEL]);
        s.hardness = p[HARDNESS];
        s.position = p[POSITION];
        s.vel_bright = p[VEL_BRIGHT];
        s.bow_pressure = p[BOW_PRESSURE];
        s.bow_position = p[BOW_POSITION];
        s.bow_attack = p[BOW_ATTACK];
        s.vibrato = p[VIBRATO];
        s.vibrato_rate = p[VIBRATO_RATE];
        s.bowed_body = p[BOWED_BODY].round() as usize;
        s.decay = p[DECAY];
        s.hf_damp = p[HF_DAMP];
        s.stiffness = p[STIFFNESS];
        s.unison = p[UNISON];
        s.material = p[MATERIAL].round() as usize;
        s.body = p[BODY];
        s.release_damp = p[RELEASE_DAMP];
        s.width = p[WIDTH];
        s.decay_track = p[DECAY_TRACK];
        s.two_stage = p[TWO_STAGE];
        self.sympathetic = if s.model == Model::String {
            p[SYMPATHETIC]
        } else {
            0.0
        };
        let key = (s.decay, s.hf_damp, s.decay_track);
        if self.sympathetic > 0.0 && key != self.open_tuned_for {
            self.open_tuned_for = key;
            for (st, &note) in self.open_strings.iter_mut().zip(&OPEN_STRINGS) {
                // Undamped open strings follow the same register decay as
                // played ones.
                let track = ((60.0 - note as f32) / 12.0 * s.decay_track).exp2();
                let t60 = (s.decay * track).clamp(0.05, 40.0);
                st.tune(sr, midi_to_freq(note as f32), s.hf_damp, 0.0, t60);
            }
        }
        self.body = p[BODY];
        let body_type = if s.model == Model::String {
            (p[BODY_TYPE].round().max(0.0) as usize).min(BODY_TYPES.len() - 1)
        } else {
            0
        };
        if body_type != self.body_type {
            self.body_type = body_type;
            if body_type > 0 {
                let modes = GUITAR_BODIES[body_type - 1];
                for (ch, bank) in self.body_modes.iter_mut().enumerate() {
                    // The right channel's modes sit ~1% higher: a body radiates
                    // differently in each direction, which widens the image.
                    let spread = if ch == 0 { 1.0 } else { 1.012 };
                    for (b, (f, q, _)) in bank.iter_mut().zip(modes) {
                        *b = Biquad::band_pass(f * spread, q, sr);
                    }
                }
            }
        }
        // Body resonances: a guitar-like box for strings and mallets, a broader
        // soundboard for the piano. Bowed voices carry their own body.
        if self.body_model != Some(s.model) {
            let (freqs, res) = match s.model {
                Model::Piano => ([100.0, 260.0, 800.0], 0.35),
                _ => ([98.0, 204.0, 405.0], 0.6),
            };
            self.body_coefs = freqs.map(|f| SvfCoefs::new(FilterMode::BandPass, f, res, sr));
            self.body_model = Some(s.model);
        }
        self.tone_bypass = p[TONE] >= 19_000.0;
        self.tone = SvfCoefs::new(FilterMode::LowPass, p[TONE], 0.0, sr);
    }

    /// Body resonance and tone filter for one output sample of channel `ch`.
    fn respond(&mut self, ch: usize, x: f32, body: f32) -> f32 {
        let mut y = x;
        if body > 0.0 && self.body_type > 0 {
            let modes = GUITAR_BODIES[self.body_type - 1];
            let mut res = 0.0;
            for (b, (_, _, gain)) in self.body_modes[ch].iter_mut().zip(modes) {
                res += b.process(y) * gain;
            }
            y = y * (1.0 - 0.4 * body) + res * body * MODAL_BODY_GAIN;
        } else if body > 0.0 {
            let f = &mut self.body_filters[ch];
            let res = f[0].process(&self.body_coefs[0], y) * 1.4
                + f[1].process(&self.body_coefs[1], y) * 1.1
                + f[2].process(&self.body_coefs[2], y) * 0.8;
            y = y * (1.0 - 0.4 * body) + res * body;
        }
        if !self.tone_bypass {
            y = self.tone_filters[ch].process(&self.tone, y);
        }
        y
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        self.tmp_l[..n].fill(0.0);
        self.tmp_r[..n].fill(0.0);
        self.poly
            .render(&self.shared, &mut self.tmp_l[..n], &mut self.tmp_r[..n]);
        let body = if self.shared.model == Model::Bowed {
            0.0
        } else {
            self.body
        };
        let drive = self.sympathetic * SYMPATHETIC_DRIVE;
        for i in 0..n {
            let (mut a, mut b) = (self.tmp_l[i], self.tmp_r[i]);
            if drive > 0.0 {
                // The played strings shake the bridge, which drives the open
                // strings; they radiate through the body like the rest.
                let x = 0.5 * (a + b) * drive;
                let ring: f32 = self.open_strings.iter_mut().map(|st| st.tick_in(x)).sum();
                a += ring * SYMPATHETIC_LEVEL;
                b += ring * SYMPATHETIC_LEVEL;
            }
            l[i] += self.respond(0, a, body);
            r[i] += self.respond(1, b, body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::Sample;
    use crate::synth::SynthKind;

    fn synth_with(overrides: &[(&str, f32)]) -> PhysicalSynth {
        crate::dsp::init_tables();
        let mut s = PhysicalSynth::new(48_000.0);
        let mut params = SynthKind::Physical.defaults();
        for (k, v) in overrides {
            params[SynthKind::Physical.index_of(k).unwrap()] = *v;
        }
        s.update(&params);
        s
    }

    fn render(s: &mut PhysicalSynth, frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let (mut l, mut r) = ([0.0f32; MAX_BLOCK], [0.0f32; MAX_BLOCK]);
        while out.len() < frames {
            l.fill(0.0);
            r.fill(0.0);
            s.render(&mut l, &mut r);
            out.extend(l.iter().zip(&r).map(|(a, b)| 0.5 * (a + b)));
        }
        out
    }

    fn peak(v: &[f32]) -> f32 {
        v.iter().fold(0.0f32, |m, x| m.max(x.abs()))
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// Pitch of a rendered note, via the sampler's YIN detector.
    fn measured_pitch(s: &mut PhysicalSynth, note: u8) -> f32 {
        s.poly.note_on(note, 0.8, &s.shared);
        let out = render(s, 48_000);
        assert!(
            out.iter().all(|v| v.is_finite()) && peak(&out) > 0.02,
            "note {note}: peak {}",
            peak(&out)
        );
        Sample::new("t", out, None, 48_000.0)
            .detect_pitch()
            .unwrap_or(f32::NAN)
    }

    #[test]
    fn strings_are_in_tune_across_the_range_and_settings() {
        for (damp, stiff) in [(0.0, 0.0), (0.4, 0.0), (1.0, 0.0), (0.4, 0.5)] {
            for note in [40u8, 52, 69, 84] {
                let mut s = synth_with(&[("hf_damp", damp), ("stiffness", stiff), ("body", 0.0)]);
                let p = measured_pitch(&mut s, note);
                assert!(
                    (p - note as f32).abs() < 0.05,
                    "note {note} damp {damp} stiff {stiff}: measured {p}"
                );
            }
        }
    }

    #[test]
    fn mallets_are_in_tune() {
        for material in [0.0, 1.0, 6.0] {
            for note in [57u8, 72, 84] {
                let mut s = synth_with(&[("model", 1.0), ("material", material), ("body", 0.0)]);
                let p = measured_pitch(&mut s, note);
                assert!(
                    (p - note as f32).abs() < 0.05,
                    "material {material} note {note}: measured {p}"
                );
            }
        }
    }

    #[test]
    fn pianos_are_in_tune_across_the_keyboard() {
        for note in [28u8, 40, 52, 64, 76, 88] {
            let mut s = synth_with(&[("model", 2.0), ("stiffness", 0.3), ("body", 0.0)]);
            let p = measured_pitch(&mut s, note);
            assert!(
                (p - note as f32).abs() < 0.06,
                "piano note {note}: measured {p}"
            );
        }
    }

    #[test]
    fn piano_dampers_stop_notes_except_the_top_octaves() {
        for (note, should_ring) in [(60u8, false), (96, true)] {
            let mut s = synth_with(&[("model", 2.0), ("release_damp", 1.0), ("decay", 8.0)]);
            s.poly.note_on(note, 0.9, &s.shared);
            render(&mut s, 4_800);
            s.poly.note_off(note);
            let after = render(&mut s, 24_000);
            let late = rms(&after[19_200..]);
            assert_eq!(
                late > 1e-3,
                should_ring,
                "note {note}: rms after release {late}"
            );
        }
    }

    #[test]
    fn harder_piano_strikes_are_brighter() {
        let zc = |vel: f32| {
            let mut s = synth_with(&[("model", 2.0), ("body", 0.0)]);
            s.poly.note_on(48, vel, &s.shared);
            let out = render(&mut s, 9_600);
            out.windows(2)
                .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
                .count()
        };
        assert!(
            zc(1.0) as f32 > zc(0.2) as f32 * 1.3,
            "loud {} soft {}",
            zc(1.0),
            zc(0.2)
        );
    }

    #[test]
    fn bowed_strings_sustain_while_held_and_stop_when_released() {
        let mut s = synth_with(&[("model", 3.0)]);
        s.poly.note_on(64, 0.8, &s.shared);
        let held = render(&mut s, 96_000);
        assert!(held.iter().all(|v| v.is_finite()));
        let (early, late) = (rms(&held[24_000..48_000]), rms(&held[72_000..96_000]));
        assert!(early > 0.02, "bow didn't start the string: {early}");
        assert!(
            late > early * 0.6,
            "note died while bowing: {early} -> {late}"
        );
        s.poly.note_off(64);
        render(&mut s, 96_000);
        assert_eq!(s.poly.active_voices(), 0);
    }

    #[test]
    fn bowed_strings_are_in_tune() {
        for note in [43u8, 55, 62, 69, 76, 84] {
            let mut s = synth_with(&[("model", 3.0), ("vibrato", 0.0), ("body", 0.0)]);
            let p = measured_pitch(&mut s, note);
            assert!(
                (p - note as f32).abs() < 0.12,
                "bowed note {note}: measured {p}"
            );
        }
    }

    #[test]
    fn bowed_extremes_stay_bounded() {
        for (pressure, position) in [(0.0, 0.08), (1.0, 0.25), (1.0, 0.08), (0.0, 0.25)] {
            for body in [0.0, 1.0, 2.0, 3.0] {
                let mut s = synth_with(&[
                    ("model", 3.0),
                    ("bow_pressure", pressure),
                    ("bow_position", position),
                    ("bowed_body", body),
                ]);
                for note in [36u8, 55, 76] {
                    s.poly.note_on(note, 1.0, &s.shared);
                }
                s.poly.set_modwheel(1.0);
                let out = render(&mut s, 48_000);
                assert!(out.iter().all(|v| v.is_finite()));
                assert!(
                    peak(&out) < 2.0,
                    "pressure {pressure} position {position} body {body}: {}",
                    peak(&out)
                );
            }
        }
    }

    fn harmonic(out: &[f32], f: f32) -> f32 {
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, x) in out.iter().enumerate() {
            let ph = std::f32::consts::TAU * f * i as f32 / 48_000.0;
            re += x * ph.cos();
            im += x * ph.sin();
        }
        (re * re + im * im).sqrt() / out.len() as f32 * 2.0
    }

    /// With default bow settings, every note E2-E6 must settle into Helmholtz
    /// motion (the fundamental is the strongest harmonic), whatever the
    /// velocity or mod wheel, rather than multi-slipping or sticking.
    #[test]
    fn bowed_strings_find_helmholtz_motion() {
        for vel in [0.3f32, 1.0] {
            for wheel in [0.0f32, 1.0] {
                for note in [40u8, 43, 48, 52, 55, 60, 62, 67, 69, 74, 76, 81, 84, 88] {
                    let mut s = synth_with(&[("model", 3.0), ("vibrato", 0.0), ("body", 0.0)]);
                    s.poly.set_modwheel(wheel);
                    s.poly.note_on(note, vel, &s.shared);
                    let out = render(&mut s, 48_000);
                    let f0 = midi_to_freq(note as f32);
                    let h: Vec<f32> = (1..=6)
                        .map(|k| harmonic(&out[24_000..], f0 * k as f32))
                        .collect();
                    let strongest_other = h[1..].iter().fold(0.0f32, |m, v| m.max(*v));
                    assert!(
                        h[0] > 0.005 && h[0] >= 0.8 * strongest_other,
                        "note {note} vel {vel} wheel {wheel}: harmonics {h:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn controls_follow_the_model() {
        let mut p = SynthKind::Physical.defaults()[COMMON.len()..].to_vec();
        p[MODEL] = 2.0; // piano
        assert!(
            param_visible(&p, UNISON)
                && !param_visible(&p, MATERIAL)
                && !param_visible(&p, BOW_PRESSURE)
        );
        p[MODEL] = 3.0; // bowed
        assert!(
            param_visible(&p, BOW_PRESSURE)
                && !param_visible(&p, HARDNESS)
                && !param_visible(&p, UNISON)
        );
        p[MODEL] = 1.0; // mallet
        assert!(param_visible(&p, MATERIAL) && param_visible(&p, HARDNESS));
    }

    #[test]
    fn every_material_rings_and_dies_away() {
        for material in 0..MATERIALS.len() {
            let mut s = synth_with(&[
                ("model", 1.0),
                ("material", material as f32),
                ("decay", 0.3),
            ]);
            s.poly.note_on(60, 1.0, &s.shared);
            let out = render(&mut s, 48_000 * 8);
            assert!(out.iter().all(|v| v.is_finite()));
            assert!(
                peak(&out) > 0.02 && peak(&out) < 1.5,
                "material {material} peak {}",
                peak(&out)
            );
            assert_eq!(
                s.poly.active_voices(),
                0,
                "material {material} still ringing"
            );
        }
    }

    #[test]
    fn release_damping_mutes_a_string() {
        let mut s = synth_with(&[("decay", 10.0), ("release_damp", 1.0)]);
        s.poly.note_on(60, 1.0, &s.shared);
        render(&mut s, 4_800);
        s.poly.note_off(60);
        render(&mut s, 48_000);
        assert_eq!(s.poly.active_voices(), 0);
    }

    #[test]
    fn harder_strikes_are_brighter() {
        let zc = |hard: f32| {
            let mut s = synth_with(&[("hardness", hard), ("vel_bright", 0.0), ("body", 0.0)]);
            s.poly.note_on(48, 1.0, &s.shared);
            let out = render(&mut s, 4_800);
            out.windows(2)
                .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
                .count()
        };
        assert!(zc(1.0) > zc(0.1) * 2, "hard {} soft {}", zc(1.0), zc(0.1));
    }

    #[test]
    fn factory_patches_play_cleanly() {
        crate::dsp::init_tables();
        for patch in crate::patch::factory_patches()
            .into_iter()
            .filter(|p| p.kind == SynthKind::Physical)
        {
            let mut s = PhysicalSynth::new(48_000.0);
            s.update(&patch.values());
            for note in [48u8, 60, 67, 76] {
                s.poly.note_on(note, 0.9, &s.shared);
            }
            let out = render(&mut s, 48_000 * 2);
            assert!(out.iter().all(|v| v.is_finite()), "{}", patch.name);
            assert!(
                peak(&out) > 0.05 && peak(&out) < 1.5,
                "{}: peak {}",
                patch.name,
                peak(&out)
            );
        }
    }

    fn db_curve(out: &[f32]) -> Vec<f32> {
        out.chunks(2_400)
            .map(|c| 20.0 * (rms(c) + 1e-9).log10())
            .collect()
    }

    const GUITAR: [(&str, f32); 5] = [
        ("hardness", 0.7),
        ("position", 0.12),
        ("decay", 6.0),
        ("hf_damp", 0.25),
        ("body", 0.0),
    ];

    /// Seconds until a note has fallen 40 dB from its start.
    fn time_to_minus_40(extra: &[(&str, f32)], note: u8) -> f32 {
        let mut o = GUITAR.to_vec();
        o.extend_from_slice(extra);
        let mut s = synth_with(&o);
        s.poly.note_on(note, 0.8, &s.shared);
        let c = db_curve(&render(&mut s, 48_000 * 10));
        let top = c[..4].iter().cloned().fold(-200.0, f32::max);
        c.iter().position(|d| *d < top - 40.0).unwrap_or(c.len()) as f32 * 0.05
    }

    #[test]
    fn decay_track_shortens_high_notes() {
        let flat: Vec<f32> = [40, 60, 88].map(|n| time_to_minus_40(&[], n)).to_vec();
        assert!(
            flat.iter().all(|t| (t / flat[1] - 1.0).abs() < 0.3),
            "{flat:?}"
        );
        let tracked: Vec<f32> = [40, 60, 88]
            .map(|n| time_to_minus_40(&[("decay_track", 0.67)], n))
            .to_vec();
        assert!(
            tracked[0] > 1.8 * tracked[1] && tracked[1] > 1.8 * tracked[2],
            "{tracked:?}"
        );
    }

    /// Two polarizations: a quick first decay, then a slower tail; same
    /// attack level, still in tune.
    #[test]
    fn two_stage_decay_bends_the_envelope() {
        let shape = |two: f32| {
            let mut o = GUITAR.to_vec();
            o.push(("two_stage", two));
            let mut s = synth_with(&o);
            s.poly.note_on(48, 0.8, &s.shared);
            let out = render(&mut s, 48_000 * 4);
            let c = db_curve(&out);
            let (early, late) = ((c[1] - c[8]) / 0.35, (c[30] - c[70]) / 2.0);
            (early / late, rms(&out[..4_800]))
        };
        let (single, single_level) = shape(0.0);
        let (double, double_level) = shape(0.8);
        assert!(
            double > 1.4 * single,
            "early/late decay ratio {single:.2} -> {double:.2}"
        );
        let db = 20.0 * (double_level / single_level).log10();
        assert!(db.abs() < 1.5, "attack level changed by {db:.1} dB");
        for note in [40u8, 52, 64, 76] {
            let mut o = GUITAR.to_vec();
            o.push(("two_stage", 1.0));
            let p = measured_pitch(&mut synth_with(&o), note);
            assert!((p - note as f32).abs() < 0.02, "note {note}: {p}");
        }
    }

    /// Each guitar body rings at its air and main top-plate modes, well
    /// above the valley between them.
    #[test]
    fn guitar_bodies_resonate_at_their_modes() {
        for bt in 1..BODY_TYPES.len() {
            let mut s = synth_with(&[("body", 0.5), ("body_type", bt as f32)]);
            let ir: Vec<f32> = (0..48_000)
                .map(|i| s.respond(0, if i == 0 { 1.0 } else { 0.0 }, 0.5))
                .collect();
            let db = |f: f32| {
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (n, v) in ir.iter().enumerate() {
                    let ph = std::f64::consts::TAU * f as f64 * n as f64 / 48_000.0;
                    re += *v as f64 * ph.cos();
                    im -= *v as f64 * ph.sin();
                }
                20.0 * (re * re + im * im).sqrt().log10() as f32
            };
            let m = GUITAR_BODIES[bt - 1];
            let valley = db((m[0].0 * m[1].0).sqrt());
            assert!(db(m[0].0) > valley + 8.0, "{}: air mode", BODY_TYPES[bt]);
            assert!(db(m[1].0) > valley + 10.0, "{}: top mode", BODY_TYPES[bt]);
        }
    }

    /// Open strings ring on in sympathy: a damped note leaves a tail, and a
    /// held note never swells.
    #[test]
    fn sympathetic_strings_ring_but_never_swell() {
        let tail = |symp: f32| {
            let mut o = GUITAR.to_vec();
            o.extend([("sympathetic", symp), ("release_damp", 0.9)]);
            let mut s = synth_with(&o);
            s.poly.note_on(57, 0.8, &s.shared);
            render(&mut s, 4_800);
            s.poly.note_off(57);
            let out = render(&mut s, 48_000 * 2);
            20.0 * (rms(&out[48_000..72_000]) + 1e-9).log10()
        };
        let (dry, wet) = (tail(0.0), tail(1.0));
        assert!(wet > dry + 20.0, "tail {dry:.1} -> {wet:.1} dB");

        let mut o = GUITAR.to_vec();
        o.push(("sympathetic", 1.0));
        let mut s = synth_with(&o);
        for n in [40u8, 47, 52, 56, 59, 64] {
            s.poly.note_on(n, 0.8, &s.shared);
        }
        // Quarter-second windows: chords beat (equal-tempered thirds) on a
        // shorter scale even without sympathetic strings.
        let c: Vec<f32> = render(&mut s, 48_000 * 4)
            .chunks(12_000)
            .map(|c| 20.0 * (rms(c) + 1e-9).log10())
            .collect();
        for w in c.windows(2) {
            assert!(
                w[1] < w[0] + 0.5,
                "level rose: {:.1} -> {:.1} dB",
                w[0],
                w[1]
            );
        }
    }
}
