//! Monophonic 303-style acid bass: PolyBLEP saw/square into a resonant
//! 4-pole ladder low-pass with a decaying filter envelope, accent and slide.
//!
//! As on the original, accent and slide come from how notes are played:
//! notes at or above the "Accent Vel" velocity are accented (louder, snappier
//! and with an extra filter sweep that builds up over consecutive accents),
//! and a note that starts while another is still held slides to its pitch
//! without retriggering the envelopes.

use crate::dsp::{AdsrParams, Env, Ladder, midi_to_freq, poly_blep};
use crate::params::{ParamDesc as P, Unit};

use super::{BEND_RANGE, COMMON, MAX_BLOCK, PolyControl, TRANSPOSE, glide_coef};

pub const WAVES: [&str; 2] = ["Saw", "Square"];

pub const WAVE: usize = 0;
pub const TUNE: usize = 1;
pub const DRIVE: usize = 2;
pub const CUTOFF: usize = 3;
pub const RESONANCE: usize = 4;
pub const ENV_MOD: usize = 5;
pub const DECAY: usize = 6;
pub const WHEEL_CUTOFF: usize = 7;
pub const ACCENT: usize = 8;
pub const ACCENT_VEL: usize = 9;
pub const SLIDE: usize = 10;

pub static PARAMS: [P; 11] = [
    P::choice("wave", "Waveform", "Oscillator", &WAVES, 0),
    P::float("tune", "Tune", "Oscillator", -100.0, 100.0, 0.0, Unit::Cents).step(1.0),
    P::float("drive", "Drive", "Oscillator", 0.0, 1.0, 0.25, Unit::Percent),
    P::float("cutoff", "Cutoff", "Filter", 30.0, 8_000.0, 350.0, Unit::Hz).exp(),
    P::float("resonance", "Resonance", "Filter", 0.0, 1.0, 0.7, Unit::Percent),
    P::float("env_mod", "Env Mod", "Filter", 0.0, 1.0, 0.55, Unit::Percent),
    P::float("decay", "Decay", "Filter", 0.05, 3.0, 0.45, Unit::Seconds).exp(),
    P::float("wheel_cutoff", "Wheel>Cutoff", "Filter", 0.0, 1.0, 0.5, Unit::Percent),
    P::float("accent", "Accent", "Accent & Slide", 0.0, 1.0, 0.6, Unit::Percent),
    P::int("accent_vel", "Accent Vel >=", "Accent & Slide", 1, 127, 100, Unit::None),
    P::float("slide", "Slide Time", "Accent & Slide", 0.005, 0.5, 0.06, Unit::Seconds).exp(),
];

/// Filter envelope decay used for accented notes, as on the 303.
const ACCENT_DECAY: f32 = 0.2;
const OUTPUT_GAIN: f32 = 0.35;
const MAX_HELD: usize = 16;

pub struct AcidSynth {
    sample_rate: f32,
    // Cached parameters.
    square: bool,
    tune: f32,
    drive: f32,
    cutoff: f32,
    k: f32,
    env_mod: f32,
    decay_coef: f32,
    accent_decay_coef: f32,
    wheel_cutoff: f32,
    accent_amt: f32,
    accent_vel: f32,
    slide_coef: f32,
    transpose: f32,
    bend_range: f32,
    amp_params: AdsrParams,
    // Performance state.
    held: [u8; MAX_HELD],
    held_len: usize,
    sustain: bool,
    sustained: bool,
    gate: bool,
    note: u8,
    accent: bool,
    bend: f32,
    modwheel: f32,
    // Audio state.
    pitch: f32,
    phase: f32,
    filter_env: f32,
    accent_sweep: f32,
    accent_gain: f32,
    amp: Env,
    ladder: Ladder,
}

