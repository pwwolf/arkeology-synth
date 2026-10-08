//! The real-time engine: owns up to 16 synth slots, routes MIDI to them by
//! channel, mixes them through a reverb send into the master bus.
//!
//! The engine runs on the audio thread. Everything it needs arrives through a
//! lock-free command queue; anything it would have to free is pushed back to
//! the UI thread through the garbage queue so the callback never deallocates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use crossbeam_queue::ArrayQueue;

use crate::dsp::{Smooth, pan_gains};
use crate::fx::{self, FX_UNITS, FxUnit};
use crate::midi::{MidiKind, MidiMsg};
use crate::params::{Kind, ParamDesc, Scale, master};
use crate::recorder::RecordTap;
use crate::reverb::Reverb;
use crate::sample::{Builtins, Sample};
use crate::synth::{self, Instrument, MAX_BLOCK, SynthKind};

pub const MAX_SLOTS: usize = 16;
/// Timestamped MIDI events one buffer can hold; beyond this they apply at
/// the buffer's start.
const MAX_PENDING_MIDI: usize = 1024;

/// Gain staging: volume parameters map through a square law times these.
const SLOT_GAIN: f32 = 1.5;
const MASTER_GAIN: f32 = 1.5;
/// Time constant for continuous parameter changes (see `ParamGlide`).
const GLIDE_TIME: f32 = 0.02;

pub enum Command {
    Midi(MidiMsg),
    /// MIDI stamped with its arrival time on the MIDI thread. The engine
    /// places it at the matching sample of the next buffer, so timing is
    /// exact (at a constant one-buffer delay) instead of snapping to the
    /// start of whichever buffer it lands in.
    MidiAt(MidiMsg, Instant),
    NoteOn {
        slot: usize,
        note: u8,
        velocity: f32,
    },
    NoteOff {
        slot: usize,
        note: u8,
    },
    InstallSlot {
        slot: usize,
        data: Box<Slot>,
    },
    RemoveSlot {
        slot: usize,
    },
    SetParam {
        slot: usize,
        index: usize,
        value: f32,
    },
    SetMaster {
        index: usize,
        value: f32,
    },
    SetChannel {
        slot: usize,
        channel: Option<u8>,
    },
    SetMute {
        slot: usize,
        on: bool,
    },
    SetSolo {
        slot: usize,
        on: bool,
    },
    /// Replace sample `index` (a kit pad; 0 for granular/sampler).
    SetSample {
        slot: usize,
        index: usize,
        sample: Option<Arc<Sample>>,
    },
    /// Swap in a freshly built insert effect (built off the audio thread).
    SetFx {
        slot: usize,
        unit: usize,
        fx: Box<FxUnit>,
    },
    SetMasterFx {
        unit: usize,
        fx: Box<FxUnit>,
    },
    /// Start copying the master output to a recorder (replacing any other).
    StartRecording(Box<RecordTap>),
    StopRecording,
    Panic,
}

/// Values handed back to the UI thread purely so they are dropped there.
#[allow(dead_code)]
pub enum Garbage {
    Slot(Box<Slot>),
    Sample(Arc<Sample>),
    Fx(Box<FxUnit>),
    Recorder(Box<RecordTap>),
}

pub type CommandQueue = Arc<ArrayQueue<Command>>;
pub type GarbageQueue = Arc<ArrayQueue<Garbage>>;

/// Glides continuous parameters towards new values, so stepped control
/// changes (7-bit MIDI CCs, key presses, MCP) don't zipper. Switches,
/// choices and integers jump. Built on the UI thread; `set` and `step` never
/// allocate (`moving` has room for every parameter).
pub struct ParamGlide {
    targets: Vec<f32>,
    modes: Vec<GlideMode>,
    moving: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum GlideMode {
    Jump,
    /// Snap when within this distance of the target.
    Linear(f32),
    /// Logarithmic parameters (cutoffs, times) glide by ratio.
    Exp,
}

impl ParamGlide {
    pub fn new<'a>(descs: impl Iterator<Item = &'a ParamDesc>, values: &[f32]) -> Self {
        let modes: Vec<GlideMode> = descs
            .map(|d| match (d.kind, d.scale) {
                (Kind::Float, Scale::Exp) if d.min > 0.0 => GlideMode::Exp,
                (Kind::Float, _) => GlideMode::Linear((d.max - d.min) * 1e-4),
                _ => GlideMode::Jump,
            })
            .collect();
        ParamGlide {
            targets: values.to_vec(),
            moving: Vec::with_capacity(modes.len()),
            modes,
        }
    }

    /// Set a new target. Returns true if `values` changed right away (a jump).
    pub fn set(&mut self, values: &mut [f32], i: usize, v: f32) -> bool {
        let (Some(t), Some(mode)) = (self.targets.get_mut(i), self.modes.get(i)) else {
            return false;
        };
        *t = v;
        if *mode == GlideMode::Jump {
            values[i] = v;
            self.moving.retain(|&m| m != i);
            true
        } else {
            if !self.moving.contains(&i) {
                self.moving.push(i);
            }
            false
        }
    }

