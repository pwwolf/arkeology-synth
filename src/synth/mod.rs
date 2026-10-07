//! Synth engines and the shared polyphonic voice manager.

pub mod acid;
pub mod analog;
pub mod drums;
pub mod fm;
pub mod granular;
pub mod kit;
pub mod physical;
pub mod sampler;
pub mod tonewheel;
pub mod vocal;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::params::{ParamDesc as P, Unit};
use crate::sample::{Builtins, Sample};

/// Upper bound on voices per slot; the "Voices" parameter limits how many are used.
pub const MAX_VOICES: usize = 32;
/// Audio is rendered in blocks of at most this many frames.
pub const MAX_BLOCK: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SynthKind {
    Fm,
    Granular,
    Acid,
    Drums,
    Sampler,
    Analog,
    Physical,
    Kit,
    Tonewheel,
    Vocal,
}

impl SynthKind {
    pub const ALL: [SynthKind; 10] = [
        SynthKind::Fm,
        SynthKind::Analog,
        SynthKind::Physical,
        SynthKind::Tonewheel,
        SynthKind::Vocal,
        SynthKind::Granular,
        SynthKind::Acid,
        SynthKind::Drums,
        SynthKind::Kit,
        SynthKind::Sampler,
    ];

    /// How many sample files this synth type holds (patches store their paths).
    pub fn sample_slots(self) -> usize {
        match self {
            SynthKind::Granular | SynthKind::Sampler => 1,
            SynthKind::Kit => kit::PADS,
            _ => 0,
        }
    }

    /// Position in `ALL`, used for sorting.
    pub fn order(self) -> usize {
        Self::ALL.iter().position(|k| *k == self).unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            SynthKind::Fm => "FM",
            SynthKind::Granular => "GRN",
            SynthKind::Acid => "303",
            SynthKind::Drums => "DRM",
            SynthKind::Kit => "KIT",
            SynthKind::Sampler => "SMP",
            SynthKind::Analog => "ANA",
            SynthKind::Physical => "PHY",
            SynthKind::Tonewheel => "ORG",
            SynthKind::Vocal => "VOX",
        }
    }

    pub fn long_name(self) -> &'static str {
        match self {
            SynthKind::Fm => "FM (4-operator)",
            SynthKind::Granular => "Granular",
            SynthKind::Acid => "Acid (303-style mono bass)",
            SynthKind::Drums => "Drums (808/909-style kit)",
            SynthKind::Kit => "Drum Kit (sample pads)",
            SynthKind::Sampler => "Sampler (classic / one-shot / slice)",
            SynthKind::Analog => "Analog (poly subtractive)",
            SynthKind::Physical => "Physical (string / mallet models)",
            SynthKind::Tonewheel => "Tonewheel organ (drawbars + rotary)",
            SynthKind::Vocal => "Vocal (formant choir / talkbox)",
        }
    }

    fn specific(self) -> &'static [P] {
        match self {
            SynthKind::Fm => &fm::PARAMS,
            SynthKind::Granular => &granular::PARAMS,
            SynthKind::Acid => &acid::PARAMS,
            SynthKind::Drums => &drums::PARAMS,
            SynthKind::Kit => &kit::PARAMS,
            SynthKind::Sampler => &sampler::PARAMS,
            SynthKind::Analog => &analog::PARAMS,
            SynthKind::Physical => &physical::PARAMS,
            SynthKind::Tonewheel => &tonewheel::PARAMS,
            SynthKind::Vocal => &vocal::PARAMS,
        }
    }

    pub fn param_count(self) -> usize {
        self.fx_base() + crate::fx::PARAMS.len()
    }

    /// Whether parameter `i` applies given the current values: hides the
    /// settings of inactive FX types and of other physical models.
    pub fn param_visible(self, values: &[f32], i: usize) -> bool {
        if !crate::fx::visible(values, self.fx_base(), i) {
            return false;
        }
        let c = COMMON.len();
        if self == SynthKind::Physical && (c..self.fx_base()).contains(&i) && values.len() > c {
            return physical::param_visible(&values[c..], i - c);
        }
        true
    }

    /// Index where this synth's insert FX parameters start.
    pub fn fx_base(self) -> usize {
        COMMON.len() + self.specific().len()
    }

    /// Full parameter list: the common output section, the synth's own
    /// parameters, then its insert FX chain.
    pub fn param(self, i: usize) -> &'static P {
        let fx = self.fx_base();
        if i < COMMON.len() {
            &COMMON[i]
        } else if i < fx {
            &self.specific()[i - COMMON.len()]
        } else {
            &crate::fx::PARAMS[i - fx]
        }
    }

    pub fn params(self) -> impl Iterator<Item = &'static P> {
        COMMON
            .iter()
            .chain(self.specific().iter())
            .chain(crate::fx::PARAMS.iter())
    }

    pub fn defaults(self) -> Vec<f32> {
        self.params().map(|p| p.default).collect()
    }

    pub fn index_of(self, key: &str) -> Option<usize> {
        self.params().position(|p| p.key == key)
    }
}