impl AcidSynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = AcidSynth {
            sample_rate,
            square: false,
            tune: 0.0,
            drive: 0.0,
            cutoff: 350.0,
            k: 0.0,
            env_mod: 0.0,
            decay_coef: 0.0,
            accent_decay_coef: 0.0,
            wheel_cutoff: 0.0,
            accent_amt: 0.0,
            accent_vel: 100.0,
            slide_coef: 0.0,
            transpose: 0.0,
            bend_range: 2.0,
            amp_params: AdsrParams::new(0.002, 2.0, 0.5, 0.012, sample_rate),
            held: [0; MAX_HELD],
            held_len: 0,
            sustain: false,
            sustained: false,
            gate: false,
            note: 48,
            accent: false,
            bend: 0.0,
            modwheel: 0.0,
            pitch: 48.0,
            phase: 0.0,
            filter_env: 0.0,
            accent_sweep: 0.0,
            accent_gain: 1.0,
            amp: Env::default(),
            ladder: Ladder::default(),
        };
        s.update(&super::SynthKind::Acid.defaults());
        s
    }

    pub fn update(&mut self, params: &[f32]) {
        let sr = self.sample_rate;
        self.transpose = params[TRANSPOSE].round();
        self.bend_range = params[BEND_RANGE].round();
        let p = &params[COMMON.len()..];
        self.square = p[WAVE].round() as usize == 1;
        self.tune = p[TUNE] / 100.0;
        self.drive = p[DRIVE];
        self.cutoff = p[CUTOFF];
        self.k = p[RESONANCE] * 3.9;
        self.env_mod = p[ENV_MOD];
        // Filter envelope falls by 40 dB over the decay time.
        self.decay_coef = (0.01f32.ln() / (p[DECAY] * sr)).exp();
        self.accent_decay_coef = (0.01f32.ln() / (ACCENT_DECAY * sr)).exp();
        self.wheel_cutoff = p[WHEEL_CUTOFF];
        self.accent_amt = p[ACCENT];
        self.accent_vel = p[ACCENT_VEL];
        self.slide_coef = glide_coef(p[SLIDE], sr);
    }

    fn push_held(&mut self, note: u8) {
        self.remove_held(note);
        if self.held_len == MAX_HELD {
            self.held.copy_within(1.., 0);
            self.held_len -= 1;
        }
        self.held[self.held_len] = note;
        self.held_len += 1;
    }

    fn remove_held(&mut self, note: u8) {
        if let Some(i) = self.held[..self.held_len].iter().position(|&n| n == note) {
            self.held.copy_within(i + 1..self.held_len, i);
            self.held_len -= 1;
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: f32) {
        let legato = self.gate && self.held_len > 0;
        self.push_held(note);
        self.note = note;
        self.accent = velocity * 127.0 >= self.accent_vel - 0.5;
        self.sustained = false;
        if !legato {
            // A fresh note: jump to pitch and retrigger both envelopes.
            self.pitch = note as f32;
            self.filter_env = 1.0;
            self.gate = true;
            self.amp.trigger();
        }
        // Legato notes slide: `render` glides the pitch towards `self.note`.
    }

    fn gate_off(&mut self) {
        self.gate = false;
        self.amp.release();
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        if self.amp.is_idle() {
            return;
        }
        let sr = self.sample_rate;
        let target = self.note as f32;
        let offset = self.transpose + self.bend * self.bend_range + self.tune;
        let env_coef = if self.accent { self.accent_decay_coef } else { self.decay_coef };
        let sweep_coef = 1.0 - (-1.0 / (0.06 * sr)).exp();
        let gain_coef = 1.0 - (-1.0 / (0.004 * sr)).exp();
        let accent_level = if self.accent { self.accent_amt } else { 0.0 };
        let wheel_oct = self.modwheel * self.wheel_cutoff * 3.0;
        let drive_pre = 1.0 + self.drive * 9.0;
        let comp = 1.0 + 0.5 * self.k;

        for i in 0..n {
            self.pitch = target + (self.pitch - target) * self.slide_coef;
            let dt = (midi_to_freq(self.pitch + offset) / sr).min(0.45);
            self.phase += dt;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            let t = self.phase;
            let osc = if self.square {
                let naive = if t < 0.5 { 1.0 } else { -1.0 };
                naive + poly_blep(t, dt) - poly_blep((t + 0.5).fract(), dt)
            } else {
                2.0 * t - 1.0 - poly_blep(t, dt)
            };

            self.filter_env *= env_coef;
            // Accent sweep: smoothed, so back-to-back accents ride higher.
            self.accent_sweep += (self.filter_env * accent_level - self.accent_sweep) * sweep_coef;
            self.accent_gain += (1.0 + accent_level - self.accent_gain) * gain_coef;

            let octaves = self.env_mod * 4.5 * self.filter_env + self.accent_sweep * 3.0 + wheel_oct;
            let fc = (self.cutoff * octaves.exp2()).min(sr * 0.45);
            let g = (std::f32::consts::PI * fc / sr).tan();
            let y = self.ladder.process(osc * 0.8, g, self.k) * comp;
            let y = y * (1.0 - self.drive) + (y * drive_pre).tanh() * self.drive * 0.7;

            let out = y * self.amp.next(&self.amp_params) * self.accent_gain * OUTPUT_GAIN;
            l[i] += out;
            r[i] += out;
        }
    }
}