    /// Move gliding values `k` (0..=1) of the way to their targets. Returns
    /// true if anything changed.
    pub fn step(&mut self, values: &mut [f32], k: f32) -> bool {
        if self.moving.is_empty() {
            return false;
        }
        let (targets, modes) = (&self.targets, &self.modes);
        self.moving.retain(|&i| {
            let (v, t) = (&mut values[i], targets[i]);
            match modes[i] {
                GlideMode::Exp if *v > 0.0 && t > 0.0 => {
                    let ratio = t / *v;
                    if (ratio - 1.0).abs() < 1e-4 {
                        *v = t;
                        return false;
                    }
                    *v *= ratio.powf(k);
                    true
                }
                GlideMode::Linear(eps) if (t - *v).abs() > eps => {
                    *v += (t - *v) * k;
                    true
                }
                _ => {
                    *v = t;
                    false
                }
            }
        });
        true
    }
}

/// Fraction of the remaining distance a glide covers in `frames`.
fn glide_step(frames: usize, sample_rate: f32) -> f32 {
    1.0 - (-(frames as f32) / (GLIDE_TIME * sample_rate)).exp()
}

pub struct Slot {
    params: Vec<f32>,
    glide: ParamGlide,
    dirty: bool,
    channel: Option<u8>,
    mute: bool,
    solo: bool,
    inst: Instrument,
    gain: Smooth,
    pan: Smooth,
    send: Smooth,
    fx_base: usize,
    fx: [Box<FxUnit>; FX_UNITS],
}

impl Slot {
    pub fn new(
        kind: SynthKind,
        params: Vec<f32>,
        channel: Option<u8>,
        sample_rate: f32,
        builtins: &Builtins,
        samples: &[Option<Arc<Sample>>],
    ) -> Box<Slot> {
        let mut inst = Instrument::new(kind, sample_rate, builtins);
        for (i, s) in samples.iter().enumerate().filter(|(_, s)| s.is_some()) {
            // The returned (previous) sample is None for a fresh instrument.
            let _ = inst.set_sample(i, s.clone());
        }
        inst.update(&params);
        let ctrl_rate = sample_rate / MAX_BLOCK as f32;
        let fx_base = kind.fx_base();
        let fx = std::array::from_fn(|u| {
            FxUnit::from_values(fx::unit_values(&params, fx_base, u), sample_rate)
        });
        Box::new(Slot {
            fx_base,
            fx,
            glide: ParamGlide::new(kind.params(), &params),
            dirty: false,
            channel,
            mute: false,
            solo: false,
            inst,
            gain: Smooth::new(0.0, 0.02, sample_rate),
            pan: Smooth::new(params[synth::PAN], 0.03, ctrl_rate),
            send: Smooth::new(params[synth::REVERB_SEND], 0.03, ctrl_rate),
            params,
        })
    }

    pub fn with_flags(mut self: Box<Self>, mute: bool, solo: bool) -> Box<Self> {
        self.mute = mute;
        self.solo = solo;
        self
    }

    fn listens_to(&self, channel: u8) -> bool {
        self.channel.is_none_or(|c| c == channel)
    }
}

#[derive(Default)]
pub struct SlotTelemetry {
    pub peak: AtomicU32,
    pub voices: AtomicU32,
    pub notes: AtomicU32,
}

#[derive(Default)]
pub struct Telemetry {
    pub slots: [SlotTelemetry; MAX_SLOTS],
    pub peak_l: AtomicU32,
    pub peak_r: AtomicU32,
    pub cpu: AtomicU32,
    /// The latest master output, for the spectrum analyzer.
    pub scope: crate::spectrum::Scope,
}

/// Peak-hold store: for non-negative floats the bit pattern orders like the value.
#[inline]
fn store_max(a: &AtomicU32, v: f32) {
    a.fetch_max(v.max(0.0).to_bits(), Ordering::Relaxed);
}

/// Read and reset a peak written with `store_max`.
pub fn take_peak(a: &AtomicU32) -> f32 {
    f32::from_bits(a.swap(0, Ordering::Relaxed))
}

pub struct Engine {
    sample_rate: f32,
    slots: [Option<Box<Slot>>; MAX_SLOTS],
    commands: CommandQueue,
    garbage: GarbageQueue,
    telemetry: Arc<Telemetry>,
    master: [f32; master::PARAMS.len()],
    master_fx: [Box<FxUnit>; FX_UNITS],
    master_glide: ParamGlide,
    /// Master FX or reverb settings changed.
    master_dirty: bool,
    recorder: Option<Box<RecordTap>>,
    master_gain: Smooth,
    reverb: Reverb,
    buf_l: [f32; MAX_BLOCK],
    buf_r: [f32; MAX_BLOCK],
    mix_l: [f32; MAX_BLOCK],
    mix_r: [f32; MAX_BLOCK],
    send_l: [f32; MAX_BLOCK],
    send_r: [f32; MAX_BLOCK],
    cpu_avg: f32,
    /// Timestamped MIDI for the current buffer: (sample offset, message).
    /// Preallocated; never grows on the audio thread.
    pending: Vec<(usize, MidiMsg)>,
    last_callback: Option<Instant>,
}

