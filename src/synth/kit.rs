//! Sample drum kit: 16 pads, each playing its own one-shot sample when its
//! MIDI note arrives. Pads default to the General MIDI drum map, have their
//! own tune / level / pan / decay / filter, and pads sharing a choke group
//! cut each other off (closed hats choke open hats). Note-offs are ignored.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::dsp::{FilterMode, Svf, SvfCoefs, pan_gains, semitones_to_ratio};
use crate::params::{ParamDesc as P, Unit};
use crate::sample::Sample;

use super::{BEND_RANGE, COMMON, MAX_BLOCK, PolyControl, TRANSPOSE};

pub const PADS: usize = 16;
pub const VEL_SENS: usize = 0;
pub const VEL_CUTOFF: usize = 1;
pub const PAD_BASE: usize = 2;
pub const PAD_STRIDE: usize = 7;
pub const P_NOTE: usize = 0;
pub const P_TUNE: usize = 1;
pub const P_LEVEL: usize = 2;
pub const P_PAN: usize = 3;
pub const P_DECAY: usize = 4;
pub const P_CUTOFF: usize = 5;
pub const P_CHOKE: usize = 6;

/// What each pad is for by default (General MIDI drum notes).
pub const PAD_ROLES: [&str; PADS] = [
    "Kick",
    "Snare",
    "Closed Hat",
    "Open Hat",
    "Clap",
    "Rim",
    "Low Tom",
    "Mid Tom",
    "High Tom",
    "Crash",
    "Ride",
    "Pedal Hat",
    "Snare 2",
    "Tambourine",
    "Cowbell",
    "Shaker",
];

macro_rules! pad_params {
    ($n:literal, $note:expr, $choke:expr) => {{
        const G: &str = concat!("Pad ", $n);
        [
            P::int(
                concat!("pad", $n, "_note"),
                "Note",
                G,
                0,
                127,
                $note,
                Unit::Note,
            ),
            P::float(
                concat!("pad", $n, "_tune"),
                "Tune",
                G,
                -24.0,
                24.0,
                0.0,
                Unit::Semitones,
            )
            .step(0.5),
            P::float(
                concat!("pad", $n, "_level"),
                "Level",
                G,
                0.0,
                1.0,
                0.8,
                Unit::Percent,
            ),
            P::float(
                concat!("pad", $n, "_pan"),
                "Pan",
                G,
                -1.0,
                1.0,
                0.0,
                Unit::Pan,
            ),
            P::float(
                concat!("pad", $n, "_decay"),
                "Decay",
                G,
                0.02,
                10.0,
                10.0,
                Unit::Seconds,
            )
            .exp(),
            P::float(
                concat!("pad", $n, "_cutoff"),
                "Cutoff",
                G,
                200.0,
                20_000.0,
                20_000.0,
                Unit::Hz,
            )
            .exp(),
            P::int(
                concat!("pad", $n, "_choke"),
                "Choke Group",
                G,
                0,
                4,
                $choke,
                Unit::None,
            ),
        ]
    }};
}

const fn build_params() -> [P; PAD_BASE + PADS * PAD_STRIDE] {
    let head = [
        P::float("vel_sens", "Vel Sens", "Kit", 0.0, 1.0, 0.7, Unit::Percent),
        P::float(
            "vel_cutoff",
            "Vel>Cutoff",
            "Kit",
            0.0,
            1.0,
            0.0,
            Unit::Percent,
        ),
    ];
    let pads: [[P; PAD_STRIDE]; PADS] = [
        pad_params!("1", 36, 0),
        pad_params!("2", 38, 0),
        pad_params!("3", 42, 1),
        pad_params!("4", 46, 1),
        pad_params!("5", 39, 0),
        pad_params!("6", 37, 0),
        pad_params!("7", 41, 0),
        pad_params!("8", 45, 0),
        pad_params!("9", 48, 0),
        pad_params!("10", 49, 0),
        pad_params!("11", 51, 0),
        pad_params!("12", 44, 1),
        pad_params!("13", 40, 0),
        pad_params!("14", 54, 0),
        pad_params!("15", 56, 0),
        pad_params!("16", 70, 0),
    ];
    let mut out = [head[0]; PAD_BASE + PADS * PAD_STRIDE];
    out[1] = head[1];
    let mut p = 0;
    while p < PADS {
        let mut k = 0;
        while k < PAD_STRIDE {
            out[PAD_BASE + p * PAD_STRIDE + k] = pads[p][k];
            k += 1;
        }
        p += 1;
    }
    out
}

