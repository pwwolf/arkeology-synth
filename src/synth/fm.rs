//! 4-operator phase-modulation ("FM") synth with 8 algorithms, operator
//! feedback, per-operator envelopes and velocity sensitivity.

use crate::dsp::{AdsrParams, Env, midi_to_freq, sin_cycles};
use crate::params::{ParamDesc as P, Unit};

use super::{COMMON, Controls, GLIDE_PARAM, Poly, VOICES_PARAM, Voice, glide};

pub const ALGORITHMS: [&str; 8] = [
    "1: 4>3>2>1",
    "2: (3+4)>2>1",
    "3: (4>3)+2>1",
    "4: 4>(2,3)>1",
    "5: 2>1 + 4>3",
    "6: 4>(1,2,3)",
    "7: 2>1 + 3 + 4",
    "8: 1+2+3+4",
];

/// For each algorithm: per-operator bitmask of modulators, and a carrier mask.
/// Operators are 0-based here (op 1 == index 0); a modulator always has a
/// higher index than its target so operators can be computed 3, 2, 1, 0.
const ALGO_TABLE: [([u8; 4], u8); 8] = [
    ([0b0010, 0b0100, 0b1000, 0], 0b0001),
    ([0b0010, 0b1100, 0, 0], 0b0001),
    ([0b0110, 0, 0b1000, 0], 0b0001),
    ([0b0110, 0b1000, 0b1000, 0], 0b0001),
    ([0b0010, 0, 0b1000, 0], 0b0101),
    ([0b1000, 0b1000, 0b1000, 0], 0b0111),
    ([0b0010, 0, 0, 0], 0b1101),
    ([0, 0, 0, 0], 0b1111),
];

pub fn is_carrier(algorithm: usize, op: usize) -> bool {
    ALGO_TABLE[algorithm.min(7)].1 & (1 << op) != 0
}

// Local (synth-specific) parameter indices.
pub const VOICES: usize = 0;
pub const GLIDE: usize = 1;
pub const ALGO: usize = 2;
pub const FEEDBACK: usize = 3;
pub const BRIGHTNESS: usize = 4;
pub const VIB_RATE: usize = 5;
pub const VIB_DEPTH: usize = 6;
pub const WHEEL_VIB: usize = 7;
pub const OP_BASE: usize = 8;
pub const OP_STRIDE: usize = 8;
pub const OP_RATIO: usize = 0;
pub const OP_DETUNE: usize = 1;
pub const OP_LEVEL: usize = 2;
pub const OP_ATTACK: usize = 3;
pub const OP_DECAY: usize = 4;
pub const OP_SUSTAIN: usize = 5;
pub const OP_RELEASE: usize = 6;
pub const OP_VEL: usize = 7;

macro_rules! op_params {
    ($n:literal, $ratio:expr, $level:expr, $decay:expr, $sustain:expr) => {
        [
            P::float(concat!("op", $n, "_ratio"), "Ratio", concat!("Op ", $n), 0.5, 16.0, $ratio, Unit::Ratio)
                .step(0.5),
            P::float(concat!("op", $n, "_detune"), "Detune", concat!("Op ", $n), -50.0, 50.0, 0.0, Unit::Cents)
                .step(1.0),
            P::float(concat!("op", $n, "_level"), "Level", concat!("Op ", $n), 0.0, 1.0, $level, Unit::Percent),
            P::float(concat!("op", $n, "_attack"), "Attack", concat!("Op ", $n), 0.001, 10.0, 0.002, Unit::Seconds)
                .exp(),
            P::float(concat!("op", $n, "_decay"), "Decay", concat!("Op ", $n), 0.005, 20.0, $decay, Unit::Seconds)
                .exp(),
            P::float(concat!("op", $n, "_sustain"), "Sustain", concat!("Op ", $n), 0.0, 1.0, $sustain, Unit::Percent),
            P::float(concat!("op", $n, "_release"), "Release", concat!("Op ", $n), 0.005, 20.0, 0.3, Unit::Seconds)
                .exp(),
            P::float(concat!("op", $n, "_vel"), "Vel Sens", concat!("Op ", $n), 0.0, 1.0, 0.5, Unit::Percent),
        ]
    };
}

const fn build_params() -> [P; OP_BASE + 4 * OP_STRIDE] {
    let head = [
        VOICES_PARAM,
        GLIDE_PARAM,
        P::choice("algorithm", "Algorithm", "FM", &ALGORITHMS, 0),
        P::float("feedback", "Feedback", "FM", 0.0, 1.0, 0.0, Unit::Percent),
        P::float("brightness", "Brightness", "FM", 0.0, 2.0, 1.0, Unit::Percent),
        P::float("vib_rate", "Vibrato Rate", "FM", 0.1, 12.0, 5.0, Unit::Hz).exp(),
        P::float("vib_depth", "Vibrato Depth", "FM", 0.0, 100.0, 0.0, Unit::Cents).step(1.0),
        P::float("wheel_vib", "Wheel>Vibrato", "FM", 0.0, 100.0, 30.0, Unit::Cents).step(1.0),
    ];
    let ops = [
        op_params!("1", 1.0, 0.8, 1.5, 0.6),
        op_params!("2", 1.0, 0.45, 0.8, 0.3),
        op_params!("3", 2.0, 0.25, 0.5, 0.2),
        op_params!("4", 1.0, 0.0, 0.5, 0.2),
    ];
    let mut out = [head[0]; OP_BASE + 4 * OP_STRIDE];
    let mut i = 0;
    while i < OP_BASE {
        out[i] = head[i];
        i += 1;
    }
    let mut op = 0;
    while op < 4 {
        let mut k = 0;
        while k < OP_STRIDE {
            out[OP_BASE + op * OP_STRIDE + k] = ops[op][k];
            k += 1;
        }
        op += 1;
    }
    out
}

