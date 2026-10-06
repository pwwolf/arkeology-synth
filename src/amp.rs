//! Guitar rig effects: a stompbox (overdrive, distortion, fuzz) and an amp
//! (cascaded tube preamp, tone stack, power amp with sag, speaker cabinet).
//! The nonlinear stages run 4x oversampled, so heavy gain on bright synth
//! sources doesn't fold harmonics back down as aliasing.

use crate::dsp::Biquad;

pub const PEDAL_TYPES: [&str; 3] = ["Overdrive", "Distortion", "Fuzz"];
pub const AMP_MODELS: [&str; 4] = ["Clean", "Crunch", "Lead", "High Gain"];
pub const CABINETS: [&str; 4] = ["Off", "1x12 Open", "2x12", "4x12 Closed"];

const OVERSAMPLE: usize = 4;

#[inline]
fn db(x: f32) -> f32 {
    10f32.powf(x / 20.0)
}

/// One-pole filter: `lp` smooths, `hp` is its complement.
#[derive(Clone, Copy, Default)]
struct OnePole {
    a: f32,
    z: f32,
}

impl OnePole {
    fn tune(&mut self, fc: f32, sr: f32) {
        self.a = (-std::f32::consts::TAU * fc / sr).exp();
    }

    #[inline]
    fn lp(&mut self, x: f32) -> f32 {
        self.z = x + (self.z - x) * self.a;
        self.z
    }

    #[inline]
    fn hp(&mut self, x: f32) -> f32 {
        x - self.lp(x)
    }
}

/// 4x oversampling around a per-sample nonlinearity: zero-stuff, 8th-order
/// Butterworth anti-imaging filter, process, the same filter again as
/// anti-aliasing, keep every 4th sample.
#[derive(Clone, Copy)]
struct Oversampler {
    up: [Biquad; 4],
    down: [Biquad; 4],
}

impl Oversampler {
    fn new(sr: f32) -> Self {
        let osr = sr * OVERSAMPLE as f32;
        let filters = [0.5098, 0.6013, 0.9000, 2.5629].map(|q| Biquad::low_pass(0.42 * sr, q, osr));
        Oversampler {
            up: filters,
            down: filters,
        }
    }

