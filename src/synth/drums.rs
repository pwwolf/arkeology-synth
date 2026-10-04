//! Synthesized 808/909-style drum kit. Each MIDI note in the General MIDI
//! drum map triggers one drum; drums are one-shots (note-offs are ignored),
//! closed and pedal hats choke a ringing open hat, and velocity sets level.
//! Transpose and pitch bend retune the whole kit.

use crate::dsp::{FilterMode, Rng, Svf, SvfCoefs, semitones_to_ratio, sin_cycles};
use crate::params::{ParamDesc as P, Unit};

use super::{BEND_RANGE, COMMON, MAX_BLOCK, PolyControl, TRANSPOSE};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drum {
    Kick,
    Snare,
    Rim,
    Clap,
    ClosedHat,
    OpenHat,
    LowTom,
    MidTom,
    HighTom,
    Cowbell,
    Cymbal,
}

const DRUMS: [Drum; 11] = [
    Drum::Kick,
    Drum::Snare,
    Drum::Rim,
    Drum::Clap,
    Drum::ClosedHat,
    Drum::OpenHat,
    Drum::LowTom,
    Drum::MidTom,
    Drum::HighTom,
    Drum::Cowbell,
    Drum::Cymbal,
];
const N: usize = DRUMS.len();

/// General MIDI drum map → index into `DRUMS`.
fn drum_for_note(note: u8) -> Option<usize> {
    Some(match note {
        35 | 36 => 0,
        38 | 40 => 1,
        37 => 2,
        39 => 3,
        42 | 44 => 4,
        46 => 5,
        41 | 43 => 6,
        45 | 47 => 7,
        48 | 50 => 8,
        56 => 9,
        49 | 51 | 52 | 55 | 57 | 59 => 10,
        _ => return None,
    })
}

pub const VEL_SENS: usize = 0;
pub const DRIVE: usize = 1;
pub const DRUM_BASE: usize = 2;
pub const DRUM_STRIDE: usize = 4;
pub const D_TUNE: usize = 0;
pub const D_DECAY: usize = 1;
pub const D_TONE: usize = 2;
pub const D_LEVEL: usize = 3;

macro_rules! drum_params {
    ($key:literal, $group:literal, $tone:literal, $dmin:expr, $dmax:expr, $ddef:expr, $tdef:expr, $ldef:expr) => {
        [
            P::float(
                concat!($key, "_tune"),
                "Tune",
                $group,
                -12.0,
                12.0,
                0.0,
                Unit::Semitones,
            )
            .step(0.5),
            P::float(
                concat!($key, "_decay"),
                "Decay",
                $group,
                $dmin,
                $dmax,
                $ddef,
                Unit::Seconds,
            )
            .exp(),
            P::float(
                concat!($key, "_tone"),
                $tone,
                $group,
                0.0,
                1.0,
                $tdef,
                Unit::Percent,
            ),
            P::float(
                concat!($key, "_level"),
                "Level",
                $group,
                0.0,
                1.0,
                $ldef,
                Unit::Percent,
            ),
        ]
    };
}

const fn build_params() -> [P; DRUM_BASE + N * DRUM_STRIDE] {
    let head = [
        P::float("vel_sens", "Vel Sens", "Kit", 0.0, 1.0, 0.6, Unit::Percent),
        P::float("drive", "Drive", "Kit", 0.0, 1.0, 0.15, Unit::Percent),
    ];
    // Group titles carry the MIDI note (and its name) that plays the drum.
    let drums = [
        drum_params!("kick", "Kick · 36 C2", "Punch", 0.05, 2.0, 0.5, 0.5, 0.85),
        drum_params!(
            "snare",
            "Snare · 38 D2",
            "Snappy",
            0.03,
            1.0,
            0.18,
            0.6,
            0.75
        ),
        drum_params!("rim", "Rim · 37 C#2", "Tone", 0.01, 0.3, 0.04, 0.5, 0.6),
        drum_params!("clap", "Clap · 39 D#2", "Spread", 0.05, 1.5, 0.25, 0.5, 0.7),
        drum_params!(
            "chat",
            "Closed Hat · 42 F#2",
            "Tone",
            0.01,
            0.5,
            0.05,
            0.5,
            0.55
        ),
        drum_params!(
            "ohat",
            "Open Hat · 46 A#2",
            "Tone",
            0.05,
            2.0,
            0.4,
            0.5,
            0.5
        ),
        drum_params!(
            "ltom",
            "Low Tom · 41 F2",
            "Punch",
            0.05,
            2.0,
            0.45,
            0.4,
            0.7
        ),
        drum_params!("mtom", "Mid Tom · 45 A2", "Punch", 0.05, 2.0, 0.4, 0.4, 0.7),
        drum_params!(
            "htom",
            "High Tom · 48 C3",
            "Punch",
            0.05,
            2.0,
            0.35,
            0.4,
            0.7
        ),
        drum_params!(
            "cowbell",
            "Cowbell · 56 G#3",
            "Tone",
            0.05,
            1.5,
            0.3,
            0.5,
            0.5
        ),
        drum_params!(
            "cymbal",
            "Cymbal · 49 C#3",
            "Tone",
            0.2,
            5.0,
            1.6,
            0.5,
            0.45
        ),
    ];
    let mut out = [head[0]; DRUM_BASE + N * DRUM_STRIDE];
    out[1] = head[1];
    let mut d = 0;
    while d < N {
        let mut k = 0;
        while k < DRUM_STRIDE {
            out[DRUM_BASE + d * DRUM_STRIDE + k] = drums[d][k];
            k += 1;
        }
        d += 1;
    }
    out
}