impl Engine {
    pub fn new(
        sample_rate: f32,
        commands: CommandQueue,
        garbage: GarbageQueue,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        let mut master_vals = [0.0; master::PARAMS.len()];
        for (v, p) in master_vals.iter_mut().zip(master::PARAMS.iter()) {
            *v = p.default;
        }
        let mut e = Engine {
            sample_rate,
            slots: Default::default(),
            commands,
            garbage,
            telemetry,
            master: master_vals,
            master_gain: Smooth::new(0.0, 0.05, sample_rate),
            reverb: Reverb::new(sample_rate),
            buf_l: [0.0; MAX_BLOCK],
            buf_r: [0.0; MAX_BLOCK],
            mix_l: [0.0; MAX_BLOCK],
            mix_r: [0.0; MAX_BLOCK],
            send_l: [0.0; MAX_BLOCK],
            send_r: [0.0; MAX_BLOCK],
            cpu_avg: 0.0,
            pending: Vec::with_capacity(MAX_PENDING_MIDI),
            last_callback: None,
            master_fx: std::array::from_fn(|_| FxUnit::off(sample_rate)),
            master_glide: ParamGlide::new(master::PARAMS.iter(), &master_vals),
            master_dirty: false,
            recorder: None,
        };
        e.apply_reverb_params();
        e
    }

    fn discard(&self, g: Garbage) {
        // If the queue is somehow full we drop in place rather than block.
        let _ = self.garbage.push(g);
    }

