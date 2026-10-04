//! Tonewheel organ: nine drawbars of free-running sine "tonewheels" with
//! foldback, single-trigger harmonic percussion, key click, a scanner
//! vibrato/chorus, tube overdrive and a rotary speaker (horn and drum
//! rotors with Doppler, amplitude modulation and inertia). The mod wheel
//! switches the rotary speaker between slow and fast.

use crate::dsp::{FilterMode, Rng, Svf, SvfCoefs, midi_to_freq, semitones_to_ratio, sin_cycles};
use crate::params::{ParamDesc as P, Unit};

use super::{COMMON, Controls, MAX_BLOCK, Poly, PolyControl, VOICES_PARAM, Voice};

pub const DRAWBARS: usize = 9;
/// Footages as multiples of the 8' fundamental.
pub const RATIOS: [f32; DRAWBARS] = [0.5, 1.5, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0];
pub const VIBRATO_MODES: [&str; 7] = ["Off", "V1", "V2", "V3", "C1", "C2", "C3"];

pub const VOICES: usize = 0;
pub const DRAWBAR_BASE: usize = 1;
pub const PERC: usize = 10;
pub const PERC_HARMONIC: usize = 11;
pub const PERC_VOLUME: usize = 12;
pub const PERC_DECAY: usize = 13;
pub const CLICK: usize = 14;
pub const VIBRATO: usize = 15;
pub const DRIVE: usize = 16;
pub const ROTARY: usize = 17;
pub const ROTARY_SPEED: usize = 18;

pub static PARAMS: [P; 19] = [
    VOICES_PARAM,
    P::int("db_16", "16'", "Drawbars", 0, 8, 8, Unit::None),
    P::int("db_5_1_3", "5 1/3'", "Drawbars", 0, 8, 8, Unit::None),
    P::int("db_8", "8'", "Drawbars", 0, 8, 8, Unit::None),
    P::int("db_4", "4'", "Drawbars", 0, 8, 0, Unit::None),
    P::int("db_2_2_3", "2 2/3'", "Drawbars", 0, 8, 0, Unit::None),
    P::int("db_2", "2'", "Drawbars", 0, 8, 0, Unit::None),
    P::int("db_1_3_5", "1 3/5'", "Drawbars", 0, 8, 0, Unit::None),
    P::int("db_1_1_3", "1 1/3'", "Drawbars", 0, 8, 0, Unit::None),
    P::int("db_1", "1'", "Drawbars", 0, 8, 0, Unit::None),
    P::toggle("perc", "Percussion", "Percussion", true),
    P::choice(
        "perc_harmonic",
        "Harmonic",
        "Percussion",
        &["Second", "Third"],
        1,
    ),
    P::choice(
        "perc_volume",
        "Volume",
        "Percussion",
        &["Soft", "Normal"],
        1,
    ),
    P::choice("perc_decay", "Decay", "Percussion", &["Fast", "Slow"], 0),
    P::float("click", "Key Click", "Tone", 0.0, 1.0, 0.4, Unit::Percent),
    P::choice("vibrato", "Vibrato/Chorus", "Tone", &VIBRATO_MODES, 6),
    P::float("drive", "Overdrive", "Tone", 0.0, 1.0, 0.2, Unit::Percent),
    P::toggle("rotary", "Rotary Speaker", "Rotary", true),
    P::choice("rotary_speed", "Speed", "Rotary", &["Slow", "Fast"], 0),
];

/// Highest and lowest tonewheel frequencies; drawbar pitches outside fold back.
const TOP_WHEEL: f32 = 5_920.0;
const BOTTOM_WHEEL: f32 = 32.7;
const VOICE_GAIN: f32 = 0.09;

fn drawbar_gain(level: f32) -> f32 {
    if level < 0.5 {
        0.0
    } else {
        10f32.powf((level.round() - 8.0) * 3.0 / 20.0)
    }
}

#[derive(Default)]
pub struct OrganShared {
    sample_rate: f32,
    gains: [f32; DRAWBARS],
    perc_on: bool,
    perc_ratio: f32,
    perc_level: f32,
    perc_coef: f32,
    click: f32,
    /// Running time in seconds: tonewheels free-run, so a key picks up each
    /// wheel at its current phase.
    time: f64,
    /// Set by the synth for the note being started (single-trigger percussion).
    perc_trigger: bool,
}