pub static PARAMS: [P; DRUM_BASE + N * DRUM_STRIDE] = build_params();

/// The 808's six detuned square oscillators used for hats and cymbals.
const METAL_FREQS: [f32; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];
const OUTPUT_GAIN: f32 = 0.9;
const FLOOR: f32 = 1.0e-4;

#[derive(Clone, Copy, Default)]
struct DrumParams {
    ratio: f32,
    decay: f32,
    tone: f32,
    level: f32,
}

#[derive(Clone, Copy, Default)]
struct DrumVoice {
    active: bool,
    age: u32,
    gain: f32,
    /// Main amplitude envelope.
    amp: f32,
    amp_coef: f32,
    /// Secondary envelope: pitch sweep, noise, or attack transient.
    aux: f32,
    aux_coef: f32,
    freq: f32,
    tone: f32,
    phase: [f32; 6],
    /// Clap burst spacing in samples.
    spacing: u32,
    /// Choke fade (1 = not choked).
    choke: f32,
    choking: bool,
    f1: Svf,
    f2: Svf,
    c1: SvfCoefs,
    c2: SvfCoefs,
}

#[inline]
fn square(p: f32) -> f32 {
    if p < 0.5 { 1.0 } else { -1.0 }
}

#[inline]
fn advance(phase: &mut f32, inc: f32) -> f32 {
    *phase += inc;
    *phase -= phase.floor();
    *phase
}

pub struct DrumsSynth {
    sample_rate: f32,
    kit: [DrumParams; N],
    voices: [[DrumVoice; 2]; N],
    next: [usize; N],
    vel_sens: f32,
    drive: f32,
    transpose: f32,
    bend: f32,
    bend_range: f32,
    rng: Rng,
    buf: [f32; MAX_BLOCK],
}

