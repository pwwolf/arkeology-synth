//! Polyphonic sampler with three modes:
//!
//! - **Classic**: the sample is pitched across the keyboard from a root note,
//!   with an optional crossfaded sustain loop.
//! - **One-shot**: every note plays the whole start..end region; note-off is
//!   ignored.
//! - **Slice**: the region is cut into equal slices or at detected transients,
//!   mapped to consecutive notes from "Base Note", each played as a one-shot.

use std::sync::Arc;

use crate::dsp::{AdsrParams, Env, FilterMode, Svf, SvfCoefs, semitones_to_ratio};
use crate::params::{ParamDesc as P, Unit};
use crate::sample::{Builtins, Sample};

use super::{COMMON, Controls, GLIDE_PARAM, MAX_BLOCK, Poly, VOICES_PARAM, Voice, glide, granular};

pub const MODES: [&str; 3] = ["Classic", "One-shot", "Slice"];
pub const SLICE_BY: [&str; 2] = ["Equal", "Transient"];

pub const VOICES: usize = 0;
pub const GLIDE: usize = 1;
pub const SOURCE: usize = 2;
pub const MODE: usize = 3;
pub const ROOT: usize = 4;
pub const TUNE: usize = 5;
pub const START: usize = 6;
pub const END: usize = 7;
pub const REVERSE: usize = 8;
pub const LOOP: usize = 9;
pub const LOOP_START: usize = 10;
pub const LOOP_END: usize = 11;
pub const CROSSFADE: usize = 12;
pub const SLICES: usize = 13;
pub const SLICE_MODE: usize = 14;
pub const SENSITIVITY: usize = 15;
pub const BASE_NOTE: usize = 16;
pub const ATTACK: usize = 17;
pub const DECAY: usize = 18;
pub const SUSTAIN: usize = 19;
pub const RELEASE: usize = 20;
pub const VEL_AMP: usize = 21;
pub const FILTER_TYPE: usize = 22;
pub const CUTOFF: usize = 23;
pub const RESONANCE: usize = 24;
pub const VEL_CUTOFF: usize = 25;

pub const MAX_SLICES: usize = 64;

pub static PARAMS: [P; 26] = [
    VOICES_PARAM,
    GLIDE_PARAM,
    P::choice("source", "Source", "Sample", &granular::SOURCES, 2),
    P::choice("mode", "Mode", "Sample", &MODES, 0),
    P::int("root", "Root Note", "Sample", 0, 127, 60, Unit::Note),
    P::float("tune", "Tune", "Sample", -100.0, 100.0, 0.0, Unit::Cents).step(1.0),
    P::float("start", "Start", "Sample", 0.0, 1.0, 0.0, Unit::Percent).step(0.005),
    P::float("end", "End", "Sample", 0.0, 1.0, 1.0, Unit::Percent).step(0.005),
    P::toggle("reverse", "Reverse", "Sample", false),
    P::toggle("loop", "Loop", "Loop (Classic)", true),
    P::float("loop_start", "Loop Start", "Loop (Classic)", 0.0, 1.0, 0.2, Unit::Percent).step(0.005),
    P::float("loop_end", "Loop End", "Loop (Classic)", 0.0, 1.0, 0.8, Unit::Percent).step(0.005),
    P::float("crossfade", "Crossfade", "Loop (Classic)", 0.0, 0.5, 0.1, Unit::Percent),
    P::int("slices", "Slices", "Slice", 2, MAX_SLICES as i32, 16, Unit::None),
    P::choice("slice_by", "Slice By", "Slice", &SLICE_BY, 0),
    P::float("sensitivity", "Sensitivity", "Slice", 0.0, 1.0, 0.5, Unit::Percent),
    P::int("base_note", "Base Note", "Slice", 0, 127, 36, Unit::Note),
    P::float("attack", "Attack", "Amp Env", 0.001, 10.0, 0.002, Unit::Seconds).exp(),
    P::float("decay", "Decay", "Amp Env", 0.005, 20.0, 1.0, Unit::Seconds).exp(),
    P::float("sustain", "Sustain", "Amp Env", 0.0, 1.0, 1.0, Unit::Percent),
    P::float("release", "Release", "Amp Env", 0.005, 20.0, 0.3, Unit::Seconds).exp(),
    P::float("vel_amp", "Vel>Amp", "Amp Env", 0.0, 1.0, 0.7, Unit::Percent),
    P::choice("filter_type", "Type", "Filter", &granular::FILTERS, 0),
    P::float("cutoff", "Cutoff", "Filter", 20.0, 20_000.0, 20_000.0, Unit::Hz).exp(),
    P::float("resonance", "Resonance", "Filter", 0.0, 1.0, 0.0, Unit::Percent),
    P::float("vel_cutoff", "Vel>Cutoff", "Filter", 0.0, 1.0, 0.0, Unit::Percent),
];

