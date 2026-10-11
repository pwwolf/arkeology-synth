//! Wind instruments for the physical synth: a waveguide bore driven by a
//! nonlinear excitation, after the classic STK designs (Cook, Scavone).
//!
//! - **Clarinet**: a cylindrical bore (closed at the reed, so the loop
//!   inverts and only odd harmonics are strong) behind a pressure-controlled
//!   reed whose reflection follows a linear "reed table".
//! - **Sax**: the same reed on a conical bore, modelled as two delay
//!   sections either side of the reed (which brings back the even harmonics).
//! - **Flute**: an open-open bore (every harmonic resonates) driven by an
//!   air jet. The jet carries the sound at the embouchure hole across it in
//!   about half a period and splits on the far edge through a smooth
//!   (tanh) curve; the jet sitting slightly off the edge makes even
//!   harmonics. Breath sets the jet's flow, not its operating point, so it
//!   changes loudness without changing register. (STK's flute instead feeds
//!   the breath into a cubic and uses an inverting bore 3/2 of a period
//!   long, which only supports odd harmonics: it sounds like a clarinet.)
//!
//! Each instrument only speaks inside a window of breath pressure (below it
//! the reed or jet can't sustain an oscillation; above it the reed chokes or
//! the jet overblows), so the player's breath drive is mapped into that
//! window. Loop lengths subtract the loop filters' phase delay at the
//! fundamental, and every coefficient is set from frequencies rather than
//! per-sample constants, so tuning and behaviour hold at any sample rate.

use crate::dsp::{Rng, midi_to_freq, sin_cycles};

pub const INSTRUMENTS: [&str; 3] = ["Clarinet", "Sax", "Flute"];

const DELAY_LEN: usize = 4096;
/// Reed bore losses: a one-pole low-pass this many harmonics above the note.
const CLARINET_LOSS_HARMONICS: f32 = 12.0;
const SAX_LOSS_HARMONICS: f32 = 10.0;
/// Where the sax reed sits along its two bore sections. STK's 0.2 overblows
/// to the octave on most notes; 0.32 keeps the fundamental across the range
/// and the breath window.
const SAX_POSITION: f32 = 0.32;
/// Flute bore loss: a one-pole low-pass this many harmonics above the note
/// (it keeps the saturating jet from building up a strong third harmonic).
const FLUTE_LOSS_HARMONICS: f32 = 3.0;
/// Round-trip reflection of the open-open flute bore.
const FLUTE_REFLECTION: f32 = 0.9;
/// The jet crosses the embouchure hole in half a period, and its gain is
/// negative (it's deflected away from the sound's flow), so the loop is in
/// phase at the fundamental.
const FLUTE_JET: f32 = 0.5;
const FLUTE_JET_GAIN: f32 = -0.3;
/// Extra jet gain at the start of a note, fading over this many seconds.
const FLUTE_TONGUE: f32 = 2.0;
const FLUTE_TONGUE_TIME: f32 = 0.2;
/// The tongued puff's turbulence, fading over the first 60 ms.
const FLUTE_PUFF: f32 = 0.3;
/// Turbulence in the jet, which starts the oscillation.
const FLUTE_SEED: f32 = 0.01;
/// Even-order shaping of the flute's radiated sound: the second harmonic
/// sits at about half this relative to the fundamental (before radiation).
/// The jet loop alone leaves it 20 dB down; a real flute's is strong.
const FLUTE_EVEN: f32 = 1.0;
/// How long a new note's bore stays dark before opening up (seconds).
const ATTACK_DARK: f32 = 0.1;
/// DC blocker inside the flute loop.
const FLUTE_DC_HZ: f32 = 20.0;
/// Output gain per instrument, bringing their natural levels together.
const LEVEL: [f32; 3] = [2.66, 1.47, 0.47];
/// Pitch wander at full Drift, cents.
const DRIFT_CENTS: f32 = 8.0;
/// Breath (level) wander at full Drift, as a fraction of the level.
const DRIFT_LEVEL: f32 = 0.15;
/// How far breath wander and breath vibrato also move the pressure (in units
/// of breath drive). Kept small, and faded out near the edges of the speaking
/// window, so it never tips a note into another register.
const BREATH_TO_DRIVE: f32 = 0.08;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Instrument {
    #[default]
    Clarinet,
    Sax,
    Flute,
}