impl PolyControl for AcidSynth {
    fn note_off(&mut self, note: u8) {
        self.remove_held(note);
        if self.held_len > 0 {
            // Last-note priority: slide back to the most recent held note.
            if note == self.note {
                self.note = self.held[self.held_len - 1];
            }
        } else if self.gate {
            if self.sustain {
                self.sustained = true;
            } else {
                self.gate_off();
            }
        }
    }

    fn set_sustain(&mut self, on: bool) {
        self.sustain = on;
        if !on && self.sustained {
            self.sustained = false;
            if self.held_len == 0 {
                self.gate_off();
            }
        }
    }

    fn set_bend(&mut self, v: f32) {
        self.bend = v.clamp(-1.0, 1.0);
    }

    fn set_modwheel(&mut self, v: f32) {
        self.modwheel = v.clamp(0.0, 1.0);
    }

    fn all_notes_off(&mut self) {
        self.held_len = 0;
        self.sustain = false;
        self.sustained = false;
        self.gate_off();
    }

    fn active_voices(&self) -> usize {
        usize::from(!self.amp.is_idle())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(s: &mut AcidSynth, frames: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames);
        let (mut l, mut r) = ([0.0f32; MAX_BLOCK], [0.0f32; MAX_BLOCK]);
        while out.len() < frames {
            l.fill(0.0);
            r.fill(0.0);
            s.render(&mut l, &mut r);
            out.extend_from_slice(&l);
        }
        out
    }

    #[test]
    fn legato_slides_without_retrigger() {
        crate::dsp::init_tables();
        let mut s = AcidSynth::new(48_000.0);
        s.note_on(36, 0.5);
        render(&mut s, 4800);
        let env_before = s.filter_env;
        s.note_on(48, 0.5); // overlaps: slide
        assert_eq!(s.filter_env, env_before);
        assert!(s.pitch < 37.0);
        render(&mut s, 9600);
        assert!((s.pitch - 48.0).abs() < 0.01);
        // Releasing the newer note slides back to the held one.
        s.note_off(48);
        assert_eq!(s.note, 36);
        s.note_off(36);
        render(&mut s, 4800);
        assert_eq!(s.active_voices(), 0);
    }

    #[test]
    fn detached_notes_retrigger_and_accent() {
        crate::dsp::init_tables();
        let mut s = AcidSynth::new(48_000.0);
        s.note_on(36, 0.5);
        render(&mut s, 4800);
        s.note_off(36);
        s.note_on(40, 1.0);
        assert_eq!(s.filter_env, 1.0);
        assert!(s.accent);
        let out = render(&mut s, 9600);
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(out.iter().any(|v| v.abs() > 0.05));
    }
}