pub static PARAMS: [P; OP_BASE + 4 * OP_STRIDE] = build_params();

/// Peak modulation depth in cycles for a modulator at full level.
const MOD_DEPTH: f32 = 1.5;
const FEEDBACK_DEPTH: f32 = 0.35;
const VOICE_GAIN: f32 = 0.35;

#[derive(Clone, Copy, Default)]
struct OpShared {
    ratio: f32,
    level: f32,
    vel_sens: f32,
    env: AdsrParams,
}

#[derive(Default)]
pub struct FmShared {
    sample_rate: f32,
    algo: usize,
    feedback: f32,
    brightness: f32,
    vib_inc: f32,
    vib_depth: f32,
    wheel_vib: f32,
    ops: [OpShared; 4],
}

#[derive(Default)]
pub struct FmVoice {
    note: u8,
    pitch: f32,
    vel: f32,
    phases: [f32; 4],
    envs: [Env; 4],
    fb: [f32; 2],
    lfo: f32,
    active: bool,
}

impl Voice for FmVoice {
    type Shared = FmShared;

    fn start(&mut self, note: u8, velocity: f32, from_note: Option<f32>, _s: &FmShared) {
        if !self.active {
            self.phases = [0.0; 4];
            self.fb = [0.0; 2];
            self.lfo = 0.0;
            self.pitch = from_note.unwrap_or(note as f32);
        } else if from_note.is_none() {
            self.pitch = note as f32;
        }
        self.note = note;
        self.vel = velocity;
        self.active = true;
        for e in &mut self.envs {
            e.trigger();
        }
    }

    fn release(&mut self) {
        for e in &mut self.envs {
            e.release();
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn render(&mut self, s: &FmShared, ctl: &Controls, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        self.pitch = glide(self.pitch, self.note as f32, ctl.glide_coef, n);
        self.lfo = (self.lfo + s.vib_inc * n as f32).fract();
        let vib_semis = sin_cycles(self.lfo) * (s.vib_depth + ctl.modwheel * s.wheel_vib) / 100.0;
        let freq = midi_to_freq(self.pitch + ctl.pitch + vib_semis);

        let (mods, carriers) = ALGO_TABLE[s.algo];
        let carrier_count = carriers.count_ones() as f32;
        let mut inc = [0.0f32; 4];
        let mut amp = [0.0f32; 4];
        for op in 0..4 {
            let o = &s.ops[op];
            inc[op] = freq * o.ratio / s.sample_rate;
            let vel = 1.0 - o.vel_sens * (1.0 - self.vel);
            amp[op] = if carriers & (1 << op) != 0 {
                o.level * vel / carrier_count.sqrt()
            } else {
                o.level * o.level * s.brightness * MOD_DEPTH * vel
            };
        }
        let fb_amt = s.feedback * FEEDBACK_DEPTH;

        for i in 0..n {
            let mut out = [0.0f32; 4];
            for op in (0..4).rev() {
                let env = self.envs[op].next(&s.ops[op].env);
                let mut m = 0.0;
                let mut mask = mods[op];
                while mask != 0 {
                    let j = mask.trailing_zeros() as usize;
                    m += out[j];
                    mask &= mask - 1;
                }
                if op == 3 {
                    m += fb_amt * (self.fb[0] + self.fb[1]) * 0.5;
                }
                out[op] = sin_cycles(self.phases[op] + m) * env * amp[op];
                let p = self.phases[op] + inc[op];
                self.phases[op] = p - p.floor();
            }
            self.fb = [self.fb[1], out[3]];
            let mut sum = 0.0;
            let mut mask = carriers;
            while mask != 0 {
                let j = mask.trailing_zeros() as usize;
                sum += out[j];
                mask &= mask - 1;
            }
            let y = sum * VOICE_GAIN;
            l[i] += y;
            r[i] += y;
        }

        self.active = (0..4).any(|op| carriers & (1 << op) != 0 && !self.envs[op].is_idle());
    }
}

pub struct FmSynth {
    pub poly: Poly<FmVoice>,
    pub shared: FmShared,
    sample_rate: f32,
}

impl FmSynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut synth = FmSynth {
            poly: Poly::new(|_| FmVoice::default()),
            shared: FmShared::default(),
            sample_rate,
        };
        let defaults: Vec<f32> = super::SynthKind::Fm.defaults();
        synth.update(&defaults);
        synth
    }

    pub fn update(&mut self, params: &[f32]) {
        let sr = self.sample_rate;
        let p = &params[COMMON.len()..];
        self.poly.update_common(params, p[VOICES], p[GLIDE], sr);
        let s = &mut self.shared;
        s.sample_rate = sr;
        s.algo = (p[ALGO].round() as usize).min(7);
        s.feedback = p[FEEDBACK];
        s.brightness = p[BRIGHTNESS];
        s.vib_inc = p[VIB_RATE] / sr;
        s.vib_depth = p[VIB_DEPTH];
        s.wheel_vib = p[WHEEL_VIB];
        for (op, o) in s.ops.iter_mut().enumerate() {
            let b = OP_BASE + op * OP_STRIDE;
            o.ratio = p[b + OP_RATIO] * (p[b + OP_DETUNE] / 1200.0).exp2();
            o.level = p[b + OP_LEVEL];
            o.vel_sens = p[b + OP_VEL];
            o.env = AdsrParams::new(
                p[b + OP_ATTACK],
                p[b + OP_DECAY],
                p[b + OP_SUSTAIN],
                p[b + OP_RELEASE],
                sr,
            );
        }
    }
}