pub fn instrument_from(v: f32) -> Instrument {
    match v.round() as usize {
        1 => Instrument::Sax,
        2 => Instrument::Flute,
        _ => Instrument::Clarinet,
    }
}

impl Instrument {
    /// Breath pressures between which the instrument speaks on every note.
    /// A reed's window scales inversely with its stiffness.
    fn window(self, embouchure: f32) -> (f32, f32) {
        match self {
            Instrument::Clarinet => {
                let k = reed_stiffness(embouchure);
                (0.64 / k, 1.0 / k)
            }
            Instrument::Sax => {
                let k = reed_stiffness(embouchure);
                (0.68 / k, 0.86 / k)
            }
            Instrument::Flute => (0.4, 1.0),
        }
    }

    /// Sound radiated from the bell and tone holes: a first-order high-pass
    /// (+6 dB/octave up to where the wavelength matches the opening, flat
    /// above) with some of the bore pressure kept below it. Returns (cutoff
    /// Hz, dry floor).
    fn radiation(self) -> (f32, f32) {
        match self {
            Instrument::Clarinet => (1_500.0, 0.2),
            Instrument::Sax => (1_200.0, 0.25),
            Instrument::Flute => (2_000.0, 0.25),
        }
    }

    /// How vibrato splits between pitch (the jaw or lip) and breath (the
    /// diaphragm): flute vibrato is mostly breath.
    fn vibrato_split(self) -> (f32, f32) {
        match self {
            Instrument::Clarinet => (1.0, 0.3),
            Instrument::Sax => (1.0, 0.4),
            Instrument::Flute => (0.3, 1.0),
        }
    }

    /// Lowest note frequency whose bore still fits the delay lines.
    fn min_freq(self, sr: f32) -> f32 {
        let periods = match self {
            Instrument::Clarinet => 0.5,
            Instrument::Sax => 1.0,
            // The loop's DC blocker leads by up to a quarter period, which
            // the bore makes up.
            Instrument::Flute => 1.3,
        };
        sr * periods / (DELAY_LEN as f32 - 8.0)
    }
}

/// Reed table slope multiplier for an embouchure of 0..1 (soft to stiff).
fn reed_stiffness(embouchure: f32) -> f32 {
    0.6 + 0.8 * embouchure.clamp(0.0, 1.0)
}

/// The player's settings, shared by every voice.
#[derive(Clone, Copy, Default)]
pub struct WindSettings {
    pub instrument: Instrument,
    /// Reed stiffness or jet length, 0..1.
    pub embouchure: f32,
    /// Breath noise, 0..1.
    pub noise: f32,
    /// Attack time of the breath, seconds.
    pub attack: f32,
    /// Vibrato depth in cents and rate in Hz.
    pub vibrato: f32,
    pub vibrato_rate: f32,
    /// Slow random wander of pitch, breath and vibrato rate, 0..1.
    pub drift: f32,
}

/// A slow random wander in -1..1: it heads for a new random target every
/// 0.3-0.8 s and glides there, like a player's small unsteadiness.
#[derive(Clone, Copy, Default)]
struct Wander {
    value: f32,
    target: f32,
    timer: f32,
}

impl Wander {
    fn step(&mut self, rng: &mut Rng, dt: f32) -> f32 {
        self.timer -= dt;
        if self.timer <= 0.0 {
            self.target = rng.bipolar();
            self.timer = 0.3 + 0.5 * rng.unipolar();
        }
        self.value += (self.target - self.value) * (1.0 - (-dt / 0.35).exp());
        self.value
    }
}