    fn apply_reverb_params(&mut self) {
        self.reverb.set(
            self.master[master::REVERB_TYPE].round().max(0.0) as usize,
            self.master[master::REVERB_SIZE],
            self.master[master::REVERB_DAMP],
            self.master[master::REVERB_WIDTH],
        );
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Midi(m) | Command::MidiAt(m, _) => self.handle_midi(m),
            Command::NoteOn {
                slot,
                note,
                velocity,
            } => {
                if let Some(s) = self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    s.inst.note_on(note, velocity);
                    self.telemetry.slots[slot]
                        .notes
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Command::NoteOff { slot, note } => {
                if let Some(s) = self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    s.inst.note_off(note);
                }
            }
            Command::InstallSlot { slot, data } => {
                if slot < MAX_SLOTS {
                    if let Some(old) = self.slots[slot].replace(data) {
                        self.discard(Garbage::Slot(old));
                    }
                } else {
                    self.discard(Garbage::Slot(data));
                }
            }
            Command::RemoveSlot { slot } => {
                if let Some(old) = self.slots.get_mut(slot).and_then(|s| s.take()) {
                    self.discard(Garbage::Slot(old));
                }
            }
            Command::SetParam { slot, index, value } => {
                if let Some(s) = self.slots.get_mut(slot).and_then(|s| s.as_mut())
                    && index < s.params.len()
                    && s.glide.set(&mut s.params, index, value)
                {
                    s.dirty = true;
                }
            }
            Command::SetMaster { index, value } => {
                if index < self.master.len()
                    && self.master_glide.set(&mut self.master, index, value)
                {
                    self.master_dirty = true;
                }
            }
            Command::SetFx { slot, unit, fx } => {
                let old = match self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    Some(s) if unit < FX_UNITS => {
                        s.dirty = true;
                        std::mem::replace(&mut s.fx[unit], fx)
                    }
                    _ => fx,
                };
                self.discard(Garbage::Fx(old));
            }
            Command::SetMasterFx { unit, fx } => {
                if unit < FX_UNITS {
                    self.master_dirty = true;
                    let old = std::mem::replace(&mut self.master_fx[unit], fx);
                    self.discard(Garbage::Fx(old));
                } else {
                    self.discard(Garbage::Fx(fx));
                }
            }
            Command::SetChannel { slot, channel } => {
                if let Some(s) = self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    s.inst.all_notes_off();
                    s.channel = channel;
                }
            }
            Command::SetMute { slot, on } => {
                if let Some(s) = self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    s.mute = on;
                }
            }
            Command::SetSolo { slot, on } => {
                if let Some(s) = self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    s.solo = on;
                }
            }
            Command::SetSample {
                slot,
                index,
                sample,
            } => {
                let old = match self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    Some(s) => s.inst.set_sample(index, sample),
                    None => sample,
                };
                if let Some(old) = old {
                    self.discard(Garbage::Sample(old));
                }
            }
            Command::StartRecording(tap) => {
                if let Some(old) = self.recorder.replace(tap) {
                    old.finish();
                    self.discard(Garbage::Recorder(old));
                }
            }
            Command::StopRecording => {
                if let Some(old) = self.recorder.take() {
                    old.finish();
                    self.discard(Garbage::Recorder(old));
                }
            }
            Command::Panic => {
                for s in self.slots.iter_mut().flatten() {
                    s.inst.all_notes_off();
                }
            }
        }
    }

    fn handle_midi(&mut self, m: MidiMsg) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let Some(s) = slot.as_mut() else { continue };
            if !s.listens_to(m.channel) {
                continue;
            }
            match m.kind {
                MidiKind::NoteOn { note, velocity } => {
                    s.inst.note_on(note, velocity as f32 / 127.0);
                    self.telemetry.slots[i]
                        .notes
                        .fetch_add(1, Ordering::Relaxed);
                }
                MidiKind::NoteOff { note } => s.inst.note_off(note),
                MidiKind::Cc { cc: 1, value } => s.inst.set_modwheel(value as f32 / 127.0),
                MidiKind::Cc { cc: 64, value } => s.inst.set_sustain(value >= 64),
                MidiKind::Cc { cc: 120 | 123, .. } => s.inst.all_notes_off(),
                MidiKind::PitchBend(v) => s.inst.set_bend(v),
                _ => {}
            }
        }
    }

    /// Render interleaved output. Called from the audio callback.
    pub fn process(&mut self, out: &mut [f32], channels: usize) {
        self.process_at(out, channels, Instant::now());
    }

    /// `process` with the callback time passed in (so tests control the clock).
    pub fn process_at(&mut self, out: &mut [f32], channels: usize, now: Instant) {
        let started = Instant::now();
        let channels = channels.max(1);
        let frames = out.len() / channels;

        // Timestamped MIDI that arrived since the previous callback lands at
        // the same distance into this buffer; everything else applies now.
        let prev = self.last_callback.replace(now);
        self.pending.clear();
        while let Some(cmd) = self.commands.pop() {
            match cmd {
                Command::MidiAt(m, at) if self.pending.len() < self.pending.capacity() => {
                    let offset = match prev {
                        Some(p) if at > p => {
                            let samples = (at - p).as_secs_f64() * self.sample_rate as f64;
                            (samples as usize).min(frames.saturating_sub(1))
                        }
                        _ => 0,
                    };
                    // Arrival order is time order: keep offsets non-decreasing.
                    let offset = offset.max(self.pending.last().map_or(0, |e| e.0));
                    self.pending.push((offset, m));
                }
                other => self.handle(other),
            }
        }

        let mut done = 0;
        let mut next = 0;
        while done < frames {
            while let Some(&(offset, m)) = self.pending.get(next) {
                if offset > done {
                    break;
                }
                self.handle_midi(m);
                next += 1;
            }
            let until = self.pending.get(next).map_or(frames, |e| e.0).min(frames);
            let n = (until - done).clamp(1, MAX_BLOCK);
            self.render_block(n);
            for i in 0..n {
                let frame = &mut out[(done + i) * channels..(done + i + 1) * channels];
                if channels == 1 {
                    frame[0] = 0.5 * (self.mix_l[i] + self.mix_r[i]);
                } else {
                    frame[0] = self.mix_l[i];
                    frame[1] = self.mix_r[i];
                    frame[2..].fill(0.0);
                }
            }
            done += n;
        }
        // (Only reachable for an empty buffer.)
        while let Some(&(_, m)) = self.pending.get(next) {
            self.handle_midi(m);
            next += 1;
        }
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let voices = slot.as_mut().map_or(0, |s| s.inst.active_voices());
            self.telemetry.slots[i]
                .voices
                .store(voices as u32, Ordering::Relaxed);
        }
        if frames > 0 {
            let budget = frames as f32 / self.sample_rate;
            let load = started.elapsed().as_secs_f32() / budget;
            self.cpu_avg += (load - self.cpu_avg) * 0.1;
            self.telemetry
                .cpu
                .store(self.cpu_avg.to_bits(), Ordering::Relaxed);
        }
    }

    fn render_block(&mut self, n: usize) {
        self.mix_l[..n].fill(0.0);
        self.mix_r[..n].fill(0.0);
        self.send_l[..n].fill(0.0);
        self.send_r[..n].fill(0.0);
        let any_solo = self.slots.iter().flatten().any(|s| s.solo);
        let k = glide_step(n, self.sample_rate);

        for (idx, slot) in self.slots.iter_mut().enumerate() {
            let Some(s) = slot.as_mut() else { continue };
            if s.glide.step(&mut s.params, k) {
                s.dirty = true;
            }
            if s.dirty {
                s.inst.update(&s.params);
                for (u, unit) in s.fx.iter_mut().enumerate() {
                    unit.update(fx::unit_values(&s.params, s.fx_base, u));
                }
                s.dirty = false;
            }
            let (bl, br) = (&mut self.buf_l[..n], &mut self.buf_r[..n]);
            bl.fill(0.0);
            br.fill(0.0);
            s.inst.render(bl, br);
            // Insert effects, pre-fader.
            for unit in &mut s.fx {
                unit.process(bl, br);
            }

            let audible = !s.mute && (!any_solo || s.solo);
            let vol = s.params[synth::VOLUME];
            let target = if audible { vol * vol * SLOT_GAIN } else { 0.0 };
            let (pl, pr) = pan_gains(s.pan.next(s.params[synth::PAN]));
            // Compensated pan law: unity gain at centre.
            let (pl, pr) = (pl * std::f32::consts::SQRT_2, pr * std::f32::consts::SQRT_2);
            let send = s.send.next(s.params[synth::REVERB_SEND]);
            let mut peak = 0.0f32;
            for i in 0..n {
                let g = s.gain.next(target);
                let l = bl[i] * g * pl;
                let r = br[i] * g * pr;
                self.mix_l[i] += l;
                self.mix_r[i] += r;
                self.send_l[i] += l * send;
                self.send_r[i] += r * send;
                peak = peak.max(l.abs()).max(r.abs());
            }
            store_max(&self.telemetry.slots[idx].peak, peak);
        }

        self.reverb
            .process(&mut self.send_l[..n], &mut self.send_r[..n]);
        let ret = self.master[master::REVERB_RETURN];
        for i in 0..n {
            self.mix_l[i] += self.send_l[i] * ret;
            self.mix_r[i] += self.send_r[i] * ret;
        }
        // Master insert effects, after the reverb return and before the
        // master volume and clipper.
        if self.master_glide.step(&mut self.master, k) {
            self.master_dirty = true;
        }
        if self.master_dirty {
            for (u, unit) in self.master_fx.iter_mut().enumerate() {
                unit.update(fx::unit_values(&self.master, master::FX_BASE, u));
            }
            self.apply_reverb_params();
            self.master_dirty = false;
        }
        for unit in &mut self.master_fx {
            unit.process(&mut self.mix_l[..n], &mut self.mix_r[..n]);
        }
        let vol = self.master[master::VOLUME];
        let drive = self.master[master::DRIVE];
        let pre = 1.0 + drive * 8.0;
        let post = 1.0 / (1.0 + drive * 2.5);
        let (mut peak_l, mut peak_r) = (0.0f32, 0.0f32);
        for i in 0..n {
            let g = self.master_gain.next(vol * vol * MASTER_GAIN);
            let l = soft_clip(self.mix_l[i] * g * pre) * post;
            let r = soft_clip(self.mix_r[i] * g * pre) * post;
            self.mix_l[i] = l;
            self.mix_r[i] = r;
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());
        }
        store_max(&self.telemetry.peak_l, peak_l);
        store_max(&self.telemetry.peak_r, peak_r);
        self.telemetry
            .scope
            .write(&self.mix_l[..n], &self.mix_r[..n]);
        if let Some(rec) = &self.recorder {
            rec.write(&self.mix_l[..n], &self.mix_r[..n]);
        }
    }
}

