//! Insert effects. Every synth slot and the master bus has `FX_UNITS` units in
//! series. Each unit's parameters live in a fixed block of `STRIDE` values:
//! type, mix, then one segment per effect type, of which only the selected
//! type's segment is shown and used.
//!
//! A unit's DSP state (delay lines etc.) is allocated when it is built, on the
//! UI thread; the engine only swaps finished units in.

use crate::dsp::{Biquad, FilterMode, Svf, SvfCoefs, sin_cycles};
use crate::params::{ParamDesc as P, Unit};
use crate::reverb::Reverb;
use crate::synth::MAX_BLOCK;

pub const FX_UNITS: usize = 3;
pub const STRIDE: usize = 42;
pub const TYPE: usize = 0;
pub const MIX: usize = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FxKind {
    Off,
    Delay,
    Reverb,
    Chorus,
    Flanger,
    Phaser,
    Drive,
    Filter,
    Eq,
    Compressor,
    Crusher,
    Tremolo,
}

pub const KIND_NAMES: [&str; 12] = [
    "Off",
    "Delay",
    "Reverb",
    "Chorus",
    "Flanger",
    "Phaser",
    "Drive",
    "Filter",
    "EQ",
    "Compressor",
    "Crusher",
    "Tremolo",
];
const KINDS: [FxKind; 12] = [
    FxKind::Off,
    FxKind::Delay,
    FxKind::Reverb,
    FxKind::Chorus,
    FxKind::Flanger,
    FxKind::Phaser,
    FxKind::Drive,
    FxKind::Filter,
    FxKind::Eq,
    FxKind::Compressor,
    FxKind::Crusher,
    FxKind::Tremolo,
];
/// (offset within the unit, length) of each kind's parameter segment.
const SEGMENTS: [(usize, usize); 12] = [
    (2, 0),
    (2, 4),
    (6, 4),
    (10, 2),
    (12, 3),
    (15, 3),
    (18, 4),
    (22, 5),
    (27, 4),
    (31, 5),
    (36, 2),
    (38, 4),
];
/// Mix to apply when a unit switches to each kind: time effects blend,
/// processors replace the signal.
const DEFAULT_MIX: [f32; 12] = [0.5, 0.3, 0.3, 0.5, 0.5, 0.5, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];

pub const DRIVE_MODES: [&str; 4] = ["Soft", "Hard", "Fold", "Tube"];
pub const FILTER_TYPES: [&str; 5] = [
    "LowPass",
    "BandPass",
    "HighPass",
    "LowPass 24",
    "HighPass 24",
];
pub const TREMOLO_SHAPES: [&str; 2] = ["Sine", "Square"];

impl FxKind {
    pub fn from_value(v: f32) -> FxKind {
        KINDS[(v.round().max(0.0) as usize).min(KINDS.len() - 1)]
    }

    pub fn name(self) -> &'static str {
        KIND_NAMES[self as usize]
    }

    pub fn default_mix(self) -> f32 {
        DEFAULT_MIX[self as usize]
    }
}