/// Read a delay line `delay` samples behind the write head, with cubic
/// (Lagrange) interpolation. Linear interpolation's treble loss varies with
/// the fractional delay, which tips some notes into the wrong register.
#[inline]
fn read(buf: &[f32], w: usize, delay: f32) -> f32 {
    let mask = DELAY_LEN - 1;
    let pos = w as f32 + DELAY_LEN as f32 - delay;
    let k = pos as usize;
    let d = pos - k as f32;
    let (xm1, x0, x1, x2) = (
        buf[(k + mask) & mask],
        buf[k & mask],
        buf[(k + 1) & mask],
        buf[(k + 2) & mask],
    );
    // Third-order Lagrange through x[-1..2] at fraction d.
    let c0 = -d * (d - 1.0) * (d - 2.0) / 6.0;
    let c1 = (d + 1.0) * (d - 1.0) * (d - 2.0) / 2.0;
    let c2 = -(d + 1.0) * d * (d - 2.0) / 2.0;
    let c3 = (d + 1.0) * d * (d - 1.0) / 6.0;
    c0 * xm1 + c1 * x0 + c2 * x1 + c3 * x2
}

/// Phase delay (samples) at `w` of the one-pole low-pass (1-p)/(1 - p z^-1).
fn one_pole_delay(p: f32, w: f64) -> f32 {
    let p = p as f64;
    (-(-p * w.sin()).atan2(1.0 - p * w.cos()) / w) as f32
}

/// Phase delay (samples, negative: it leads) at `w` of the DC blocker
/// (1 - z^-1)/(1 - p z^-1).
fn dc_block_delay(p: f32, w: f64) -> f32 {
    let p = p as f64;
    let num = w.sin().atan2(1.0 - w.cos());
    let den = (p * w.sin()).atan2(1.0 - p * w.cos());
    (-(num - den) / w) as f32
}

pub struct WindPipe {
    bore: Vec<f32>,
    aux: Vec<f32>,
    w: usize,
    instrument: Instrument,
    /// Current and target lengths of the bore and auxiliary (jet / upper
    /// bore section) delays.
    d_bore: f32,
    d_bore_target: f32,
    d_aux: f32,
    d_aux_target: f32,
    lp_pole: f32,
    lp_y: f32,
    /// DC blocker inside the flute loop.
    loop_pole: f32,
    loop_x1: f32,
    loop_y1: f32,
    /// DC blocker on the output (the breath itself is a DC offset).
    dc_x1: f32,
    dc_y1: f32,
    /// Level follower for the flute's even-harmonic shaping.
    env: f32,
    env_coef: f32,
    /// Radiation high-pass: coefficients (b0, a1), dry floor, level
    /// normalisation; and state.
    rad: (f32, f32, f32, f32),
    rad_x1: f32,
    rad_y1: f32,
    pressure: f32,
    attack_coef: f32,
    release_coef: f32,
    vib_phase: f32,
    age: f32,
    /// Wander of pitch, breath and vibrato rate.
    wander: [Wander; 3],
    rng: Rng,
}

impl WindPipe {
    pub fn new(seed: u32) -> Self {
        WindPipe {
            bore: vec![0.0; DELAY_LEN],
            aux: vec![0.0; DELAY_LEN],
            w: 0,
            instrument: Instrument::Clarinet,
            d_bore: 100.0,
            d_bore_target: 100.0,
            d_aux: 10.0,
            d_aux_target: 10.0,
            lp_pole: 0.5,
            lp_y: 0.0,
            loop_pole: 0.997,
            loop_x1: 0.0,
            loop_y1: 0.0,
            dc_x1: 0.0,
            dc_y1: 0.0,
            env: 0.0,
            env_coef: 0.999,
            rad: (1.0, 0.0, 0.0, 1.0),
            rad_x1: 0.0,
            rad_y1: 0.0,
            pressure: 0.0,
            attack_coef: 0.99,
            release_coef: 0.99,
            vib_phase: 0.0,
            age: 0.0,
            wander: [Wander::default(); 3],
            rng: Rng::new(0x5A5E_0000 ^ seed.wrapping_mul(0x9E37_79B9)),
        }
    }