pub static PARAMS: [P; PAD_BASE + PADS * PAD_STRIDE] = build_params();

/// Which pad a synth-specific parameter index belongs to.
pub fn pad_of(local: usize) -> Option<usize> {
    local
        .checked_sub(PAD_BASE)
        .map(|r| r / PAD_STRIDE)
        .filter(|p| *p < PADS)
}

const MAX_VOICES: usize = 32;
const OUTPUT_GAIN: f32 = 0.6;
/// Fade at a sample's end so samples that don't decay to silence don't click.
const END_FADE: f32 = 64.0;

#[derive(Clone, Copy, Default)]
struct PadParams {
    note: u8,
    tune: f32,
    level: f32,
    pan: f32,
    decay: f32,
    cutoff: f32,
    choke: u8,
}

#[derive(Clone, Copy, Default)]
struct KitVoice {
    active: bool,
    pad: usize,
    pos: f64,
    rate: f64,
    gain: (f32, f32),
    env: f32,
    env_coef: f32,
    choke: f32,
    choking: bool,
    filtered: bool,
    coefs: SvfCoefs,
    svf: [Svf; 2],
    stamp: u64,
}

pub struct KitSynth {
    sample_rate: f32,
    pads: [PadParams; PADS],
    samples: [Option<Arc<Sample>>; PADS],
    vel_sens: f32,
    vel_cutoff: f32,
    transpose: f32,
    bend: f32,
    bend_range: f32,
    voices: [KitVoice; MAX_VOICES],
    stamp: u64,
}