    #[inline]
    fn run(&mut self, x: f32, mut f: impl FnMut(f32) -> f32) -> f32 {
        let mut out = 0.0;
        for k in 0..OVERSAMPLE {
            let mut u = if k == 0 { x * OVERSAMPLE as f32 } else { 0.0 };
            for b in &mut self.up {
                u = b.process(u);
            }
            let mut y = f(u);
            for b in &mut self.down {
                y = b.process(y);
            }
            out = y;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Pedal
// ---------------------------------------------------------------------------

struct PedalChannel {
    os: Oversampler,
    /// Pre-clipping high-pass (sets how much bass gets distorted).
    hp: OnePole,
    /// Op-amp gain-bandwidth limit (distortion).
    gbw: OnePole,
    dc: OnePole,
    tone: OnePole,
}

pub struct Pedal {
    kind: usize,
    gain: f32,
    out: f32,
    ch: [PedalChannel; 2],
}

impl Pedal {
    pub fn new(sr: f32) -> Self {
        let ch = || PedalChannel {
            os: Oversampler::new(sr),
            hp: OnePole::default(),
            gbw: OnePole::default(),
            dc: OnePole::default(),
            tone: OnePole::default(),
        };
        Pedal {
            kind: 0,
            gain: 1.0,
            out: 1.0,
            ch: [ch(), ch()],
        }
    }

    /// `p`: type, drive (0..1), tone (0..1), level (dB).
    pub fn set(&mut self, p: &[f32], sr: f32) {
        let osr = sr * OVERSAMPLE as f32;
        self.kind = (p[0].round().max(0.0) as usize).min(PEDAL_TYPES.len() - 1);
        let (drive, tone) = (p[1], p[2]);
        // (gain range dB, pre-clip high-pass Hz, tone sweep Hz, makeup dB)
        let (gain_db, hp, (t0, t1), makeup): (f32, f32, (f32, f32), f32) = match self.kind {
            // Tube Screamer style: only the mids above ~720 Hz are clipped.
            0 => (12.0 + drive * 30.0, 720.0, (700.0, 6_000.0), -6.0),
            1 => (10.0 + drive * 45.0, 90.0, (500.0, 12_000.0), -14.0),
            _ => (14.0 + drive * 36.0, 40.0, (900.0, 9_000.0), -13.0),
        };
        self.gain = db(gain_db);
        self.out = db(p[3] + makeup);
        let tone_hz = t0 * (t1 / t0).powf(tone);
        // An op-amp running out of gain-bandwidth darkens at high gain.
        let gbw_hz = (1.5e6 / self.gain).clamp(2_000.0, 0.45 * osr);
        for c in &mut self.ch {
            c.hp.tune(hp, osr);
            c.gbw.tune(gbw_hz, osr);
            c.dc.tune(10.0, sr);
            c.tone.tune(tone_hz, sr);
        }
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let (kind, gain, out) = (self.kind, self.gain, self.out);
        for (c, buf) in self.ch.iter_mut().zip([l, r]) {
            let PedalChannel {
                os,
                hp,
                gbw,
                dc,
                tone,
            } = c;
            for x in buf.iter_mut() {
                let y = os.run(*x, |u| match kind {
                    // Clean signal plus soft-clipped mids (diodes in the feedback loop).
                    0 => 0.5 * u + 0.6 * (gain * hp.hp(u) / 0.6).tanh(),
                    // Hard-kneed diodes to ground after a big op-amp gain.
                    1 => {
                        let v = gbw.lp(gain * hp.hp(u));
                        v / (1.0 + v * v * v * v).sqrt().sqrt()
                    }
                    // Asymmetric transistor clipping: rich in even harmonics.
                    _ => {
                        let v = gain * hp.hp(u);
                        if v >= 0.0 {
                            v.tanh()
                        } else {
                            0.55 * (v / 0.55).tanh()
                        }
                    }
                });
                *x = tone.lp(dc.hp(y)) * out;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Amp
// ---------------------------------------------------------------------------

struct Voicing {
    stages: usize,
    /// Per-stage gain (dB) at Gain 0 and Gain 1.
    stage_db: (f32, f32),
    input_hp: f32,
    coupling_hp: f32,
    miller_lp: f32,
    mid_hz: f32,
    /// The tone stack's built-in mid character at Mid = 50%.
    mid_db: f32,
    power_drive: f32,
    sag: f32,
    makeup_db: f32,
    /// Output reduction per dB of preamp gain away from Gain 50%, so the knob
    /// mostly changes the amount of distortion, not just the volume.
    gain_comp: f32,
}

const VOICINGS: [Voicing; 4] = [
    // Clean: two stages, scooped mids, a sagging power section.
    Voicing {
        stages: 2,
        stage_db: (-6.0, 12.0),
        input_hp: 40.0,
        coupling_hp: 25.0,
        miller_lp: 12_000.0,
        mid_hz: 450.0,
        mid_db: -4.0,
        power_drive: 0.9,
        sag: 0.35,
        makeup_db: 0.0,
        gain_comp: 0.5,
    },
    // Crunch: two hotter stages, mids forward.
    Voicing {
        stages: 2,
        stage_db: (4.0, 22.0),
        input_hp: 70.0,
        coupling_hp: 40.0,
        miller_lp: 9_000.0,
        mid_hz: 650.0,
        mid_db: 0.0,
        power_drive: 1.4,
        sag: 0.45,
        makeup_db: -11.5,
        gain_comp: 0.1,
    },
    // Lead: three stages, smooth sustain.
    Voicing {
        stages: 3,
        stage_db: (6.0, 22.0),
        input_hp: 90.0,
        coupling_hp: 60.0,
        miller_lp: 7_500.0,
        mid_hz: 700.0,
        mid_db: 1.0,
        power_drive: 1.6,
        sag: 0.3,
        makeup_db: -13.0,
        gain_comp: 0.1,
    },
    // High gain: four stages, tight lows going in, scooped mids, stiff power amp.
    Voicing {
        stages: 4,
        stage_db: (8.0, 24.0),
        input_hp: 140.0,
        coupling_hp: 90.0,
        miller_lp: 6_500.0,
        mid_hz: 750.0,
        mid_db: -3.0,
        power_drive: 1.3,
        sag: 0.15,
        makeup_db: -12.5,
        gain_comp: 0.05,
    },
];

const MAX_STAGES: usize = 4;

/// A triode gain stage's transfer curve: grid conduction compresses the
/// positive swing sooner than cutoff limits the negative one, so it adds even
/// harmonics. Unity slope at zero.
#[inline]
fn triode(x: f32) -> f32 {
    if x >= 0.0 {
        x.tanh()
    } else {
        1.5 * (x / 1.5).tanh()
    }
}

/// Speaker cabinet as a filter chain: low-end roll-off with a resonant bump,
/// a lower-mid dip, a presence peak and a steep top-end roll-off.
fn cabinet(cab: usize, sr: f32) -> [Biquad; 6] {
    // (hp, hp Q, bump Hz, bump dB, dip Hz, dip dB, peak Hz, peak dB, lp Hz)
    let (hp, hp_q, bump, bump_db, dip, dip_db, peak, peak_db, lp) = match cab {
        1 => (75.0, 0.7, 110.0, 2.0, 600.0, -2.0, 2_200.0, 3.0, 5_500.0),
        2 => (80.0, 0.8, 120.0, 3.0, 450.0, -3.0, 2_500.0, 4.0, 5_000.0),
        _ => (85.0, 1.0, 100.0, 4.0, 500.0, -4.0, 2_000.0, 5.0, 4_500.0),
    };
    [
        Biquad::high_pass(hp, hp_q, sr),
        Biquad::peaking(bump, bump_db, 1.3, sr),
        Biquad::peaking(dip, dip_db, 1.0, sr),
        Biquad::peaking(peak, peak_db, 1.6, sr),
        Biquad::low_pass(lp, 0.54, sr),
        Biquad::low_pass(lp, 1.31, sr),
    ]
}

struct AmpChannel {
    pre_os: Oversampler,
    power_os: Oversampler,
    input_hp: OnePole,
    coupling: [OnePole; MAX_STAGES],
    miller: [OnePole; MAX_STAGES],
    tone: [Biquad; 3],
    presence: Biquad,
    cab: [Biquad; 6],
    sag_env: f32,
}

pub struct Amp {
    stages: usize,
    stage_gain: f32,
    power_drive: f32,
    sag: f32,
    sag_attack: f32,
    sag_release: f32,
    cab_on: bool,
    out: f32,
    ch: [AmpChannel; 2],
}

impl Amp {
    pub fn new(sr: f32) -> Self {
        let ch = || AmpChannel {
            pre_os: Oversampler::new(sr),
            power_os: Oversampler::new(sr),
            input_hp: OnePole::default(),
            coupling: [OnePole::default(); MAX_STAGES],
            miller: [OnePole::default(); MAX_STAGES],
            tone: [Biquad::peaking(1_000.0, 0.0, 1.0, sr); 3],
            presence: Biquad::peaking(1_000.0, 0.0, 1.0, sr),
            cab: cabinet(3, sr),
            sag_env: 0.0,
        };
        Amp {
            stages: 2,
            stage_gain: 1.0,
            power_drive: 1.0,
            sag: 0.0,
            sag_attack: 0.0,
            sag_release: 0.0,
            cab_on: true,
            out: 1.0,
            ch: [ch(), ch()],
        }
    }

    /// `p`: model, gain, bass, mid, treble, presence (0..1), cabinet, level (dB).
    pub fn set(&mut self, p: &[f32], sr: f32) {
        let osr = sr * OVERSAMPLE as f32;
        let v = &VOICINGS[(p[0].round().max(0.0) as usize).min(VOICINGS.len() - 1)];
        let (gain, bass, mid, treble, presence) = (p[1], p[2], p[3], p[4], p[5]);
        let cab = (p[6].round().max(0.0) as usize).min(CABINETS.len() - 1);
        self.stages = v.stages;
        let stage_db = v.stage_db.0 + (v.stage_db.1 - v.stage_db.0) * gain;
        let mid_db = (v.stage_db.0 + v.stage_db.1) * 0.5;
        self.stage_gain = db(stage_db);
        self.power_drive = v.power_drive;
        self.sag = v.sag;
        self.sag_attack = (-1.0 / (0.01 * sr)).exp();
        self.sag_release = (-1.0 / (0.15 * sr)).exp();
        self.cab_on = cab > 0;
        let comp = v.gain_comp * (stage_db - mid_db) * v.stages as f32;
        self.out = db(p[7] + v.makeup_db - comp);
        let tone = [
            Biquad::low_shelf(120.0, (bass - 0.5) * 20.0, sr),
            Biquad::peaking(v.mid_hz, (mid - 0.5) * 20.0 + v.mid_db, 0.8, sr),
            Biquad::high_shelf(2_800.0, (treble - 0.5) * 20.0, sr),
        ];
        let presence = Biquad::high_shelf(4_000.0, presence * 10.0, sr);
        let cab = cabinet(cab, sr);
        for c in &mut self.ch {
            c.input_hp.tune(v.input_hp, osr);
            for s in 0..MAX_STAGES {
                c.coupling[s].tune(v.coupling_hp, osr);
                c.miller[s].tune(v.miller_lp, osr);
            }
            // Retune rather than replace: no clicks while a knob moves.
            for (b, n) in c.tone.iter_mut().zip(tone) {
                b.retune(n);
            }
            c.presence.retune(presence);
            for (b, n) in c.cab.iter_mut().zip(cab) {
                b.retune(n);
            }
        }
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let (stages, g) = (self.stages, self.stage_gain);
        for (c, buf) in self.ch.iter_mut().zip([l, r]) {
            for x in buf.iter_mut() {
                // Preamp: cascaded, inverting triode stages.
                let AmpChannel {
                    pre_os,
                    input_hp,
                    coupling,
                    miller,
                    ..
                } = c;
                let mut y = pre_os.run(*x, |u| {
                    let mut v = input_hp.hp(u);
                    for s in 0..stages {
                        v = triode(v * g);
                        v = -miller[s].lp(coupling[s].hp(v));
                    }
                    v
                });
                for b in &mut c.tone {
                    y = b.process(y);
                }
                // Power amp: push-pull saturation that sags under load.
                let drive = self.power_drive / (1.0 + self.sag * c.sag_env * 2.0);
                y = c.power_os.run(y, |u| (u * drive).tanh());
                let level = y.abs();
                let coef = if level > c.sag_env {
                    self.sag_attack
                } else {
                    self.sag_release
                };
                c.sag_env = level + (c.sag_env - level) * coef;
                y = c.presence.process(y);
                if self.cab_on {
                    for b in &mut c.cab {
                        y = b.process(y);
                    }
                }
                *x = y * self.out;
            }
        }
    }
}