macro_rules! fx_unit {
    ($n:literal) => {{
        const G: &str = concat!("FX ", $n);
        [
            P::choice(concat!("fx", $n, "_type"), "Type", G, &KIND_NAMES, 0),
            P::float(
                concat!("fx", $n, "_mix"),
                "Mix",
                G,
                0.0,
                1.0,
                0.5,
                Unit::Percent,
            ),
            // Delay
            P::float(
                concat!("fx", $n, "_delay_time"),
                "Time",
                G,
                0.01,
                2.0,
                0.375,
                Unit::Seconds,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_delay_feedback"),
                "Feedback",
                G,
                0.0,
                0.95,
                0.4,
                Unit::Percent,
            ),
            P::toggle(concat!("fx", $n, "_delay_pingpong"), "Ping-Pong", G, true),
            P::float(
                concat!("fx", $n, "_delay_tone"),
                "Tone",
                G,
                500.0,
                20_000.0,
                6_000.0,
                Unit::Hz,
            )
            .exp(),
            // Reverb
            P::float(
                concat!("fx", $n, "_reverb_size"),
                "Size",
                G,
                0.0,
                1.0,
                0.7,
                Unit::Percent,
            ),
            P::float(
                concat!("fx", $n, "_reverb_damp"),
                "Damping",
                G,
                0.0,
                1.0,
                0.4,
                Unit::Percent,
            ),
            P::float(
                concat!("fx", $n, "_reverb_width"),
                "Width",
                G,
                0.0,
                1.0,
                1.0,
                Unit::Percent,
            ),
            P::float(
                concat!("fx", $n, "_reverb_predelay"),
                "Pre-Delay",
                G,
                0.0,
                0.2,
                0.02,
                Unit::Seconds,
            ),
            // Chorus
            P::float(
                concat!("fx", $n, "_chorus_rate"),
                "Rate",
                G,
                0.05,
                5.0,
                0.6,
                Unit::Hz,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_chorus_depth"),
                "Depth",
                G,
                0.0,
                1.0,
                0.5,
                Unit::Percent,
            ),
            // Flanger
            P::float(
                concat!("fx", $n, "_flanger_rate"),
                "Rate",
                G,
                0.02,
                5.0,
                0.25,
                Unit::Hz,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_flanger_depth"),
                "Depth",
                G,
                0.0,
                1.0,
                0.7,
                Unit::Percent,
            ),
            P::float(
                concat!("fx", $n, "_flanger_feedback"),
                "Feedback",
                G,
                -0.95,
                0.95,
                0.5,
                Unit::Percent,
            ),
            // Phaser
            P::float(
                concat!("fx", $n, "_phaser_rate"),
                "Rate",
                G,
                0.05,
                5.0,
                0.4,
                Unit::Hz,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_phaser_depth"),
                "Depth",
                G,
                0.0,
                1.0,
                0.7,
                Unit::Percent,
            ),
            P::float(
                concat!("fx", $n, "_phaser_feedback"),
                "Feedback",
                G,
                0.0,
                0.9,
                0.5,
                Unit::Percent,
            ),
            // Drive
            P::choice(concat!("fx", $n, "_drive_mode"), "Mode", G, &DRIVE_MODES, 0),
            P::float(
                concat!("fx", $n, "_drive_amount"),
                "Drive",
                G,
                0.0,
                1.0,
                0.5,
                Unit::Percent,
            ),
            P::float(
                concat!("fx", $n, "_drive_tone"),
                "Tone",
                G,
                500.0,
                20_000.0,
                8_000.0,
                Unit::Hz,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_drive_output"),
                "Output",
                G,
                -24.0,
                6.0,
                -6.0,
                Unit::Decibels,
            )
            .step(0.5),
            // Filter
            P::choice(
                concat!("fx", $n, "_filter_type"),
                "Filter",
                G,
                &FILTER_TYPES,
                0,
            ),
            P::float(
                concat!("fx", $n, "_filter_cutoff"),
                "Cutoff",
                G,
                20.0,
                20_000.0,
                1_000.0,
                Unit::Hz,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_filter_resonance"),
                "Resonance",
                G,
                0.0,
                1.0,
                0.5,
                Unit::Percent,
            ),
            P::float(
                concat!("fx", $n, "_filter_lfo_rate"),
                "LFO Rate",
                G,
                0.05,
                10.0,
                1.0,
                Unit::Hz,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_filter_lfo_depth"),
                "LFO Depth",
                G,
                0.0,
                1.0,
                0.0,
                Unit::Percent,
            ),
            // EQ
            P::float(
                concat!("fx", $n, "_eq_low"),
                "Low",
                G,
                -15.0,
                15.0,
                0.0,
                Unit::Decibels,
            )
            .step(0.5),
            P::float(
                concat!("fx", $n, "_eq_mid"),
                "Mid",
                G,
                -15.0,
                15.0,
                0.0,
                Unit::Decibels,
            )
            .step(0.5),
            P::float(
                concat!("fx", $n, "_eq_mid_freq"),
                "Mid Freq",
                G,
                200.0,
                5_000.0,
                1_000.0,
                Unit::Hz,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_eq_high"),
                "High",
                G,
                -15.0,
                15.0,
                0.0,
                Unit::Decibels,
            )
            .step(0.5),
            // Compressor
            P::float(
                concat!("fx", $n, "_comp_threshold"),
                "Threshold",
                G,
                -40.0,
                0.0,
                -18.0,
                Unit::Decibels,
            )
            .step(0.5),
            P::float(
                concat!("fx", $n, "_comp_ratio"),
                "Ratio",
                G,
                1.0,
                20.0,
                4.0,
                Unit::None,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_comp_attack"),
                "Attack",
                G,
                0.0001,
                0.1,
                0.01,
                Unit::Seconds,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_comp_release"),
                "Release",
                G,
                0.01,
                1.0,
                0.12,
                Unit::Seconds,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_comp_makeup"),
                "Makeup",
                G,
                0.0,
                24.0,
                4.0,
                Unit::Decibels,
            )
            .step(0.5),
            // Crusher
            P::int(
                concat!("fx", $n, "_crush_bits"),
                "Bits",
                G,
                1,
                16,
                8,
                Unit::None,
            ),
            P::float(
                concat!("fx", $n, "_crush_downsample"),
                "Downsample",
                G,
                1.0,
                32.0,
                4.0,
                Unit::None,
            )
            .exp(),
            // Tremolo
            P::float(
                concat!("fx", $n, "_trem_rate"),
                "Rate",
                G,
                0.1,
                20.0,
                5.0,
                Unit::Hz,
            )
            .exp(),
            P::float(
                concat!("fx", $n, "_trem_depth"),
                "Depth",
                G,
                0.0,
                1.0,
                0.6,
                Unit::Percent,
            ),
            P::choice(
                concat!("fx", $n, "_trem_shape"),
                "Shape",
                G,
                &TREMOLO_SHAPES,
                0,
            ),
            P::toggle(concat!("fx", $n, "_trem_pan"), "Auto-Pan", G, false),
        ]
    }};
}