impl KitSynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = KitSynth {
            sample_rate,
            pads: [PadParams::default(); PADS],
            samples: Default::default(),
            vel_sens: 0.7,
            vel_cutoff: 0.0,
            transpose: 0.0,
            bend: 0.0,
            bend_range: 2.0,
            voices: [KitVoice::default(); MAX_VOICES],
            stamp: 0,
        };
        s.update(&super::SynthKind::Kit.defaults());
        s
    }

    pub fn update(&mut self, params: &[f32]) {
        self.transpose = params[TRANSPOSE].round();
        self.bend_range = params[BEND_RANGE].round();
        let p = &params[COMMON.len()..];
        self.vel_sens = p[VEL_SENS];
        self.vel_cutoff = p[VEL_CUTOFF];
        for (i, pad) in self.pads.iter_mut().enumerate() {
            let b = PAD_BASE + i * PAD_STRIDE;
            *pad = PadParams {
                note: p[b + P_NOTE].round().clamp(0.0, 127.0) as u8,
                tune: p[b + P_TUNE],
                level: p[b + P_LEVEL],
                pan: p[b + P_PAN],
                decay: p[b + P_DECAY],
                cutoff: p[b + P_CUTOFF],
                choke: p[b + P_CHOKE].round() as u8,
            };
        }
    }

    /// Swap a pad's sample; returns the old one so the caller can free it
    /// off the audio thread. Voices still playing that pad are cut.
    pub fn set_pad_sample(
        &mut self,
        pad: usize,
        sample: Option<Arc<Sample>>,
    ) -> Option<Arc<Sample>> {
        if pad >= PADS {
            return sample;
        }
        for v in self.voices.iter_mut().filter(|v| v.active && v.pad == pad) {
            v.active = false;
        }
        std::mem::replace(&mut self.samples[pad], sample)
    }

    pub fn note_on(&mut self, note: u8, velocity: f32) {
        for pad in 0..PADS {
            if self.pads[pad].note == note && self.samples[pad].is_some() {
                self.trigger(pad, velocity);
            }
        }
    }

    fn trigger(&mut self, pad: usize, velocity: f32) {
        let sr = self.sample_rate;
        let p = self.pads[pad];
        let Some(src_rate) = self.samples[pad].as_ref().map(|s| s.sample_rate) else {
            return;
        };
        if p.choke > 0 {
            let pads = self.pads;
            for v in self
                .voices
                .iter_mut()
                .filter(|v| v.active && pads[v.pad].choke == p.choke)
            {
                v.choking = true;
            }
        }
        self.stamp += 1;
        let slot = self
            .voices
            .iter()
            .position(|v| !v.active)
            .unwrap_or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, v)| v.stamp)
                    .map_or(0, |(i, _)| i)
            });
        let semis = p.tune + self.transpose + self.bend * self.bend_range;
        let rate = semitones_to_ratio(semis) as f64 * (src_rate / sr) as f64;
        let vel_gain = crate::dsp::velocity_gain(velocity, self.vel_sens);
        let (l, r) = pan_gains(p.pan);
        let g = p.level * p.level * 1.5 * vel_gain * OUTPUT_GAIN * std::f32::consts::SQRT_2;
        let fc = p.cutoff * (self.vel_cutoff * 3.0 * (velocity - 1.0)).exp2();
        self.voices[slot] = KitVoice {
            active: true,
            pad,
            pos: 0.0,
            rate,
            gain: (l * g, r * g),
            env: 1.0,
            env_coef: (0.001f32.ln() / (p.decay.max(0.005) * sr)).exp(),
            choke: 1.0,
            choking: false,
            filtered: fc < 19_000.0,
            coefs: SvfCoefs::new(FilterMode::LowPass, fc, 0.0, sr),
            svf: [Svf::default(); 2],
            stamp: self.stamp,
        };
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(MAX_BLOCK);
        let choke_coef = (-1.0 / (0.004 * self.sample_rate)).exp();
        for v in self.voices.iter_mut().filter(|v| v.active) {
            let Some(sample) = &self.samples[v.pad] else {
                v.active = false;
                continue;
            };
            let len = sample.len() as f64;
            for i in 0..n {
                let remaining = len - 1.0 - v.pos;
                if remaining <= 0.0 {
                    v.active = false;
                    break;
                }
                let edge = ((remaining / v.rate) as f32 / END_FADE).min(1.0);
                let (mut a, mut b) = sample.read_stereo(v.pos);
                if v.filtered {
                    a = v.svf[0].process(&v.coefs, a);
                    b = v.svf[1].process(&v.coefs, b);
                }
                let g = v.env * v.choke * edge;
                l[i] += a * g * v.gain.0;
                r[i] += b * g * v.gain.1;
                v.pos += v.rate;
                v.env *= v.env_coef;
                if v.choking {
                    v.choke *= choke_coef;
                }
            }
            if v.env < 1e-4 || v.choke < 1e-4 {
                v.active = false;
            }
        }
    }
}

impl PolyControl for KitSynth {
    fn note_off(&mut self, _note: u8) {}

    fn set_sustain(&mut self, _on: bool) {}

    fn set_bend(&mut self, v: f32) {
        self.bend = v.clamp(-1.0, 1.0);
    }

    fn set_modwheel(&mut self, _v: f32) {}

    fn all_notes_off(&mut self) {
        for v in self.voices.iter_mut().filter(|v| v.active) {
            v.choking = true;
        }
    }

    fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }
}

// ---------------------------------------------------------------------------
// Loading a folder as a kit
// ---------------------------------------------------------------------------