impl DrumsSynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = DrumsSynth {
            sample_rate,
            kit: [DrumParams::default(); N],
            voices: [[DrumVoice::default(); 2]; N],
            next: [0; N],
            vel_sens: 0.6,
            drive: 0.0,
            transpose: 0.0,
            bend: 0.0,
            bend_range: 2.0,
            rng: Rng::new(0xD5_EED5),
            buf: [0.0; MAX_BLOCK],
        };
        s.update(&super::SynthKind::Drums.defaults());
        s
    }

    pub fn update(&mut self, params: &[f32]) {
        self.transpose = params[TRANSPOSE].round();
        self.bend_range = params[BEND_RANGE].round();
        let p = &params[COMMON.len()..];
        self.vel_sens = p[VEL_SENS];
        self.drive = p[DRIVE];
        for (d, k) in self.kit.iter_mut().enumerate() {
            let b = DRUM_BASE + d * DRUM_STRIDE;
            *k = DrumParams {
                ratio: semitones_to_ratio(p[b + D_TUNE]),
                decay: p[b + D_DECAY],
                tone: p[b + D_TONE],
                level: p[b + D_LEVEL],
            };
        }
    }

    /// Per-sample coefficient for a decay of 60 dB over `time` seconds.
    fn coef(&self, time: f32) -> f32 {
        (0.001f32.ln() / (time.max(0.001) * self.sample_rate)).exp()
    }

    pub fn note_on(&mut self, note: u8, velocity: f32) {
        let Some(d) = drum_for_note(note) else { return };
        let drum = DRUMS[d];
        let k = self.kit[d];
        let sr = self.sample_rate;
        let ratio = k.ratio * semitones_to_ratio(self.transpose + self.bend * self.bend_range);
        let gain = k.level * (1.0 - self.vel_sens * (1.0 - velocity));

        // Closed and pedal hats choke a ringing open hat.
        if drum == Drum::ClosedHat {
            for v in &mut self.voices[5] {
                v.choking = true;
            }
        }

        let mut v = DrumVoice {
            active: true,
            gain,
            amp: 1.0,
            amp_coef: self.coef(k.decay),
            tone: k.tone,
            choke: 1.0,
            ..DrumVoice::default()
        };
        let lp = |fc: f32, q: f32| SvfCoefs::new(FilterMode::LowPass, fc, q, sr);
        let bp = |fc: f32, q: f32| SvfCoefs::new(FilterMode::BandPass, fc, q, sr);
        let hp = |fc: f32, q: f32| SvfCoefs::new(FilterMode::HighPass, fc, q, sr);
        match drum {
            Drum::Kick => {
                v.freq = 48.0 * ratio;
                v.aux = 1.0;
                v.aux_coef = (-1.0 / (0.03 * sr)).exp();
            }
            Drum::Snare => {
                v.freq = 185.0 * ratio;
                v.amp_coef = self.coef((k.decay * 0.5).max(0.04));
                v.aux = 1.0;
                v.aux_coef = self.coef(k.decay);
                v.c1 = hp(1800.0 * ratio, 0.1);
                v.c2 = lp(9000.0, 0.0);
            }
            Drum::Rim => {
                v.freq = 480.0 * ratio;
                v.c1 = hp(300.0, 0.2);
            }
            Drum::Clap => {
                v.spacing = ((0.006 + 0.01 * k.tone) * sr) as u32;
                v.c1 = bp(1100.0 * ratio, 0.45);
                v.c2 = hp(500.0, 0.0);
            }
            Drum::ClosedHat | Drum::OpenHat | Drum::Cymbal => {
                v.freq = ratio;
                v.phase = std::array::from_fn(|_| self.rng.unipolar());
                let (center, low) = if drum == Drum::Cymbal {
                    (5500.0, 3000.0)
                } else {
                    (9000.0, 5500.0)
                };
                v.c1 = bp(center * ratio, 0.35);
                v.c2 = hp((low + 4000.0 * k.tone) * ratio, 0.1);
            }
            Drum::LowTom | Drum::MidTom | Drum::HighTom => {
                let base = match drum {
                    Drum::LowTom => 90.0,
                    Drum::MidTom => 130.0,
                    _ => 180.0,
                };
                v.freq = base * ratio;
                v.aux = 1.0;
                v.aux_coef = (-1.0 / (0.06 * sr)).exp();
            }
            Drum::Cowbell => {
                v.freq = ratio;
                v.aux = 1.0;
                v.aux_coef = (-1.0 / (0.015 * sr)).exp();
                v.c1 = bp((1800.0 + 2000.0 * k.tone) * ratio, 0.55);
            }
        }
        // Alternate between two voices per drum so a retrigger doesn't cut
        // off the previous hit's tail.
        let slot = self.next[d];
        self.next[d] ^= 1;
        self.voices[d][slot] = v;
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let inv_sr = 1.0 / self.sample_rate;
        let choke_coef = (-1.0 / (0.004 * self.sample_rate)).exp();
        let buf = &mut self.buf[..n];
        buf.fill(0.0);
        let mut any = false;
        for (d, pair) in self.voices.iter_mut().enumerate() {
            for v in pair.iter_mut().filter(|v| v.active) {
                any = true;
                render_voice(DRUMS[d], v, buf, inv_sr, choke_coef, &mut self.rng);
            }
        }
        if !any {
            return;
        }
        let drive = self.drive;
        let pre = 1.0 + drive * 6.0;
        for i in 0..n {
            let x = buf[i] * OUTPUT_GAIN;
            let y = x * (1.0 - drive) + (x * pre).tanh() * drive * 0.8;
            l[i] += y;
            r[i] += y;
        }
    }
}