const fn build_table() -> [P; FX_UNITS * STRIDE] {
    let units: [[P; STRIDE]; FX_UNITS] = [fx_unit!("1"), fx_unit!("2"), fx_unit!("3")];
    let mut out = [units[0][0]; FX_UNITS * STRIDE];
    let mut u = 0;
    while u < FX_UNITS {
        let mut k = 0;
        while k < STRIDE {
            out[u * STRIDE + k] = units[u][k];
            k += 1;
        }
        u += 1;
    }
    out
}

/// Parameter table for one chain of `FX_UNITS` units.
pub const TABLE: [P; FX_UNITS * STRIDE] = build_table();
pub static PARAMS: [P; FX_UNITS * STRIDE] = TABLE;

/// Whether parameter `i` is shown: FX parameters belonging to a type other
/// than the unit's current one are hidden. `fx_base` is where the FX block
/// starts in `values`.
pub fn visible(values: &[f32], fx_base: usize, i: usize) -> bool {
    if i < fx_base {
        return true;
    }
    let rel = i - fx_base;
    let (unit, off) = (rel / STRIDE, rel % STRIDE);
    if off == TYPE {
        return true;
    }
    let kind = FxKind::from_value(values.get(fx_base + unit * STRIDE).copied().unwrap_or(0.0));
    if kind == FxKind::Off {
        return false;
    }
    if off == MIX {
        return true;
    }
    let (start, len) = SEGMENTS[kind as usize];
    (start..start + len).contains(&off)
}

/// If `i` is a unit's Type parameter, which unit.
pub fn type_param_unit(fx_base: usize, i: usize) -> Option<usize> {
    let rel = i.checked_sub(fx_base)?;
    (rel % STRIDE == TYPE && rel / STRIDE < FX_UNITS).then_some(rel / STRIDE)
}

/// The slice of `values` holding unit `unit`'s parameters.
pub fn unit_values(values: &[f32], fx_base: usize, unit: usize) -> &[f32] {
    &values[fx_base + unit * STRIDE..fx_base + (unit + 1) * STRIDE]
}

#[inline]
fn db_to_gain(db: f32) -> f32 {
    (db * (std::f32::consts::LN_10 / 20.0)).exp()
}

// ---------------------------------------------------------------------------
// Effect DSP
// ---------------------------------------------------------------------------

/// Linear-interpolated read `delay` samples behind `write` in a power-of-two buffer.
#[inline]
fn read_delay(buf: &[f32], write: usize, delay: f32) -> f32 {
    let len = buf.len();
    let pos = write as f32 + len as f32 - delay;
    let i = pos as usize;
    let f = pos - i as f32;
    let a = buf[i & (len - 1)];
    let b = buf[(i + 1) & (len - 1)];
    a + (b - a) * f
}

struct Delay {
    buf: [Vec<f32>; 2],
    w: usize,
    time: f32,
    current: f32,
    feedback: f32,
    pingpong: bool,
    tone: f32,
    lp: [f32; 2],
}

impl Delay {
    fn new(sr: f32) -> Self {
        let len = ((2.05 * sr) as usize).next_power_of_two();
        Delay {
            buf: [vec![0.0; len], vec![0.0; len]],
            w: 0,
            time: 0.0,
            current: -1.0,
            feedback: 0.4,
            pingpong: true,
            tone: 0.5,
            lp: [0.0; 2],
        }
    }

    fn set(&mut self, p: &[f32], sr: f32) {
        self.time = (p[0] * sr).clamp(1.0, self.buf[0].len() as f32 - 4.0);
        if self.current < 0.0 {
            self.current = self.time;
        }
        self.feedback = p[1];
        self.pingpong = p[2] >= 0.5;
        self.tone = (-std::f32::consts::TAU * p[3] / sr).exp();
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let mask = self.buf[0].len() - 1;
        for i in 0..l.len() {
            // Glide the delay time (tape-like) instead of jumping.
            self.current += (self.time - self.current) * 0.0005;
            let dl = read_delay(&self.buf[0], self.w, self.current);
            let dr = read_delay(&self.buf[1], self.w, self.current);
            self.lp[0] = (1.0 - self.tone) * dl + self.tone * self.lp[0];
            self.lp[1] = (1.0 - self.tone) * dr + self.tone * self.lp[1];
            let (fl, fr) = (
                (self.lp[0] * self.feedback).tanh(),
                (self.lp[1] * self.feedback).tanh(),
            );
            if self.pingpong {
                self.buf[0][self.w] = 0.5 * (l[i] + r[i]) + fr;
                self.buf[1][self.w] = fl;
            } else {
                self.buf[0][self.w] = l[i] + fl;
                self.buf[1][self.w] = r[i] + fr;
            }
            self.w = (self.w + 1) & mask;
            l[i] = dl;
            r[i] = dr;
        }
    }
}