const VOICE_GAIN: f32 = 0.5;
/// Fade applied at the end of a region (and at slice ends) to avoid clicks.
const EDGE_FADE: f32 = 96.0;
const MIN_REGION: usize = 64;
/// Transient slices closer together than this (seconds) are merged.
const MIN_SLICE_GAP: f32 = 0.03;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Classic,
    OneShot,
    Slice,
}

/// Slice boundaries in frames: slice `i` spans `points[i]..points[i + 1]`.
#[derive(Clone, Copy, Debug)]
pub struct Slices {
    pub points: [usize; MAX_SLICES + 1],
    pub count: usize,
}

impl Default for Slices {
    fn default() -> Self {
        Slices { points: [0; MAX_SLICES + 1], count: 0 }
    }
}

impl Slices {
    pub fn range(&self, i: usize) -> Option<(usize, usize)> {
        (i < self.count).then(|| (self.points[i], self.points[i + 1]))
    }
}

/// Everything about how a sample is laid out for playback, derived from the
/// parameters. Shared by the audio engine and the waveform display so they
/// always agree. Allocation-free.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub mode: Mode,
    pub start: usize,
    pub end: usize,
    pub reverse: bool,
    /// Loop region in frames, if looping applies (Classic mode, loop on).
    pub looping: Option<(usize, usize)>,
    pub crossfade: usize,
    pub slices: Slices,
    pub base_note: u8,
}

impl Layout {
    /// `p` is the synth-specific parameter slice (after the common section).
    pub fn new(p: &[f32], sample: &Sample) -> Self {
        let len = sample.len();
        let frac = |v: f32| ((v.clamp(0.0, 1.0) * len as f32) as usize).min(len);
        let (mut start, mut end) = (frac(p[START]), frac(p[END]));
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        if end - start < MIN_REGION {
            end = (start + MIN_REGION).min(len);
            start = end.saturating_sub(MIN_REGION);
        }
        let mode = match p[MODE].round() as usize {
            1 => Mode::OneShot,
            2 => Mode::Slice,
            _ => Mode::Classic,
        };
        let mut looping = None;
        let mut crossfade = 0;
        if mode == Mode::Classic && p[LOOP] >= 0.5 {
            let (mut ls, mut le) = (frac(p[LOOP_START]), frac(p[LOOP_END]));
            if ls > le {
                std::mem::swap(&mut ls, &mut le);
            }
            let (ls, le) = (ls.max(start), le.min(end));
            if le > ls + MIN_REGION {
                looping = Some((ls, le));
                // The crossfade reads material just outside the loop, so it
                // can't be longer than what exists on either side.
                let want = (p[CROSSFADE] * (le - ls) as f32) as usize;
                crossfade = want.min(ls).min(len - le);
            }
        }
        let slices = if mode == Mode::Slice {
            compute_slices(sample, start, end, p[SLICES].round() as usize, p[SLICE_MODE] >= 0.5, p[SENSITIVITY])
        } else {
            Slices::default()
        };
        Layout {
            mode,
            start,
            end,
            reverse: p[REVERSE] >= 0.5,
            looping,
            crossfade,
            slices,
            base_note: p[BASE_NOTE].round().clamp(0.0, 127.0) as u8,
        }
    }

    /// The region a note plays, or `None` if it maps to no slice.
    pub fn region_for(&self, note: u8) -> Option<(usize, usize)> {
        match self.mode {
            Mode::Slice => self.slices.range((note as usize).checked_sub(self.base_note as usize)?),
            _ => Some((self.start, self.end)),
        }
    }
}