    pub fn clear(&mut self) {
        self.bore.fill(0.0);
        self.aux.fill(0.0);
        self.lp_y = 0.0;
        self.loop_x1 = 0.0;
        self.loop_y1 = 0.0;
        self.dc_x1 = 0.0;
        self.dc_y1 = 0.0;
        self.rad_x1 = 0.0;
        self.rad_y1 = 0.0;
        self.env = 0.0;
        self.pressure = 0.0;
        self.age = 0.0;
    }

    /// Set delay lengths (and loss filter) for `f0`. With `snap`, jump to
    /// them (a new note) rather than gliding (bends, vibrato).
    fn tune(&mut self, sr: f32, f0: f32, snap: bool) {
        let f0 = f0.clamp(self.instrument.min_freq(sr), sr * 0.1);
        let w = std::f64::consts::TAU * f0 as f64 / sr as f64;
        let total = sr / f0;
        // While a note builds up, a darker bore favours the fundamental, so
        // the oscillation can't lock onto a higher register (as a clean
        // tongued attack does); the loss then opens up over the next 0.1 s.
        let settle = if self.instrument == Instrument::Flute {
            1.0
        } else {
            0.25 + 0.75 * ((self.age - ATTACK_DARK) / ATTACK_DARK).clamp(0.0, 1.0)
        };
        let loss_pole = |fc: f32| (-std::f32::consts::TAU * fc * settle / sr).exp().min(0.95);
        match self.instrument {
            Instrument::Clarinet => {
                // The loop inverts, so it spans half a period.
                self.lp_pole = loss_pole(f0 * CLARINET_LOSS_HARMONICS);
                let lp = one_pole_delay(self.lp_pole, w);
                self.d_bore_target = (total * 0.5 - lp).max(2.0);
                self.d_aux_target = 0.0;
            }
            Instrument::Sax => {
                // Two bore sections either side of the reed (STK's
                // Saxofony): together they span the period.
                self.lp_pole = loss_pole(f0 * SAX_LOSS_HARMONICS);
                let lp = one_pole_delay(self.lp_pole, w);
                let d = (total - lp).max(4.0);
                self.d_aux_target = (d * SAX_POSITION).max(1.0);
                self.d_bore_target = d - self.d_aux_target;
            }
            Instrument::Flute => {
                // Open at both ends: the round trip doesn't invert and spans
                // the period.
                self.lp_pole = loss_pole(f0 * FLUTE_LOSS_HARMONICS);
                self.loop_pole = (-std::f32::consts::TAU * FLUTE_DC_HZ / sr).exp();
                let filters = one_pole_delay(self.lp_pole, w) + dc_block_delay(self.loop_pole, w);
                self.d_bore_target = (total - filters).max(4.0);
                self.d_aux_target = (total * FLUTE_JET).max(1.0);
            }
        }
        let max = DELAY_LEN as f32 - 4.0;
        self.d_bore_target = self.d_bore_target.min(max);
        self.d_aux_target = self.d_aux_target.min(max);
        if snap {
            self.d_bore = self.d_bore_target;
            self.d_aux = self.d_aux_target;
        }
    }

