//! Polyphonic analog-style subtractive synth: two PolyBLEP oscillators plus
//! sub and noise, optional unison stacking, a 12 dB (state-variable) or
//! 24 dB (ladder) resonant low-pass with its own envelope, a global LFO and a
//! Juno-style stereo chorus.

use crate::dsp::{
    AdsrParams, Env, FilterMode, Ladder, Rng, Svf, SvfCoefs, midi_to_freq, pan_gains, poly_blep,
    sin_cycles,
};
use crate::params::{ParamDesc as P, Unit};

use super::{COMMON, Controls, GLIDE_PARAM, MAX_BLOCK, Poly, VOICES_PARAM, Voice, glide};

pub const WAVES: [&str; 3] = ["Saw", "Pulse", "Triangle"];
pub const POLES: [&str; 2] = ["12 dB", "24 dB"];
pub const LFO_WAVES: [&str; 4] = ["Sine", "Triangle", "Square", "S&H"];
pub const CHORUS_MODES: [&str; 4] = ["Off", "I", "II", "I+II"];

pub const VOICES: usize = 0;
pub const GLIDE: usize = 1;
pub const OSC1_WAVE: usize = 2;
pub const OSC2_WAVE: usize = 3;
pub const OSC2_SEMI: usize = 4;
pub const OSC2_DETUNE: usize = 5;
pub const PULSE_WIDTH: usize = 6;
pub const OSC_MIX: usize = 7;
pub const SUB: usize = 8;
pub const NOISE: usize = 9;
pub const DRIFT: usize = 10;
pub const UNISON: usize = 11;
pub const UNISON_DETUNE: usize = 12;
pub const UNISON_SPREAD: usize = 13;
pub const CUTOFF: usize = 14;
pub const RESONANCE: usize = 15;
pub const POLES_P: usize = 16;
pub const FILTER_ENV: usize = 17;
pub const KEY_TRACK: usize = 18;
pub const VEL_FILTER: usize = 19;
pub const F_ATTACK: usize = 20;
pub const F_DECAY: usize = 21;
pub const F_SUSTAIN: usize = 22;
pub const F_RELEASE: usize = 23;
pub const ATTACK: usize = 24;
pub const DECAY: usize = 25;
pub const SUSTAIN: usize = 26;
pub const RELEASE: usize = 27;
pub const VEL_AMP: usize = 28;
pub const LFO_WAVE: usize = 29;
pub const LFO_RATE: usize = 30;
pub const LFO_PITCH: usize = 31;
pub const LFO_FILTER: usize = 32;
pub const LFO_PW: usize = 33;
pub const WHEEL_VIB: usize = 34;
pub const CHORUS: usize = 35;

pub const MAX_UNISON: usize = 7;

