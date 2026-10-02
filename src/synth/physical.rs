//! Physical-modelling synth with two models:
//!
//! - **String**: extended Karplus-Strong. A delay loop tuned to the note's
//!   period (with a first-order Thiran all-pass for the fractional part)
//!   contains a one-zero damping filter (brightness), an optional chain of
//!   dispersion all-passes (stiffness, as in piano strings) and a loop gain
//!   set from the decay time. The phase delay of every loop element is
//!   measured at the fundamental and subtracted, so notes stay in tune at any
//!   setting. It is excited by filtered noise, comb-filtered by pick position.
//! - **Mallet**: modal synthesis. A short raised-cosine strike (its width set
//!   by hardness) rings a bank of two-pole resonators tuned to a material's
//!   partial ratios, with higher modes decaying faster.

use crate::dsp::{FilterMode, Rng, Svf, SvfCoefs, midi_to_freq, pan_gains};
use crate::params::{ParamDesc as P, Unit};

use super::{COMMON, Controls, MAX_BLOCK, Poly, VOICES_PARAM, Voice};

pub const MODELS: [&str; 2] = ["String", "Mallet"];
pub const MATERIALS: [&str; 7] = ["Wood", "Metal", "Glass", "Free Bar", "Bell", "Membrane", "Tine"];

pub const VOICES: usize = 0;
pub const MODEL: usize = 1;
pub const HARDNESS: usize = 2;
pub const POSITION: usize = 3;
pub const VEL_BRIGHT: usize = 4;
pub const DECAY: usize = 5;
pub const HF_DAMP: usize = 6;
pub const STIFFNESS: usize = 7;
pub const MATERIAL: usize = 8;
pub const BODY: usize = 9;
pub const RELEASE_DAMP: usize = 10;
pub const TONE: usize = 11;
pub const WIDTH: usize = 12;

pub static PARAMS: [P; 13] = [
    VOICES_PARAM,
    P::choice("model", "Model", "Model", &MODELS, 0),
    P::float("hardness", "Hardness", "Exciter", 0.0, 1.0, 0.5, Unit::Percent),
    P::float("position", "Position", "Exciter", 0.02, 0.5, 0.18, Unit::Percent),
    P::float("vel_bright", "Vel>Bright", "Exciter", 0.0, 1.0, 0.6, Unit::Percent),
    P::float("decay", "Decay", "Resonator", 0.05, 30.0, 3.0, Unit::Seconds).exp(),
    P::float("hf_damp", "HF Damping", "Resonator", 0.0, 1.0, 0.4, Unit::Percent),
    P::float("stiffness", "Stiffness", "Resonator", 0.0, 1.0, 0.0, Unit::Percent),
    P::choice("material", "Mallet Material", "Resonator", &MATERIALS, 0),
    P::float("body", "Body", "Response", 0.0, 1.0, 0.25, Unit::Percent),
    P::float("release_damp", "Release Damp", "Response", 0.0, 1.0, 0.6, Unit::Percent),
    P::float("tone", "Tone", "Response", 200.0, 20_000.0, 20_000.0, Unit::Hz).exp(),
    P::float("width", "Stereo Width", "Response", 0.0, 1.0, 0.4, Unit::Percent),
];

/// Partial ratios, relative amplitudes and a decay multiplier per material.
struct Material {
    ratios: &'static [f32],
    amps: &'static [f32],
    decay: f32,
}

const MATERIAL_TABLE: [Material; 7] = [
    // Tuned marimba bar: partials tuned to 1:4:10.
    Material { ratios: &[1.0, 3.99, 10.0], amps: &[1.0, 0.45, 0.2], decay: 0.5 },
    // Vibraphone-like metal bar: long, nearly harmonic ring.
    Material { ratios: &[1.0, 4.0, 10.0, 17.6], amps: &[1.0, 0.35, 0.18, 0.08], decay: 3.0 },
    Material { ratios: &[1.0, 2.32, 4.25, 6.63, 9.38], amps: &[1.0, 0.6, 0.4, 0.3, 0.2], decay: 1.5 },
    // Free (untuned) bar, xylophone-like.
    Material { ratios: &[1.0, 2.756, 5.404, 8.933, 13.344], amps: &[1.0, 0.5, 0.35, 0.25, 0.15], decay: 0.8 },
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
    Material { ratios: &[1.0, 5.9, 15.4], amps: &[1.0, 0.2, 0.08], decay: 2.0 },
];

const MAX_MODES: usize = 8;
const DELAY_LEN: usize = 4096;
const DISPERSION_STAGES: usize = 4;
const STRING_GAIN: f32 = 0.45;
const MALLET_GAIN: f32 = 0.32;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Model {
    #[default]
    String,
    Mallet,
}