/// Assign audio files to pads by their names ("kick", "snare", "hh",
/// "open hat", "tom low", "ride"...). Files that match no role fill the
/// remaining empty pads in name order. Returns one entry per pad.
pub fn assign_files(files: &[PathBuf]) -> [Option<PathBuf>; PADS] {
    let mut pads: [Option<PathBuf>; PADS] = Default::default();
    let mut sorted = files.to_vec();
    sorted.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    let mut leftovers = Vec::new();
    let mut toms = Vec::new();
    for path in sorted {
        match role_of(&path) {
            Role::Pad(p) if pads[p].is_none() => pads[p] = Some(path),
            // A second snare goes to "Snare 2".
            Role::Pad(1) if pads[12].is_none() => pads[12] = Some(path),
            Role::Tom(Some(p)) if pads[p].is_none() => pads[p] = Some(path),
            Role::Tom(_) => toms.push(path),
            _ => leftovers.push(path),
        }
    }
    // Unlabelled toms fill low, mid, high in order.
    for path in toms {
        match [6, 7, 8].into_iter().find(|&p| pads[p].is_none()) {
            Some(p) => pads[p] = Some(path),
            None => leftovers.push(path),
        }
    }
    for path in leftovers {
        if let Some(p) = pads.iter().position(Option::is_none) {
            pads[p] = Some(path);
        }
    }
    pads
}

enum Role {
    Pad(usize),
    /// A tom, with its pad if the name says low / mid / high.
    Tom(Option<usize>),
    Unknown,
}