    /// Start (or re-tongue) a note. The instrument is fixed for the life of
    /// the note (as the physical model is for every voice): switching it
    /// takes effect from the next note, rather than running one
    /// instrument's loop on another's state.
    pub fn start(&mut self, s: &WindSettings, sr: f32, f0: f32) {
        if s.instrument != self.instrument {
            self.clear();
            self.instrument = s.instrument;
        }
        self.age = 0.0;
        self.tune(sr, f0, true);
        self.attack_coef = (-1.0 / (s.attack.max(0.002) * 0.3 * sr)).exp();
        self.release_coef = (-1.0 / (0.03 * sr)).exp();
        self.env_coef = (-1.0 / (0.02 * sr)).exp();
        // Bilinear first-order high-pass: y = b0 (x - x1) + a1 y1. It
        // shapes each note's harmonics; its gain at the fundamental is
        // normalised (to A4's), or high notes would come out up to 18 dB
        // louder than low ones.
        let (fc, floor) = self.instrument.radiation();
        let k = (std::f32::consts::PI * fc / sr).tan();
        let gain_at = |f: f32| {
            let r = f / fc;
            r / (1.0 + r * r).sqrt() + floor
        };
        let norm = gain_at(440.0) / gain_at(f0.max(20.0));
        self.rad = (1.0 / (1.0 + k), (1.0 - k) / (1.0 + k), floor, norm);
    }