// Indices into the common section (shared by every synth).
pub const VOLUME: usize = 0;
pub const PAN: usize = 1;
pub const REVERB_SEND: usize = 2;
pub const TRANSPOSE: usize = 3;
pub const BEND_RANGE: usize = 4;

pub static COMMON: [P; 5] = [
    P::float("volume", "Volume", "Output", 0.0, 1.0, 0.75, Unit::Percent),
    P::float("pan", "Pan", "Output", -1.0, 1.0, 0.0, Unit::Pan),
    P::float(
        "reverb_send",
        "Reverb Send",
        "Output",
        0.0,
        1.0,
        0.2,
        Unit::Percent,
    ),
    P::int(
        "transpose",
        "Transpose",
        "Output",
        -36,
        36,
        0,
        Unit::Semitones,
    ),
    P::int(
        "bend_range",
        "Bend Range",
        "Output",
        0,
        24,
        2,
        Unit::Semitones,
    ),
];

/// Polyphonic synths start their own table with these (they join the
/// "Output" group in the editor); mono synths leave out Voices.
pub const VOICES_PARAM: P = P::int(
    "voices",
    "Voices",
    "Output",
    1,
    MAX_VOICES as i32,
    12,
    Unit::None,
);
pub const GLIDE_PARAM: P = P::float("glide", "Glide", "Output", 0.0, 2.0, 0.0, Unit::Seconds);

// ---------------------------------------------------------------------------
// Polyphony
// ---------------------------------------------------------------------------

/// Performance controls shared by all voices of one slot.
#[derive(Clone, Copy, Default, Debug)]
pub struct Controls {
    /// Pitch offset in semitones (transpose + pitch bend), already combined.
    pub pitch: f32,
    /// Mod wheel, 0..=1.
    pub modwheel: f32,
    /// Per-sample glide coefficient (0 = no glide).
    pub glide_coef: f32,
}

pub trait Voice {
    type Shared;
    /// `from_note` is the note to glide from, if glide is active.
    fn start(&mut self, note: u8, velocity: f32, from_note: Option<f32>, shared: &Self::Shared);
    fn release(&mut self);
    /// Stop quickly regardless of mode (panic, voice-count reduction).
    /// One-shot voices ignore `release`, so they override this.
    fn kill(&mut self) {
        self.release();
    }
    fn is_active(&self) -> bool;
    /// Add this voice's output into `l` / `r`.
    fn render(&mut self, shared: &Self::Shared, ctl: &Controls, l: &mut [f32], r: &mut [f32]);
}

struct VoiceSlot<V> {
    voice: V,
    note: u8,
    held: bool,
    sustained: bool,
    stamp: u64,
}

pub struct Poly<V> {
    slots: Vec<VoiceSlot<V>>,
    limit: usize,
    sustain: bool,
    stamp: u64,
    last_note: Option<u8>,
    pub ctl: Controls,
    transpose: f32,
    bend: f32,
    bend_range: f32,
}