pub struct OrganVoice {
    active: bool,
    note: u8,
    freqs: [f32; DRAWBARS],
    phases: [f32; DRAWBARS],
    perc_phase: f32,
    perc_freq: f32,
    perc_env: f32,
    gate: f32,
    gate_target: f32,
    click_env: f32,
    click_level: f32,
    click_lp: f32,
    rng: Rng,
}

impl OrganVoice {
    fn new(seed: u32) -> Self {
        OrganVoice {
            active: false,
            note: 60,
            freqs: [0.0; DRAWBARS],
            phases: [0.0; DRAWBARS],
            perc_phase: 0.0,
            perc_freq: 0.0,
            perc_env: 0.0,
            gate: 0.0,
            gate_target: 0.0,
            click_env: 0.0,
            click_level: 0.0,
            click_lp: 0.0,
            rng: Rng::new(0x0E6A_0000 ^ seed.wrapping_mul(0x9E37_79B9)),
        }
    }
}

/// Fold a drawbar pitch back into the tonewheel range.
fn fold(mut f: f32) -> f32 {
    while f > TOP_WHEEL {
        f *= 0.5;
    }
    while f < BOTTOM_WHEEL {
        f *= 2.0;
    }
    f
}

impl Voice for OrganVoice {
    type Shared = OrganShared;

    fn start(&mut self, note: u8, _velocity: f32, _from: Option<f32>, s: &OrganShared) {
        self.note = note;
        let f0 = midi_to_freq(note as f32);
        for (k, r) in RATIOS.iter().enumerate() {
            self.freqs[k] = fold(f0 * r);
            // Free-running wheels: join each at its current phase.
            self.phases[k] = (self.freqs[k] as f64 * s.time).fract() as f32;
        }
        self.perc_freq = fold(f0 * s.perc_ratio);
        self.perc_phase = (self.perc_freq as f64 * s.time).fract() as f32;
        self.perc_env = if s.perc_on && s.perc_trigger {
            s.perc_level
        } else {
            0.0
        };
        self.gate_target = 1.0;
        self.click_level = s.click;
        self.click_env = s.click;
        self.active = true;
    }