struct ReverbFx {
    reverb: Reverb,
    pre: [Vec<f32>; 2],
    w: usize,
    predelay: usize,
}

impl ReverbFx {
    fn new(sr: f32) -> Self {
        let len = ((0.21 * sr) as usize).next_power_of_two();
        ReverbFx {
            reverb: Reverb::new(sr),
            pre: [vec![0.0; len], vec![0.0; len]],
            w: 0,
            predelay: 0,
        }
    }

    fn set(&mut self, p: &[f32], sr: f32) {
        self.reverb.set(p[0], p[1], p[2]);
        self.predelay = ((p[3] * sr) as usize).min(self.pre[0].len() - 1);
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let mask = self.pre[0].len() - 1;
        for i in 0..l.len() {
            self.pre[0][self.w] = l[i];
            self.pre[1][self.w] = r[i];
            let read = (self.w + self.pre[0].len() - self.predelay) & mask;
            l[i] = self.pre[0][read];
            r[i] = self.pre[1][read];
            self.w = (self.w + 1) & mask;
        }
        self.reverb.process(l, r);
    }
}

/// Modulated delay used by the chorus and flanger.
struct ModDelay {
    buf: [Vec<f32>; 2],
    w: usize,
    phase: f32,
    rate: f32,
    base: f32,
    range: f32,
    feedback: f32,
    last: [f32; 2],
}

impl ModDelay {
    fn new(sr: f32) -> Self {
        let len = ((0.04 * sr) as usize).next_power_of_two();
        ModDelay {
            buf: [vec![0.0; len], vec![0.0; len]],
            w: 0,
            phase: 0.0,
            rate: 0.0,
            base: 0.0,
            range: 0.0,
            feedback: 0.0,
            last: [0.0; 2],
        }
    }

    fn set_chorus(&mut self, p: &[f32], sr: f32) {
        self.rate = p[0] / sr;
        self.base = 0.007 * sr;
        self.range = p[1] * 0.006 * sr;
        self.feedback = 0.0;
    }

    fn set_flanger(&mut self, p: &[f32], sr: f32) {
        self.rate = p[0] / sr;
        self.base = 0.0005 * sr;
        self.range = p[1] * 0.005 * sr;
        self.feedback = p[2];
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let mask = self.buf[0].len() - 1;
        for i in 0..l.len() {
            // Quadrature LFOs give the left and right channels different sweeps.
            for (ch, offset) in [(0usize, 0.0f32), (1, 0.25)] {
                let lfo = 0.5 + 0.5 * sin_cycles(self.phase + offset);
                let wet = read_delay(&self.buf[ch], self.w, self.base + self.range * lfo);
                let x = if ch == 0 { &mut l[i] } else { &mut r[i] };
                self.buf[ch][self.w] = *x + (self.last[ch] * self.feedback).clamp(-1.5, 1.5);
                self.last[ch] = wet;
                *x = wet;
            }
            self.w = (self.w + 1) & mask;
            self.phase = (self.phase + self.rate).fract();
        }
    }
}

const PHASER_STAGES: usize = 6;

struct Phaser {
    stages: [[(f32, f32); PHASER_STAGES]; 2],
    phase: f32,
    rate: f32,
    depth: f32,
    feedback: f32,
    last: [f32; 2],
    coef: [f32; 2],
    sr: f32,
}

impl Phaser {
    fn new(sr: f32) -> Self {
        Phaser {
            stages: [[(0.0, 0.0); PHASER_STAGES]; 2],
            phase: 0.0,
            rate: 0.0,
            depth: 0.0,
            feedback: 0.0,
            last: [0.0; 2],
            coef: [0.0; 2],
            sr,
        }
    }

    fn set(&mut self, p: &[f32], sr: f32) {
        self.rate = p[0] / sr;
        self.depth = p[1];
        self.feedback = p[2];
        self.sr = sr;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len() {
            if i % 8 == 0 {
                for (ch, offset) in [(0usize, 0.0f32), (1, 0.25)] {
                    let lfo = 0.5 + 0.5 * sin_cycles(self.phase + offset);
                    // Sweep the all-pass corner from 200 Hz up to ~4 kHz.
                    let fc = 200.0 * (lfo * self.depth * 4.3).exp2();
                    let t = (std::f32::consts::PI * fc / self.sr).tan();
                    self.coef[ch] = (t - 1.0) / (t + 1.0);
                }
            }
            for ch in 0..2 {
                let a = self.coef[ch];
                let x = if ch == 0 { &mut l[i] } else { &mut r[i] };
                let mut y = *x + self.last[ch] * self.feedback;
                for st in &mut self.stages[ch] {
                    let out = a * y + st.0 - a * st.1;
                    st.0 = y;
                    st.1 = out;
                    y = out;
                }
                self.last[ch] = y;
                *x = y;
            }
            self.phase = (self.phase + self.rate).fract();
        }
    }
}