#[derive(Default)]
pub struct PhysicalShared {
    sample_rate: f32,
    model: Model,
    hardness: f32,
    position: f32,
    vel_bright: f32,
    decay: f32,
    hf_damp: f32,
    stiffness: f32,
    material: usize,
    release_damp: f32,
    width: f32,
}

/// Phase delay in samples of H(e^{jw}) given its value at `w`.
fn phase_delay(re: f64, im: f64, w: f64) -> f64 {
    -im.atan2(re) / w
}

/// First-order all-pass (a + z^-1)/(1 + a z^-1) evaluated at `w`.
fn allpass_at(a: f64, w: f64) -> (f64, f64) {
    let (c, s) = (w.cos(), w.sin());
    // numerator a + e^{-jw}, denominator 1 + a e^{-jw}
    let (nr, ni) = (a + c, -s);
    let (dr, di) = (1.0 + a * c, -a * s);
    let den = dr * dr + di * di;
    ((nr * dr + ni * di) / den, (ni * dr - nr * di) / den)
}

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
    // String state.
    buf: Vec<f32>,
    exc: Vec<f32>,
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
    dc_x1: f32,
    dc_y1: f32,
    // Mallet state.
    modes: [Mode; MAX_MODES],
    mode_count: usize,
    mallet_f0: f32,
    pulse_len: u32,
    pulse_amp: f32,
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
            buf: vec![0.0; DELAY_LEN],
            exc: vec![0.0; DELAY_LEN],
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
            dc_x1: 0.0,
            dc_y1: 0.0,
            modes: [Mode::default(); MAX_MODES],
            mode_count: 0,
            mallet_f0: 440.0,
            pulse_len: 0,
            pulse_amp: 0.0,
        }
    }

    fn effective_hardness(&self, s: &PhysicalShared) -> f32 {
        (s.hardness + s.vel_bright * (self.vel - 0.7)).clamp(0.0, 1.0)
    }

    fn decay_time(&self, s: &PhysicalShared) -> f32 {
        if self.killed {
            0.01
        } else if self.released && s.release_damp > 0.0 {
            // Damping blends towards a quick 30 ms mute.
            let t = s.decay * (1.0 - s.release_damp) + 0.03 * s.release_damp;
            t.min(s.decay)
        } else {
            s.decay
        }
    }

    /// Set up the string loop for fundamental `f0`.
    fn tune_string(&mut self, s: &PhysicalShared, f0: f32) {
        let sr = s.sample_rate;
        let f0 = f0.clamp(sr / (DELAY_LEN as f32 - 8.0), sr * 0.2);
        let w = std::f64::consts::TAU * f0 as f64 / sr as f64;
        let period = (sr / f0) as f64;

        self.damp_s = 0.03 + 0.47 * s.hf_damp;
        let sd = self.damp_s as f64;
        let (lp_re, lp_im) = ((1.0 - sd) + sd * w.cos(), -sd * w.sin());
        let lp_delay = phase_delay(lp_re, lp_im, w);
        let lp_mag = (lp_re * lp_re + lp_im * lp_im).sqrt() as f32;

        self.disp_a = -0.75 * s.stiffness;
        let (dr, di) = allpass_at(self.disp_a as f64, w);
        let disp_delay = if s.stiffness > 0.0 { phase_delay(dr, di, w) * DISPERSION_STAGES as f64 } else { 0.0 };

        let remaining = (period - lp_delay - disp_delay).max(2.5);
        // Keep the Thiran all-pass delay within 0.5..1.5 samples for accuracy.
        let n = (remaining - 0.5).floor().max(1.0);
        let d = remaining - n;
        self.n_int = n as usize;
        self.ap_a = ((1.0 - d) / (1.0 + d)) as f32;
        self.period = period as f32;

        // Loop gain for a 60 dB decay over the decay time, compensating the
        // damping filter's loss at the fundamental.
        let per_period = 10f32.powf(-3.0 / (f0 * self.decay_time(s)));
        self.loop_gain = (per_period / lp_mag).min(0.9999);
    }

    fn excite_string(&mut self, s: &PhysicalShared) {
        let sr = s.sample_rate;
        let n = (self.period.round() as usize).clamp(2, DELAY_LEN - 1);
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
        let amp = self.vel / (2.0 * peak);
        // Add into the samples the loop reads next, so re-plucks of a
        // ringing string add to its vibration rather than replacing it.
        let mask = DELAY_LEN - 1;
        let start = self.w + DELAY_LEN - self.n_int;
        for i in 0..n.min(self.n_int) {
            self.buf[(start + i) & mask] += self.exc[i] * amp;
        }
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
            // Strike position shapes which modes are excited.
            let pos = 0.25 + 0.75 * (std::f32::consts::PI * (k + 1) as f32 * s.position).sin().abs();
            // Softer mallets excite upper modes less.
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
        // the strike's spectrum always covers the fundamental (a fixed-width
        // pulse would leave high notes almost silent).
        let contact = (0.0004 + (1.0 - h) * 0.004).min(0.6 / self.mallet_f0);
        let len = (s.sample_rate * contact).max(2.0);
        self.pulse_len = len as u32;
        // Unit-area pulse: low-frequency response is independent of hardness.
        self.pulse_amp = self.vel * 2.0 / len;
        self.age = 0;
    }
}