fn compute_slices(sample: &Sample, start: usize, end: usize, count: usize, transient: bool, sensitivity: f32) -> Slices {
    let count = count.clamp(1, MAX_SLICES);
    let mut s = Slices::default();
    if !transient {
        for i in 0..=count {
            s.points[i] = start + (end - start) * i / count;
        }
        s.count = count;
        return s;
    }
    // Strongest onsets above the threshold, kept apart by a minimum gap.
    let threshold = 1.0 - sensitivity.clamp(0.0, 1.0);
    let gap = (MIN_SLICE_GAP * sample.sample_rate) as usize;
    let mut picked = [0usize; MAX_SLICES];
    let mut n = 0;
    for &(pos, strength) in &sample.onsets {
        if n + 1 >= count || strength < threshold {
            break;
        }
        if pos <= start + gap || pos + gap >= end {
            continue;
        }
        if picked[..n].iter().all(|&q| q.abs_diff(pos) >= gap) {
            picked[n] = pos;
            n += 1;
        }
    }
    picked[..n].sort_unstable();
    s.points[0] = start;
    s.points[1..=n].copy_from_slice(&picked[..n]);
    s.points[n + 1] = end;
    s.count = n + 1;
    s
}

pub struct SamplerShared {
    sample_rate: f32,
    source: Option<Arc<Sample>>,
    layout: Option<Layout>,
    root: f32,
    tune_ratio: f32,
    env: AdsrParams,
    vel_amp: f32,
    filter_mode: FilterMode,
    cutoff: f32,
    resonance: f32,
    vel_cutoff: f32,
    filter_bypass: bool,
}

pub struct SamplerVoice {
    active: bool,
    note: u8,
    pitch: f32,
    vel: f32,
    pos: f64,
    /// Region being played, captured at note start.
    region: (f64, f64),
    reverse: bool,
    oneshot: bool,
    keytrack: bool,
    env: Env,
    filters: [Svf; 2],
    killing: bool,
    kill_gain: f32,
}

impl SamplerVoice {
    fn new() -> Self {
        SamplerVoice {
            active: false,
            note: 60,
            pitch: 60.0,
            vel: 1.0,
            pos: 0.0,
            region: (0.0, 0.0),
            reverse: false,
            oneshot: false,
            keytrack: true,
            env: Env::default(),
            filters: [Svf::default(); 2],
            killing: false,
            kill_gain: 1.0,
        }
    }
}

impl Voice for SamplerVoice {
    type Shared = SamplerShared;

    fn start(&mut self, note: u8, velocity: f32, from_note: Option<f32>, s: &SamplerShared) {
        let (Some(layout), Some(_)) = (&s.layout, &s.source) else {
            self.active = false;
            return;
        };
        let Some((a, b)) = layout.region_for(note) else {
            self.active = false;
            return;
        };
        if !self.active || from_note.is_none() {
            self.pitch = from_note.unwrap_or(note as f32);
        }
        if !self.active {
            self.filters = [Svf::default(); 2];
        }
        self.note = note;
        self.vel = velocity;
        self.region = (a as f64, b as f64);
        self.reverse = layout.reverse;
        self.pos = if self.reverse { b as f64 - 1.0 } else { a as f64 };
        self.oneshot = layout.mode != Mode::Classic;
        self.keytrack = layout.mode != Mode::Slice;
        self.killing = false;
        self.kill_gain = 1.0;
        self.active = true;
        self.env.trigger();
    }

    fn release(&mut self) {
        if !self.oneshot {
            self.env.release();
        }
    }

