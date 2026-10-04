//! Polyphonic granular synth. Each voice runs its own grain cloud over a
//! source sample (a loaded WAV or one of the built-in sources), pitched by
//! the played note, through an amp envelope and a state-variable filter.

use std::sync::Arc;

use crate::dsp::{
    AdsrParams, Env, FilterMode, Rng, Svf, SvfCoefs, pan_gains, semitones_to_ratio, sin_cycles,
};
use crate::params::{ParamDesc as P, Unit};
use crate::sample::{Builtins, Sample};

use super::{COMMON, Controls, GLIDE_PARAM, MAX_BLOCK, Poly, VOICES_PARAM, Voice, glide};

pub const SOURCES: [&str; 6] = ["File", "Choir", "Glass", "Saw", "Pluck", "Noise"];
pub const SHAPES: [&str; 4] = ["Smooth", "Triangle", "Flat", "Perc"];
pub const FILTERS: [&str; 3] = ["LowPass", "BandPass", "HighPass"];

pub const VOICES: usize = 0;
pub const GLIDE: usize = 1;
pub const SOURCE: usize = 2;
pub const POSITION: usize = 3;
pub const SPRAY: usize = 4;
pub const SPEED: usize = 5;
pub const SIZE: usize = 6;
pub const DENSITY: usize = 7;
pub const JITTER: usize = 8;
pub const PITCH_SPRAY: usize = 9;
pub const FINE: usize = 10;
pub const SPREAD: usize = 11;
pub const REVERSE: usize = 12;
pub const SHAPE: usize = 13;
pub const KEYTRACK: usize = 14;
pub const ATTACK: usize = 15;
pub const DECAY: usize = 16;
pub const SUSTAIN: usize = 17;
pub const RELEASE: usize = 18;
pub const FILTER_TYPE: usize = 19;
pub const CUTOFF: usize = 20;
pub const RESONANCE: usize = 21;
pub const WHEEL_SPRAY: usize = 22;