fn role_of(path: &Path) -> Role {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // Split into words ("HH_Closed-01" -> ["hh", "closed"]) and also keep the
    // whole name for compound words like "openhat".
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| !w.is_empty())
        .collect();
    let has = |keys: &[&str]| words.iter().any(|w| keys.iter().any(|k| w.starts_with(k)));
    let exact = |keys: &[&str]| words.iter().any(|w| keys.contains(w));
    let hat = has(&["hat", "hihat", "hh"]) || exact(&["ch", "oh", "chh", "ohh", "phh"]);
    if (hat && has(&["open"])) || exact(&["oh", "ohh"]) || name.contains("openhat") {
        Role::Pad(3)
    } else if (hat && has(&["pedal", "foot"])) || exact(&["phh"]) {
        Role::Pad(11)
    } else if hat || name.contains("closedhat") {
        Role::Pad(2)
    } else if has(&["kick", "kik", "bassdrum"]) || exact(&["bd", "bass"]) {
        Role::Pad(0)
    } else if has(&["snare", "snr"]) || exact(&["sd", "sn"]) {
        Role::Pad(1)
    } else if has(&["clap", "clp"]) {
        Role::Pad(4)
    } else if has(&["rim", "sidestick", "stick"]) {
        Role::Pad(5)
    } else if has(&["tom"]) || exact(&["lt", "mt", "ht", "ft"]) {
        if has(&["low", "lo", "floor"]) || exact(&["lt", "ft"]) {
            Role::Tom(Some(6))
        } else if has(&["mid", "med"]) || exact(&["mt"]) {
            Role::Tom(Some(7))
        } else if has(&["high", "hi"]) || exact(&["ht"]) {
            Role::Tom(Some(8))
        } else {
            Role::Tom(None)
        }
    } else if has(&["crash", "splash", "china"]) {
        Role::Pad(9)
    } else if has(&["ride"]) {
        Role::Pad(10)
    } else if has(&["tamb"]) {
        Role::Pad(13)
    } else if has(&["cowbell", "bell"]) {
        Role::Pad(14)
    } else if has(&["shake", "shaker", "maraca", "cabasa"]) {
        Role::Pad(15)
    } else if has(&["cym"]) {
        Role::Pad(9)
    } else {
        Role::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::SynthKind;

    fn tone(freq: f32, seconds: f32) -> Arc<Sample> {
        let sr = 48_000.0;
        let data = (0..(sr * seconds) as usize)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / sr).sin())
            .collect();
        Arc::new(Sample::new("t", data, None, sr))
    }

    fn render(k: &mut KitSynth, frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let (mut l, mut r) = ([0.0f32; MAX_BLOCK], [0.0f32; MAX_BLOCK]);
        while out.len() < frames {
            l.fill(0.0);
            r.fill(0.0);
            k.render(&mut l, &mut r);
            out.extend_from_slice(&l);
        }
        out
    }

    fn kit_with(overrides: &[(&str, f32)]) -> KitSynth {
        crate::dsp::init_tables();
        let mut k = KitSynth::new(48_000.0);
        let mut p = SynthKind::Kit.defaults();
        for (key, v) in overrides {
            p[SynthKind::Kit.index_of(key).unwrap()] = *v;
        }
        k.update(&p);
        k
    }

    #[test]
    fn pads_play_on_their_notes_and_end_with_the_sample() {
        let mut k = kit_with(&[]);
        k.set_pad_sample(0, Some(tone(100.0, 0.25)));
        k.note_on(60, 1.0); // no pad there
        assert_eq!(k.active_voices(), 0);
        k.note_on(36, 1.0);
        let out = render(&mut k, 4_800);
        assert!(out.iter().any(|v| v.abs() > 0.05));
        render(&mut k, 12_000);
        assert_eq!(k.active_voices(), 0, "voice outlived its sample");
    }

    #[test]
    fn tune_and_decay_apply() {
        let mut k = kit_with(&[("pad1_tune", 12.0), ("pad1_decay", 0.05)]);
        k.set_pad_sample(0, Some(tone(200.0, 2.0)));
        k.note_on(36, 1.0);
        let out = render(&mut k, 4_800);
        let crossings = out[..2_400]
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        // 400 Hz for 50 ms = 40 crossings.
        assert!((36..=44).contains(&crossings), "{crossings} crossings");
        render(&mut k, 9_600);
        assert_eq!(k.active_voices(), 0, "decay didn't end the voice");
    }

    #[test]
    fn closed_hat_chokes_open_hat_but_others_overlap() {
        let mut k = kit_with(&[]);
        for pad in [0, 2, 3] {
            k.set_pad_sample(pad, Some(tone(300.0, 2.0)));
        }
        k.note_on(46, 1.0); // open hat
        k.note_on(36, 1.0); // kick: no choke group
        render(&mut k, 2_400);
        k.note_on(42, 1.0); // closed hat chokes the open hat
        render(&mut k, 4_800);
        let pads: Vec<usize> = k
            .voices
            .iter()
            .filter(|v| v.active)
            .map(|v| v.pad)
            .collect();
        assert!(!pads.contains(&3), "open hat still ringing: {pads:?}");
        assert!(pads.contains(&0) && pads.contains(&2));
    }

    #[test]
    fn folders_map_by_name() {
        let names = [
            "Kick 01.wav",
            "SD_tight.wav",
            "HH Closed.wav",
            "OpenHat.wav",
            "clap.wav",
            "Tom Low.wav",
            "Tom Hi.wav",
            "tom.wav",
            "crash 1.wav",
            "Snare Rim.wav",
            "snare2.wav",
            "Ride.wav",
            "weird noise.wav",
        ];
        let files: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
        let pads = assign_files(&files);
        let at = |p: usize| {
            pads[p]
                .as_ref()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        assert_eq!(at(0), "Kick 01.wav");
        assert_eq!(at(2), "HH Closed.wav");
        assert_eq!(at(3), "OpenHat.wav");
        assert_eq!(at(4), "clap.wav");
        assert_eq!(at(6), "Tom Low.wav");
        assert_eq!(at(7), "tom.wav", "unlabelled tom fills mid");
        assert_eq!(at(8), "Tom Hi.wav");
        assert_eq!(at(9), "crash 1.wav");
        assert_eq!(at(10), "Ride.wav");
        // Three snares: the first two (by name) take Snare and Snare 2.
        let snares = [at(1), at(12)];
        assert!(snares.contains(&"SD_tight.wav".to_string()), "{snares:?}");
        assert!(
            pads.iter()
                .flatten()
                .any(|p| p.to_string_lossy() == "weird noise.wav"),
            "leftovers fill free pads"
        );
        assert_eq!(pads.iter().flatten().count(), names.len());
    }
}