/// Transparent below 0.8, then smoothly saturates towards 1.0.
#[inline]
fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.8 {
        x
    } else {
        let y = 0.8 + 0.2 * ((a - 0.8) / 0.2).tanh();
        y.copysign(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample;

    fn rms(buf: &[f32]) -> f32 {
        (buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32).sqrt()
    }

    fn render_note(kind: SynthKind) -> Vec<f32> {
        render_note_at(kind, 60)
    }

    fn render_note_at(kind: SynthKind, note: u8) -> Vec<f32> {
        crate::dsp::init_tables();
        let sr = 48_000.0;
        let builtins = sample::builtins();
        let cmds: CommandQueue = Arc::new(ArrayQueue::new(64));
        let garbage: GarbageQueue = Arc::new(ArrayQueue::new(64));
        let mut e = Engine::new(sr, cmds.clone(), garbage, Arc::new(Telemetry::default()));
        let slot = Slot::new(kind, kind.defaults(), Some(0), sr, &builtins, &[]);
        cmds.push(Command::InstallSlot {
            slot: 0,
            data: slot,
        })
        .ok();
        cmds.push(Command::Midi(MidiMsg {
            channel: 0,
            kind: MidiKind::NoteOn {
                note,
                velocity: 100,
            },
        }))
        .ok();
        // Notes on other channels must not reach the slot.
        cmds.push(Command::Midi(MidiMsg {
            channel: 5,
            kind: MidiKind::NoteOn {
                note: 72,
                velocity: 100,
            },
        }))
        .ok();
        let mut out = vec![0.0; 2 * 48_000];
        e.process(&mut out, 2);
        out
    }

    #[test]
    fn fm_makes_sound() {
        let out = render_note(SynthKind::Fm);
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(rms(&out) > 0.01, "rms {}", rms(&out));
        assert!(out.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn granular_makes_sound() {
        let out = render_note(SynthKind::Granular);
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(rms(&out) > 0.01, "rms {}", rms(&out));
    }

    #[test]
    fn acid_makes_sound() {
        let out = render_note(SynthKind::Acid);
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(rms(&out) > 0.01, "rms {}", rms(&out));
        assert!(out.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn drums_make_sound() {
        let out = render_note_at(SynthKind::Drums, 36);
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(rms(&out) > 0.01, "rms {}", rms(&out));
        assert!(out.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn insert_delay_rings_on_after_the_synth_stops() {
        crate::dsp::init_tables();
        let sr = 48_000.0;
        let builtins = sample::builtins();
        let cmds: CommandQueue = Arc::new(ArrayQueue::new(64));
        let garbage: GarbageQueue = Arc::new(ArrayQueue::new(64));
        let mut e = Engine::new(sr, cmds.clone(), garbage, Arc::new(Telemetry::default()));
        let kind = SynthKind::Drums;
        let mut values = kind.defaults();
        let set = |v: &mut Vec<f32>, k: &str, x: f32| v[kind.index_of(k).unwrap()] = x;
        set(&mut values, "reverb_send", 0.0);
        set(&mut values, "fx1_type", 1.0); // Delay
        set(&mut values, "fx1_mix", 0.5);
        set(&mut values, "fx1_delay_time", 0.5);
        set(&mut values, "fx1_delay_feedback", 0.6);
        let slot = Slot::new(kind, values, Some(9), sr, &builtins, &[]);
        cmds.push(Command::InstallSlot {
            slot: 0,
            data: slot,
        })
        .ok();
        cmds.push(Command::NoteOn {
            slot: 0,
            note: 37,
            velocity: 1.0,
        })
        .ok(); // short rimshot
        let mut out = vec![0.0; 2 * 48_000 * 2];
        e.process(&mut out, 2);
        // The rim is ~40 ms long; echoes should still be audible a second later.
        let window = |s: f32| &out[(s * 96_000.0) as usize..((s + 0.1) * 96_000.0) as usize];
        assert!(
            rms(window(1.0)) > 0.005,
            "no echo tail: {}",
            rms(window(1.0))
        );
        assert!(rms(window(0.2)) < 1e-4, "echo arrived early");
    }

    #[test]
    fn continuous_params_glide_and_switches_jump() {
        let kind = SynthKind::Analog;
        let mut values = kind.defaults();
        let mut g = ParamGlide::new(kind.params(), &values);
        let cutoff = kind.index_of("cutoff").unwrap();
        let level = kind.index_of("resonance").unwrap();
        assert!(kind.param(level).default > 0.0);
        let unison = kind.index_of("unison").unwrap();
        let capacity = g.moving.capacity();
        assert_eq!(g.modes[cutoff], GlideMode::Exp);

        values[cutoff] = 200.0;
        g.targets[cutoff] = 200.0;
        assert!(!g.set(&mut values, cutoff, 3_200.0), "cutoff should glide");
        assert!(!g.set(&mut values, level, 0.0), "levels should glide");
        assert!(g.set(&mut values, unison, 5.0), "integers jump");
        assert_eq!(values[unison], 5.0);

        // Log-scale glides pass the geometric midpoint (800 Hz), not the linear one.
        let k = glide_step(MAX_BLOCK, 48_000.0);
        let mut crossed = false;
        while g.step(&mut values, k) {
            if !crossed && values[cutoff] >= 800.0 {
                crossed = true;
                assert!(
                    values[level] < kind.param(level).default * 0.6,
                    "both glide together"
                );
            }
        }
        assert_eq!(
            (values[cutoff], values[level]),
            (3_200.0, 0.0),
            "arrives exactly"
        );
        assert!(crossed);
        assert_eq!(g.moving.capacity(), capacity, "no allocation");
    }

    /// A CC sweep through the engine: the cutoff lags the stepped target
    /// briefly, settles within ~0.2 s, and the master drive glides too.
    #[test]
    fn engine_glides_set_param() {
        let sr = 48_000.0;
        let cmds: CommandQueue = Arc::new(ArrayQueue::new(256));
        let mut e = Engine::new(
            sr,
            cmds.clone(),
            Arc::new(ArrayQueue::new(64)),
            Arc::new(Telemetry::default()),
        );
        let kind = SynthKind::Analog;
        let slot = Slot::new(kind, kind.defaults(), Some(0), sr, &sample::builtins(), &[]);
        cmds.push(Command::InstallSlot {
            slot: 0,
            data: slot,
        })
        .ok();
        let cutoff = kind.index_of("cutoff").unwrap();
        let mut out = vec![0.0; 2 * MAX_BLOCK];
        e.process(&mut out, 2);
        let start = e.slots[0].as_ref().unwrap().params[cutoff];

        cmds.push(Command::SetParam {
            slot: 0,
            index: cutoff,
            value: start * 4.0,
        })
        .ok();
        cmds.push(Command::SetMaster {
            index: master::DRIVE,
            value: 1.0,
        })
        .ok();
        e.process(&mut out, 2);
        let now = e.slots[0].as_ref().unwrap().params[cutoff];
        assert!(now > start && now < start * 1.5, "one block in: {now}");
        assert!(e.master[master::DRIVE] > 0.0 && e.master[master::DRIVE] < 0.2);

        let mut out = vec![0.0; 2 * 9_600];
        e.process(&mut out, 2);
        assert_eq!(e.slots[0].as_ref().unwrap().params[cutoff], start * 4.0);
        assert_eq!(e.master[master::DRIVE], 1.0);
    }

    /// Held notes on the acoustic guitar patches keep ringing: a gentle
    /// first-second drop and a clearly audible tail at four seconds.
    #[test]
    fn acoustic_guitars_sustain() {
        crate::dsp::init_tables();
        let sr = 48_000.0;
        let builtins = sample::builtins();
        for name in ["Steel Guitar", "Nylon Guitar", "Parlor Guitar"] {
            let p = crate::patch::factory_patches()
                .into_iter()
                .find(|p| p.name == name)
                .unwrap();
            let cmds: CommandQueue = Arc::new(ArrayQueue::new(256));
            let mut e = Engine::new(
                sr,
                cmds.clone(),
                Arc::new(ArrayQueue::new(64)),
                Arc::new(Telemetry::default()),
            );
            let slot = Slot::new(p.kind, p.values(), Some(0), sr, &builtins, &[]);
            cmds.push(Command::InstallSlot {
                slot: 0,
                data: slot,
            })
            .ok();
            cmds.push(Command::NoteOn {
                slot: 0,
                note: 52,
                velocity: 0.8,
            })
            .ok();
            let mut out = vec![0.0; 2 * 48_000 * 5];
            e.process(&mut out, 2);
            let db = |t: f32| {
                let i = (t * sr) as usize * 2;
                let w = &out[i..i + 9_600];
                20.0 * ((w.iter().map(|v| v * v).sum::<f32>() / w.len() as f32).sqrt() + 1e-9)
                    .log10()
            };
            let start = db(0.05);
            assert!(
                db(1.0) - start > -10.0,
                "{name}: {:.1} dB after 1 s",
                db(1.0) - start
            );
            assert!(
                db(4.0) - start > -30.0,
                "{name}: {:.1} dB after 4 s",
                db(4.0) - start
            );
        }
    }

    /// Timestamped MIDI starts at its exact sample in the next buffer.
    #[test]
    fn timestamped_midi_is_sample_accurate() {
        use std::time::Duration;
        crate::dsp::init_tables();
        let sr = 48_000.0;
        let ms = |x: f64| Duration::from_secs_f64(x / 1000.0);
        // First sample index where the output (left channel) becomes audible.
        let onsets = |out: &[f32]| -> Vec<usize> {
            let mut found = Vec::new();
            let mut quiet = 0;
            for (i, v) in out.chunks(2).map(|f| f[0].abs()).enumerate() {
                if v > 1e-3 {
                    if quiet > 24 || found.is_empty() && quiet == i {
                        found.push(i);
                    }
                    quiet = 0;
                } else {
                    quiet += 1;
                }
            }
            found
        };
        let engine = || {
            let cmds: CommandQueue = Arc::new(ArrayQueue::new(256));
            let e = Engine::new(
                sr,
                cmds.clone(),
                Arc::new(ArrayQueue::new(64)),
                Arc::new(Telemetry::default()),
            );
            let kind = SynthKind::Drums;
            let mut v = kind.defaults();
            v[synth::REVERB_SEND] = 0.0;
            let slot = Slot::new(kind, v, None, sr, &sample::builtins(), &[]);
            cmds.push(Command::InstallSlot {
                slot: 0,
                data: slot,
            })
            .ok();
            (e, cmds)
        };
        let hat = |at| {
            Command::MidiAt(
                MidiMsg {
                    channel: 9,
                    kind: MidiKind::NoteOn {
                        note: 42,
                        velocity: 120,
                    },
                },
                at,
            )
        };

        // A note 4 ms into the interval starts 192 samples into the buffer.
        let (mut e, cmds) = engine();
        let t0 = Instant::now();
        let mut out = vec![0.0; 2 * 1024];
        e.process_at(&mut out, 2, t0);
        cmds.push(hat(t0 + ms(4.0))).ok();
        e.process_at(&mut out, 2, t0 + ms(21.33));
        assert_eq!(onsets(&out).first(), Some(&192), "{:?}", onsets(&out));

        // Two hits 1.5 ms apart stay exactly 72 samples apart.
        let (mut e, cmds) = engine();
        e.process_at(&mut out, 2, t0);
        cmds.push(Command::MidiAt(
            MidiMsg {
                channel: 9,
                kind: MidiKind::NoteOn {
                    note: 36,
                    velocity: 120,
                },
            },
            t0 + ms(2.0),
        ))
        .ok();
        cmds.push(Command::MidiAt(
            MidiMsg {
                channel: 9,
                kind: MidiKind::NoteOn {
                    note: 39,
                    velocity: 120,
                },
            },
            t0 + ms(3.5),
        ))
        .ok();
        e.process_at(&mut out, 2, t0 + ms(21.33));
        let first = onsets(&out)[0];
        assert_eq!(first, 96);
        let mut alone = vec![0.0; 2 * 1024];
        let (mut e2, cmds2) = engine();
        e2.process_at(&mut alone, 2, t0);
        cmds2
            .push(Command::MidiAt(
                MidiMsg {
                    channel: 9,
                    kind: MidiKind::NoteOn {
                        note: 36,
                        velocity: 120,
                    },
                },
                t0 + ms(2.0),
            ))
            .ok();
        e2.process_at(&mut alone, 2, t0 + ms(21.33));
        let diverge = out
            .chunks(2)
            .zip(alone.chunks(2))
            .position(|(a, b)| (a[0] - b[0]).abs() > 1e-6);
        assert_eq!(
            diverge,
            Some(96 + 72),
            "the second hit starts 72 samples after the first"
        );

        // Late (before the previous callback) and unstamped MIDI apply at once.
        let (mut e, cmds) = engine();
        e.process_at(&mut out, 2, t0 + ms(10.0));
        cmds.push(hat(t0)).ok();
        e.process_at(&mut out, 2, t0 + ms(31.33));
        assert_eq!(onsets(&out).first(), Some(&0));
        let (mut e, cmds) = engine();
        e.process_at(&mut out, 2, t0);
        cmds.push(Command::Midi(MidiMsg {
            channel: 9,
            kind: MidiKind::NoteOn {
                note: 42,
                velocity: 120,
            },
        }))
        .ok();
        e.process_at(&mut out, 2, t0 + ms(21.33));
        assert_eq!(onsets(&out).first(), Some(&0));
    }

    /// Playing softly is clearly quieter on velocity-sensitive patches (the
    /// velocity curve reaches about -12 dB at 64 and -24 dB at 32 at full
    /// sensitivity), while the organ ignores velocity as a real one does.
    #[test]
    fn velocity_is_audible() {
        crate::dsp::init_tables();
        let sr = 48_000.0;
        let builtins = sample::builtins();
        let soft_minus_hard = |name: &str| {
            let p = crate::patch::factory_patches()
                .into_iter()
                .find(|p| p.name == name)
                .unwrap();
            let note = if p.kind == SynthKind::Drums { 38 } else { 60 };
            let level = |velocity: u8| {
                let cmds: CommandQueue = Arc::new(ArrayQueue::new(256));
                let mut e = Engine::new(
                    sr,
                    cmds.clone(),
                    Arc::new(ArrayQueue::new(64)),
                    Arc::new(Telemetry::default()),
                );
                let mut v = p.values();
                v[synth::REVERB_SEND] = 0.0;
                let slot = Slot::new(p.kind, v, Some(0), sr, &builtins, &[]);
                cmds.push(Command::InstallSlot {
                    slot: 0,
                    data: slot,
                })
                .ok();
                cmds.push(Command::Midi(MidiMsg {
                    channel: 0,
                    kind: MidiKind::NoteOn { note, velocity },
                }))
                .ok();
                let mut out = vec![0.0; 2 * 48_000];
                e.process(&mut out, 2);
                20.0 * ((out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt() + 1e-9)
                    .log10()
            };
            level(32) - level(127)
        };
        for name in [
            "E.Piano",
            "Analog Init",
            "909 Kit",
            "Sampler Init",
            "Choir Aah",
            "Grand Piano",
            "Choir Cloud",
        ] {
            let db = soft_minus_hard(name);
            assert!(db < -11.0, "{name}: velocity 32 only {db:.1} dB below 127");
        }
        assert!(
            soft_minus_hard("Jazz Organ").abs() < 0.5,
            "organs ignore velocity"
        );
    }

    /// Factory patches for the melodic engines must neither vanish nor slam
    /// the master clipper when playing a chord.
    #[test]
    fn factory_patch_levels_are_balanced() {
        crate::dsp::init_tables();
        let sr = 48_000.0;
        let builtins = sample::builtins();
        let kinds = [
            SynthKind::Fm,
            SynthKind::Granular,
            SynthKind::Analog,
            SynthKind::Tonewheel,
            SynthKind::Vocal,
        ];
        for p in crate::patch::factory_patches()
            .into_iter()
            .filter(|p| kinds.contains(&p.kind))
        {
            let cmds: CommandQueue = Arc::new(ArrayQueue::new(256));
            let garbage: GarbageQueue = Arc::new(ArrayQueue::new(64));
            let mut e = Engine::new(sr, cmds.clone(), garbage, Arc::new(Telemetry::default()));
            let slot = Slot::new(p.kind, p.values(), Some(0), sr, &builtins, &[]);
            cmds.push(Command::InstallSlot {
                slot: 0,
                data: slot,
            })
            .ok();
            for n in [48u8, 52, 55, 60] {
                cmds.push(Command::NoteOn {
                    slot: 0,
                    note: n,
                    velocity: 0.8,
                })
                .ok();
            }
            let mut out = vec![0.0; 2 * 72_000];
            e.process(&mut out, 2);
            let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(out.iter().all(|v| v.is_finite()), "{}", p.name);
            assert!(
                (0.12..0.85).contains(&peak),
                "{}: chord peak {peak}",
                p.name
            );
        }
    }
}