struct Drive {
    mode: u8,
    pre: f32,
    out: f32,
    tone: f32,
    lp: [f32; 2],
    dc: [(f32, f32); 2],
}

impl Drive {
    fn set(&mut self, p: &[f32], sr: f32) {
        self.mode = p[0].round() as u8;
        self.pre = db_to_gain(p[1] * 36.0);
        self.tone = (-std::f32::consts::TAU * p[2] / sr).exp();
        self.out = db_to_gain(p[3]);
    }

    #[inline]
    fn shape(&self, x: f32) -> f32 {
        match self.mode {
            0 => x.tanh(),
            1 => x.clamp(-1.0, 1.0),
            // Sine wavefolder: folds back instead of flattening.
            2 => sin_cycles(x * 0.25),
            // Biased tanh adds even harmonics; the DC blocker removes the offset.
            _ => (x + 0.3).tanh() - 0.3f32.tanh(),
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (ch, buf) in [l, r].into_iter().enumerate() {
            for x in buf.iter_mut() {
                let y = self.shape(*x * self.pre);
                let (x1, y1) = self.dc[ch];
                let blocked = y - x1 + 0.995 * y1;
                self.dc[ch] = (y, blocked);
                self.lp[ch] = (1.0 - self.tone) * blocked + self.tone * self.lp[ch];
                *x = self.lp[ch] * self.out;
            }
        }
    }
}

struct FilterFx {
    mode: FilterMode,
    /// 24 dB/oct: a second state-variable stage in series.
    steep: bool,
    svf2: [Svf; 2],
    coefs2: SvfCoefs,
    cutoff: f32,
    resonance: f32,
    rate: f32,
    depth: f32,
    phase: f32,
    coefs: SvfCoefs,
    svf: [Svf; 2],
    sr: f32,
}

impl FilterFx {
    fn set(&mut self, p: &[f32], sr: f32) {
        let kind = p[0].round() as usize;
        self.mode = match kind {
            1 => FilterMode::BandPass,
            2 | 4 => FilterMode::HighPass,
            _ => FilterMode::LowPass,
        };
        self.steep = kind >= 3;
        self.cutoff = p[1];
        self.resonance = p[2];
        self.rate = p[3] / sr;
        self.depth = p[4];
        self.sr = sr;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len() {
            if i % 8 == 0 {
                let oct = self.depth * 3.0 * sin_cycles(self.phase);
                let fc = self.cutoff * oct.exp2();
                self.coefs = SvfCoefs::new(self.mode, fc, self.resonance, self.sr);
                // In 24 dB mode only the second stage resonates, so the peak
                // matches the 12 dB mode instead of doubling.
                if self.steep {
                    self.coefs = SvfCoefs::new(self.mode, fc, 0.0, self.sr);
                    self.coefs2 = SvfCoefs::new(self.mode, fc, self.resonance, self.sr);
                }
            }
            l[i] = self.svf[0].process(&self.coefs, l[i]);
            r[i] = self.svf[1].process(&self.coefs, r[i]);
            if self.steep {
                l[i] = self.svf2[0].process(&self.coefs2, l[i]);
                r[i] = self.svf2[1].process(&self.coefs2, r[i]);
            }
            self.phase = (self.phase + self.rate).fract();
        }
    }
}

#[derive(Default)]
struct Eq {
    bands: [[Biquad; 3]; 2],
}

impl Eq {
    fn set(&mut self, p: &[f32], sr: f32) {
        let low = Biquad::low_shelf(200.0, p[0], sr);
        let mid = Biquad::peaking(p[2], p[1], 0.8, sr);
        let high = Biquad::high_shelf(5_000.0, p[3], sr);
        for ch in &mut self.bands {
            ch[0].retune(low);
            ch[1].retune(mid);
            ch[2].retune(high);
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (ch, buf) in [l, r].into_iter().enumerate() {
            for x in buf.iter_mut() {
                let mut y = *x;
                for b in &mut self.bands[ch] {
                    y = b.process(y);
                }
                *x = y;
            }
        }
    }
}

#[derive(Default)]
struct Compressor {
    threshold: f32,
    ratio: f32,
    attack: f32,
    release: f32,
    makeup: f32,
    env_db: f32,
}

impl Compressor {
    fn set(&mut self, p: &[f32], sr: f32) {
        self.threshold = p[0];
        self.ratio = p[1].max(1.0);
        self.attack = (-1.0 / (p[2] * sr)).exp();
        self.release = (-1.0 / (p[3] * sr)).exp();
        self.makeup = p[4];
        if self.env_db == 0.0 {
            self.env_db = -120.0;
        }
    }