    fn kill(&mut self) {
        self.killing = true;
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn render(&mut self, s: &SamplerShared, ctl: &Controls, l: &mut [f32], r: &mut [f32]) {
        let (Some(src), Some(layout)) = (&s.source, &s.layout) else {
            self.active = false;
            return;
        };
        let n = l.len().min(MAX_BLOCK);
        self.pitch = glide(self.pitch, self.note as f32, ctl.glide_coef, n);
        let semis = if self.keytrack { self.pitch - s.root + ctl.pitch } else { ctl.pitch };
        let rate = (semitones_to_ratio(semis) * s.tune_ratio * src.sample_rate / s.sample_rate) as f64;
        let inc = if self.reverse { -rate } else { rate };

        // Loop points are read live so they can be moved while a note plays.
        let looping = if self.oneshot { None } else { layout.looping };
        let xf = layout.crossfade as f64;
        let amp = VOICE_GAIN * (1.0 - s.vel_amp * (1.0 - self.vel));
        let filter = (!s.filter_bypass).then(|| {
            let fc = s.cutoff * (s.vel_cutoff * 3.0 * (self.vel - 1.0)).exp2();
            SvfCoefs::new(s.filter_mode, fc, s.resonance, s.sample_rate)
        });
        let kill_coef = (-1.0 / (0.003 * s.sample_rate)).exp();
        let (ra, rb) = self.region;

        for i in 0..n {
            let mut edge = 1.0f32;
            if looping.is_none() {
                let remaining = if self.reverse { self.pos - ra } else { rb - self.pos };
                if remaining <= 0.0 {
                    self.active = false;
                    break;
                }
                edge = ((remaining / rate) as f32 / EDGE_FADE).min(1.0);
            }
            let (mut a, mut b) = src.read_stereo(self.pos);
            if let Some((ls, le)) = looping {
                let (ls, le) = (ls as f64, le as f64);
                let len = le - ls;
                if xf > 0.0 {
                    // Fade into the material on the far side of the loop so
                    // the wrap point is seamless.
                    let t = if self.reverse { (ls + xf - self.pos) / xf } else { (self.pos - (le - xf)) / xf };
                    if t > 0.0 {
                        let t = t.min(1.0) as f32;
                        let other = if self.reverse { self.pos + len } else { self.pos - len };
                        let (oa, ob) = src.read_stereo(other);
                        a = a * (1.0 - t) + oa * t;
                        b = b * (1.0 - t) + ob * t;
                    }
                }
                self.pos += inc;
                if !self.reverse && self.pos >= le {
                    self.pos -= len;
                } else if self.reverse && self.pos < ls {
                    self.pos += len;
                }
            } else {
                self.pos += inc;
            }
            let mut g = self.env.next(&s.env) * amp * edge;
            if self.killing {
                self.kill_gain *= kill_coef;
                g *= self.kill_gain;
            }
            if let Some(c) = &filter {
                a = self.filters[0].process(c, a);
                b = self.filters[1].process(c, b);
            }
            l[i] += a * g;
            r[i] += b * g;
        }
        if self.env.is_idle() || (self.killing && self.kill_gain < 1e-4) {
            self.active = false;
        }
    }
}

pub struct SamplerSynth {
    pub poly: Poly<SamplerVoice>,
    pub shared: SamplerShared,
    params: Vec<f32>,
    builtins: Builtins,
    file: Option<Arc<Sample>>,
    source_index: usize,
    sample_rate: f32,
}

impl SamplerSynth {
    pub fn new(sample_rate: f32, builtins: Builtins) -> Self {
        let defaults = super::SynthKind::Sampler.defaults();
        let mut synth = SamplerSynth {
            poly: Poly::new(|_| SamplerVoice::new()),
            shared: SamplerShared {
                sample_rate,
                source: None,
                layout: None,
                root: 60.0,
                tune_ratio: 1.0,
                env: AdsrParams::default(),
                vel_amp: 0.7,
                filter_mode: FilterMode::LowPass,
                cutoff: 20_000.0,
                resonance: 0.0,
                vel_cutoff: 0.0,
                filter_bypass: true,
            },
            params: defaults.clone(),
            builtins,
            file: None,
            source_index: usize::MAX,
            sample_rate,
        };
        synth.update(&defaults);
        synth
    }

    fn select_source(&mut self) {
        let next = if self.source_index == 0 {
            self.file.clone()
        } else {
            self.builtins.get(self.source_index - 1).cloned()
        };
        // Any Arc dropped here is still owned by `file`, `builtins` or the UI.
        self.shared.source = next;
        self.refresh_layout();
    }

    fn refresh_layout(&mut self) {
        let p = &self.params[COMMON.len()..];
        self.shared.layout = self.shared.source.as_deref().filter(|s| !s.is_empty()).map(|s| Layout::new(p, s));
    }

    pub fn set_file_sample(&mut self, sample: Option<Arc<Sample>>) -> Option<Arc<Sample>> {
        let old = std::mem::replace(&mut self.file, sample);
        self.select_source();
        old
    }

