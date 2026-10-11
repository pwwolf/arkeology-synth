//! Wind instruments for the physical synth: a waveguide bore driven by a
//! nonlinear excitation, after the classic STK designs (Cook, Scavone).
//!
//! - **Clarinet**: a cylindrical bore (closed at the reed, so the loop
//!   inverts and only odd harmonics are strong) behind a pressure-controlled
//!   reed whose reflection follows a linear "reed table".
//! - **Sax**: the same reed on a conical bore, modelled as two delay
//!   sections either side of the reed (which brings back the even harmonics).
//! - **Flute**: an air jet across the embouchure hole: a jet delay feeding a
//!   cubic jet nonlinearity, coupled to an open bore.
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
/// Flute bore loss cutoff (STK's pole 0.65 at 44.1 kHz).
const FLUTE_LOSS_HZ: f32 = 3_000.0;
/// The flute bore is 3/2 of the note's period (STK: "we're overblowing
/// here"), lengthened slightly for the pitch the jet adds; the jet takes
/// about a third of it.
const FLUTE_BORE: f32 = 1.5;
const FLUTE_JET: f32 = 0.32;
/// DC blocker inside the flute loop.
const FLUTE_DC_HZ: f32 = 20.0;
/// Output gain per instrument, bringing their natural levels together.
const LEVEL: [f32; 3] = [1.5, 1.0, 0.5];

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
                (0.64 / k, 0.86 / k)
            }
            Instrument::Sax => {
                let k = reed_stiffness(embouchure);
                (0.64 / k, 0.86 / k)
            }
            Instrument::Flute => (0.92, 1.24),
        }
    }

    /// Lowest note frequency whose bore still fits the delay lines.
    fn min_freq(self, sr: f32) -> f32 {
        let periods = match self {
            Instrument::Clarinet => 0.5,
            Instrument::Sax => 1.0,
            Instrument::Flute => FLUTE_BORE * 1.1,
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
}

/// Read a delay line `delay` samples behind the write head (linear
/// interpolation).
#[inline]
fn read(buf: &[f32], w: usize, delay: f32) -> f32 {
    let mask = DELAY_LEN - 1;
    let pos = w as f32 + DELAY_LEN as f32 - delay;
    let k = pos as usize;
    let fr = pos - k as f32;
    let a0 = buf[k & mask];
    a0 + (buf[(k + 1) & mask] - a0) * fr
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
    pressure: f32,
    attack_coef: f32,
    release_coef: f32,
    vib_phase: f32,
    vib_time: f32,
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
            pressure: 0.0,
            attack_coef: 0.99,
            release_coef: 0.99,
            vib_phase: 0.0,
            vib_time: 0.0,
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
        self.pressure = 0.0;
        self.vib_time = 0.0;
    }

    /// Set delay lengths (and loss filter) for `f0`. With `snap`, jump to
    /// them (a new note) rather than gliding (bends, vibrato).
    fn tune(&mut self, s: &WindSettings, sr: f32, f0: f32, snap: bool) {
        self.instrument = s.instrument;
        let f0 = f0.clamp(s.instrument.min_freq(sr), sr * 0.1);
        let w = std::f64::consts::TAU * f0 as f64 / sr as f64;
        let total = sr / f0;
        let loss_pole = |fc: f32| (-std::f32::consts::TAU * fc / sr).exp().min(0.95);
        match s.instrument {
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
                self.lp_pole = (-std::f32::consts::TAU * FLUTE_LOSS_HZ / sr).exp();
                self.loop_pole = (-std::f32::consts::TAU * FLUTE_DC_HZ / sr).exp();
                let filters = one_pole_delay(self.lp_pole, w) + dc_block_delay(self.loop_pole, w);
                // The jet pulls the pitch sharp, increasingly so up the range
                // (measured: +4.5 cents at C4, then ~0.8 cents per semitone).
                let note = 69.0 + 12.0 * (f0 / 440.0).log2();
                let sharp = 4.5 + 0.8 * (note - 60.0);
                let d = (total * FLUTE_BORE * (sharp / 1200.0).exp2() - filters).max(4.0);
                self.d_bore_target = d;
                // Embouchure shortens or lengthens the jet a little.
                let jet = FLUTE_JET * (1.0 + 0.06 * (s.embouchure - 0.5));
                self.d_aux_target = (d * jet).max(1.0);
            }
        }
        if snap {
            self.d_bore = self.d_bore_target;
            self.d_aux = self.d_aux_target;
        }
    }

    /// Start (or re-tongue) a note.
    pub fn start(&mut self, s: &WindSettings, sr: f32, f0: f32) {
        if s.instrument != self.instrument {
            self.clear();
        }
        self.tune(s, sr, f0, true);
        self.attack_coef = (-1.0 / (s.attack.max(0.002) * 0.3 * sr)).exp();
        self.release_coef = (-1.0 / (0.03 * sr)).exp();
        self.vib_time = 0.0;
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
        // Delayed vibrato, fading in once the note has settled.
        self.vib_time += n as f32 / sr;
        self.vib_phase = (self.vib_phase + s.vibrato_rate * n as f32 / sr).fract();
        let onset = ((self.vib_time - 0.2) / 0.5).clamp(0.0, 1.0);
        let vib = s.vibrato * onset * sin_cycles(self.vib_phase) / 100.0;
        self.tune(s, sr, midi_to_freq(pitch + vib), false);

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
        let level = LEVEL[self.instrument as usize];
        // Breath noise as a fraction of the pressure; the flute's speaking
        // window is narrower, so it gets less.
        let noise = s.noise
            * match self.instrument {
                Instrument::Flute => 0.12,
                _ => 0.2,
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
                    let x = -self.lp_y;
                    let t = x - self.loop_x1 + self.loop_pole * self.loop_y1;
                    self.loop_x1 = x;
                    self.loop_y1 = t;
                    self.aux[w] = breath - 0.5 * t;
                    let jet = read(&self.aux, w, self.d_aux);
                    let jet = (jet * (jet * jet - 1.0)).clamp(-1.0, 1.0);
                    self.bore[w] = jet + 0.5 * t;
                    bore_out
                }
            };
            self.w = (self.w + 1) & mask;
            let v = y - self.dc_x1 + 0.995 * self.dc_y1;
            self.dc_x1 = y;
            self.dc_y1 = v;
            let v = v * level;
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
}