impl Voice for PhysicalVoice {
    type Shared = PhysicalShared;

    fn start(&mut self, note: u8, velocity: f32, _from_note: Option<f32>, s: &PhysicalShared) {
        if !self.active || note != self.note || self.model != s.model {
            // A different note or model: start from silence.
            self.buf.fill(0.0);
            self.modes = [Mode::default(); MAX_MODES];
            self.ap_x1 = 0.0;
            self.ap_y1 = 0.0;
            self.lp_x1 = 0.0;
            self.disp = [(0.0, 0.0); DISPERSION_STAGES];
            self.dc_x1 = 0.0;
            self.dc_y1 = 0.0;
            self.tuned_for = f32::NAN;
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
                Model::Mallet => self.tune_mallet(s, f0, !self.pending || self.tuned_for.is_finite()),
            }
            self.tuned_for = pitch;
        }
        if self.pending {
            match self.model {
                Model::String => self.excite_string(s),
                Model::Mallet => self.excite_mallet(s),
            }
            self.pending = false;
        }

        let mut peak = 0.0f32;
        match self.model {
            Model::String => {
                let mask = DELAY_LEN - 1;
                let (a, sd, g) = (self.ap_a, self.damp_s, self.loop_gain);
                let da = self.disp_a;
                let dispersive = da != 0.0;
                for i in 0..n {
                    let x = self.buf[(self.w + DELAY_LEN - self.n_int) & mask];
                    // Fractional delay (Thiran all-pass).
                    let mut y = a * x + self.ap_x1 - a * self.ap_y1;
                    self.ap_x1 = x;
                    self.ap_y1 = y;
                    if dispersive {
                        for st in &mut self.disp {
                            let out = da * y + st.0 - da * st.1;
                            st.0 = y;
                            st.1 = out;
                            y = out;
                        }
                    }
                    // Damping: one-zero low-pass.
                    let z = (1.0 - sd) * y + sd * self.lp_x1;
                    self.lp_x1 = y;
                    self.buf[self.w] = z * g;
                    self.w = (self.w + 1) & mask;
                    // DC blocker on the output.
                    let out = z - self.dc_x1 + 0.995 * self.dc_y1;
                    self.dc_x1 = z;
                    self.dc_y1 = out;
                    let o = out * STRING_GAIN;
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
        }
        // Free the voice once it has been effectively silent for a while.
        if peak < 1e-4 {
            self.quiet_blocks += 1;
            if self.quiet_blocks > 8 {
                self.active = false;
            }
        } else {
            self.quiet_blocks = 0;
        }
    }
}

pub struct PhysicalSynth {
    pub poly: Poly<PhysicalVoice>,
    pub shared: PhysicalShared,
    sample_rate: f32,
    body: f32,
    body_filters: [[Svf; 3]; 2],
    body_coefs: [SvfCoefs; 3],
    tone: SvfCoefs,
    tone_bypass: bool,
    tone_filters: [Svf; 2],
    tmp_l: [f32; MAX_BLOCK],
    tmp_r: [f32; MAX_BLOCK],
}

impl PhysicalSynth {
    pub fn new(sample_rate: f32) -> Self {
        let body_coefs = [98.0, 204.0, 405.0].map(|f| SvfCoefs::new(FilterMode::BandPass, f, 0.6, sample_rate));
        let mut synth = PhysicalSynth {
            poly: Poly::new(|i| PhysicalVoice::new(i as u32 + 1)),
            shared: PhysicalShared::default(),
            sample_rate,
            body: 0.0,
            body_filters: [[Svf::default(); 3]; 2],
            body_coefs,
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
        s.model = if p[MODEL].round() as usize == 1 { Model::Mallet } else { Model::String };
        s.hardness = p[HARDNESS];
        s.position = p[POSITION];
        s.vel_bright = p[VEL_BRIGHT];
        s.decay = p[DECAY];
        s.hf_damp = p[HF_DAMP];
        s.stiffness = p[STIFFNESS];
        s.material = p[MATERIAL].round() as usize;
        s.release_damp = p[RELEASE_DAMP];
        s.width = p[WIDTH];
        self.body = p[BODY];
        self.tone_bypass = p[TONE] >= 19_000.0;
        self.tone = SvfCoefs::new(FilterMode::LowPass, p[TONE], 0.0, sr);
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let (tl, tr) = (&mut self.tmp_l[..n], &mut self.tmp_r[..n]);
        tl.fill(0.0);
        tr.fill(0.0);
        self.poly.render(&self.shared, tl, tr);
        let body = self.body;
        for (ch, buf) in [&mut *tl, &mut *tr].into_iter().enumerate() {
            for x in buf.iter_mut() {
                let mut y = *x;
                if body > 0.0 {
                    // Wooden body: three broad resonances blended with the string.
                    let f = &mut self.body_filters[ch];
                    let res = f[0].process(&self.body_coefs[0], y) * 1.4
                        + f[1].process(&self.body_coefs[1], y) * 1.1
                        + f[2].process(&self.body_coefs[2], y) * 0.8;
                    y = y * (1.0 - 0.4 * body) + res * body;
                }
                if !self.tone_bypass {
                    y = self.tone_filters[ch].process(&self.tone, y);
                }
                *x = y;
            }
        }
        for i in 0..n {
            l[i] += tl[i];
            r[i] += tr[i];
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

    /// Pitch of a rendered note, via the sampler's YIN detector.
    fn measured_pitch(s: &mut PhysicalSynth, note: u8) -> f32 {
        s.poly.note_on(note, 0.8, &s.shared);
        let out = render(s, 48_000);
        let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(out.iter().all(|v| v.is_finite()) && peak > 0.02, "note {note}: peak {peak}");
        Sample::new("t", out, None, 48_000.0).detect_pitch().unwrap_or(f32::NAN)
    }

    #[test]
    fn strings_are_in_tune_across_the_range_and_settings() {
        for (damp, stiff) in [(0.0, 0.0), (0.4, 0.0), (1.0, 0.0), (0.4, 0.5)] {
            for note in [40u8, 52, 69, 84] {
                let mut s = synth_with(&[("hf_damp", damp), ("stiffness", stiff), ("body", 0.0)]);
                let p = measured_pitch(&mut s, note);
                assert!((p - note as f32).abs() < 0.05, "note {note} damp {damp} stiff {stiff}: measured {p}");
            }
        }
    }

    #[test]
    fn mallets_are_in_tune() {
        // Materials whose strongest partials sit on the harmonic series.
        for material in [0.0, 1.0, 6.0] {
            for note in [57u8, 72, 84] {
                let mut s = synth_with(&[("model", 1.0), ("material", material), ("body", 0.0)]);
                let p = measured_pitch(&mut s, note);
                assert!((p - note as f32).abs() < 0.05, "material {material} note {note}: measured {p}");
            }
        }
    }

    #[test]
    fn every_material_rings_and_dies_away() {
        for material in 0..MATERIALS.len() {
            let mut s = synth_with(&[("model", 1.0), ("material", material as f32), ("decay", 0.3)]);
            s.poly.note_on(60, 1.0, &s.shared);
            let out = render(&mut s, 48_000 * 8);
            assert!(out.iter().all(|v| v.is_finite()));
            let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak > 0.02 && peak < 1.5, "material {material} peak {peak}");
            assert_eq!(s.poly.active_voices(), 0, "material {material} still ringing");
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
        // Zero-crossing rate is a cheap brightness proxy.
        let zc = |hard: f32| {
            let mut s = synth_with(&[("hardness", hard), ("vel_bright", 0.0), ("body", 0.0)]);
            s.poly.note_on(48, 1.0, &s.shared);
            let out = render(&mut s, 4_800);
            out.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
        };
        assert!(zc(1.0) > zc(0.1) * 2, "hard {} soft {}", zc(1.0), zc(0.1));
    }

    #[test]
    fn factory_patches_play_cleanly() {
        crate::dsp::init_tables();
        for patch in crate::patch::factory_patches().into_iter().filter(|p| p.kind == SynthKind::Physical) {
            let mut s = PhysicalSynth::new(48_000.0);
            s.update(&patch.values());
            for note in [48u8, 60, 67, 76] {
                s.poly.note_on(note, 0.9, &s.shared);
            }
            let out = render(&mut s, 48_000 * 2);
            let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(out.iter().all(|v| v.is_finite()), "{}", patch.name);
            assert!(peak > 0.05 && peak < 1.5, "{}: peak {peak}", patch.name);
        }
    }
}