fn render_voice(
    drum: Drum,
    v: &mut DrumVoice,
    out: &mut [f32],
    inv_sr: f32,
    choke_coef: f32,
    rng: &mut Rng,
) {
    for o in out.iter_mut() {
        let y = match drum {
            Drum::Kick => {
                let f = v.freq * (1.0 + v.tone * 5.0 * v.aux);
                let s = sin_cycles(advance(&mut v.phase[0], f * inv_sr));
                v.aux *= v.aux_coef;
                (s * 1.3).tanh() * v.amp
            }
            Drum::Snare => {
                let f = v.freq * (1.0 + 0.15 * v.amp);
                let p1 = advance(&mut v.phase[0], f * inv_sr);
                let p2 = advance(&mut v.phase[1], f * 1.78 * inv_sr);
                let body = (sin_cycles(p1) * 0.75 + sin_cycles(p2) * 0.5) * v.amp;
                let noise = v.f2.process(&v.c2, v.f1.process(&v.c1, rng.bipolar()));
                v.aux *= v.aux_coef;
                body * (1.0 - 0.5 * v.tone) + noise * v.aux * v.tone * 2.2
            }
            Drum::Rim => {
                let p1 = advance(&mut v.phase[0], v.freq * inv_sr);
                let p2 = advance(&mut v.phase[1], v.freq * 3.5 * inv_sr);
                let x = sin_cycles(p1) * (1.0 - 0.5 * v.tone) + sin_cycles(p2) * v.tone;
                v.f1.process(&v.c1, x) * v.amp * 1.4
            }
            Drum::Clap => {
                let noise = v.f2.process(&v.c2, v.f1.process(&v.c1, rng.bipolar())) * 3.5;
                let bursts = 3 * v.spacing;
                let env = if v.age < bursts {
                    let ph = (v.age % v.spacing) as f32 / v.spacing as f32;
                    let e = 1.0 - ph;
                    e * e * e * e
                } else {
                    let a = v.amp;
                    v.amp *= v.amp_coef;
                    a
                };
                // Keep the main envelope full until the bursts finish.
                noise * env
            }
            Drum::ClosedHat | Drum::OpenHat | Drum::Cymbal => {
                let mut metal = 0.0;
                for (k, f) in METAL_FREQS.iter().enumerate() {
                    metal += square(advance(&mut v.phase[k], f * v.freq * inv_sr));
                }
                let noise_amt = if drum == Drum::Cymbal { 0.5 } else { 0.25 };
                let x = metal * (1.0 / 6.0) * (1.0 - noise_amt) + rng.bipolar() * noise_amt;
                let y = v.f2.process(&v.c2, v.f1.process(&v.c1, x)) * 4.0;
                y * v.amp
            }
            Drum::LowTom | Drum::MidTom | Drum::HighTom => {
                let f = v.freq * (1.0 + v.tone * 1.2 * v.aux);
                let s = sin_cycles(advance(&mut v.phase[0], f * inv_sr));
                v.aux *= v.aux_coef;
                (s + rng.bipolar() * 0.04 * v.aux) * v.amp
            }
            Drum::Cowbell => {
                let p1 = advance(&mut v.phase[0], 540.0 * v.freq * inv_sr);
                let p2 = advance(&mut v.phase[1], 800.0 * v.freq * inv_sr);
                let x = (square(p1) + square(p2)) * 0.5;
                let env = 0.6 * v.aux + 0.4 * v.amp;
                v.aux *= v.aux_coef;
                v.f1.process(&v.c1, x) * env * 1.4
            }
        };
        if drum != Drum::Clap {
            v.amp *= v.amp_coef;
        }
        if v.choking {
            v.choke *= choke_coef;
        }
        *o += y * v.gain * v.choke;
        v.age += 1;
    }
    let clap_pending = drum == Drum::Clap && v.age < 3 * v.spacing;
    let ringing = v.amp > FLOOR || (drum == Drum::Snare && v.aux > FLOOR);
    v.active = clap_pending || (ringing && v.choke > FLOOR);
}

impl PolyControl for DrumsSynth {
    fn note_off(&mut self, _note: u8) {}

    fn set_sustain(&mut self, _on: bool) {}

    fn set_bend(&mut self, v: f32) {
        self.bend = v.clamp(-1.0, 1.0);
    }

    fn set_modwheel(&mut self, _v: f32) {}

    fn all_notes_off(&mut self) {
        for v in self.voices.iter_mut().flatten() {
            v.choking = true;
        }
    }

    fn active_voices(&self) -> usize {
        self.voices.iter().flatten().filter(|v| v.active).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(s: &mut DrumsSynth, frames: usize) -> Vec<f32> {
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
    fn every_drum_sounds_and_ends() {
        crate::dsp::init_tables();
        for note in [36u8, 37, 38, 39, 41, 42, 45, 46, 48, 49, 56] {
            let mut s = DrumsSynth::new(48_000.0);
            s.note_on(note, 1.0);
            let out = render(&mut s, 48_000 * 6);
            let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(out.iter().all(|v| v.is_finite()), "note {note}");
            assert!(peak > 0.05 && peak < 1.5, "note {note} peak {peak}");
            assert_eq!(s.active_voices(), 0, "note {note} still ringing");
        }
    }

    #[test]
    fn closed_hat_chokes_open_hat() {
        crate::dsp::init_tables();
        let mut s = DrumsSynth::new(48_000.0);
        s.note_on(46, 1.0);
        render(&mut s, 2400);
        s.note_on(42, 1.0);
        render(&mut s, 4800);
        assert!(s.voices[5].iter().all(|v| !v.active));
    }

    #[test]
    fn unmapped_notes_are_ignored() {
        let mut s = DrumsSynth::new(48_000.0);
        s.note_on(60, 1.0);
        assert_eq!(s.active_voices(), 0);
    }
}