impl<V: Voice> Poly<V> {
    pub fn new(make: impl Fn(usize) -> V) -> Self {
        Poly {
            slots: (0..MAX_VOICES)
                .map(|i| VoiceSlot {
                    voice: make(i),
                    note: 0,
                    held: false,
                    sustained: false,
                    stamp: 0,
                })
                .collect(),
            limit: 8,
            sustain: false,
            stamp: 0,
            last_note: None,
            ctl: Controls::default(),
            transpose: 0.0,
            bend: 0.0,
            bend_range: 2.0,
        }
    }

    /// Apply the common section of the parameter vector plus the voice count
    /// and glide time from the synth's own section.
    pub fn update_common(&mut self, p: &[f32], voices: f32, glide: f32, sample_rate: f32) {
        let limit = (voices.round() as usize).clamp(1, MAX_VOICES);
        if limit < self.limit {
            for s in &mut self.slots[limit..] {
                s.voice.kill();
                s.held = false;
                s.sustained = false;
            }
        }
        self.limit = limit;
        self.transpose = p[TRANSPOSE].round();
        self.bend_range = p[BEND_RANGE].round();
        self.ctl.glide_coef = glide_coef(glide, sample_rate);
        self.refresh_pitch();
    }

    fn refresh_pitch(&mut self) {
        self.ctl.pitch = self.transpose + self.bend * self.bend_range;
    }

    pub fn note_on(&mut self, note: u8, velocity: f32, shared: &V::Shared) {
        self.stamp += 1;
        let limit = self.limit;
        let from = if self.ctl.glide_coef > 0.0 {
            self.last_note.map(f32::from)
        } else {
            None
        };
        self.last_note = Some(note);

        // Re-use a voice already playing this note, else a free one, else steal:
        // prefer the oldest released voice, then the oldest held one.
        let idx = self.slots[..limit]
            .iter()
            .position(|s| s.voice.is_active() && s.note == note)
            .or_else(|| {
                self.slots[..limit]
                    .iter()
                    .position(|s| !s.voice.is_active())
            })
            .or_else(|| {
                self.slots[..limit]
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| !s.held)
                    .min_by_key(|(_, s)| s.stamp)
                    .map(|(i, _)| i)
            })
            .or_else(|| {
                self.slots[..limit]
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| s.stamp)
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);

        let s = &mut self.slots[idx];
        s.note = note;
        s.held = true;
        s.sustained = false;
        s.stamp = self.stamp;
        s.voice.start(note, velocity, from, shared);
    }

    pub fn note_off(&mut self, note: u8) {
        for s in &mut self.slots {
            if s.held && s.note == note {
                s.held = false;
                if self.sustain {
                    s.sustained = true;
                } else {
                    s.voice.release();
                }
            }
        }
    }

    pub fn set_sustain(&mut self, on: bool) {
        self.sustain = on;
        if !on {
            for s in &mut self.slots {
                if s.sustained {
                    s.sustained = false;
                    s.voice.release();
                }
            }
        }
    }

    pub fn set_bend(&mut self, bend: f32) {
        self.bend = bend.clamp(-1.0, 1.0);
        self.refresh_pitch();
    }

    pub fn set_modwheel(&mut self, v: f32) {
        self.ctl.modwheel = v.clamp(0.0, 1.0);
    }

    pub fn all_notes_off(&mut self) {
        self.sustain = false;
        for s in &mut self.slots {
            s.held = false;
            s.sustained = false;
            s.voice.kill();
        }
    }

    pub fn active_voices(&self) -> usize {
        self.slots.iter().filter(|s| s.voice.is_active()).count()
    }

    pub fn render(&mut self, shared: &V::Shared, l: &mut [f32], r: &mut [f32]) {
        let ctl = self.ctl;
        for s in &mut self.slots {
            if s.voice.is_active() {
                s.voice.render(shared, &ctl, l, r);
            }
        }
    }
}