    /// Gain change in dB for a detector level, with a 6 dB soft knee.
    fn gain_db(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold;
        let slope = 1.0 / self.ratio - 1.0;
        if over <= -3.0 {
            0.0
        } else if over >= 3.0 {
            over * slope
        } else {
            (over + 3.0).powi(2) / 12.0 * slope
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len() {
            // Stereo-linked peak detector, smoothed in the dB domain.
            let level = 20.0 * (l[i].abs().max(r[i].abs()) + 1e-9).log10();
            let coef = if level > self.env_db {
                self.attack
            } else {
                self.release
            };
            self.env_db = level + (self.env_db - level) * coef;
            let g = db_to_gain(self.gain_db(self.env_db) + self.makeup);
            l[i] *= g;
            r[i] *= g;
        }
    }
}

#[derive(Default)]
struct Crusher {
    levels: f32,
    factor: f32,
    acc: f32,
    held: [f32; 2],
}

impl Crusher {
    fn set(&mut self, p: &[f32]) {
        self.levels = 2f32.powf(p[0].round() - 1.0);
        self.factor = p[1].max(1.0);
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len() {
            self.acc += 1.0;
            if self.acc >= self.factor {
                self.acc -= self.factor;
                self.held = [
                    (l[i] * self.levels).round() / self.levels,
                    (r[i] * self.levels).round() / self.levels,
                ];
            }
            l[i] = self.held[0];
            r[i] = self.held[1];
        }
    }
}

#[derive(Default)]
struct Tremolo {
    rate: f32,
    depth: f32,
    square: bool,
    pan: bool,
    phase: f32,
}

impl Tremolo {
    fn set(&mut self, p: &[f32], sr: f32) {
        self.rate = p[0] / sr;
        self.depth = p[1];
        self.square = p[2] >= 0.5;
        self.pan = p[3] >= 0.5;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len() {
            let s = sin_cycles(self.phase);
            // A steep tanh makes a square wave without clicks.
            let lfo = if self.square { (s * 6.0).tanh() } else { s };
            let gl = 1.0 - self.depth * (0.5 + 0.5 * lfo);
            let gr = if self.pan {
                1.0 - self.depth * (0.5 - 0.5 * lfo)
            } else {
                gl
            };
            l[i] *= gl;
            r[i] *= gr;
            self.phase = (self.phase + self.rate).fract();
        }
    }
}

enum Dsp {
    Off,
    Delay(Delay),
    Reverb(ReverbFx),
    Chorus(ModDelay),
    Flanger(ModDelay),
    Phaser(Phaser),
    Drive(Drive),
    Filter(FilterFx),
    Eq(Eq),
    Compressor(Compressor),
    Crusher(Crusher),
    Tremolo(Tremolo),
}

/// One insert effect: its DSP plus a dry/wet mix.
pub struct FxUnit {
    mix: f32,
    sample_rate: f32,
    dsp: Dsp,
    dry_l: [f32; MAX_BLOCK],
    dry_r: [f32; MAX_BLOCK],
}

impl FxUnit {
    /// Build a unit from its parameter block (allocates; call off the audio thread).
    pub fn from_values(p: &[f32], sample_rate: f32) -> Box<FxUnit> {
        let kind = FxKind::from_value(p[TYPE]);
        let sr = sample_rate;
        let dsp = match kind {
            FxKind::Off => Dsp::Off,
            FxKind::Delay => Dsp::Delay(Delay::new(sr)),
            FxKind::Reverb => Dsp::Reverb(ReverbFx::new(sr)),
            FxKind::Chorus => Dsp::Chorus(ModDelay::new(sr)),
            FxKind::Flanger => Dsp::Flanger(ModDelay::new(sr)),
            FxKind::Phaser => Dsp::Phaser(Phaser::new(sr)),
            FxKind::Drive => Dsp::Drive(Drive {
                mode: 0,
                pre: 1.0,
                out: 1.0,
                tone: 0.0,
                lp: [0.0; 2],
                dc: [(0.0, 0.0); 2],
            }),
            FxKind::Filter => Dsp::Filter(FilterFx {
                mode: FilterMode::LowPass,
                steep: false,
                svf2: [Svf::default(); 2],
                coefs2: SvfCoefs::default(),
                cutoff: 1000.0,
                resonance: 0.0,
                rate: 0.0,
                depth: 0.0,
                phase: 0.0,
                coefs: SvfCoefs::default(),
                svf: [Svf::default(); 2],
                sr,
            }),
            FxKind::Eq => Dsp::Eq(Eq::default()),
            FxKind::Compressor => Dsp::Compressor(Compressor::default()),
            FxKind::Crusher => Dsp::Crusher(Crusher::default()),
            FxKind::Tremolo => Dsp::Tremolo(Tremolo::default()),
        };
        let mut unit = Box::new(FxUnit {
            mix: 1.0,
            sample_rate,
            dsp,
            dry_l: [0.0; MAX_BLOCK],
            dry_r: [0.0; MAX_BLOCK],
        });
        unit.update(p);
        unit
    }