pub static PARAMS: [P; 36] = [
    VOICES_PARAM,
    GLIDE_PARAM,
    P::choice("osc1_wave", "Osc 1 Wave", "Oscillators", &WAVES, 0),
    P::choice("osc2_wave", "Osc 2 Wave", "Oscillators", &WAVES, 0),
    P::int(
        "osc2_semi",
        "Osc 2 Semi",
        "Oscillators",
        -24,
        24,
        0,
        Unit::Semitones,
    ),
    P::float(
        "osc2_detune",
        "Osc 2 Detune",
        "Oscillators",
        -50.0,
        50.0,
        7.0,
        Unit::Cents,
    )
    .step(1.0),
    P::float(
        "pulse_width",
        "Pulse Width",
        "Oscillators",
        0.05,
        0.95,
        0.5,
        Unit::Percent,
    ),
    P::float(
        "osc_mix",
        "Osc 1<>2 Mix",
        "Oscillators",
        0.0,
        1.0,
        0.5,
        Unit::Percent,
    ),
    P::float(
        "sub",
        "Sub Osc",
        "Oscillators",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
    P::float(
        "noise",
        "Noise",
        "Oscillators",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
    P::float(
        "drift",
        "Analog Drift",
        "Oscillators",
        0.0,
        1.0,
        0.3,
        Unit::Percent,
    ),
    P::int(
        "unison",
        "Unison",
        "Unison",
        1,
        MAX_UNISON as i32,
        1,
        Unit::None,
    ),
    P::float(
        "unison_detune",
        "Detune",
        "Unison",
        0.0,
        1.0,
        0.3,
        Unit::Percent,
    ),
    P::float(
        "unison_spread",
        "Stereo Spread",
        "Unison",
        0.0,
        1.0,
        0.7,
        Unit::Percent,
    ),
    P::float(
        "cutoff",
        "Cutoff",
        "Filter",
        20.0,
        20_000.0,
        2_000.0,
        Unit::Hz,
    )
    .exp(),
    P::float(
        "resonance",
        "Resonance",
        "Filter",
        0.0,
        1.0,
        0.2,
        Unit::Percent,
    ),
    P::choice("poles", "Slope", "Filter", &POLES, 1),
    P::float(
        "filter_env",
        "Env Amount",
        "Filter",
        -1.0,
        1.0,
        0.4,
        Unit::Percent,
    ),
    P::float(
        "key_track",
        "Key Track",
        "Filter",
        0.0,
        1.0,
        0.5,
        Unit::Percent,
    ),
    P::float(
        "vel_filter",
        "Vel>Cutoff",
        "Filter",
        0.0,
        1.0,
        0.3,
        Unit::Percent,
    ),
    P::float(
        "f_attack",
        "Attack",
        "Filter Env",
        0.001,
        10.0,
        0.005,
        Unit::Seconds,
    )
    .exp(),
    P::float(
        "f_decay",
        "Decay",
        "Filter Env",
        0.005,
        20.0,
        0.6,
        Unit::Seconds,
    )
    .exp(),
    P::float(
        "f_sustain",
        "Sustain",
        "Filter Env",
        0.0,
        1.0,
        0.3,
        Unit::Percent,
    ),
    P::float(
        "f_release",
        "Release",
        "Filter Env",
        0.005,
        20.0,
        0.5,
        Unit::Seconds,
    )
    .exp(),
    P::float(
        "attack",
        "Attack",
        "Amp Env",
        0.001,
        10.0,
        0.005,
        Unit::Seconds,
    )
    .exp(),
    P::float("decay", "Decay", "Amp Env", 0.005, 20.0, 0.5, Unit::Seconds).exp(),
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
        0.4,
        Unit::Seconds,
    )
    .exp(),
    P::float(
        "vel_amp",
        "Vel>Amp",
        "Amp Env",
        0.0,
        1.0,
        0.5,
        Unit::Percent,
    ),
    P::choice("lfo_wave", "Wave", "LFO", &LFO_WAVES, 1),
    P::float("lfo_rate", "Rate", "LFO", 0.05, 20.0, 4.0, Unit::Hz).exp(),
    P::float("lfo_pitch", "> Pitch", "LFO", 0.0, 100.0, 0.0, Unit::Cents).step(1.0),
    P::float(
        "lfo_filter",
        "> Cutoff",
        "LFO",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
    P::float(
        "lfo_pw",
        "> Pulse Width",
        "LFO",
        0.0,
        1.0,
        0.0,
        Unit::Percent,
    ),
    P::float(
        "wheel_vib",
        "Wheel>Vibrato",
        "LFO",
        0.0,
        100.0,
        30.0,
        Unit::Cents,
    )
    .step(1.0),
    P::choice("chorus", "Mode", "Chorus", &CHORUS_MODES, 0),
];

const VOICE_GAIN: f32 = 0.28;
/// Filter coefficients are recomputed every this many samples.
const CONTROL_STEP: usize = 8;

#[derive(Default)]
pub struct AnalogShared {
    sample_rate: f32,
    wave1: u8,
    wave2: u8,
    osc2_semis: f32,
    pulse_width: f32,
    mix: f32,
    sub: f32,
    noise: f32,
    drift: f32,
    unison: usize,
    unison_detune: f32,
    unison_spread: f32,
    cutoff: f32,
    resonance: f32,
    ladder: bool,
    filter_env: f32,
    key_track: f32,
    vel_filter: f32,
    filter_adsr: AdsrParams,
    amp_adsr: AdsrParams,
    vel_amp: f32,
    lfo_pitch: f32,
    lfo_filter: f32,
    lfo_pw: f32,
    wheel_vib: f32,
    /// Current global LFO value (-1..1), updated once per block.
    lfo: f32,
}

#[inline]
fn osc(wave: u8, phase: f32, dt: f32, pw: f32) -> f32 {
    match wave {
        0 => 2.0 * phase - 1.0 - poly_blep(phase, dt),
        1 => {
            let naive = if phase < pw { 1.0 } else { -1.0 };
            naive + poly_blep(phase, dt) - poly_blep((phase - pw).rem_euclid(1.0), dt)
        }
        _ => 1.0 - 4.0 * (phase - 0.5).abs(),
    }
}

#[inline]
fn wrap(p: &mut f32, inc: f32) {
    *p += inc;
    if *p >= 1.0 {
        *p -= 1.0;
    }
}

pub struct AnalogVoice {
    active: bool,
    note: u8,
    pitch: f32,
    vel: f32,
    phase1: [f32; MAX_UNISON],
    phase2: [f32; MAX_UNISON],
    sub_phase: f32,
    /// Per-note random detune of each oscillator in cents (analog drift).
    drift: [f32; 2],
    amp_env: Env,
    filter_env: Env,
    ladders: [Ladder; 2],
    svfs: [Svf; 2],
    rng: Rng,
}

impl AnalogVoice {
    fn new(seed: u32) -> Self {
        AnalogVoice {
            active: false,
            note: 60,
            pitch: 60.0,
            vel: 1.0,
            phase1: [0.0; MAX_UNISON],
            phase2: [0.0; MAX_UNISON],
            sub_phase: 0.0,
            drift: [0.0; 2],
            amp_env: Env::default(),
            filter_env: Env::default(),
            ladders: [Ladder::default(); 2],
            svfs: [Svf::default(); 2],
            rng: Rng::new(0xA11A_0000 ^ seed.wrapping_mul(0x9E37_79B9)),
        }
    }
}

impl Voice for AnalogVoice {
    type Shared = AnalogShared;

    fn start(&mut self, note: u8, velocity: f32, from_note: Option<f32>, s: &AnalogShared) {
        if !self.active {
            // Free-running oscillators: random start phases, like real analogs.
            for p in self.phase1.iter_mut().chain(self.phase2.iter_mut()) {
                *p = self.rng.unipolar();
            }
            self.ladders = [Ladder::default(); 2];
            self.svfs = [Svf::default(); 2];
            self.pitch = from_note.unwrap_or(note as f32);
        } else if from_note.is_none() {
            self.pitch = note as f32;
        }
        self.drift = [
            self.rng.bipolar() * s.drift * 6.0,
            self.rng.bipolar() * s.drift * 6.0,
        ];
        self.note = note;
        self.vel = velocity;
        self.active = true;
        self.amp_env.trigger();
        self.filter_env.trigger();
    }

    fn release(&mut self) {
        self.amp_env.release();
        self.filter_env.release();
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn render(&mut self, s: &AnalogShared, ctl: &Controls, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let sr = s.sample_rate;
        self.pitch = glide(self.pitch, self.note as f32, ctl.glide_coef, n);
        let vib_cents = s.lfo * (s.lfo_pitch + ctl.modwheel * s.wheel_vib);
        let base = self.pitch + ctl.pitch + vib_cents / 100.0;
        let f1 = midi_to_freq(base + self.drift[0] / 100.0);
        let f2 = midi_to_freq(base + s.osc2_semis + self.drift[1] / 100.0);
        let fsub = f1 * 0.5;
        let pw = (s.pulse_width + s.lfo_pw * 0.4 * s.lfo).clamp(0.05, 0.95);

        // Unison: symmetric detune (cents) and pan positions.
        let uni = s.unison.clamp(1, MAX_UNISON);
        let mut ratios = [1.0f32; MAX_UNISON];
        let mut gains = [(1.0f32, 1.0f32); MAX_UNISON];
        let stereo = uni > 1 && s.unison_spread > 0.0;
        for k in 0..uni {
            let pos = if uni == 1 {
                0.0
            } else {
                k as f32 / (uni - 1) as f32 * 2.0 - 1.0
            };
            ratios[k] = (pos * s.unison_detune * 50.0 / 1200.0).exp2();
            gains[k] = if stereo {
                pan_gains(pos * s.unison_spread)
            } else {
                (1.0, 1.0)
            };
        }
        let uni_norm = 1.0 / (uni as f32).sqrt();
        let (g1, g2) = ((1.0 - s.mix) * uni_norm, s.mix * uni_norm);

        let amp_scale = VOICE_GAIN * crate::dsp::velocity_gain(self.vel, s.vel_amp);
        let k_ladder = s.resonance * 3.9;
        let comp = 1.0 + 0.5 * k_ladder;
        // The state-variable filter's resonant peak grows as 1/k; trim its
        // input as resonance rises so sweeping resonance doesn't jump in level.
        let svf_k = 2.0 - 1.96 * s.resonance;
        let svf_trim = 0.4 + 0.3 * svf_k;
        let key_oct = s.key_track * (self.pitch - 60.0) / 12.0;
        let vel_oct = s.vel_filter * 3.0 * (self.vel - 1.0);
        let lfo_oct = s.lfo_filter * 3.0 * s.lfo;

        let mut g = 0.0;
        let mut svf = SvfCoefs::default();
        for i in 0..n {
            let fenv = self.filter_env.next(&s.filter_adsr);
            if i % CONTROL_STEP == 0 {
                let oct = s.filter_env * 6.0 * fenv + key_oct + vel_oct + lfo_oct;
                let fc = (s.cutoff * oct.exp2()).clamp(20.0, sr * 0.45);
                if s.ladder {
                    g = (std::f32::consts::PI * fc / sr).tan();
                } else {
                    svf = SvfCoefs::new(FilterMode::LowPass, fc, s.resonance, sr);
                }
            }

            let (mut xl, mut xr) = (0.0f32, 0.0f32);
            for k in 0..uni {
                let dt1 = f1 * ratios[k] / sr;
                let dt2 = f2 * ratios[k] / sr;
                let v = osc(s.wave1, self.phase1[k], dt1, pw) * g1
                    + osc(s.wave2, self.phase2[k], dt2, pw) * g2;
                wrap(&mut self.phase1[k], dt1);
                wrap(&mut self.phase2[k], dt2);
                xl += v * gains[k].0;
                xr += v * gains[k].1;
            }
            let dts = fsub / sr;
            let common =
                osc(1, self.sub_phase, dts, 0.5) * s.sub * 0.7 + self.rng.bipolar() * s.noise * 0.5;
            wrap(&mut self.sub_phase, dts);
            xl += common;
            xr += common;

            let (yl, yr) = if s.ladder {
                let yl = self.ladders[0].process(xl * 0.5, g, k_ladder) * comp * 2.0;
                let yr = if stereo {
                    self.ladders[1].process(xr * 0.5, g, k_ladder) * comp * 2.0
                } else {
                    yl
                };
                (yl, yr)
            } else {
                let yl = self.svfs[0].process(&svf, xl * svf_trim);
                let yr = if stereo {
                    self.svfs[1].process(&svf, xr * svf_trim)
                } else {
                    yl
                };
                (yl, yr)
            };
            let a = self.amp_env.next(&s.amp_adsr) * amp_scale;
            l[i] += yl * a;
            r[i] += yr * a;
        }
        self.active = !self.amp_env.is_idle();
    }
}

/// Juno-style chorus: two modulated delay lines, the right channel's
/// modulation inverted, mixed with the dry signal.
struct Chorus {
    buf: [Vec<f32>; 2],
    write: usize,
    phase: f32,
}

impl Chorus {
    fn new(sample_rate: f32) -> Self {
        let len = ((0.03 * sample_rate) as usize).next_power_of_two();
        Chorus {
            buf: [vec![0.0; len], vec![0.0; len]],
            write: 0,
            phase: 0.0,
        }
    }

    fn process(&mut self, mode: u8, l: &mut [f32], r: &mut [f32], sample_rate: f32) {
        let (rate, depth_ms) = match mode {
            1 => (0.5, 1.6),
            2 => (0.83, 2.4),
            _ => (8.0, 0.25),
        };
        let mask = self.buf[0].len() - 1;
        let base = 0.0035 * sample_rate;
        let depth = depth_ms * 0.001 * sample_rate;
        let inc = rate / sample_rate;
        for i in 0..l.len() {
            self.buf[0][self.write] = l[i];
            self.buf[1][self.write] = r[i];
            // Triangle LFO, as in the original BBD chorus.
            let tri = 1.0 - 4.0 * (self.phase - 0.5).abs();
            self.phase = (self.phase + inc).fract();
            for (ch, sign) in [(0usize, 1.0f32), (1, -1.0)] {
                let delay = base + depth * 0.5 * (1.0 + sign * tri);
                let pos = self.write as f32 - delay;
                let p = pos.rem_euclid(self.buf[ch].len() as f32);
                let idx = p as usize;
                let frac = p - idx as f32;
                let a = self.buf[ch][idx & mask];
                let b = self.buf[ch][(idx + 1) & mask];
                let wet = a + (b - a) * frac;
                let out = if ch == 0 { &mut l[i] } else { &mut r[i] };
                *out = (*out + wet) * 0.7;
            }
            self.write = (self.write + 1) & mask;
        }
    }
}

pub struct AnalogSynth {
    pub poly: Poly<AnalogVoice>,
    pub shared: AnalogShared,
    sample_rate: f32,
    lfo_wave: u8,
    lfo_inc: f32,
    lfo_phase: f32,
    lfo_hold: f32,
    rng: Rng,
    chorus_mode: u8,
    chorus: Chorus,
    tmp_l: [f32; MAX_BLOCK],
    tmp_r: [f32; MAX_BLOCK],
}

impl AnalogSynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut synth = AnalogSynth {
            poly: Poly::new(|i| AnalogVoice::new(i as u32 + 1)),
            shared: AnalogShared::default(),
            sample_rate,
            lfo_wave: 1,
            lfo_inc: 0.0,
            lfo_phase: 0.0,
            lfo_hold: 0.0,
            rng: Rng::new(0x5EED_A11A),
            chorus_mode: 0,
            chorus: Chorus::new(sample_rate),
            tmp_l: [0.0; MAX_BLOCK],
            tmp_r: [0.0; MAX_BLOCK],
        };
        synth.update(&super::SynthKind::Analog.defaults());
        synth
    }

    pub fn update(&mut self, params: &[f32]) {
        let sr = self.sample_rate;
        let p = &params[COMMON.len()..];
        self.poly.update_common(params, p[VOICES], p[GLIDE], sr);
        let s = &mut self.shared;
        s.sample_rate = sr;
        s.wave1 = p[OSC1_WAVE].round() as u8;
        s.wave2 = p[OSC2_WAVE].round() as u8;
        s.osc2_semis = p[OSC2_SEMI].round() + p[OSC2_DETUNE] / 100.0;
        s.pulse_width = p[PULSE_WIDTH];
        s.mix = p[OSC_MIX];
        s.sub = p[SUB];
        s.noise = p[NOISE];
        s.drift = p[DRIFT];
        s.unison = p[UNISON].round() as usize;
        s.unison_detune = p[UNISON_DETUNE];
        s.unison_spread = p[UNISON_SPREAD];
        s.cutoff = p[CUTOFF];
        s.resonance = p[RESONANCE];
        s.ladder = p[POLES_P].round() as usize == 1;
        s.filter_env = p[FILTER_ENV];
        s.key_track = p[KEY_TRACK];
        s.vel_filter = p[VEL_FILTER];
        s.filter_adsr = AdsrParams::new(p[F_ATTACK], p[F_DECAY], p[F_SUSTAIN], p[F_RELEASE], sr);
        s.amp_adsr = AdsrParams::new(p[ATTACK], p[DECAY], p[SUSTAIN], p[RELEASE], sr);
        s.vel_amp = p[VEL_AMP];
        s.lfo_pitch = p[LFO_PITCH];
        s.lfo_filter = p[LFO_FILTER];
        s.lfo_pw = p[LFO_PW];
        s.wheel_vib = p[WHEEL_VIB];
        self.lfo_wave = p[LFO_WAVE].round() as u8;
        self.lfo_inc = p[LFO_RATE] / sr;
        self.chorus_mode = p[CHORUS].round() as u8;
    }

    fn advance_lfo(&mut self, frames: usize) {
        self.lfo_phase += self.lfo_inc * frames as f32;
        if self.lfo_phase >= 1.0 {
            self.lfo_phase -= self.lfo_phase.floor();
            self.lfo_hold = self.rng.bipolar();
        }
        let p = self.lfo_phase;
        self.shared.lfo = match self.lfo_wave {
            0 => sin_cycles(p),
            1 => 1.0 - 4.0 * (p - 0.5).abs(),
            2 => {
                if p < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            // Sample & hold: a new random value each cycle.
            _ => self.lfo_hold,
        };
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        self.advance_lfo(n);
        if self.chorus_mode == 0 {
            self.poly.render(&self.shared, l, r);
            return;
        }
        let (tl, tr) = (&mut self.tmp_l[..n], &mut self.tmp_r[..n]);
        tl.fill(0.0);
        tr.fill(0.0);
        self.poly.render(&self.shared, tl, tr);
        // The chorus keeps running so delay tails fade out naturally.
        self.chorus
            .process(self.chorus_mode, tl, tr, self.sample_rate);
        for i in 0..n {
            l[i] += tl[i];
            r[i] += tr[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::SynthKind;

    fn synth_with(overrides: &[(&str, f32)]) -> AnalogSynth {
        crate::dsp::init_tables();
        let mut s = AnalogSynth::new(48_000.0);
        let mut params = SynthKind::Analog.defaults();
        for (k, v) in overrides {
            params[SynthKind::Analog.index_of(k).unwrap()] = *v;
        }
        s.update(&params);
        s
    }

    fn render(s: &mut AnalogSynth, frames: usize) -> (Vec<f32>, Vec<f32>) {
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

    fn peak(v: &[f32]) -> f32 {
        v.iter().fold(0.0f32, |m, x| m.max(x.abs()))
    }

    #[test]
    fn every_setting_is_finite_and_bounded() {
        for (poles, wave, unison, chorus) in [
            (0.0, 0.0, 1.0, 0.0),
            (1.0, 1.0, 7.0, 3.0),
            (1.0, 2.0, 3.0, 1.0),
            (0.0, 1.0, 5.0, 2.0),
        ] {
            let mut s = synth_with(&[
                ("poles", poles),
                ("osc1_wave", wave),
                ("unison", unison),
                ("chorus", chorus),
                ("resonance", 0.95),
                ("sub", 1.0),
                ("noise", 0.5),
                ("lfo_wave", 3.0),
                ("lfo_filter", 1.0),
            ]);
            for note in [36u8, 48, 60, 64, 67, 72] {
                s.poly.note_on(note, 1.0, &s.shared);
            }
            let (l, r) = render(&mut s, 24_000);
            assert!(
                l.iter().chain(&r).all(|v| v.is_finite()),
                "{poles} {wave} {unison} {chorus}"
            );
            let p = peak(&l).max(peak(&r));
            assert!(
                p > 0.05 && p < 2.0,
                "peak {p} for {poles} {wave} {unison} {chorus}"
            );
        }
    }

    #[test]
    fn unison_spreads_stereo_and_mono_stays_centred() {
        let mut mono = synth_with(&[("unison", 1.0), ("drift", 0.0)]);
        mono.poly.note_on(60, 1.0, &mono.shared);
        let (l, r) = render(&mut mono, 4_800);
        assert_eq!(l, r);

        let mut wide = synth_with(&[("unison", 5.0), ("unison_spread", 1.0)]);
        wide.poly.note_on(60, 1.0, &wide.shared);
        let (l, r) = render(&mut wide, 4_800);
        assert!(l.iter().zip(&r).any(|(a, b)| (a - b).abs() > 1e-3));
    }

    #[test]
    fn release_ends_the_voice() {
        let mut s = synth_with(&[("release", 0.05), ("chorus", 2.0)]);
        s.poly.note_on(60, 1.0, &s.shared);
        render(&mut s, 4_800);
        s.poly.note_off(60);
        render(&mut s, 9_600);
        assert_eq!(s.poly.active_voices(), 0);
    }
}