/// Per-sample coefficient for an exponential glide lasting roughly `time` seconds.
pub fn glide_coef(time: f32, sample_rate: f32) -> f32 {
    if time > 0.0005 {
        (-1.0 / (time * 0.3 * sample_rate)).exp()
    } else {
        0.0
    }
}

/// Glide helper: move `current` towards `target` with a per-sample coefficient,
/// applied over `frames` samples at once.
#[inline]
pub fn glide(current: f32, target: f32, coef: f32, frames: usize) -> f32 {
    if coef <= 0.0 {
        target
    } else {
        target + (current - target) * coef.powi(frames as i32)
    }
}

// ---------------------------------------------------------------------------
// Instrument: static dispatch over the available synth engines
// ---------------------------------------------------------------------------

pub enum Instrument {
    Fm(fm::FmSynth),
    Granular(granular::GranularSynth),
    Acid(acid::AcidSynth),
    Drums(Box<drums::DrumsSynth>),
    Kit(Box<kit::KitSynth>),
    Sampler(Box<sampler::SamplerSynth>),
    Analog(Box<analog::AnalogSynth>),
    Physical(Box<physical::PhysicalSynth>),
    Tonewheel(Box<tonewheel::TonewheelSynth>),
    Vocal(Box<vocal::VocalSynth>),
}

impl Instrument {
    pub fn new(kind: SynthKind, sample_rate: f32, builtins: &Builtins) -> Self {
        match kind {
            SynthKind::Fm => Instrument::Fm(fm::FmSynth::new(sample_rate)),
            SynthKind::Granular => {
                Instrument::Granular(granular::GranularSynth::new(sample_rate, builtins.clone()))
            }
            SynthKind::Acid => Instrument::Acid(acid::AcidSynth::new(sample_rate)),
            SynthKind::Drums => Instrument::Drums(Box::new(drums::DrumsSynth::new(sample_rate))),
            SynthKind::Kit => Instrument::Kit(Box::new(kit::KitSynth::new(sample_rate))),
            SynthKind::Analog => {
                Instrument::Analog(Box::new(analog::AnalogSynth::new(sample_rate)))
            }
            SynthKind::Physical => {
                Instrument::Physical(Box::new(physical::PhysicalSynth::new(sample_rate)))
            }
            SynthKind::Tonewheel => {
                Instrument::Tonewheel(Box::new(tonewheel::TonewheelSynth::new(sample_rate)))
            }
            SynthKind::Vocal => Instrument::Vocal(Box::new(vocal::VocalSynth::new(sample_rate))),
            SynthKind::Sampler => Instrument::Sampler(Box::new(sampler::SamplerSynth::new(
                sample_rate,
                builtins.clone(),
            ))),
        }
    }