    fn release(&mut self) {
        self.gate_target = 0.0;
        // Key contacts click on release too, more quietly.
        self.click_env = self.click_env.max(self.click_level * 0.4);
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn render(&mut self, s: &OrganShared, ctl: &Controls, l: &mut [f32], r: &mut [f32]) {
        let sr = s.sample_rate;
        let bend = semitones_to_ratio(ctl.pitch);
        let incs: [f32; DRAWBARS] = std::array::from_fn(|k| self.freqs[k] * bend / sr);
        let perc_inc = self.perc_freq * bend / sr;
        let gate_coef = if self.gate_target > self.gate {
            0.012
        } else {
            0.006
        };
        let click_coef = (-1.0 / (0.0025 * sr)).exp();
        for i in 0..l.len() {
            self.gate += (self.gate_target - self.gate) * gate_coef;
            let mut x = 0.0;
            for ((phase, inc), gain) in self.phases.iter_mut().zip(&incs).zip(&s.gains) {
                if *gain > 0.0 {
                    x += sin_cycles(*phase) * gain;
                }
                let p = *phase + inc;
                *phase = p - p.floor();
            }
            x *= self.gate;
            if self.perc_env > 1e-4 {
                x += sin_cycles(self.perc_phase) * self.perc_env * 1.6;
                self.perc_env *= s.perc_coef;
            }
            let p = self.perc_phase + perc_inc;
            self.perc_phase = p - p.floor();
            if self.click_env > 1e-4 {
                // Band-limited contact noise.
                self.click_lp += (self.rng.bipolar() - self.click_lp) * 0.35;
                x += self.click_lp * self.click_env * 2.0;
                self.click_env *= click_coef;
            }
            let y = x * VOICE_GAIN;
            l[i] += y;
            r[i] += y;
        }
        if self.gate_target == 0.0 && self.gate < 1e-4 && self.click_env < 1e-4 {
            self.active = false;
        }
    }
}

/// Scanner vibrato/chorus: a short modulated delay line.
struct Scanner {
    buf: Vec<f32>,
    w: usize,
    phase: f32,
}

impl Scanner {
    fn process(&mut self, mode: usize, x: &mut [f32], sr: f32) {
        if mode == 0 {
            return;
        }
        let depth_ms = [0.0, 0.3, 0.6, 1.0, 0.3, 0.6, 1.0][mode.min(6)];
        let chorus = mode >= 4;
        let mask = self.buf.len() - 1;
        let base = 0.0015 * sr;
        let depth = depth_ms * 0.001 * sr;
        let inc = 6.9 / sr;
        for v in x.iter_mut() {
            self.buf[self.w] = *v;
            let d = base + depth * 0.5 * (1.0 + sin_cycles(self.phase));
            let pos = self.w as f32 + self.buf.len() as f32 - d;
            let k = pos as usize;
            let f = pos - k as f32;
            let a = self.buf[k & mask];
            let wet = a + (self.buf[(k + 1) & mask] - a) * f;
            *v = if chorus { 0.5 * (*v + wet) } else { wet };
            self.w = (self.w + 1) & mask;
            self.phase = (self.phase + inc).fract();
        }
    }
}

/// One rotor (horn or drum): Doppler delay, amplitude modulation and two
/// microphones on opposite sides for stereo.
struct Rotor {
    buf: Vec<f32>,
    w: usize,
    angle: f32,
    rate: f32,
    slow: f32,
    fast: f32,
    inertia: f32,
    doppler: f32,
    am: f32,
}

impl Rotor {
    fn new(sr: f32, slow: f32, fast: f32, inertia_s: f32, doppler_ms: f32, am: f32) -> Self {
        Rotor {
            buf: vec![0.0; ((0.01 * sr) as usize).next_power_of_two()],
            w: 0,
            angle: 0.0,
            rate: slow,
            slow,
            fast,
            inertia: inertia_s,
            doppler: doppler_ms * 0.001 * sr,
            am,
        }
    }

    fn process(&mut self, x: f32, fast: bool, sr: f32) -> (f32, f32) {
        let target = if fast { self.fast } else { self.slow };
        self.rate += (target - self.rate) / (self.inertia * sr);
        self.angle = (self.angle + self.rate / sr).fract();
        let mask = self.buf.len() - 1;
        self.buf[self.w] = x;
        let mut out = [0.0f32; 2];
        for (m, o) in out.iter_mut().enumerate() {
            let a = self.angle + m as f32 * 0.5;
            let d = 2.0 + self.doppler * (1.0 + sin_cycles(a));
            let pos = self.w as f32 + self.buf.len() as f32 - d;
            let k = pos as usize;
            let f = pos - k as f32;
            let s0 = self.buf[k & mask];
            let s = s0 + (self.buf[(k + 1) & mask] - s0) * f;
            *o = s * (1.0 - self.am * 0.5 * (1.0 - sin_cycles(a + 0.25)));
        }
        self.w = (self.w + 1) & mask;
        (out[0], out[1])
    }
}

pub struct TonewheelSynth {
    pub poly: Poly<OrganVoice>,
    pub shared: OrganShared,
    sample_rate: f32,
    held: usize,
    vibrato: usize,
    drive: f32,
    rotary: bool,
    fast: bool,
    modwheel: f32,
    scanner: Scanner,
    horn: Rotor,
    drum: Rotor,
    xover: Svf,
    xover_coefs: SvfCoefs,
    tmp_l: [f32; MAX_BLOCK],
    tmp_r: [f32; MAX_BLOCK],
}

impl TonewheelSynth {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate;
        let mut s = TonewheelSynth {
            poly: Poly::new(|i| OrganVoice::new(i as u32 + 1)),
            shared: OrganShared::default(),
            sample_rate: sr,
            held: 0,
            vibrato: 6,
            drive: 0.2,
            rotary: true,
            fast: false,
            modwheel: 0.0,
            scanner: Scanner {
                buf: vec![0.0; ((0.005 * sr) as usize).next_power_of_two()],
                w: 0,
                phase: 0.0,
            },
            // Horn: light, spins up fast; drum: heavy, slow to change speed.
            horn: Rotor::new(sr, 0.8, 6.7, 0.25, 0.45, 0.5),
            drum: Rotor::new(sr, 0.67, 5.6, 1.4, 0.15, 0.35),
            xover: Svf::default(),
            xover_coefs: SvfCoefs::new(FilterMode::LowPass, 800.0, 0.0, sr),
            tmp_l: [0.0; MAX_BLOCK],
            tmp_r: [0.0; MAX_BLOCK],
        };
        s.update(&super::SynthKind::Tonewheel.defaults());
        s
    }