pub static PARAMS: [P; 23] = [
    VOICES_PARAM,
    GLIDE_PARAM,
    P::choice("source", "Source", "Source", &SOURCES, 1),
    P::float(
        "position",
        "Position",
        "Source",
        0.0,
        1.0,
        0.3,
        Unit::Percent,
    ),
    P::float("spray", "Spray", "Source", 0.0, 1.0, 0.1, Unit::Percent),
    P::float("speed", "Scan Speed", "Source", -2.0, 2.0, 0.0, Unit::Ratio).step(0.05),
    P::float(
        "size",
        "Grain Size",
        "Grains",
        0.005,
        2.0,
        0.12,
        Unit::Seconds,
    )
    .exp(),
    P::float("density", "Density", "Grains", 1.0, 200.0, 25.0, Unit::Hz).exp(),
    P::float("jitter", "Jitter", "Grains", 0.0, 1.0, 0.3, Unit::Percent),
    P::float(
        "pitch_spray",
        "Pitch Spray",
        "Grains",
        0.0,
        12.0,
        0.0,
        Unit::Semitones,
    )
    .step(0.1),
    P::float(
        "fine",
        "Fine Tune",
        "Grains",
        -100.0,
        100.0,
        0.0,
        Unit::Cents,
    )
    .step(1.0),
    P::float(
        "spread",
        "Stereo Spread",
        "Grains",
        0.0,
        1.0,
        0.6,
        Unit::Percent,
    ),
    P::float(
        "reverse",
        "Reverse Prob",
        "Grains",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
    P::choice("shape", "Window", "Grains", &SHAPES, 0),
    P::toggle("keytrack", "Key Track", "Grains", true),
    P::float(
        "attack",
        "Attack",
        "Amp Env",
        0.001,
        10.0,
        0.3,
        Unit::Seconds,
    )
    .exp(),
    P::float("decay", "Decay", "Amp Env", 0.005, 20.0, 1.0, Unit::Seconds).exp(),
    P::float(
        "sustain",
        "Sustain",
        "Amp Env",
        0.0,
        1.0,
        0.8,
        Unit::Percent,
    ),
    P::float(
        "release",
        "Release",
        "Amp Env",
        0.005,
        20.0,
        1.5,
        Unit::Seconds,
    )
    .exp(),
    P::choice("filter_type", "Type", "Filter", &FILTERS, 0),
    P::float(
        "cutoff",
        "Cutoff",
        "Filter",
        20.0,
        20_000.0,
        12_000.0,
        Unit::Hz,
    )
    .exp(),
    P::float(
        "resonance",
        "Resonance",
        "Filter",
        0.0,
        1.0,
        0.1,
        Unit::Percent,
    ),
    P::float(
        "wheel_spray",
        "Wheel>Spray",
        "Filter",
        0.0,
        1.0,
        0.5,
        Unit::Percent,
    ),
];

const MAX_GRAINS: usize = 64;

#[derive(Default)]
pub struct GranularShared {
    sample_rate: f32,
    source: Option<Arc<Sample>>,
    position: f32,
    spray: f32,
    speed: f32,
    size_samples: f32,
    interval: f32,
    jitter: f32,
    pitch_spray: f32,
    fine_ratio: f32,
    spread: f32,
    reverse: f32,
    shape: u8,
    keytrack: bool,
    env: AdsrParams,
    filter: SvfCoefs,
    filter_bypass: bool,
    wheel_spray: f32,
    grain_gain: f32,
}

#[derive(Clone, Copy, Default)]
struct Grain {
    pos: f64,
    inc: f64,
    age: f32,
    inv_len: f32,
    gain_l: f32,
    gain_r: f32,
    delay: usize,
}

pub struct GranularVoice {
    grains: [Grain; MAX_GRAINS],
    count: usize,
    note: u8,
    pitch: f32,
    vel: f32,
    env: Env,
    spawn_acc: f32,
    next_interval: f32,
    scan: f32,
    filters: [Svf; 2],
    rng: Rng,
    active: bool,
    buf_l: [f32; MAX_BLOCK],
    buf_r: [f32; MAX_BLOCK],
}

impl GranularVoice {
    fn new(seed: u32) -> Self {
        GranularVoice {
            grains: [Grain::default(); MAX_GRAINS],
            count: 0,
            note: 60,
            pitch: 60.0,
            vel: 1.0,
            env: Env::default(),
            spawn_acc: 0.0,
            next_interval: 1.0,
            scan: 0.0,
            filters: [Svf::default(); 2],
            rng: Rng::new(0x9E37_79B9 ^ seed.wrapping_mul(2_654_435_761)),
            active: false,
            buf_l: [0.0; MAX_BLOCK],
            buf_r: [0.0; MAX_BLOCK],
        }
    }

    fn spawn(&mut self, s: &GranularShared, src: &Sample, ratio: f32, spray: f32, offset: usize) {
        if self.count >= MAX_GRAINS {
            return;
        }
        let center = s.position + self.scan + spray * self.rng.bipolar() * 0.5;
        let start = center.rem_euclid(1.0) as f64 * src.len() as f64;
        let mut inc = ratio * semitones_to_ratio(s.pitch_spray * self.rng.bipolar());
        if self.rng.unipolar() < s.reverse {
            inc = -inc;
        }
        let (gl, gr) = pan_gains(s.spread * self.rng.bipolar());
        self.grains[self.count] = Grain {
            pos: start,
            inc: inc as f64,
            age: 0.0,
            inv_len: 1.0 / s.size_samples,
            gain_l: gl * s.grain_gain,
            gain_r: gr * s.grain_gain,
            delay: offset,
        };
        self.count += 1;
    }
}

#[inline]
fn window(shape: u8, t: f32) -> f32 {
    match shape {
        // Hann
        0 => {
            let s = sin_cycles(t * 0.5);
            s * s
        }
        1 => 1.0 - (2.0 * t - 1.0).abs(),
        // Tukey-ish: 10% raised-cosine edges, flat middle.
        2 => {
            let edge = 0.1;
            if t < edge {
                let s = sin_cycles(t / edge * 0.25);
                s * s
            } else if t > 1.0 - edge {
                let s = sin_cycles((1.0 - t) / edge * 0.25);
                s * s
            } else {
                1.0
            }
        }
        // Percussive: fast attack, cubic decay.
        _ => {
            if t < 0.02 {
                t / 0.02
            } else {
                let x = 1.0 - (t - 0.02) / 0.98;
                x * x * x
            }
        }
    }
}

impl Voice for GranularVoice {
    type Shared = GranularShared;

    fn start(&mut self, note: u8, velocity: f32, from_note: Option<f32>, _s: &GranularShared) {
        if !self.active {
            self.count = 0;
            self.scan = 0.0;
            self.spawn_acc = 0.0;
            self.next_interval = 0.0; // spawn on the first sample
            self.filters = [Svf::default(); 2];
            self.pitch = from_note.unwrap_or(note as f32);
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

    fn render(&mut self, s: &GranularShared, ctl: &Controls, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let src = match &s.source {
            Some(src) if !src.is_empty() => &**src,
            _ => {
                // Nothing to play: let the envelope run out so the voice frees up.
                for _ in 0..n {
                    self.env.next(&s.env);
                }
                self.active = !self.env.is_idle();
                return;
            }
        };

        self.pitch = glide(self.pitch, self.note as f32, ctl.glide_coef, n);
        let semis = if s.keytrack {
            self.pitch - 60.0 + ctl.pitch
        } else {
            ctl.pitch
        };
        let ratio = semitones_to_ratio(semis) * s.fine_ratio * (src.sample_rate / s.sample_rate);

        let advance = s.speed * n as f32 * src.sample_rate / s.sample_rate / src.len() as f32;
        self.scan = (self.scan + advance).rem_euclid(1.0);
        let spray = (s.spray + ctl.modwheel * s.wheel_spray).min(1.0);

        // Schedule grain onsets for this block.
        for i in 0..n {
            self.spawn_acc += 1.0;
            if self.spawn_acc >= self.next_interval {
                self.spawn_acc -= self.next_interval;
                self.next_interval = s.interval * (1.0 + s.jitter * self.rng.bipolar() * 0.9);
                self.spawn(s, src, ratio, spray, i);
            }
        }

        // Render grains into the voice buffer.
        self.buf_l[..n].fill(0.0);
        self.buf_r[..n].fill(0.0);
        let mut g = 0;
        while g < self.count {
            let grain = &mut self.grains[g];
            let mut dead = false;
            for i in grain.delay..n {
                let t = grain.age * grain.inv_len;
                if t >= 1.0 {
                    dead = true;
                    break;
                }
                let v = src.read(grain.pos) * window(s.shape, t);
                self.buf_l[i] += v * grain.gain_l;
                self.buf_r[i] += v * grain.gain_r;
                grain.pos += grain.inc;
                grain.age += 1.0;
            }
            grain.delay = 0;
            if dead {
                self.count -= 1;
                self.grains[g] = self.grains[self.count];
            } else {
                g += 1;
            }
        }

        let amp = 0.25 + 0.75 * self.vel;
        for i in 0..n {
            let e = self.env.next(&s.env) * amp;
            let (mut a, mut b) = (self.buf_l[i] * e, self.buf_r[i] * e);
            if !s.filter_bypass {
                a = self.filters[0].process(&s.filter, a);
                b = self.filters[1].process(&s.filter, b);
            }
            l[i] += a;
            r[i] += b;
        }
        self.active = !self.env.is_idle();
    }
}

pub struct GranularSynth {
    pub poly: Poly<GranularVoice>,
    pub shared: GranularShared,
    builtins: Builtins,
    file: Option<Arc<Sample>>,
    source_index: usize,
    sample_rate: f32,
}

impl GranularSynth {
    pub fn new(sample_rate: f32, builtins: Builtins) -> Self {
        let mut synth = GranularSynth {
            poly: Poly::new(|i| GranularVoice::new(i as u32 + 1)),
            shared: GranularShared::default(),
            builtins,
            file: None,
            source_index: 1,
            sample_rate,
        };
        synth.update(&super::SynthKind::Granular.defaults());
        synth
    }

    fn select_source(&mut self) {
        let next = if self.source_index == 0 {
            self.file.clone()
        } else {
            self.builtins.get(self.source_index - 1).cloned()
        };
        // Every Arc we drop here is still owned by `self.file`, `builtins` or
        // the UI, so this never frees memory on the audio thread.
        self.shared.source = next;
    }

    pub fn set_file_sample(&mut self, sample: Option<Arc<Sample>>) -> Option<Arc<Sample>> {
        let old = std::mem::replace(&mut self.file, sample);
        self.select_source();
        old
    }

    pub fn update(&mut self, params: &[f32]) {
        let sr = self.sample_rate;
        let p = &params[COMMON.len()..];
        self.poly.update_common(params, p[VOICES], p[GLIDE], sr);
        let source_index = (p[SOURCE].round() as usize).min(SOURCES.len() - 1);
        if source_index != self.source_index || self.shared.source.is_none() {
            self.source_index = source_index;
            self.select_source();
        }
        let s = &mut self.shared;
        s.sample_rate = sr;
        s.position = p[POSITION];
        s.spray = p[SPRAY];
        s.speed = p[SPEED];
        s.size_samples = (p[SIZE] * sr).max(16.0);
        s.interval = sr / p[DENSITY].max(0.1);
        s.jitter = p[JITTER];
        s.pitch_spray = p[PITCH_SPRAY];
        s.fine_ratio = (p[FINE] / 1200.0).exp2();
        s.spread = p[SPREAD];
        s.reverse = p[REVERSE];
        s.shape = p[SHAPE].round() as u8;
        s.keytrack = p[KEYTRACK] >= 0.5;
        s.env = AdsrParams::new(p[ATTACK], p[DECAY], p[SUSTAIN], p[RELEASE], sr);
        let mode = match p[FILTER_TYPE].round() as usize {
            1 => FilterMode::BandPass,
            2 => FilterMode::HighPass,
            _ => FilterMode::LowPass,
        };
        s.filter_bypass =
            mode == FilterMode::LowPass && p[CUTOFF] >= 19_000.0 && p[RESONANCE] < 0.05;
        s.filter = SvfCoefs::new(mode, p[CUTOFF], p[RESONANCE], sr);
        s.wheel_spray = p[WHEEL_SPRAY];
        let overlap = p[SIZE] * p[DENSITY];
        s.grain_gain = 1.0 / overlap.max(1.0).sqrt();
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        self.poly.render(&self.shared, l, r);
    }
}