    pub fn off(sample_rate: f32) -> Box<FxUnit> {
        let mut p = [0.0; STRIDE];
        p[TYPE] = 0.0;
        Self::from_values(&p, sample_rate)
    }

    /// Apply parameter values (the unit's block). Allocation-free.
    pub fn update(&mut self, p: &[f32]) {
        self.mix = p[MIX];
        let sr = self.sample_rate;
        let seg = |kind: FxKind| {
            let (s, n) = SEGMENTS[kind as usize];
            &p[s..s + n]
        };
        match &mut self.dsp {
            Dsp::Off => {}
            Dsp::Delay(d) => d.set(seg(FxKind::Delay), sr),
            Dsp::Reverb(d) => d.set(seg(FxKind::Reverb), sr),
            Dsp::Chorus(d) => d.set_chorus(seg(FxKind::Chorus), sr),
            Dsp::Flanger(d) => d.set_flanger(seg(FxKind::Flanger), sr),
            Dsp::Phaser(d) => d.set(seg(FxKind::Phaser), sr),
            Dsp::Drive(d) => d.set(seg(FxKind::Drive), sr),
            Dsp::Filter(d) => d.set(seg(FxKind::Filter), sr),
            Dsp::Eq(d) => d.set(seg(FxKind::Eq), sr),
            Dsp::Compressor(d) => d.set(seg(FxKind::Compressor), sr),
            Dsp::Crusher(d) => d.set(seg(FxKind::Crusher)),
            Dsp::Tremolo(d) => d.set(seg(FxKind::Tremolo), sr),
        }
    }

    /// Process a block in place (n <= MAX_BLOCK).
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let (l, r) = (&mut l[..n], &mut r[..n]);
        if matches!(self.dsp, Dsp::Off) {
            return;
        }
        self.dry_l[..n].copy_from_slice(l);
        self.dry_r[..n].copy_from_slice(r);
        match &mut self.dsp {
            Dsp::Off => {}
            Dsp::Delay(d) => d.process(l, r),
            Dsp::Reverb(d) => d.process(l, r),
            Dsp::Chorus(d) | Dsp::Flanger(d) => d.process(l, r),
            Dsp::Phaser(d) => d.process(l, r),
            Dsp::Drive(d) => d.process(l, r),
            Dsp::Filter(d) => d.process(l, r),
            Dsp::Eq(d) => d.process(l, r),
            Dsp::Compressor(d) => d.process(l, r),
            Dsp::Crusher(d) => d.process(l, r),
            Dsp::Tremolo(d) => d.process(l, r),
        }
        let (wet, dry) = (self.mix, 1.0 - self.mix);
        for i in 0..n {
            l[i] = self.dry_l[i] * dry + l[i] * wet;
            r[i] = self.dry_r[i] * dry + r[i] * wet;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(kind: FxKind, overrides: &[(&str, f32)]) -> Box<FxUnit> {
        crate::dsp::init_tables();
        let mut p: Vec<f32> = TABLE[..STRIDE].iter().map(|d| d.default).collect();
        p[TYPE] = kind as usize as f32;
        p[MIX] = 1.0;
        for (key, v) in overrides {
            let i = TABLE[..STRIDE]
                .iter()
                .position(|d| d.key == format!("fx1_{key}"))
                .unwrap();
            p[i] = *v;
        }
        FxUnit::from_values(&p, 48_000.0)
    }

    fn run(u: &mut FxUnit, input: impl Fn(usize) -> f32, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut ol, mut or) = (Vec::new(), Vec::new());
        let mut t = 0;
        while ol.len() < frames {
            let mut l = [0.0f32; MAX_BLOCK];
            for x in l.iter_mut() {
                *x = input(t);
                t += 1;
            }
            let mut r = l;
            u.process(&mut l, &mut r);
            ol.extend_from_slice(&l);
            or.extend_from_slice(&r);
        }
        (ol, or)
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
    }

    fn sine(freq: f32, amp: f32) -> impl Fn(usize) -> f32 {
        move |t| amp * (std::f32::consts::TAU * freq * t as f32 / 48_000.0).sin()
    }