    /// Render a block, adding `gain` times the output into `out`. `pitch` is
    /// the note plus bend in semitones; `drive` the breath (0..1, mapped
    /// into the instrument's speaking window); `released` stops the breath.
    /// Returns the block's peak.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        s: &WindSettings,
        sr: f32,
        pitch: f32,
        drive: f32,
        released: bool,
        out: &mut [f32],
    ) -> f32 {
        let n = out.len();
        let dt = n as f32 / sr;
        self.age += dt;
        let [w_pitch, w_breath, w_rate] = &mut self.wander;
        let (drift_pitch, drift_breath, drift_rate) = (
            w_pitch.step(&mut self.rng, dt) * s.drift,
            w_breath.step(&mut self.rng, dt) * s.drift,
            w_rate.step(&mut self.rng, dt) * s.drift,
        );
        // Delayed vibrato, fading in once the note has settled, its rate
        // wandering a little.
        let rate = s.vibrato_rate * (1.0 + 0.1 * drift_rate);
        self.vib_phase = (self.vib_phase + rate * dt).fract();
        let onset = ((self.age - 0.2) / 0.5).clamp(0.0, 1.0);
        let wave = onset * sin_cycles(self.vib_phase);
        let (vib_pitch, vib_breath) = self.instrument.vibrato_split();
        let cents = s.vibrato * vib_pitch * wave + DRIFT_CENTS * drift_pitch;
        self.tune(sr, midi_to_freq(pitch + cents / 100.0), false);

        // Breath vibrato and wander: mostly loudness, plus a little pressure
        // away from the window's edges.
        let breath_mod = vib_breath * s.vibrato / 50.0 * 0.3 * wave + DRIFT_LEVEL * drift_breath;
        let drive = drive.clamp(0.0, 1.0);
        let edge = (2.0 * drive.min(1.0 - drive)).min(1.0);
        let drive = drive + BREATH_TO_DRIVE / 0.3 * breath_mod * edge;
        let (lo, hi) = self.instrument.window(s.embouchure);
        let target = if released {
            0.0
        } else {
            lo + (hi - lo) * drive.clamp(0.0, 1.0)
        };
        let coef = if released {
            self.release_coef
        } else {
            self.attack_coef
        };
        let step_bore = (self.d_bore_target - self.d_bore) / n as f32;
        let step_aux = (self.d_aux_target - self.d_aux) / n as f32;
        let reed_slope = -0.3 * reed_stiffness(s.embouchure);
        // Where the flute's jet meets the edge: embouchure moves it. The
        // jet's gain is normalised to the curve's slope there, so the loop
        // gain (and so whether the note speaks) doesn't change with it.
        let offset = 0.25 + 0.5 * s.embouchure.clamp(0.0, 1.0);
        let offset_out = offset.tanh();
        let slope = |x: f32| 1.0 - x.tanh().powi(2);
        // A tongued attack: the jet starts strong, so the note speaks
        // quickly instead of growing slowly out of turbulence, then relaxes
        // to its playing gain.
        let tongue = 1.0 + FLUTE_TONGUE * (1.0 - self.age / FLUTE_TONGUE_TIME).max(0.0);
        let jet_gain = FLUTE_JET_GAIN * slope(0.5) / slope(offset) * tongue;
        let seed = FLUTE_SEED + FLUTE_PUFF * (1.0 - self.age / 0.06).max(0.0);
        let level = LEVEL[self.instrument as usize] * (1.0 + breath_mod);
        let (rad_b0, rad_a1, rad_floor, rad_norm) = self.rad;
        // Breath noise as a fraction of the pressure. Too much tips the sax
        // into its octave and the flute (whose window is narrower) into
        // higher registers, especially while a note builds up, so it fades
        // in once the attack has settled.
        let noise_in = ((self.age - 2.0 * ATTACK_DARK) / (2.0 * ATTACK_DARK)).clamp(0.0, 1.0);
        let noise = s.noise
            * noise_in
            * match self.instrument {
                Instrument::Clarinet => 0.2,
                Instrument::Sax => 0.15,
                Instrument::Flute => 0.3,
            };
        let mask = DELAY_LEN - 1;
        let mut peak = 0.0f32;
        for o in out.iter_mut() {
            self.d_bore += step_bore;
            self.d_aux += step_aux;
            self.pressure = target + (self.pressure - target) * coef;
            let breath = self.pressure * (1.0 + noise * self.rng.bipolar());
            let w = self.w;
            let y = match self.instrument {
                Instrument::Clarinet => {
                    let bore_out = read(&self.bore, w, self.d_bore);
                    self.lp_y = (1.0 - self.lp_pole) * bore_out + self.lp_pole * self.lp_y;
                    let diff = -0.95 * self.lp_y - breath;
                    let reed = (0.7 + reed_slope * diff).clamp(-1.0, 1.0);
                    let x = breath + diff * reed;
                    self.bore[w] = x;
                    x
                }
                Instrument::Sax => {
                    let lower = read(&self.bore, w, self.d_bore);
                    let upper = read(&self.aux, w, self.d_aux);
                    self.lp_y = (1.0 - self.lp_pole) * lower + self.lp_pole * self.lp_y;
                    let temp = -0.95 * self.lp_y;
                    let at_reed = temp - upper;
                    let diff = breath - at_reed;
                    let reed = (0.7 - reed_slope * diff).clamp(-1.0, 1.0);
                    self.aux[w] = temp;
                    self.bore[w] = breath - diff * reed - temp;
                    at_reed
                }
                Instrument::Flute => {
                    let bore_out = read(&self.bore, w, self.d_bore);
                    self.lp_y = (1.0 - self.lp_pole) * bore_out + self.lp_pole * self.lp_y;
                    // The sound returning to the embouchure (DC-blocked).
                    let a = self.lp_y - self.loop_x1 + self.loop_pole * self.loop_y1;
                    self.loop_x1 = self.lp_y;
                    self.loop_y1 = a;
                    // It deflects the jet, which crosses the hole and splits
                    // on the edge. Breath sets the jet's flow; a faster jet is
                    // deflected proportionally less, so the loop gain (and the
                    // register) doesn't depend on it, only the loudness. Below
                    // a gentle breath the gain falls, so the note dies away
                    // once the player stops blowing.
                    self.aux[w] = a;
                    let j = read(&self.aux, w, self.d_aux) + seed * self.rng.bipolar();
                    let jet =
                        breath * ((jet_gain * j / breath.max(0.25) + offset).tanh() - offset_out);
                    self.bore[w] = jet + FLUTE_REFLECTION * a;
                    // Even harmonics, scaled to the level so their share
                    // doesn't depend on loudness.
                    // (Bounded by the sample itself while the follower
                    // catches up with an attack.)
                    self.env = bore_out.abs() + (self.env - bore_out.abs()) * self.env_coef;
                    let amp = (self.env * std::f32::consts::FRAC_PI_2).max(bore_out.abs()) + 1e-4;
                    bore_out + FLUTE_EVEN * bore_out * bore_out / amp
                }
            };
            self.w = (self.w + 1) & mask;
            let v = y - self.dc_x1 + 0.995 * self.dc_y1;
            self.dc_x1 = y;
            self.dc_y1 = v;
            // Radiation from the bell and tone holes.
            let hp = rad_b0 * (v - self.rad_x1) + rad_a1 * self.rad_y1;
            self.rad_x1 = v;
            self.rad_y1 = hp;
            let v = (hp + rad_floor * v) * rad_norm * level;
            *o += v;
            peak = peak.max(v.abs());
        }
        peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::Sample;

    fn play(s: &WindSettings, sr: f32, note: u8, drive: f32, secs: f32) -> Vec<f32> {
        crate::dsp::init_tables();
        let mut p = WindPipe::new(1);
        p.start(s, sr, midi_to_freq(note as f32));
        let mut out = vec![0.0; (secs * sr) as usize];
        for chunk in out.chunks_mut(64) {
            p.render(s, sr, note as f32, drive, false, chunk);
        }
        out
    }

    fn settings(instrument: Instrument, embouchure: f32, noise: f32) -> WindSettings {
        WindSettings {
            instrument,
            embouchure,
            noise,
            attack: 0.05,
            vibrato: 0.0,
            vibrato_rate: 5.0,
            drift: 0.0,
        }
    }

    /// Each instrument's written range, roughly.
    fn range(i: Instrument) -> &'static [u8] {
        match i {
            Instrument::Clarinet => &[50, 57, 64, 71, 78, 85, 91],
            Instrument::Sax => &[49, 55, 61, 67, 73, 79, 84],
            Instrument::Flute => &[60, 66, 72, 78, 84, 90, 96],
        }
    }

    /// Every note across each instrument's range speaks at its own pitch
    /// (not silent, not overblown) for any breath, embouchure and noise, at
    /// both common sample rates.
    #[test]
    fn winds_speak_in_tune_across_breath_and_embouchure() {
        let mut failures = Vec::new();
        for inst in [Instrument::Clarinet, Instrument::Sax, Instrument::Flute] {
            for sr in [44_100.0f32, 48_000.0] {
                for (emb, noise) in [(0.0, 0.3), (0.5, 0.3), (1.0, 0.3), (0.5, 0.0), (0.5, 1.0)] {
                    let s = settings(inst, emb, noise);
                    for drive in [0.0f32, 0.5, 1.0] {
                        for &note in range(inst) {
                            let out = play(&s, sr, note, drive, 0.8);
                            let tail = &out[(0.4 * sr) as usize..];
                            let peak = tail.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                            let p = Sample::new("t", tail.to_vec(), None, sr)
                                .detect_pitch()
                                .unwrap_or(f32::NAN);
                            let cents = (p - note as f32) * 100.0;
                            if !(peak > 0.1 && cents.abs() < 15.0) {
                                failures.push(format!(
                                    "{inst:?} sr {sr} emb {emb} noise {noise} drive {drive} \
                                     note {note}: {cents:+.0} c, peak {peak:.2}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Switching instrument while a note is held doesn't touch that note.
    #[test]
    fn instrument_is_fixed_for_a_held_note() {
        crate::dsp::init_tables();
        let (sr, note) = (48_000.0, 64u8);
        let clarinet = settings(Instrument::Clarinet, 0.5, 0.3);
        let flute = settings(Instrument::Flute, 0.5, 0.3);
        let run = |switch: bool| {
            let mut p = WindPipe::new(1);
            p.start(&clarinet, sr, midi_to_freq(note as f32));
            let mut out = vec![0.0; 24_000];
            for (k, chunk) in out.chunks_mut(64).enumerate() {
                let s = if switch && k >= 150 {
                    &flute
                } else {
                    &clarinet
                };
                p.render(s, sr, note as f32, 0.5, false, chunk);
            }
            out
        };
        assert_eq!(run(false), run(true));
    }
}
