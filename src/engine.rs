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
use crate::midi::{MidiKind, MidiMsg};
use crate::params::master;
use crate::reverb::Reverb;
use crate::sample::{Builtins, Sample};
use crate::synth::{self, Instrument, MAX_BLOCK, SynthKind};

pub const MAX_SLOTS: usize = 16;

/// Gain staging: volume parameters map through a square law times these.
const SLOT_GAIN: f32 = 1.5;
const MASTER_GAIN: f32 = 1.5;

pub enum Command {
    Midi(MidiMsg),
    NoteOn { slot: usize, note: u8, velocity: f32 },
    NoteOff { slot: usize, note: u8 },
    InstallSlot { slot: usize, data: Box<Slot> },
    RemoveSlot { slot: usize },
    SetParam { slot: usize, index: usize, value: f32 },
    SetMaster { index: usize, value: f32 },
    SetChannel { slot: usize, channel: Option<u8> },
    SetMute { slot: usize, on: bool },
    SetSolo { slot: usize, on: bool },
    SetSample { slot: usize, sample: Option<Arc<Sample>> },
    Panic,
}

/// Values handed back to the UI thread purely so they are dropped there.
#[allow(dead_code)]
pub enum Garbage {
    Slot(Box<Slot>),
    Sample(Arc<Sample>),
}

pub type CommandQueue = Arc<ArrayQueue<Command>>;
pub type GarbageQueue = Arc<ArrayQueue<Garbage>>;

pub struct Slot {
    params: Vec<f32>,
    dirty: bool,
    channel: Option<u8>,
    mute: bool,
    solo: bool,
    inst: Instrument,
    gain: Smooth,
    pan: Smooth,
    send: Smooth,
}

impl Slot {
    pub fn new(
        kind: SynthKind,
        params: Vec<f32>,
        channel: Option<u8>,
        sample_rate: f32,
        builtins: &Builtins,
        sample: Option<Arc<Sample>>,
    ) -> Box<Slot> {
        let mut inst = Instrument::new(kind, sample_rate, builtins);
        // The returned (previous) sample is None for a fresh instrument.
        let _ = inst.set_sample(sample);
        inst.update(&params);
        let ctrl_rate = sample_rate / MAX_BLOCK as f32;
        Box::new(Slot {
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
    master_gain: Smooth,
    reverb: Reverb,
    buf_l: [f32; MAX_BLOCK],
    buf_r: [f32; MAX_BLOCK],
    mix_l: [f32; MAX_BLOCK],
    mix_r: [f32; MAX_BLOCK],
    send_l: [f32; MAX_BLOCK],
    send_r: [f32; MAX_BLOCK],
    cpu_avg: f32,
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
            self.master[master::REVERB_SIZE],
            self.master[master::REVERB_DAMP],
            self.master[master::REVERB_WIDTH],
        );
    }

    fn drain_commands(&mut self) {
        while let Some(cmd) = self.commands.pop() {
            self.handle(cmd);
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Midi(m) => self.handle_midi(m),
            Command::NoteOn { slot, note, velocity } => {
                if let Some(s) = self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    s.inst.note_on(note, velocity);
                    self.telemetry.slots[slot].notes.fetch_add(1, Ordering::Relaxed);
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
                    && let Some(p) = s.params.get_mut(index)
                {
                    *p = value;
                    s.dirty = true;
                }
            }
            Command::SetMaster { index, value } => {
                if let Some(p) = self.master.get_mut(index) {
                    *p = value;
                    self.apply_reverb_params();
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
            Command::SetSample { slot, sample } => {
                let old = match self.slots.get_mut(slot).and_then(|s| s.as_mut()) {
                    Some(s) => s.inst.set_sample(sample),
                    None => sample,
                };
                if let Some(old) = old {
                    self.discard(Garbage::Sample(old));
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
                    self.telemetry.slots[i].notes.fetch_add(1, Ordering::Relaxed);
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
        let started = Instant::now();
        self.drain_commands();
        let channels = channels.max(1);
        let frames = out.len() / channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
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
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let voices = slot.as_mut().map_or(0, |s| s.inst.active_voices());
            self.telemetry.slots[i].voices.store(voices as u32, Ordering::Relaxed);
        }
        if frames > 0 {
            let budget = frames as f32 / self.sample_rate;
            let load = started.elapsed().as_secs_f32() / budget;
            self.cpu_avg += (load - self.cpu_avg) * 0.1;
            self.telemetry.cpu.store(self.cpu_avg.to_bits(), Ordering::Relaxed);
        }
    }

    fn render_block(&mut self, n: usize) {
        self.mix_l[..n].fill(0.0);
        self.mix_r[..n].fill(0.0);
        self.send_l[..n].fill(0.0);
        self.send_r[..n].fill(0.0);
        let any_solo = self.slots.iter().flatten().any(|s| s.solo);

        for (idx, slot) in self.slots.iter_mut().enumerate() {
            let Some(s) = slot.as_mut() else { continue };
            if s.dirty {
                s.inst.update(&s.params);
                s.dirty = false;
            }
            let (bl, br) = (&mut self.buf_l[..n], &mut self.buf_r[..n]);
            bl.fill(0.0);
            br.fill(0.0);
            s.inst.render(bl, br);

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

        self.reverb.process(&mut self.send_l[..n], &mut self.send_r[..n]);
        let ret = self.master[master::REVERB_RETURN];
        let vol = self.master[master::VOLUME];
        let drive = self.master[master::DRIVE];
        let pre = 1.0 + drive * 8.0;
        let post = 1.0 / (1.0 + drive * 2.5);
        let (mut peak_l, mut peak_r) = (0.0f32, 0.0f32);
        for i in 0..n {
            let g = self.master_gain.next(vol * vol * MASTER_GAIN);
            let l = soft_clip((self.mix_l[i] + self.send_l[i] * ret) * g * pre) * post;
            let r = soft_clip((self.mix_r[i] + self.send_r[i] * ret) * g * pre) * post;
            self.mix_l[i] = l;
            self.mix_r[i] = r;
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());
        }
        store_max(&self.telemetry.peak_l, peak_l);
        store_max(&self.telemetry.peak_r, peak_r);
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
        let slot = Slot::new(kind, kind.defaults(), Some(0), sr, &builtins, None);
        cmds.push(Command::InstallSlot { slot: 0, data: slot }).ok();
        cmds.push(Command::Midi(MidiMsg {
            channel: 0,
            kind: MidiKind::NoteOn { note, velocity: 100 },
        }))
        .ok();
        // Notes on other channels must not reach the slot.
        cmds.push(Command::Midi(MidiMsg {
            channel: 5,
            kind: MidiKind::NoteOn { note: 72, velocity: 100 },
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
}