    #[test]
    fn table_layout_matches_segments() {
        assert_eq!(TABLE.len(), FX_UNITS * STRIDE);
        for (k, &(start, len)) in SEGMENTS.iter().enumerate().skip(1) {
            let prefix = match KINDS[k] {
                FxKind::Compressor => "comp",
                FxKind::Crusher => "crush",
                FxKind::Tremolo => "trem",
                other => other.name(),
            }
            .to_ascii_lowercase();
            for d in &TABLE[start..start + len] {
                assert!(
                    d.key.starts_with(&format!("fx1_{prefix}_")),
                    "{} not in {:?}",
                    d.key,
                    KINDS[k]
                );
            }
        }
        assert_eq!(SEGMENTS.last().map(|(s, n)| s + n), Some(STRIDE));
    }

    #[test]
    fn visibility_follows_type() {
        let mut v: Vec<f32> = TABLE.iter().map(|d| d.default).collect();
        let base = 0;
        assert!(
            visible(&v, base, TYPE) && !visible(&v, base, MIX),
            "off hides mix"
        );
        v[TYPE] = FxKind::Delay as usize as f32;
        assert!(visible(&v, base, MIX) && visible(&v, base, 2) && !visible(&v, base, 6));
        assert!(!visible(&v, base, STRIDE + MIX), "unit 2 still off");
        assert_eq!(type_param_unit(base, STRIDE), Some(1));
        assert_eq!(type_param_unit(base, STRIDE + 1), None);
    }

    #[test]
    fn delay_echoes_after_its_time() {
        let mut u = unit(
            FxKind::Delay,
            &[
                ("delay_time", 0.1),
                ("delay_feedback", 0.0),
                ("delay_pingpong", 0.0),
            ],
        );
        let (l, _) = run(&mut u, |t| if t == 0 { 1.0 } else { 0.0 }, 9_600);
        let peak = l
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        assert!((peak as i32 - 4_800).abs() <= 2, "echo at {peak}");
    }

    #[test]
    fn eq_boosts_and_cuts_by_the_set_amount() {
        for (key, freq) in [("eq_low", 60.0), ("eq_mid", 1_000.0), ("eq_high", 12_000.0)] {
            let mut u = unit(FxKind::Eq, &[(key, 12.0)]);
            let (l, _) = run(&mut u, sine(freq, 0.1), 48_000);
            let gain_db = 20.0 * (rms(&l[24_000..]) / (0.1 / 2f32.sqrt())).log10();
            assert!((gain_db - 12.0).abs() < 1.0, "{key}: {gain_db} dB");
        }
    }

    #[test]
    fn steep_high_pass_cuts_twice_as_hard() {
        // One octave below a 200 Hz cutoff with resonance 0 (Q = 0.5 per
        // stage): |H| = 0.25 / 1.25 -> -14 dB per stage, so -28 dB for two.
        for (kind, expected_db) in [(2.0, -14.0), (4.0, -28.0)] {
            let mut u = unit(
                FxKind::Filter,
                &[
                    ("filter_type", kind),
                    ("filter_cutoff", 200.0),
                    ("filter_resonance", 0.0),
                ],
            );
            let (l, _) = run(&mut u, sine(100.0, 0.1), 48_000);
            let db = 20.0 * (rms(&l[24_000..]) / (0.1 / 2f32.sqrt())).log10();
            assert!((db - expected_db).abs() < 1.0, "type {kind}: {db} dB");
        }
    }

    #[test]
    fn compressor_reduces_loud_signals() {
        let mut u = unit(
            FxKind::Compressor,
            &[
                ("comp_threshold", -20.0),
                ("comp_ratio", 4.0),
                ("comp_makeup", 0.0),
            ],
        );
        let (l, _) = run(&mut u, sine(200.0, 1.0), 48_000);
        let out_db = 20.0 * (l[24_000..].iter().fold(0.0f32, |m, v| m.max(v.abs()))).log10();
        // 0 dB in, -20 threshold, 4:1 -> about -15 dB out.
        assert!((out_db + 15.0).abs() < 2.0, "out {out_db} dB");
    }

    #[test]
    fn every_effect_is_stable() {
        for (k, &kind) in KINDS.iter().enumerate().skip(1) {
            let mut u = unit(
                kind,
                &[
                    ("delay_feedback", 0.95),
                    ("flanger_feedback", 0.95),
                    ("phaser_feedback", 0.9),
                ],
            );
            let noise = |t: usize| {
                ((t.wrapping_mul(2_654_435_761) >> 7) as u32 as f32 / u32::MAX as f32 - 0.5) * 1.6
            };
            let (l, r) = run(&mut u, noise, 96_000);
            let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(l.iter().chain(&r).all(|v| v.is_finite()), "{:?}", KINDS[k]);
            assert!(peak < 8.0, "{:?} peak {peak}", KINDS[k]);
        }
    }
}