    pub fn update(&mut self, params: &[f32]) {
        match self {
            Instrument::Fm(s) => s.update(params),
            Instrument::Granular(s) => s.update(params),
            Instrument::Acid(s) => s.update(params),
            Instrument::Drums(s) => s.update(params),
            Instrument::Kit(s) => s.update(params),
            Instrument::Sampler(s) => s.update(params),
            Instrument::Analog(s) => s.update(params),
            Instrument::Physical(s) => s.update(params),
            Instrument::Tonewheel(s) => s.update(params),
            Instrument::Vocal(s) => s.update(params),
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: f32) {
        match self {
            Instrument::Fm(s) => s.poly.note_on(note, velocity, &s.shared),
            Instrument::Granular(s) => s.poly.note_on(note, velocity, &s.shared),
            Instrument::Acid(s) => s.note_on(note, velocity),
            Instrument::Drums(s) => s.note_on(note, velocity),
            Instrument::Kit(s) => s.note_on(note, velocity),
            Instrument::Sampler(s) => s.note_on(note, velocity),
            Instrument::Analog(s) => s.poly.note_on(note, velocity, &s.shared),
            Instrument::Physical(s) => s.poly.note_on(note, velocity, &s.shared),
            Instrument::Tonewheel(s) => s.note_on(note, velocity),
            Instrument::Vocal(s) => s.poly.note_on(note, velocity, &s.shared),
        }
    }

    fn poly_op<R>(&mut self, f: impl FnOnce(&mut dyn PolyControl) -> R) -> R {
        match self {
            Instrument::Fm(s) => f(&mut s.poly),
            Instrument::Granular(s) => f(&mut s.poly),
            Instrument::Acid(s) => f(s),
            Instrument::Drums(s) => f(s.as_mut()),
            Instrument::Kit(s) => f(s.as_mut()),
            Instrument::Sampler(s) => f(&mut s.poly),
            Instrument::Analog(s) => f(&mut s.poly),
            Instrument::Physical(s) => f(&mut s.poly),
            Instrument::Tonewheel(s) => f(s.as_mut()),
            Instrument::Vocal(s) => f(&mut s.poly),
        }
    }

    pub fn note_off(&mut self, note: u8) {
        self.poly_op(|p| p.note_off(note))
    }

    pub fn set_sustain(&mut self, on: bool) {
        self.poly_op(|p| p.set_sustain(on))
    }

    pub fn set_bend(&mut self, v: f32) {
        self.poly_op(|p| p.set_bend(v))
    }

    pub fn set_modwheel(&mut self, v: f32) {
        self.poly_op(|p| p.set_modwheel(v))
    }

    pub fn all_notes_off(&mut self) {
        self.poly_op(|p| p.all_notes_off())
    }

    pub fn active_voices(&mut self) -> usize {
        self.poly_op(|p| p.active_voices())
    }

    /// Swap in a sample at `index` (a kit pad; 0 for single-sample synths).
    /// Returns the previous one so the caller can free it off the audio thread.
    pub fn set_sample(&mut self, index: usize, sample: Option<Arc<Sample>>) -> Option<Arc<Sample>> {
        match self {
            Instrument::Granular(s) if index == 0 => s.set_file_sample(sample),
            Instrument::Sampler(s) if index == 0 => s.set_file_sample(sample),
            Instrument::Kit(s) => s.set_pad_sample(index, sample),
            Instrument::Granular(_) | Instrument::Sampler(_) => sample,
            Instrument::Fm(_)
            | Instrument::Acid(_)
            | Instrument::Drums(_)
            | Instrument::Analog(_)
            | Instrument::Physical(_)
            | Instrument::Tonewheel(_)
            | Instrument::Vocal(_) => sample,
        }
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        match self {
            Instrument::Fm(s) => s.poly.render(&s.shared, l, r),
            Instrument::Granular(s) => s.render(l, r),
            Instrument::Acid(s) => s.render(l, r),
            Instrument::Drums(s) => s.render(l, r),
            Instrument::Kit(s) => s.render(l, r),
            Instrument::Sampler(s) => s.render(l, r),
            Instrument::Analog(s) => s.render(l, r),
            Instrument::Physical(s) => s.render(l, r),
            Instrument::Tonewheel(s) => s.render(l, r),
            Instrument::Vocal(s) => s.render(l, r),
        }
    }
}

/// Object-safe subset of `Poly` used to avoid repeating the dispatch match.
trait PolyControl {
    fn note_off(&mut self, note: u8);
    fn set_sustain(&mut self, on: bool);
    fn set_bend(&mut self, v: f32);
    fn set_modwheel(&mut self, v: f32);
    fn all_notes_off(&mut self);
    fn active_voices(&self) -> usize;
}

impl<V: Voice> PolyControl for Poly<V> {
    fn note_off(&mut self, note: u8) {
        Poly::note_off(self, note)
    }
    fn set_sustain(&mut self, on: bool) {
        Poly::set_sustain(self, on)
    }
    fn set_bend(&mut self, v: f32) {
        Poly::set_bend(self, v)
    }
    fn set_modwheel(&mut self, v: f32) {
        Poly::set_modwheel(self, v)
    }
    fn all_notes_off(&mut self) {
        Poly::all_notes_off(self)
    }
    fn active_voices(&self) -> usize {
        Poly::active_voices(self)
    }
}