    pub fn update(&mut self, params: &[f32]) {
        let sr = self.sample_rate;
        let p = &params[COMMON.len()..];
        self.poly.update_common(params, p[VOICES], 0.0, sr);
        let s = &mut self.shared;
        s.sample_rate = sr;
        for k in 0..DRAWBARS {
            s.gains[k] = drawbar_gain(p[DRAWBAR_BASE + k]);
        }
        s.perc_on = p[PERC] >= 0.5;
        s.perc_ratio = if p[PERC_HARMONIC].round() as usize == 0 {
            2.0
        } else {
            3.0
        };
        s.perc_level = if p[PERC_VOLUME].round() as usize == 0 {
            0.5
        } else {
            1.0
        };
        let perc_t60 = if p[PERC_DECAY].round() as usize == 0 {
            0.6
        } else {
            2.0
        };
        s.perc_coef = (0.001f32.ln() / (perc_t60 * sr)).exp();
        s.click = p[CLICK];
        self.vibrato = p[VIBRATO].round() as usize;
        self.drive = p[DRIVE];
        self.rotary = p[ROTARY] >= 0.5;
        self.fast = p[ROTARY_SPEED].round() as usize == 1;
    }

    pub fn note_on(&mut self, note: u8, velocity: f32) {
        // Single-trigger percussion: only when no other key is held.
        self.shared.perc_trigger = self.held == 0;
        self.held += 1;
        self.poly.note_on(note, velocity, &self.shared);
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let sr = self.sample_rate;
        let (tl, tr) = (&mut self.tmp_l[..n], &mut self.tmp_r[..n]);
        tl.fill(0.0);
        tr.fill(0.0);
        self.poly.render(&self.shared, tl, tr);
        self.shared.time += n as f64 / sr as f64;

        // Voices are mono (identical channels): process the left.
        self.scanner.process(self.vibrato, tl, sr);
        let pre = 1.0 + self.drive * 7.0;
        let post = 1.0 / (1.0 + self.drive * 2.0);
        let fast = self.fast || self.modwheel > 0.5;
        for i in 0..n {
            let x = (tl[i] * pre).tanh() * post;
            let (a, b) = if self.rotary {
                // Complementary split at 800 Hz: drum gets the lows, horn the rest.
                let lo = self.xover.process(&self.xover_coefs, x);
                let hi = x - lo;
                let (hl, hr) = self.horn.process(hi, fast, sr);
                let (dl, dr) = self.drum.process(lo, fast, sr);
                (hl + dl, hr + dr)
            } else {
                (x, x)
            };
            l[i] += a;
            r[i] += b;
        }
    }
}

impl PolyControl for TonewheelSynth {
    fn note_off(&mut self, note: u8) {
        self.held = self.held.saturating_sub(1);
        self.poly.note_off(note);
    }

    fn set_sustain(&mut self, on: bool) {
        self.poly.set_sustain(on);
    }

    fn set_bend(&mut self, v: f32) {
        self.poly.set_bend(v);
    }

    fn set_modwheel(&mut self, v: f32) {
        self.modwheel = v;
    }

    fn all_notes_off(&mut self) {
        self.held = 0;
        self.poly.all_notes_off();
    }