    pub fn update(&mut self, params: &[f32]) {
        // Keep our own copy for re-deriving the layout when the sample changes.
        self.params.copy_from_slice(params);
        let sr = self.sample_rate;
        let p = &params[COMMON.len()..];
        self.poly.update_common(params, p[VOICES], p[GLIDE], sr);
        let source_index = (p[SOURCE].round() as usize).min(granular::SOURCES.len() - 1);
        if source_index != self.source_index {
            self.source_index = source_index;
            self.select_source();
        } else {
            self.refresh_layout();
        }
        let s = &mut self.shared;
        s.root = p[ROOT].round();
        s.tune_ratio = (p[TUNE] / 1200.0).exp2();
        s.env = AdsrParams::new(p[ATTACK], p[DECAY], p[SUSTAIN], p[RELEASE], sr);
        s.vel_amp = p[VEL_AMP];
        s.filter_mode = match p[FILTER_TYPE].round() as usize {
            1 => FilterMode::BandPass,
            2 => FilterMode::HighPass,
            _ => FilterMode::LowPass,
        };
        s.cutoff = p[CUTOFF];
        s.resonance = p[RESONANCE];
        s.vel_cutoff = p[VEL_CUTOFF];
        s.filter_bypass = s.filter_mode == FilterMode::LowPass
            && p[CUTOFF] >= 19_000.0
            && p[RESONANCE] < 0.05
            && p[VEL_CUTOFF] < 0.01;
    }

    pub fn note_on(&mut self, note: u8, velocity: f32) {
        // Notes outside the slice range shouldn't steal a voice.
        if let Some(layout) = &self.shared.layout
            && layout.region_for(note).is_none()
        {
            return;
        }
        self.poly.note_on(note, velocity, &self.shared);
    }

    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        self.poly.render(&self.shared, l, r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::SynthKind;

    fn synth_with(overrides: &[(&str, f32)]) -> SamplerSynth {
        crate::dsp::init_tables();
        let mut s = SamplerSynth::new(48_000.0, crate::sample::builtins());
        let mut params = SynthKind::Sampler.defaults();
        for (k, v) in overrides {
            params[SynthKind::Sampler.index_of(k).unwrap()] = *v;
        }
        s.update(&params);
        s
    }

    fn render(s: &mut SamplerSynth, frames: usize) -> Vec<f32> {
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
    fn classic_loop_sustains_past_sample_end() {
        // The built-in sources are 4 s long; hold for 6 s.
        let mut s = synth_with(&[("loop", 1.0)]);
        s.note_on(60, 1.0);
        let out = render(&mut s, 48_000 * 6);
        let tail = &out[48_000 * 5..];
        assert!(tail.iter().any(|v| v.abs() > 0.01), "loop stopped");
        assert!(out.iter().all(|v| v.is_finite()));
        s.poly.note_off(60);
        render(&mut s, 48_000);
        assert_eq!(s.poly.active_voices(), 0);
    }

    #[test]
    fn oneshot_ignores_note_off_and_ends() {
        let mut s = synth_with(&[("mode", 1.0), ("end", 0.1)]);
        s.note_on(60, 1.0);
        s.poly.note_off(60);
        render(&mut s, 4_800);
        assert_eq!(s.poly.active_voices(), 1, "one-shot stopped at note-off");
        render(&mut s, 48_000);
        assert_eq!(s.poly.active_voices(), 0, "one-shot didn't end");
    }

    #[test]
    fn transient_slices_find_the_plucks() {
        // The built-in Pluck source has a pluck at every whole second.
        let s = synth_with(&[("source", 4.0), ("mode", 2.0), ("slice_by", 1.0), ("sensitivity", 0.8)]);
        let layout = s.shared.layout.unwrap();
        assert_eq!(layout.slices.count, 4, "{:?}", &layout.slices.points[..5]);
        for (i, p) in layout.slices.points[1..4].iter().enumerate() {
            let expected = 48_000 * (i + 1);
            assert!(p.abs_diff(expected) < 600, "slice {i} at {p}, expected ~{expected}");
        }
    }

    #[test]
    fn slice_notes_map_from_base_note() {
        let mut s = synth_with(&[("mode", 2.0), ("slices", 4.0)]);
        s.note_on(35, 1.0); // below base note: ignored
        s.note_on(40, 1.0); // beyond slice 4: ignored
        assert_eq!(s.poly.active_voices(), 0);
        s.note_on(37, 1.0);
        let out = render(&mut s, 4_800);
        assert_eq!(s.poly.active_voices(), 1);
        assert!(out.iter().any(|v| v.abs() > 0.01));
    }

    #[test]
    fn panic_stops_oneshots() {
        let mut s = synth_with(&[("mode", 1.0)]);
        s.note_on(60, 1.0);
        render(&mut s, 480);
        s.poly.all_notes_off();
        render(&mut s, 4_800);
        assert_eq!(s.poly.active_voices(), 0);
    }
}