    fn active_voices(&self) -> usize {
        self.poly.active_voices()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::Sample;
    use crate::synth::SynthKind;

    fn organ(overrides: &[(&str, f32)]) -> TonewheelSynth {
        crate::dsp::init_tables();
        let mut s = TonewheelSynth::new(48_000.0);
        let mut p = SynthKind::Tonewheel.defaults();
        for (k, v) in overrides {
            p[SynthKind::Tonewheel.index_of(k).unwrap()] = *v;
        }
        s.update(&p);
        s
    }

    fn render(s: &mut TonewheelSynth, frames: usize) -> (Vec<f32>, Vec<f32>) {
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

    const ONLY: [&str; DRAWBARS] = [
        "db_16", "db_5_1_3", "db_8", "db_4", "db_2_2_3", "db_2", "db_1_3_5", "db_1_1_3", "db_1",
    ];

    fn single_drawbar(bar: usize) -> Vec<(&'static str, f32)> {
        let mut o: Vec<(&str, f32)> = ONLY
            .iter()
            .enumerate()
            .map(|(k, key)| (*key, if k == bar { 8.0 } else { 0.0 }))
            .collect();
        o.extend([
            ("perc", 0.0),
            ("click", 0.0),
            ("vibrato", 0.0),
            ("drive", 0.0),
            ("rotary", 0.0),
        ]);
        o
    }

    fn pitch(s: &mut TonewheelSynth, note: u8) -> f32 {
        s.note_on(note, 0.8);
        let (l, _) = render(s, 24_000);
        Sample::new("t", l, None, 48_000.0)
            .detect_pitch()
            .unwrap_or(f32::NAN)
    }

    #[test]
    fn drawbars_sound_their_footage() {
        // 8' = the note, 16' an octave down, 4' an octave up, 2 2/3' a twelfth up.
        for (bar, offset) in [(2usize, 0.0f32), (0, -12.0), (3, 12.0), (4, 19.02)] {
            let mut s = organ(&single_drawbar(bar));
            let p = pitch(&mut s, 60);
            assert!((p - (60.0 + offset)).abs() < 0.1, "drawbar {bar}: {p}");
        }
    }

    #[test]
    fn top_drawbars_fold_back() {
        // C8's 1' would be ~33 kHz; it folds back into the wheel range.
        let mut s = organ(&single_drawbar(8));
        s.note_on(108, 0.8);
        let (l, _) = render(&mut s, 9_600);
        let crossings = l[4_800..]
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let freq = crossings as f32 / 2.0 / 0.1;
        assert!(
            freq > 2_900.0 && freq <= TOP_WHEEL * 1.02,
            "folded to {freq} Hz"
        );
    }

    #[test]
    fn percussion_is_single_trigger() {
        let mut s = organ(&[]);
        s.note_on(60, 1.0);
        render(&mut s, 480);
        s.note_on(64, 1.0); // legato: no percussion
        s.note_off(60);
        s.note_off(64);
        render(&mut s, 9_600);
        s.note_on(67, 1.0); // all keys were up: percussion again
        assert!(s.shared.perc_trigger);
        s.note_on(72, 1.0);
        assert!(!s.shared.perc_trigger);
    }

    #[test]
    fn rotary_has_inertia_and_stereo() {
        let mut s = organ(&[]);
        s.note_on(60, 0.8);
        render(&mut s, 4_800);
        s.set_modwheel(1.0);
        render(&mut s, 24_000); // 0.5 s after switching to fast
        assert!(s.horn.rate > 5.5, "horn {}", s.horn.rate);
        assert!(s.drum.rate < 4.0, "drum spins up slower: {}", s.drum.rate);
        let (l, r) = render(&mut s, 48_000 * 3);
        assert!(s.drum.rate > 5.0, "drum {}", s.drum.rate);
        assert!(
            l.iter().zip(&r).any(|(a, b)| (a - b).abs() > 1e-3),
            "rotary should be stereo"
        );
    }

    #[test]
    fn full_registration_stays_bounded() {
        let all: Vec<(&str, f32)> = ONLY
            .iter()
            .map(|k| (*k, 8.0))
            .chain([("drive", 1.0)])
            .collect();
        let mut s = organ(&all);
        for n in [36u8, 48, 55, 60, 64, 67, 72, 79, 84, 96] {
            s.note_on(n, 1.0);
        }
        let (l, r) = render(&mut s, 48_000);
        let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(l.iter().chain(&r).all(|v| v.is_finite()));
        assert!(peak < 1.5, "peak {peak}");
    }
}
