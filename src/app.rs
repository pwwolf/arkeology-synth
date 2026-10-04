//! UI-thread application state: the model of the rack, input handling, and
//! all communication with the audio engine and MIDI layer.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use std::net::SocketAddr;
use std::sync::mpsc::Receiver;

use crossbeam_queue::ArrayQueue;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::engine::{self, Command, CommandQueue, GarbageQueue, MAX_SLOTS, Slot, Telemetry};
use crate::fx::{self, FxKind, FxUnit};
use crate::midi::{MidiKind, MidiManager, MidiMsg, note_name};
use crate::params::{ParamDesc, StepSize, master};
use crate::patch::{
    CcMapping, Patch, PatchEntry, Session, SessionSlot, Storage, master_index, read_json,
};
use crate::recorder::Recording;
use crate::sample::{self, Builtins, Sample};
use crate::synth::{SynthKind, granular, kit, sampler};

/// Which parameter set is being viewed/edited.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Master,
    Slot(usize),
}

pub struct UiSlot {
    pub kind: SynthKind,
    pub name: String,
    pub params: Vec<f32>,
    /// 0-based MIDI channel, `None` = omni.
    pub channel: Option<u8>,
    pub mute: bool,
    pub solo: bool,
    /// Loaded sample files, one entry per sample slot of the synth type (one
    /// for granular/sampler, 16 pads for a kit). The Arcs are kept here so
    /// samples are freed on this thread, never the audio thread.
    pub samples: Vec<Option<LoadedSample>>,
    pub meter: f32,
    pub voices: u32,
    notes_seen: u32,
    pub activity: Option<Instant>,
}

pub struct LoadedSample {
    pub path: PathBuf,
    pub sample: Arc<Sample>,
}

impl UiSlot {
    pub fn sample(&self, index: usize) -> Option<&LoadedSample> {
        self.samples.get(index).and_then(Option::as_ref)
    }

    pub fn sample_paths(&self) -> Vec<Option<PathBuf>> {
        self.samples
            .iter()
            .map(|s| s.as_ref().map(|s| s.path.clone()))
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Rack,
    Params,
}

pub enum TextAction {
    SavePatch,
    SaveSession,
    Rename,
    SetValue(Target, usize),
}

pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

pub enum Popup {
    Help,
    AddSynth {
        cursor: usize,
    },
    /// Start a new rack: `cursor` picks the template, `save` keeps the
    /// current rack as a session first.
    NewRack {
        cursor: usize,
        save: bool,
    },
    ConfirmRemove {
        slot: usize,
    },
    Text {
        title: String,
        hint: String,
        input: String,
        action: TextAction,
    },
    Patches {
        all: Vec<PatchEntry>,
        filter: Option<SynthKind>,
        cursor: usize,
    },
    Sessions {
        items: Vec<PathBuf>,
        cursor: usize,
    },
    Ports {
        items: Vec<String>,
        cursor: usize,
    },
    Files {
        dir: PathBuf,
        entries: Vec<FileEntry>,
        cursor: usize,
    },
}

pub struct Status {
    pub text: String,
    pub error: bool,
    at: Instant,
}

pub struct Keyboard {
    pub enabled: bool,
    pub octave: i32,
    pub velocity: u8,
    /// Terminal reports key releases (kitty keyboard protocol).
    pub has_release: bool,
    held: HashMap<char, HeldKey>,
}

struct HeldKey {
    slot: usize,
    note: u8,
    /// Last press or repeat.
    at: Instant,
    /// A key-up that hasn't been acted on yet (see RELEASE_GRACE).
    released: Option<Instant>,
}

/// Legacy terminals don't report key-up, so notes are released after this
/// long without a key repeat. Longer than X11's default 660 ms repeat delay.
const LEGACY_NOTE_HOLD: Duration = Duration::from_millis(750);

/// X11 auto-repeat (and terminals that pass it through) turns a held key into
/// release+press pairs. A key-up only ends the note if no press of the same
/// key follows within this long.
const RELEASE_GRACE: Duration = Duration::from_millis(30);

pub struct App {
    pub sample_rate: f32,
    pub device_name: String,
    pub storage: Storage,
    builtins: Builtins,
    commands: CommandQueue,
    garbage: GarbageQueue,
    telemetry: Arc<Telemetry>,
    midi_in: Arc<ArrayQueue<MidiMsg>>,
    errors: Arc<ArrayQueue<String>>,
    pub midi: MidiManager,

    pub slots: Vec<Option<UiSlot>>,
    pub master: Vec<f32>,
    pub rack_cursor: usize,
    pub focus: Focus,
    pub param_cursor: usize,
    pub param_scroll: usize,
    pub popup: Option<Popup>,
    pub status: Option<Status>,
    pub cc_map: Vec<CcMapping>,
    pub learning: Option<(Target, usize)>,
    pub midi_log: VecDeque<String>,
    pub keyboard: Keyboard,
    pub master_meter: (f32, f32),
    pub cpu: f32,
    pub quit: bool,
    /// Tool calls from the embedded MCP server, and where it listens.
    pub mcp: Option<Receiver<crate::mcp::Job>>,
    pub mcp_addr: Option<SocketAddr>,
    /// Notes scheduled by MCP `play_notes`, sent when due.
    scheduled: Vec<(Instant, Command)>,
    /// The recording in progress, and stopped ones still being finalized.
    pub recording: Option<Recording>,
    pub(crate) finishing: Vec<Recording>,
}

pub struct AppInit {
    pub sample_rate: f32,
    pub device_name: String,
    pub storage: Storage,
    pub builtins: Builtins,
    pub commands: CommandQueue,
    pub garbage: GarbageQueue,
    pub telemetry: Arc<Telemetry>,
    pub midi_in: Arc<ArrayQueue<MidiMsg>>,
    pub errors: Arc<ArrayQueue<String>>,
    pub midi: MidiManager,
    pub has_key_release: bool,
}

impl App {
    pub fn new(init: AppInit) -> Self {
        App {
            sample_rate: init.sample_rate,
            device_name: init.device_name,
            storage: init.storage,
            builtins: init.builtins,
            commands: init.commands,
            garbage: init.garbage,
            telemetry: init.telemetry,
            midi_in: init.midi_in,
            errors: init.errors,
            midi: init.midi,
            slots: (0..MAX_SLOTS).map(|_| None).collect(),
            master: master::PARAMS.iter().map(|p| p.default).collect(),
            rack_cursor: 1,
            focus: Focus::Rack,
            param_cursor: 0,
            param_scroll: 0,
            popup: None,
            status: None,
            cc_map: Vec::new(),
            learning: None,
            midi_log: VecDeque::new(),
            keyboard: Keyboard {
                enabled: false,
                octave: 4,
                velocity: 100,
                has_release: init.has_key_release,
                held: HashMap::new(),
            },
            master_meter: (0.0, 0.0),
            cpu: 0.0,
            quit: false,
            mcp: None,
            mcp_addr: None,
            scheduled: Vec::new(),
            recording: None,
            finishing: Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Messaging helpers
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Recording
    // -----------------------------------------------------------------------

    /// Start recording the master output to `recordings/<name>.wav`
    /// (default: the date and time).
    pub fn start_recording(&mut self, name: Option<&str>) -> Result<PathBuf, String> {
        if let Some(r) = &self.recording {
            return Err(format!("already recording to {}", r.path.display()));
        }
        let stem = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => crate::patch::file_stem(n),
            None => chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string(),
        };
        let mut path = self.storage.recordings_dir().join(format!("{stem}.wav"));
        // Never overwrite an earlier take.
        let mut k = 2;
        while path.exists() {
            path = self
                .storage
                .recordings_dir()
                .join(format!("{stem} ({k}).wav"));
            k += 1;
        }
        let (rec, tap) =
            Recording::start(path.clone(), self.sample_rate).map_err(|e| format!("{e:#}"))?;
        self.send(Command::StartRecording(tap));
        self.recording = Some(rec);
        self.info(format!("recording to {}", path.display()));
        Ok(path)
    }

    /// Stop recording; the file is finalized in the background.
    pub fn stop_recording(&mut self) -> Option<(PathBuf, f64)> {
        let rec = self.recording.take()?;
        self.send(Command::StopRecording);
        let result = (rec.path.clone(), rec.seconds());
        self.finishing.push(rec);
        Some(result)
    }

    fn toggle_recording(&mut self) {
        if self.recording.is_some() {
            if let Some((path, secs)) = self.stop_recording() {
                self.info(format!(
                    "stopped recording ({}): saving {}",
                    format_duration(secs),
                    path.display()
                ));
            }
        } else if let Err(e) = self.start_recording(None) {
            self.error(e);
        }
    }

    /// Report recordings whose files have been finalized.
    pub(crate) fn poll_recordings(&mut self) {
        let mut i = 0;
        while i < self.finishing.len() {
            if !self.finishing[i].is_finished() {
                i += 1;
                continue;
            }
            let rec = self.finishing.remove(i);
            let (secs, dropped) = (rec.seconds(), rec.dropped_seconds());
            match rec.join() {
                Ok(path) if dropped > 0.0 => self.error(format!(
                    "saved {} ({}), but {dropped:.2}s were dropped because the disk fell behind",
                    path.display(),
                    format_duration(secs)
                )),
                Ok(path) => self.info(format!(
                    "saved {} ({})",
                    path.display(),
                    format_duration(secs)
                )),
                Err(e) => self.error(format!("recording failed: {e:#}")),
            }
        }
    }

    /// Stop any recording and wait (up to `timeout`) for files to be finalized.
    pub fn finish_recording(&mut self, timeout: Duration) {
        self.stop_recording();
        let deadline = Instant::now() + timeout;
        while self.finishing.iter().any(|r| !r.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.poll_recordings();
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: false,
            at: Instant::now(),
        });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: true,
            at: Instant::now(),
        });
    }

    fn send(&mut self, cmd: Command) {
        if self.commands.push(cmd).is_err() {
            self.error("engine command queue full; change dropped");
        }
    }

    // -----------------------------------------------------------------------
    // Rack model
    // -----------------------------------------------------------------------

    /// Rack rows: the master section first, then each occupied slot.
    pub fn rack_rows(&self) -> Vec<Target> {
        std::iter::once(Target::Master)
            .chain(
                self.slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.is_some())
                    .map(|(i, _)| Target::Slot(i)),
            )
            .collect()
    }

    pub fn selected(&self) -> Target {
        let rows = self.rack_rows();
        rows[self.rack_cursor.min(rows.len() - 1)]
    }

    fn select(&mut self, target: Target) {
        if let Some(pos) = self.rack_rows().iter().position(|t| *t == target) {
            if pos != self.rack_cursor {
                self.param_cursor = 0;
                self.param_scroll = 0;
            }
            self.rack_cursor = pos;
        }
    }

    /// The sample a granular/sampler slot is currently playing: its loaded
    /// file, or one of the built-in sources.
    pub fn slot_sample(&self, index: usize) -> Option<Arc<Sample>> {
        let s = self.slots.get(index)?.as_ref()?;
        let src = s.params[s.kind.index_of("source")?].round() as usize;
        if src == 0 {
            s.sample(0).map(|l| l.sample.clone())
        } else {
            self.builtins.get(src - 1).cloned()
        }
    }

    /// Lowest playable note for synths that aren't laid out chromatically.
    fn fixed_keyboard_base(&self, index: usize) -> Option<i32> {
        let s = self.slots[index].as_ref()?;
        match s.kind {
            SynthKind::Drums | SynthKind::Kit => Some(36),
            SynthKind::Sampler => {
                let p = &s.params[crate::synth::COMMON.len()..];
                (p[sampler::MODE].round() as usize == 2)
                    .then(|| p[sampler::BASE_NOTE].round() as i32)
            }
            _ => None,
        }
    }

    pub fn selected_slot(&self) -> Option<usize> {
        match self.selected() {
            Target::Slot(i) => Some(i),
            Target::Master => None,
        }
    }

    pub fn param_count(&self, t: Target) -> usize {
        match t {
            Target::Master => master::PARAMS.len(),
            Target::Slot(i) => self.slots[i].as_ref().map_or(0, |s| s.kind.param_count()),
        }
    }

    pub fn param_desc(&self, t: Target, i: usize) -> &'static ParamDesc {
        match t {
            Target::Master => &master::PARAMS[i],
            Target::Slot(s) => self.slots[s].as_ref().expect("slot exists").kind.param(i),
        }
    }

    pub fn param_value(&self, t: Target, i: usize) -> f32 {
        match t {
            Target::Master => self.master[i],
            Target::Slot(s) => self.slots[s].as_ref().map_or(0.0, |s| s.params[i]),
        }
    }

    pub fn set_param(&mut self, t: Target, i: usize, value: f32) {
        let value = self.param_desc(t, i).clamp(value);
        let old = self.param_value(t, i);
        match t {
            Target::Master => {
                self.master[i] = value;
                self.send(Command::SetMaster { index: i, value });
            }
            Target::Slot(s) => {
                if let Some(slot) = self.slots[s].as_mut() {
                    slot.params[i] = value;
                    self.send(Command::SetParam {
                        slot: s,
                        index: i,
                        value,
                    });
                }
            }
        }
        if let Some(unit) = fx::type_param_unit(self.fx_base(t), i)
            && old != value
        {
            self.rebuild_fx(t, unit);
        }
    }

    /// Where a target's insert-FX parameters start.
    pub fn fx_base(&self, t: Target) -> usize {
        match t {
            Target::Master => master::FX_BASE,
            Target::Slot(s) => self.slots[s]
                .as_ref()
                .map_or(usize::MAX, |s| s.kind.fx_base()),
        }
    }

    fn values(&self, t: Target) -> &[f32] {
        match t {
            Target::Master => &self.master,
            Target::Slot(s) => self.slots[s].as_ref().map_or(&[], |s| &s.params),
        }
    }

    /// Whether a parameter is shown (FX parameters of inactive types are hidden).
    pub fn param_visible(&self, t: Target, i: usize) -> bool {
        match t {
            Target::Master => fx::visible(&self.master, master::FX_BASE, i),
            Target::Slot(s) => self.slots[s]
                .as_ref()
                .is_some_and(|slot| slot.kind.param_visible(&slot.params, i)),
        }
    }

    pub fn visible_params(&self, t: Target) -> Vec<usize> {
        (0..self.param_count(t))
            .filter(|&i| self.param_visible(t, i))
            .collect()
    }

    /// An FX unit's type changed: give it that effect's usual mix and swap in
    /// a newly built unit (allocated here, never on the audio thread).
    fn rebuild_fx(&mut self, t: Target, unit: usize) {
        let base = self.fx_base(t) + unit * fx::STRIDE;
        let kind = FxKind::from_value(self.param_value(t, base + fx::TYPE));
        self.set_param(t, base + fx::MIX, kind.default_mix());
        let built = FxUnit::from_values(
            fx::unit_values(self.values(t), self.fx_base(t), unit),
            self.sample_rate,
        );
        match t {
            Target::Master => self.send(Command::SetMasterFx { unit, fx: built }),
            Target::Slot(slot) => self.send(Command::SetFx {
                slot,
                unit,
                fx: built,
            }),
        }
    }

    pub fn param_key(&self, t: Target, i: usize) -> &'static str {
        self.param_desc(t, i).key
    }

    pub fn mapping_for(&self, t: Target, i: usize) -> Option<&CcMapping> {
        let key = self.param_key(t, i);
        let slot = match t {
            Target::Master => None,
            Target::Slot(s) => Some(s),
        };
        self.cc_map
            .iter()
            .find(|m| m.slot == slot && m.param == key)
    }

    fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.is_none())
    }

    fn free_channel(&self) -> Option<u8> {
        (0..16u8).find(|c| !self.slots.iter().flatten().any(|s| s.channel == Some(*c)))
    }

    /// Build a synth from a patch and install it in `index`, replacing what was there.
    #[allow(clippy::too_many_arguments)]
    fn install(
        &mut self,
        index: usize,
        kind: SynthKind,
        name: &str,
        values: Vec<f32>,
        channel: Option<u8>,
        sample_paths: Vec<Option<PathBuf>>,
        mute: bool,
        solo: bool,
    ) -> bool {
        let mut samples: Vec<Option<LoadedSample>> =
            (0..kind.sample_slots()).map(|_| None).collect();
        let mut failed = Vec::new();
        let mut missing_hint = None;
        for (i, path) in sample_paths.into_iter().enumerate().take(samples.len()) {
            let Some(path) = path else { continue };
            // Relative paths (factory kits) live in the samples folder.
            let relative = path.clone();
            let path = if path.is_relative() {
                self.storage.samples_dir().join(path)
            } else {
                path
            };
            if relative.is_relative() && !path.exists() {
                let fix = if crate::vcsl::is_vcsl_path(&relative) {
                    "download it with `mise run fetch-kits` (or --fetch-kits)"
                } else {
                    "re-render the built-in kits with --render-kits"
                };
                missing_hint = Some(fix);
                continue;
            }
            match sample::load_file(&path) {
                Ok(s) => {
                    samples[i] = Some(LoadedSample {
                        path,
                        sample: Arc::new(s),
                    })
                }
                Err(e) => failed.push(format!("{e:#}")),
            }
        }
        if !failed.is_empty() {
            self.error(format!("sample: {}", failed.join("; ")));
        }
        if let Some(fix) = &missing_hint {
            self.error(format!("this kit's samples aren't on disk yet: {fix}"));
        }
        let clean = failed.is_empty() && missing_hint.is_none();
        let arcs: Vec<Option<Arc<Sample>>> = samples
            .iter()
            .map(|s| s.as_ref().map(|s| s.sample.clone()))
            .collect();
        let data = Slot::new(
            kind,
            values.clone(),
            channel,
            self.sample_rate,
            &self.builtins,
            &arcs,
        )
        .with_flags(mute, solo);
        self.send(Command::InstallSlot { slot: index, data });
        let notes_seen = self.telemetry.slots[index].notes.load(Ordering::Relaxed);
        self.slots[index] = Some(UiSlot {
            kind,
            name: name.to_string(),
            params: values,
            channel,
            mute,
            solo,
            samples,
            meter: 0.0,
            voices: 0,
            notes_seen,
            activity: None,
        });
        clean
    }

    pub fn add_synth(&mut self, kind: SynthKind) {
        let Some(index) = self.free_slot() else {
            self.error(format!("all {MAX_SLOTS} slots are in use"));
            return;
        };
        let ch10_free = !self.slots.iter().flatten().any(|s| s.channel == Some(9));
        let drums = matches!(kind, SynthKind::Drums | SynthKind::Kit);
        let channel = if drums && ch10_free {
            Some(9)
        } else {
            self.free_channel()
        };
        let name = format!("{} {}", kind.label(), index + 1);
        let _ = self.install(
            index,
            kind,
            &name,
            kind.defaults(),
            channel,
            Vec::new(),
            false,
            false,
        );
        self.select(Target::Slot(index));
        self.info(format!(
            "added {} in slot {} on {}",
            kind.long_name(),
            index + 1,
            channel_label(channel)
        ));
    }

    fn remove_slot(&mut self, index: usize) {
        self.send(Command::RemoveSlot { slot: index });
        self.slots[index] = None;
        self.cc_map.retain(|m| m.slot != Some(index));
        self.keyboard.held.retain(|_, k| k.slot != index);
        let rows = self.rack_rows().len();
        self.rack_cursor = self.rack_cursor.min(rows - 1);
        self.param_cursor = 0;
        self.param_scroll = 0;
        self.info(format!("removed slot {}", index + 1));
    }

    fn change_channel(&mut self, index: usize, dir: i32) {
        let Some(slot) = self.slots[index].as_mut() else {
            return;
        };
        // Cycle: omni, 1..16
        let pos = slot.channel.map_or(0, |c| c as i32 + 1);
        let next = (pos + dir).rem_euclid(17);
        slot.channel = if next == 0 {
            None
        } else {
            Some((next - 1) as u8)
        };
        let channel = slot.channel;
        self.send(Command::SetChannel {
            slot: index,
            channel,
        });
    }

    fn toggle_mute(&mut self, index: usize) {
        if let Some(s) = self.slots[index].as_mut() {
            s.mute = !s.mute;
            let on = s.mute;
            self.send(Command::SetMute { slot: index, on });
        }
    }

    fn toggle_solo(&mut self, index: usize) {
        if let Some(s) = self.slots[index].as_mut() {
            s.solo = !s.solo;
            let on = s.solo;
            self.send(Command::SetSolo { slot: index, on });
            let soloed = self.soloed_slots();
            if soloed.is_empty() {
                self.info("solo off: all synths audible");
            } else {
                let list: Vec<String> = soloed.iter().map(|i| (i + 1).to_string()).collect();
                self.info(format!(
                    "solo on slot {}: every other synth is silent (s again to clear)",
                    list.join(", ")
                ));
            }
        }
    }

    /// Slot indices currently soloed; while non-empty, all other slots are silent.
    pub fn soloed_slots(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref().is_some_and(|s| s.solo))
            .map(|(i, _)| i)
            .collect()
    }

    // -----------------------------------------------------------------------
    // Patches, sessions, samples
    // -----------------------------------------------------------------------

    fn current_patch(&self, index: usize) -> Option<Patch> {
        let s = self.slots[index].as_ref()?;
        Some(Patch::from_values(
            &s.name,
            s.kind,
            &s.params,
            &s.sample_paths(),
        ))
    }

    fn load_patch_into(&mut self, index: usize, patch: Patch) {
        let (channel, mute, solo) = self.slots[index]
            .as_ref()
            .map_or((self.free_channel(), false, false), |s| {
                (s.channel, s.mute, s.solo)
            });
        let values = patch.values();
        // Keep any sample error on screen rather than replacing it with "loaded".
        if self.install(
            index,
            patch.kind,
            &patch.name,
            values,
            channel,
            patch.sample_paths(),
            mute,
            solo,
        ) {
            self.info(format!(
                "loaded patch '{}' into slot {}",
                patch.name,
                index + 1
            ));
        }
    }

    fn save_patch(&mut self, name: &str) {
        let Some(index) = self.selected_slot() else {
            return;
        };
        if let Some(s) = self.slots[index].as_mut() {
            s.name = name.to_string();
        }
        let Some(patch) = self.current_patch(index) else {
            return;
        };
        match self.storage.save_patch(&patch) {
            Ok(path) => self.info(format!("saved patch to {}", path.display())),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    pub fn session(&self) -> Session {
        Session {
            master: master::PARAMS
                .iter()
                .zip(&self.master)
                .enumerate()
                .filter(|(i, _)| fx::visible(&self.master, master::FX_BASE, *i))
                .map(|(_, (p, v))| (p.key.to_string(), *v))
                .collect(),
            slots: self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let s = s.as_ref()?;
                    Some(SessionSlot {
                        index: i,
                        channel: s.channel.map(|c| c + 1),
                        mute: s.mute,
                        solo: s.solo,
                        patch: self.current_patch(i)?,
                    })
                })
                .collect(),
            cc_map: self.cc_map.clone(),
            midi_ports: self.midi.connected_names(),
        }
    }

    /// Replace the rack with an empty one or the starter rack, optionally
    /// saving the current rack as a session first. Synths, master settings
    /// and MIDI mappings reset; MIDI port connections and recording carry on.
    pub fn new_rack(
        &mut self,
        starter: bool,
        save_as: Option<String>,
    ) -> Result<Option<PathBuf>, String> {
        let saved = match save_as {
            Some(name) => Some(
                self.storage
                    .save_session(&name, &self.session())
                    .map_err(|e| format!("{e:#}"))?,
            ),
            None => None,
        };
        self.all_keyboard_notes_off();
        self.scheduled.clear();
        self.learning = None;
        // An empty session is exactly a blank rack with default master settings.
        self.apply_session(Session::default());
        if starter {
            self.default_rack();
        }
        let what = if starter {
            "starter rack"
        } else {
            "empty rack"
        };
        match &saved {
            Some(path) => self.info(format!(
                "new {what}; previous rack saved as {}",
                path.display()
            )),
            None => self.info(format!("new {what}")),
        }
        Ok(saved)
    }

    /// Default name for saving the current rack before replacing it.
    pub fn timestamped_rack_name() -> String {
        chrono::Local::now()
            .format("Rack %Y-%m-%d %H-%M")
            .to_string()
    }

    pub fn has_synths(&self) -> bool {
        self.slots.iter().any(Option::is_some)
    }

    pub fn apply_session(&mut self, session: Session) {
        self.send(Command::Panic);
        for i in 0..MAX_SLOTS {
            if self.slots[i].is_some() {
                self.send(Command::RemoveSlot { slot: i });
                self.slots[i] = None;
            }
        }
        for (i, v) in session.master_values().into_iter().enumerate() {
            self.set_param(Target::Master, i, v);
        }
        for s in session.slots {
            if s.index >= MAX_SLOTS {
                continue;
            }
            let channel = s.channel.and_then(|c| c.checked_sub(1)).filter(|c| *c < 16);
            let values = s.patch.values();
            let _ = self.install(
                s.index,
                s.patch.kind,
                &s.patch.name,
                values,
                channel,
                s.patch.sample_paths(),
                s.mute,
                s.solo,
            );
        }
        self.cc_map = session.cc_map;
        for port in &session.midi_ports {
            // Ports that aren't present right now are simply skipped.
            let _ = self.midi.connect(port);
        }
        self.rack_cursor = if self.rack_rows().len() > 1 { 1 } else { 0 };
        self.param_cursor = 0;
        self.param_scroll = 0;
    }

    fn save_session(&mut self, name: &str) {
        let session = self.session();
        match self.storage.save_session(name, &session) {
            Ok(path) => self.info(format!("saved session to {}", path.display())),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    pub fn load_session_file(&mut self, path: &Path) {
        match read_json::<Session>(path) {
            Ok(s) => {
                self.apply_session(s);
                self.info(format!("loaded session {}", path.display()));
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    pub fn autosave(&mut self) {
        let session = self.session();
        if let Err(e) = crate::patch::write_json(&self.storage.autosave_path(), &session) {
            self.error(format!("autosave failed: {e:#}"));
        }
    }

    pub fn default_rack(&mut self) {
        let patches = crate::patch::factory_patches();
        let find = |name: &str| patches.iter().find(|p| p.name == name).cloned();
        let rack = [
            ("E.Piano", 0u8),
            ("Choir Cloud", 1),
            ("Acid Classic", 2),
            ("808 Kit", 9),
        ];
        for (i, (name, channel)) in rack.iter().enumerate() {
            if let Some(p) = find(name) {
                let values = p.values();
                let _ = self.install(
                    i,
                    p.kind,
                    &p.name,
                    values,
                    Some(*channel),
                    Vec::new(),
                    false,
                    false,
                );
            }
        }
        self.rack_cursor = 1;
    }

    /// The kit pad being edited (the one containing the parameter cursor), else pad 0.
    pub fn current_pad(&self, index: usize) -> usize {
        let is_kit = self.slots[index]
            .as_ref()
            .is_some_and(|s| s.kind == SynthKind::Kit);
        let local = self.param_cursor.checked_sub(crate::synth::COMMON.len());
        if is_kit && self.selected_slot() == Some(index) {
            local.and_then(kit::pad_of).unwrap_or(0)
        } else {
            0
        }
    }

    /// Load a sample file into slot `index` at sample slot `pad` (0 unless a kit).
    pub(crate) fn load_sample(&mut self, index: usize, pad: usize, path: &Path) {
        match sample::load_file(path) {
            Ok(s) => {
                let dur = s.duration();
                let pitch = s.pitch;
                let arc = Arc::new(s);
                let kind = self.slots[index].as_ref().map(|s| s.kind);
                if kind == Some(SynthKind::Kit) {
                    self.set_pad_sample(index, pad, path.to_path_buf(), arc);
                    let role = kit::PAD_ROLES[pad.min(kit::PADS - 1)];
                    self.info(format!(
                        "pad {} ({role}): {} ({dur:.2}s)",
                        pad + 1,
                        path.display()
                    ));
                    return;
                }
                self.send(Command::SetSample {
                    slot: index,
                    index: 0,
                    sample: Some(arc.clone()),
                });
                if let Some(slot) = self.slots[index].as_mut()
                    && let Some(entry) = slot.samples.first_mut()
                {
                    *entry = Some(LoadedSample {
                        path: path.to_path_buf(),
                        sample: arc,
                    });
                }
                // Switch the source to "File" so the new sample is heard.
                if let Some(src) = kind.and_then(|k| k.index_of("source")) {
                    let file = granular::SOURCES
                        .iter()
                        .position(|s| *s == "File")
                        .unwrap_or(0);
                    self.set_param(Target::Slot(index), src, file as f32);
                }
                let mut msg = format!("loaded {} ({dur:.1}s)", path.display());
                if kind == Some(SynthKind::Sampler) {
                    msg.push_str(&self.apply_detected_pitch(index, pitch));
                }
                self.info(msg);
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn set_pad_sample(&mut self, index: usize, pad: usize, path: PathBuf, sample: Arc<Sample>) {
        self.send(Command::SetSample {
            slot: index,
            index: pad,
            sample: Some(sample.clone()),
        });
        if let Some(slot) = self.slots[index].as_mut()
            && let Some(entry) = slot.samples.get_mut(pad)
        {
            *entry = Some(LoadedSample { path, sample });
        }
    }

    /// Load every audio file in `dir` into a kit, assigning pads by file name.
    /// Returns (pad, path) for each assignment.
    pub(crate) fn load_kit_folder(
        &mut self,
        index: usize,
        dir: &Path,
    ) -> Result<Vec<(usize, PathBuf)>, String> {
        let files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && sample::is_audio_file(p))
            .collect();
        if files.is_empty() {
            return Err(format!("no WAV or FLAC files in {}", dir.display()));
        }
        let mut loaded = Vec::new();
        let mut failed = Vec::new();
        for (pad, path) in kit::assign_files(&files).into_iter().enumerate() {
            let Some(path) = path else { continue };
            match sample::load_file(&path) {
                Ok(s) => {
                    self.set_pad_sample(index, pad, path.clone(), Arc::new(s));
                    loaded.push((pad, path));
                }
                Err(e) => failed.push(format!("{e:#}")),
            }
        }
        let skipped = files.len().saturating_sub(loaded.len() + failed.len());
        let mut msg = format!(
            "loaded {} samples from {} into the kit",
            loaded.len(),
            dir.display()
        );
        if skipped > 0 {
            msg.push_str(&format!(" ({skipped} didn't fit in {} pads)", kit::PADS));
        }
        if failed.is_empty() {
            self.info(msg);
        } else {
            self.error(format!("{msg}; failed: {}", failed.join("; ")));
        }
        Ok(loaded)
    }

    /// Set a sampler's Root Note and Tune from a detected pitch so the sample
    /// plays in tune across the keyboard. Returns text for the status line.
    fn apply_detected_pitch(&mut self, index: usize, pitch: Option<f32>) -> String {
        let Some(p) = pitch else {
            return " · no clear pitch, Root Note unchanged".to_string();
        };
        let root = p.round().clamp(0.0, 127.0);
        let cents = ((p - root) * 100.0).round();
        let p_kind = SynthKind::Sampler;
        let root_i = p_kind.index_of("root").expect("root param");
        let tune_i = p_kind.index_of("tune").expect("tune param");
        self.set_param(Target::Slot(index), root_i, root);
        // Played at its root, the sample must sound exactly that note.
        self.set_param(Target::Slot(index), tune_i, -cents);
        let name = crate::midi::note_name(root as u8);
        if cents == 0.0 {
            format!(" · pitch {name}: Root Note set to {name}")
        } else {
            format!(
                " · pitch {name} {cents:+.0} ct: Root Note {name}, Tune {:+.0} ct",
                -cents
            )
        }
    }

    pub(crate) fn open_files(&mut self, dir: PathBuf) {
        let mut entries = Vec::new();
        if let Some(parent) = dir.parent() {
            entries.push(FileEntry {
                name: "..".into(),
                path: parent.to_path_buf(),
                is_dir: true,
            });
        }
        let mut list: Vec<FileEntry> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let path = e.path();
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name.starts_with('.') {
                            return None;
                        }
                        let is_dir = path.is_dir();
                        (is_dir || sample::is_audio_file(&path)).then_some(FileEntry {
                            name,
                            path,
                            is_dir,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        list.sort_by_key(|e| (!e.is_dir, e.name.to_lowercase()));
        entries.extend(list);
        self.popup = Some(Popup::Files {
            dir,
            entries,
            cursor: 0,
        });
    }

    // -----------------------------------------------------------------------
    // Periodic work
    // -----------------------------------------------------------------------

    pub fn tick(&mut self) {
        self.poll_recordings();
        if let Some(rx) = &self.mcp {
            let jobs: Vec<crate::mcp::Job> = rx.try_iter().collect();
            for job in jobs {
                let result = self.run_tool(&job.tool, &job.args);
                if let Err(e) = &result {
                    self.error(format!("MCP {}: {e}", job.tool));
                }
                let _ = job.reply.send(result);
            }
        }
        if !self.scheduled.is_empty() {
            let now = Instant::now();
            let (mut due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.scheduled)
                .into_iter()
                .partition(|(at, _)| *at <= now);
            self.scheduled = later;
            // Stable sort keeps a note-on ahead of its note-off at equal times.
            due.sort_by_key(|(at, _)| *at);
            for (_, cmd) in due {
                self.send(cmd);
            }
        }

        while let Some(e) = self.errors.pop() {
            self.error(e);
        }
        // Free anything the audio thread has finished with.
        while self.garbage.pop().is_some() {}

        while let Some(msg) = self.midi_in.pop() {
            self.on_midi(msg);
        }

        let decay = 0.82;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let Some(s) = slot.as_mut() else { continue };
            let t = &self.telemetry.slots[i];
            s.meter = engine::take_peak(&t.peak).max(s.meter * decay);
            s.voices = t.voices.load(Ordering::Relaxed);
            let notes = t.notes.load(Ordering::Relaxed);
            if notes != s.notes_seen {
                s.notes_seen = notes;
                s.activity = Some(Instant::now());
            }
        }
        let l = engine::take_peak(&self.telemetry.peak_l).max(self.master_meter.0 * decay);
        let r = engine::take_peak(&self.telemetry.peak_r).max(self.master_meter.1 * decay);
        self.master_meter = (l, r);
        self.cpu = f32::from_bits(self.telemetry.cpu.load(Ordering::Relaxed));

        self.release_keyboard_notes(Instant::now());

        if let Some(s) = &self.status
            && s.at.elapsed() > Duration::from_secs(if s.error { 10 } else { 5 })
        {
            self.status = None;
        }
    }

    fn on_midi(&mut self, msg: MidiMsg) {
        self.midi_log.push_front(msg.to_string());
        self.midi_log.truncate(64);
        let MidiKind::Cc { cc, value } = msg.kind else {
            return;
        };
        if let Some((target, index)) = self.learning.take() {
            let slot = match target {
                Target::Master => None,
                Target::Slot(s) => Some(s),
            };
            let param = self.param_key(target, index).to_string();
            self.cc_map
                .retain(|m| !(m.slot == slot && m.param == param));
            self.cc_map.push(CcMapping {
                channel: msg.channel,
                cc,
                slot,
                param,
            });
            let name = self.param_desc(target, index).name;
            self.info(format!("mapped CC{cc} (ch {}) to {name}", msg.channel + 1));
        }
        let hits: Vec<(Target, usize)> = self
            .cc_map
            .iter()
            .filter(|m| m.channel == msg.channel && m.cc == cc)
            .filter_map(|m| match m.slot {
                None => master_index(&m.param).map(|i| (Target::Master, i)),
                Some(s) => {
                    let kind = self.slots.get(s)?.as_ref()?.kind;
                    kind.index_of(&m.param).map(|i| (Target::Slot(s), i))
                }
            })
            .collect();
        for (t, i) in hits {
            let v = self.param_desc(t, i).denormalize(value as f32 / 127.0);
            self.set_param(t, i, v);
        }
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    pub fn on_key(&mut self, mut key: KeyEvent) {
        // Kitty-protocol terminals may report shifted letters as lowercase + SHIFT.
        if let KeyCode::Char(c) = key.code
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && c.is_ascii_lowercase()
        {
            key.code = KeyCode::Char(c.to_ascii_uppercase());
        }

        if key.kind == KeyEventKind::Release {
            if let KeyCode::Char(c) = key.code
                && let Some(k) = self.keyboard.held.get_mut(&c.to_ascii_lowercase())
            {
                k.released.get_or_insert(Instant::now());
            }
            return;
        }

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }

        if let Some(popup) = &self.popup {
            let typing = matches!(popup, Popup::Text { .. });
            if key.kind == KeyEventKind::Press || repeatable(key.code, typing) {
                self.on_popup_key(key);
            }
            return;
        }

        if self.keyboard.enabled && self.on_play_key(key) {
            return;
        }
        if key.kind == KeyEventKind::Repeat && !repeatable(key.code, false) {
            return;
        }
        self.on_main_key(key);
    }

    /// Returns true if the key was consumed by the play-the-keyboard mode.
    fn on_play_key(&mut self, key: KeyEvent) -> bool {
        let KeyCode::Char(c) = key.code else {
            if key.code == KeyCode::Esc {
                self.all_keyboard_notes_off();
                self.keyboard.enabled = false;
                self.info("keyboard play mode off");
                return true;
            }
            return false;
        };
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        const KEYS: &str = "awsedftgyhujkolp;'";
        if let Some(offset) = KEYS.find(c) {
            let Some(slot) = self.selected_slot() else {
                self.error("select a synth to play it from the keyboard");
                return true;
            };
            // Drum kits and sliced samples put their first sound on the home
            // row's first key; z/x still shift by octaves from there.
            let base = match self.fixed_keyboard_base(slot) {
                Some(b) => b + (self.keyboard.octave - 4) * 12,
                None => (self.keyboard.octave + 1) * 12,
            };
            let note = base + offset as i32;
            let Ok(note) = u8::try_from(note) else {
                return true;
            };
            if note > 127 {
                return true;
            }
            let now = Instant::now();
            if let Some(k) = self.keyboard.held.get_mut(&c) {
                // Key repeat (including X11's release+press pairs): keep the note alive.
                k.at = now;
                k.released = None;
                return true;
            }
            self.keyboard.held.insert(
                c,
                HeldKey {
                    slot,
                    note,
                    at: now,
                    released: None,
                },
            );
            let velocity = self.keyboard.velocity as f32 / 127.0;
            self.send(Command::NoteOn {
                slot,
                note,
                velocity,
            });
            return true;
        }
        if key.kind == KeyEventKind::Repeat {
            return c.is_ascii_lowercase();
        }
        match c {
            'z' => {
                self.keyboard.octave = (self.keyboard.octave - 1).max(-1);
                self.info(format!(
                    "octave {} (C = {})",
                    self.keyboard.octave,
                    note_name(((self.keyboard.octave + 1) * 12).clamp(0, 127) as u8)
                ));
            }
            'x' => {
                self.keyboard.octave = (self.keyboard.octave + 1).min(8);
                self.info(format!(
                    "octave {} (C = {})",
                    self.keyboard.octave,
                    note_name(((self.keyboard.octave + 1) * 12).clamp(0, 127) as u8)
                ));
            }
            'c' => {
                self.keyboard.velocity = self.keyboard.velocity.saturating_sub(20).max(7);
                self.info(format!("velocity {}", self.keyboard.velocity));
            }
            'v' => {
                self.keyboard.velocity = (self.keyboard.velocity + 20).min(127);
                self.info(format!("velocity {}", self.keyboard.velocity));
            }
            // Swallow other lowercase letters so stray typing can't trigger commands.
            c if c.is_ascii_lowercase() => {}
            _ => return false,
        }
        true
    }

    fn all_keyboard_notes_off(&mut self) {
        let held: Vec<_> = self.keyboard.held.drain().map(|(_, k)| k).collect();
        for k in held {
            self.send(Command::NoteOff {
                slot: k.slot,
                note: k.note,
            });
        }
    }

    /// End notes whose key-up has outlasted the grace period, or (in terminals
    /// without key-up events) whose key stopped repeating.
    fn release_keyboard_notes(&mut self, now: Instant) {
        let legacy = !self.keyboard.has_release;
        let done: Vec<char> = self
            .keyboard
            .held
            .iter()
            .filter(|(_, k)| match k.released {
                Some(at) => now.duration_since(at) >= RELEASE_GRACE,
                None => legacy && now.duration_since(k.at) > LEGACY_NOTE_HOLD,
            })
            .map(|(c, _)| *c)
            .collect();
        for c in done {
            if let Some(k) = self.keyboard.held.remove(&c) {
                self.send(Command::NoteOff {
                    slot: k.slot,
                    note: k.note,
                });
            }
        }
    }

    fn on_main_key(&mut self, key: KeyEvent) {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let target = self.selected();
        let slot = self.selected_slot();
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.popup = Some(Popup::Help),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Rack {
                    Focus::Params
                } else {
                    Focus::Rack
                };
            }
            KeyCode::Char(' ') => {
                self.all_keyboard_notes_off();
                self.send(Command::Panic);
                self.info("all notes off");
            }
            KeyCode::Char('k') => {
                self.keyboard.enabled = true;
                let how = if self.keyboard.has_release {
                    ""
                } else {
                    " (no key-up events in this terminal: notes auto-release)"
                };
                self.info(format!(
                    "keyboard play mode: a-' play notes, z/x octave, c/v velocity, Esc exits{how}"
                ));
            }
            KeyCode::Char('a') => self.popup = Some(Popup::AddSynth { cursor: 0 }),
            KeyCode::Char('N') => {
                self.popup = Some(Popup::NewRack {
                    cursor: 0,
                    save: self.has_synths(),
                })
            }
            KeyCode::Char('R') => self.toggle_recording(),
            KeyCode::Char('l') => {
                if slot.is_none() {
                    self.error(
                        "select a synth slot to load a patch into (or press 'a' to add one)",
                    );
                } else {
                    let all = self.storage.list_patches();
                    let current = slot.and_then(|i| self.slots[i].as_ref());
                    let filter = current.map(|s| s.kind);
                    let cursor = current
                        .and_then(|s| {
                            patch_view(&all, filter)
                                .iter()
                                .position(|e| e.patch.name == s.name)
                        })
                        .unwrap_or(0);
                    self.popup = Some(Popup::Patches {
                        all,
                        filter,
                        cursor,
                    });
                }
            }
            KeyCode::Char('w') => {
                if let Some(s) = slot.and_then(|i| self.slots[i].as_ref()) {
                    self.popup = Some(Popup::Text {
                        title: "Save patch".into(),
                        hint: format!("saved to {}", self.storage.patches_dir().display()),
                        input: s.name.clone(),
                        action: TextAction::SavePatch,
                    });
                } else {
                    self.error("select a synth slot to save its patch");
                }
            }
            KeyCode::Char('W') => {
                self.popup = Some(Popup::Text {
                    title: "Save session".into(),
                    hint: format!("saved to {}", self.storage.sessions_dir().display()),
                    input: String::new(),
                    action: TextAction::SaveSession,
                })
            }
            KeyCode::Char('L') => {
                let items = self.storage.list_sessions();
                self.popup = Some(Popup::Sessions { items, cursor: 0 });
            }
            KeyCode::Char('r') => {
                if let Some(s) = slot.and_then(|i| self.slots[i].as_ref()) {
                    self.popup = Some(Popup::Text {
                        title: "Rename synth".into(),
                        hint: String::new(),
                        input: s.name.clone(),
                        action: TextAction::Rename,
                    });
                }
            }
            KeyCode::Char('m') => {
                if let Some(i) = slot {
                    self.toggle_mute(i)
                }
            }
            KeyCode::Char('s') => {
                if let Some(i) = slot {
                    self.toggle_solo(i)
                }
            }
            KeyCode::Char('p') => {
                let items = MidiManager::available_ports();
                self.popup = Some(Popup::Ports { items, cursor: 0 });
            }
            KeyCode::Char('f') => match slot.and_then(|i| self.slots[i].as_ref().map(|s| (i, s))) {
                Some((i, s)) if s.kind.sample_slots() > 0 => {
                    // Start where this pad's (or any) current sample lives.
                    let pad = self.current_pad(i);
                    let dir = s
                        .sample(pad)
                        .or_else(|| s.samples.iter().flatten().next())
                        .and_then(|l| l.path.parent().map(Path::to_path_buf))
                        .unwrap_or_else(|| self.storage.samples_dir());
                    self.open_files(dir);
                }
                _ => self.error("select a granular synth, sampler or drum kit to load samples"),
            },
            KeyCode::Char('c') => {
                if self.param_count(target) > 0 {
                    let i = self.param_cursor.min(self.param_count(target) - 1);
                    self.learning = Some((target, i));
                    self.focus = Focus::Params;
                    self.info(format!(
                        "MIDI learn: move a knob/fader to map it to {}",
                        self.param_desc(target, i).name
                    ));
                }
            }
            KeyCode::Char('C') => {
                if self.param_count(target) > 0 {
                    let i = self.param_cursor.min(self.param_count(target) - 1);
                    let key = self.param_key(target, i);
                    let s = match target {
                        Target::Master => None,
                        Target::Slot(s) => Some(s),
                    };
                    self.cc_map.retain(|m| !(m.slot == s && m.param == key));
                    self.learning = None;
                    self.info("MIDI mapping cleared");
                }
            }
            _ => match self.focus {
                Focus::Rack => self.on_rack_key(key, shift),
                Focus::Params => self.on_params_key(key, shift),
            },
        }
    }

    fn on_rack_key(&mut self, key: KeyEvent, _shift: bool) {
        let rows = self.rack_rows().len();
        match key.code {
            KeyCode::Up => {
                if self.rack_cursor > 0 {
                    self.rack_cursor -= 1;
                    self.param_cursor = 0;
                    self.param_scroll = 0;
                }
            }
            KeyCode::Down => {
                if self.rack_cursor + 1 < rows {
                    self.rack_cursor += 1;
                    self.param_cursor = 0;
                    self.param_scroll = 0;
                }
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('-')
            | KeyCode::Char('=')
            | KeyCode::Char('+') => {
                if let Some(i) = self.selected_slot() {
                    let dir = if matches!(key.code, KeyCode::Left | KeyCode::Char('-')) {
                        -1
                    } else {
                        1
                    };
                    self.change_channel(i, dir);
                }
            }
            KeyCode::Enter => self.focus = Focus::Params,
            KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('d') => {
                if let Some(i) = self.selected_slot() {
                    self.popup = Some(Popup::ConfirmRemove { slot: i });
                }
            }
            _ => {}
        }
    }

    fn on_params_key(&mut self, key: KeyEvent, shift: bool) {
        let target = self.selected();
        let count = self.param_count(target);
        if count == 0 {
            return;
        }
        // Navigate over visible parameters only (hidden FX settings are skipped).
        let vis = self.visible_params(target);
        let pos = vis
            .iter()
            .rposition(|&i| i <= self.param_cursor)
            .unwrap_or(0);
        let cur = vis[pos];
        let group_of = |i: usize| self.param_desc(target, vis[i]).group;
        match key.code {
            KeyCode::Up => self.param_cursor = vis[pos.saturating_sub(1)],
            KeyCode::Down => self.param_cursor = vis[(pos + 1).min(vis.len() - 1)],
            KeyCode::Home => self.param_cursor = vis[0],
            KeyCode::End => self.param_cursor = vis[vis.len() - 1],
            KeyCode::PageDown => {
                let g = group_of(pos);
                let next = (pos..vis.len()).find(|&k| group_of(k) != g).unwrap_or(pos);
                self.param_cursor = vis[next];
            }
            KeyCode::PageUp => {
                // Start of this group, or of the previous one if already at the start.
                let g = group_of(pos);
                let start = (0..=pos)
                    .rev()
                    .take_while(|&k| group_of(k) == g)
                    .last()
                    .unwrap_or(pos);
                let k = if start == pos && pos > 0 {
                    let pg = group_of(pos - 1);
                    (0..pos)
                        .rev()
                        .take_while(|&k| group_of(k) == pg)
                        .last()
                        .unwrap_or(0)
                } else {
                    start
                };
                self.param_cursor = vis[k];
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(',') | KeyCode::Char('.') => {
                let dir = if matches!(key.code, KeyCode::Left | KeyCode::Char(',')) {
                    -1.0
                } else {
                    1.0
                };
                let size = match key.code {
                    KeyCode::Char(_) => StepSize::Fine,
                    _ if shift => StepSize::Coarse,
                    _ => StepSize::Normal,
                };
                let desc = self.param_desc(target, cur);
                let v = desc.adjust(self.param_value(target, cur), dir, size);
                self.set_param(target, cur, v);
            }
            KeyCode::Enter => {
                let desc = self.param_desc(target, cur);
                let hint = match desc.kind {
                    crate::params::Kind::Enum(opts) => format!("one of: {}", opts.join(", ")),
                    _ => format!("{} .. {}", desc.format(desc.min), desc.format(desc.max)),
                };
                self.popup = Some(Popup::Text {
                    title: format!("Set {}", desc.name),
                    hint,
                    input: String::new(),
                    action: TextAction::SetValue(target, cur),
                });
            }
            KeyCode::Backspace | KeyCode::Delete => {
                let d = self.param_desc(target, cur).default;
                self.set_param(target, cur, d);
            }
            KeyCode::Esc => self.focus = Focus::Rack,
            _ => {}
        }
    }

    fn on_popup_key(&mut self, key: KeyEvent) {
        let Some(mut popup) = self.popup.take() else {
            return;
        };
        let keep = match &mut popup {
            Popup::Help => false,
            Popup::NewRack { cursor, save } => match key.code {
                KeyCode::Up | KeyCode::Down => {
                    *cursor = 1 - (*cursor).min(1);
                    true
                }
                KeyCode::Char('s') => {
                    *save = !*save;
                    true
                }
                KeyCode::Enter => {
                    let save_as = save.then(App::timestamped_rack_name);
                    if let Err(e) = self.new_rack(*cursor == 1, save_as) {
                        self.error(e);
                    }
                    false
                }
                _ => key.code != KeyCode::Esc,
            },
            Popup::AddSynth { cursor } => match key.code {
                KeyCode::Up => {
                    *cursor = cursor.saturating_sub(1);
                    true
                }
                KeyCode::Down => {
                    *cursor = (*cursor + 1).min(SynthKind::ALL.len() - 1);
                    true
                }
                KeyCode::Enter => {
                    let kind = SynthKind::ALL[*cursor];
                    self.add_synth(kind);
                    false
                }
                KeyCode::Char(c @ '1'..='9') => {
                    if let Some(kind) = SynthKind::ALL.get(c as usize - '1' as usize) {
                        self.add_synth(*kind);
                    }
                    false
                }
                _ => key.code != KeyCode::Esc,
            },
            Popup::ConfirmRemove { slot } => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                    let s = *slot;
                    self.remove_slot(s);
                }
                false
            }
            Popup::Text { input, action, .. } => match key.code {
                KeyCode::Esc => false,
                KeyCode::Enter => {
                    let text = input.trim().to_string();
                    match action {
                        TextAction::SavePatch if !text.is_empty() => self.save_patch(&text),
                        TextAction::SaveSession if !text.is_empty() => self.save_session(&text),
                        TextAction::Rename if !text.is_empty() => {
                            if let Some(s) =
                                self.selected_slot().and_then(|i| self.slots[i].as_mut())
                            {
                                s.name = text;
                            }
                        }
                        TextAction::SetValue(t, i) => {
                            let (t, i) = (*t, *i);
                            match self.param_desc(t, i).parse(&text) {
                                Some(v) => self.set_param(t, i, v),
                                None => self.error(format!("couldn't parse '{text}'")),
                            }
                        }
                        _ => {}
                    }
                    false
                }
                KeyCode::Backspace => {
                    input.pop();
                    true
                }
                KeyCode::Char(c) => {
                    if input.chars().count() < 48 {
                        input.push(c);
                    }
                    true
                }
                _ => true,
            },
            Popup::Patches {
                all,
                filter,
                cursor,
            } => {
                let chosen = patch_view(all, *filter)
                    .get(*cursor)
                    .map(|e| e.patch.clone());
                match list_nav(key.code, cursor, patch_view(all, *filter).len()) {
                    ListAction::Select => {
                        if let (Some(index), Some(patch)) = (self.selected_slot(), chosen) {
                            self.load_patch_into(index, patch);
                        }
                        false
                    }
                    ListAction::Preview => {
                        // Audition while browsing: load without closing.
                        if let (Some(index), Some(patch)) = (self.selected_slot(), chosen) {
                            self.load_patch_into(index, patch);
                        }
                        true
                    }
                    ListAction::Close => false,
                    ListAction::Stay => {
                        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                            // Cycle the kind filter: each synth kind, then all.
                            let kinds = SynthKind::ALL;
                            *filter = match *filter {
                                None => Some(kinds[0]),
                                Some(k) => kinds.get(k.order() + 1).copied(),
                            };
                            *cursor = 0;
                        }
                        true
                    }
                }
            }
            Popup::Sessions { items, cursor } => match list_nav(key.code, cursor, items.len()) {
                ListAction::Select => {
                    if let Some(path) = items.get(*cursor).cloned() {
                        self.load_session_file(&path);
                    }
                    false
                }
                ListAction::Close => false,
                _ => true,
            },
            Popup::Ports { items, cursor } => match list_nav(key.code, cursor, items.len()) {
                ListAction::Select | ListAction::Preview => {
                    if let Some(name) = items.get(*cursor).cloned() {
                        if self.midi.is_connected(&name) {
                            self.midi.disconnect(&name);
                            self.info(format!("disconnected {name}"));
                        } else {
                            match self.midi.connect(&name) {
                                Ok(()) => self.info(format!("connected {name}")),
                                Err(e) => self.error(format!("{e:#}")),
                            }
                        }
                    }
                    true
                }
                ListAction::Close => false,
                ListAction::Stay => {
                    if key.code == KeyCode::Char('r') {
                        *items = MidiManager::available_ports();
                        *cursor = 0;
                    }
                    true
                }
            },
            Popup::Files {
                dir,
                entries,
                cursor,
            } => match list_nav(key.code, cursor, entries.len()) {
                ListAction::Select | ListAction::Preview => {
                    if let Some(e) = entries.get(*cursor) {
                        let path = e.path.clone();
                        if e.is_dir {
                            self.open_files(path);
                            return;
                        }
                        if let Some(i) = self.selected_slot() {
                            let pad = self.current_pad(i);
                            self.load_sample(i, pad, &path);
                        }
                    }
                    false
                }
                ListAction::Stay if key.code == KeyCode::Char('K') => {
                    // Load the folder being browsed as a whole kit.
                    let dir = dir.clone();
                    match self.selected_slot().filter(|&i| {
                        self.slots[i]
                            .as_ref()
                            .is_some_and(|s| s.kind == SynthKind::Kit)
                    }) {
                        Some(i) => {
                            if let Err(e) = self.load_kit_folder(i, &dir) {
                                self.error(e);
                                return;
                            }
                            false
                        }
                        None => true,
                    }
                }
                ListAction::Close => false,
                ListAction::Stay => {
                    if key.code == KeyCode::Backspace
                        && let Some(up) = entries
                            .first()
                            .filter(|e| e.name == "..")
                            .map(|e| e.path.clone())
                    {
                        self.open_files(up);
                        return;
                    }
                    true
                }
            },
        };
        if keep && self.popup.is_none() {
            self.popup = Some(popup);
        }
    }
}

/// The patches shown in the browser for a kind filter (`None` = all kinds).
pub fn patch_view(all: &[PatchEntry], filter: Option<SynthKind>) -> Vec<&PatchEntry> {
    all.iter()
        .filter(|e| filter.is_none_or(|k| e.patch.kind == k))
        .collect()
}

/// Keys that act again while held (terminals that report repeats separately
/// from presses). Actions like Enter, Esc or Space only fire once.
fn repeatable(code: KeyCode, typing: bool) -> bool {
    match code {
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::PageUp
        | KeyCode::PageDown => true,
        // Fine parameter steps.
        KeyCode::Char(',') | KeyCode::Char('.') => true,
        KeyCode::Char(_) | KeyCode::Backspace => typing,
        _ => false,
    }
}

enum ListAction {
    Stay,
    Select,
    /// Space: act without closing (audition a patch, toggle a port).
    Preview,
    Close,
}

fn list_nav(code: KeyCode, cursor: &mut usize, len: usize) -> ListAction {
    match code {
        KeyCode::Up => *cursor = cursor.saturating_sub(1),
        KeyCode::Down => *cursor = (*cursor + 1).min(len.saturating_sub(1)),
        KeyCode::PageUp => *cursor = cursor.saturating_sub(10),
        KeyCode::PageDown => *cursor = (*cursor + 10).min(len.saturating_sub(1)),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = len.saturating_sub(1),
        KeyCode::Enter => return ListAction::Select,
        KeyCode::Char(' ') => return ListAction::Preview,
        KeyCode::Esc | KeyCode::Char('q') => return ListAction::Close,
        _ => {}
    }
    ListAction::Stay
}

pub fn format_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn channel_label(ch: Option<u8>) -> String {
    match ch {
        None => "omni".into(),
        Some(c) => format!("ch {}", c + 1),
    }
}

mod tools;

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::engine::Telemetry;
    use crate::midi::Sink;

    pub fn test_app(dir: &Path) -> App {
        let commands: CommandQueue = Arc::new(ArrayQueue::new(4096));
        let midi_in = Arc::new(ArrayQueue::new(64));
        let midi = MidiManager::new(Sink {
            engine: commands.clone(),
            ui: midi_in.clone(),
        });
        App::new(AppInit {
            sample_rate: 48_000.0,
            device_name: "Test Device".into(),
            storage: Storage::new(dir.to_path_buf()).unwrap(),
            builtins: sample::builtins(),
            commands,
            garbage: Arc::new(ArrayQueue::new(64)),
            telemetry: Arc::new(Telemetry::default()),
            midi_in,
            errors: Arc::new(ArrayQueue::new(8)),
            midi,
            has_key_release: false,
        })
    }

    pub fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arkeology-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{temp_dir, test_app};
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_and_navigates() {
        let dir = temp_dir("ui");
        let mut app = test_app(&dir);
        app.default_rack();
        let s = screen(&mut app, 150, 42);
        assert!(s.contains("E.Piano"), "{s}");
        assert!(s.contains("Op 1 · carrier"), "{s}");
        if std::env::var("SHOW_UI").is_ok() {
            println!("{s}");
        }

        // The 303 slot renders its own sections.
        app.select(Target::Slot(2));
        let s = screen(&mut app, 150, 42);
        assert!(s.contains("Accent & Slide"), "{s}");
        app.on_key(key(KeyCode::Char('l')));
        let b = screen(&mut app, 150, 42);
        if std::env::var("SHOW_UI").is_ok() {
            println!("{s}\n{b}");
        }
        app.on_key(key(KeyCode::Esc));
        app.select(Target::Slot(3));
        let s = screen(&mut app, 150, 42);
        assert!(s.contains("Kick · 36 C2"), "{s}");
        if std::env::var("SHOW_UI").is_ok() {
            println!("{s}");
        }
        // The analog synth's page lays out all of its sections.
        app.add_synth(SynthKind::Analog);
        let idx = app.selected_slot().unwrap();
        let s = screen(&mut app, 150, 42);
        assert!(
            s.contains("Oscillators") && s.contains("Filter Env") && s.contains("Osc 2 Detune"),
            "{s}"
        );
        if std::env::var("SHOW_UI").is_ok() {
            println!("{s}");
        }
        app.remove_slot(idx);

        // A sampler shows its waveform; slice mode labels slices with notes.
        app.add_synth(SynthKind::Sampler);
        let idx = app.selected_slot().unwrap();
        let mode = SynthKind::Sampler.index_of("mode").unwrap();
        let src = SynthKind::Sampler.index_of("source").unwrap();
        let slice_by = SynthKind::Sampler.index_of("slice_by").unwrap();
        app.set_param(Target::Slot(idx), src, 4.0);
        app.set_param(Target::Slot(idx), mode, 2.0);
        app.set_param(Target::Slot(idx), slice_by, 1.0);
        let s = screen(&mut app, 150, 42);
        assert!(
            s.contains("Pluck") && s.contains("slices · notes 36"),
            "{s}"
        );
        if std::env::var("SHOW_UI").is_ok() {
            println!("{s}");
        }
        app.remove_slot(idx);
        app.select(Target::Slot(0));

        // Move to the granular slot and edit a parameter.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected(), Target::Slot(1));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Right));
        let vol = app.param_value(Target::Slot(1), crate::synth::VOLUME);
        assert!((vol - 0.76).abs() < 1e-4, "{vol}");
        for _ in 0..12 {
            app.on_key(key(KeyCode::Down));
        }
        let s = screen(&mut app, 150, 42);
        if std::env::var("SHOW_UI").is_ok() {
            println!("{s}");
        }

        // Save the patch and the session, add a synth, reload the session.
        app.on_key(key(KeyCode::Char('w')));
        app.on_key(key(KeyCode::Enter));
        assert!(dir.join("patches/Choir Cloud.json").exists());
        app.on_key(key(KeyCode::Char('W')));
        for c in "song".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(dir.join("sessions/song.json").exists());
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.rack_rows().len(), 6);
        app.load_session_file(&dir.join("sessions/song.json"));
        assert_eq!(app.rack_rows().len(), 5);
        assert_eq!(
            app.slots[3].as_ref().unwrap().channel,
            Some(9),
            "drums default to channel 10"
        );

        // The browser lists built-in patches for the slot's synth type, plus
        // the copy saved above; tab cycles the type filter.
        app.on_key(key(KeyCode::Char('l')));
        let Some(Popup::Patches { all, filter, .. }) = &app.popup else {
            panic!("no browser")
        };
        assert_eq!(*filter, Some(SynthKind::Fm));
        assert!(
            all.iter()
                .any(|e| e.is_factory() && e.patch.name == "FM Strings")
        );
        assert!(
            all.iter()
                .any(|e| !e.is_factory() && e.patch.name == "Choir Cloud")
        );
        let fm_count = patch_view(all, *filter).len();
        app.on_key(key(KeyCode::Tab));
        let Some(Popup::Patches { all, filter, .. }) = &app.popup else {
            panic!("no browser")
        };
        assert_eq!(*filter, Some(SynthKind::Analog));
        assert_ne!(patch_view(all, *filter).len(), fm_count);
        // Tab on to "all synths", then load "FM Strings" into slot 1.
        while !matches!(&app.popup, Some(Popup::Patches { filter: None, .. })) {
            app.on_key(key(KeyCode::Tab));
        }
        let Some(Popup::Patches { all, filter, .. }) = &app.popup else {
            panic!("no browser")
        };
        let pos = patch_view(all, *filter)
            .iter()
            .position(|e| e.patch.name == "FM Strings")
            .unwrap();
        for _ in 0..pos {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.slots[0].as_ref().unwrap().name, "FM Strings");
        assert!((app.param_value(Target::Slot(1), crate::synth::VOLUME) - 0.76).abs() < 1e-4);

        // Popups render.
        for k in ['?', 'l', 'p'] {
            app.on_key(key(KeyCode::Char(k)));
            let s = screen(&mut app, 150, 42);
            if std::env::var("SHOW_UI").is_ok() {
                println!("{s}");
            }
            app.on_key(key(KeyCode::Esc));
            assert!(app.popup.is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_a_sample_sets_root_from_detected_pitch() {
        let dir = temp_dir("pitch");
        let mut app = test_app(&dir);
        app.add_synth(SynthKind::Sampler);
        let slot = app.selected_slot().unwrap();
        // One second of A4 played 25 cents flat.
        let path = dir.join("samples/flat-a4.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        let freq = 440.0 * 2f32.powf(-0.25 / 12.0);
        for i in 0..44_100 {
            let t = i as f32 / 44_100.0;
            let v = (std::f32::consts::TAU * freq * t).sin() * 0.5
                + (std::f32::consts::TAU * 2.0 * freq * t).sin() * 0.2;
            w.write_sample((v * 32767.0) as i16).unwrap();
        }
        w.finalize().unwrap();

        app.load_sample(slot, 0, &path);
        let p = |key: &str| {
            app.param_value(
                Target::Slot(slot),
                SynthKind::Sampler.index_of(key).unwrap(),
            )
        };
        assert_eq!(p("root"), 69.0);
        assert!((p("tune") - 25.0).abs() <= 1.0, "tune {}", p("tune"));
        assert_eq!(p("source"), 0.0, "source switched to File");
        let status = app.status.as_ref().unwrap().text.clone();
        assert!(status.contains("Root Note A4"), "{status}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn held_keys_repeat_in_lists_but_not_for_actions() {
        let dir = temp_dir("repeat");
        let mut app = test_app(&dir);
        app.default_rack();
        let repeat = |code| KeyEvent {
            kind: KeyEventKind::Repeat,
            ..key(code)
        };

        // Holding Down scrolls the patch browser.
        app.on_key(key(KeyCode::Char('l')));
        let cursor = |app: &App| match &app.popup {
            Some(Popup::Patches { cursor, .. }) => *cursor,
            _ => panic!("browser closed"),
        };
        let start = cursor(&app);
        for _ in 0..3 {
            app.on_key(repeat(KeyCode::Down));
        }
        assert_eq!(cursor(&app), start + 3);
        // A repeated Enter doesn't load anything; a press does.
        app.on_key(repeat(KeyCode::Enter));
        assert!(app.popup.is_some());
        app.on_key(key(KeyCode::Esc));

        // Holding a key types repeatedly in text fields, and Backspace deletes.
        app.on_key(key(KeyCode::Char('W')));
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(repeat(KeyCode::Char('a')));
        app.on_key(repeat(KeyCode::Char('a')));
        app.on_key(repeat(KeyCode::Backspace));
        match &app.popup {
            Some(Popup::Text { input, .. }) => assert_eq!(input, "aa"),
            _ => panic!("text popup closed"),
        }
        app.on_key(key(KeyCode::Esc));

        // Holding Down in the rack keeps moving too.
        app.rack_cursor = 0;
        app.on_key(repeat(KeyCode::Down));
        app.on_key(repeat(KeyCode::Down));
        assert_eq!(app.rack_cursor, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// X11 auto-repeat sends release+press pairs for a held key: that must
    /// hold the note, while a real key-up still ends it.
    #[test]
    fn held_key_survives_autorepeat_release_pairs() {
        let dir = temp_dir("autorepeat");
        let mut app = test_app(&dir);
        app.default_rack();
        app.keyboard.has_release = true;
        app.on_key(key(KeyCode::Char('k')));
        while app.commands.pop().is_some() {}
        let notes = |app: &mut App| {
            let (mut on, mut off) = (0, 0);
            while let Some(cmd) = app.commands.pop() {
                on += matches!(cmd, Command::NoteOn { .. }) as usize;
                off += matches!(cmd, Command::NoteOff { .. }) as usize;
            }
            (on, off)
        };
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..key(KeyCode::Char('a'))
        };

        app.on_key(key(KeyCode::Char('a')));
        for _ in 0..20 {
            app.on_key(release);
            app.on_key(key(KeyCode::Char('a')));
            app.release_keyboard_notes(Instant::now());
        }
        assert_eq!(notes(&mut app), (1, 0), "auto-repeat must not retrigger");

        app.on_key(release);
        app.release_keyboard_notes(Instant::now());
        assert_eq!(notes(&mut app), (0, 0), "still within the grace period");
        app.release_keyboard_notes(Instant::now() + RELEASE_GRACE);
        assert_eq!(notes(&mut app), (0, 1), "a real key-up ends the note");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn insert_fx_build_show_and_save() {
        let dir = temp_dir("fx");
        let mut app = test_app(&dir);
        app.default_rack();
        let t = Target::Slot(0);
        let key = |k: &str| SynthKind::Fm.index_of(k).unwrap();
        while app.commands.pop().is_some() {}

        // Choosing a type swaps in a built unit and applies that effect's usual mix.
        app.set_param(t, key("fx1_type"), 1.0);
        let mut swapped = false;
        while let Some(cmd) = app.commands.pop() {
            swapped |= matches!(
                cmd,
                Command::SetFx {
                    slot: 0,
                    unit: 0,
                    ..
                }
            );
        }
        assert!(swapped, "no SetFx sent");
        assert_eq!(app.param_value(t, key("fx1_mix")), 0.3);
        // Re-setting the same type keeps a user's mix.
        app.set_param(t, key("fx1_mix"), 0.8);
        app.set_param(t, key("fx1_type"), 1.0);
        assert_eq!(app.param_value(t, key("fx1_mix")), 0.8);

        // Only the active type's settings are visible.
        assert!(app.param_visible(t, key("fx1_delay_time")));
        assert!(!app.param_visible(t, key("fx1_reverb_size")));
        assert!(!app.param_visible(t, key("fx2_mix")), "unit 2 is off");
        let s = screen(&mut app, 150, 60);
        assert!(
            s.contains("FX 1 · Delay") && s.contains("FX 2 · Off"),
            "{s}"
        );

        // Saved patches keep the active FX and omit hidden settings.
        app.set_param(t, key("fx1_delay_feedback"), 0.6);
        let patch = app.current_patch(0).unwrap();
        assert_eq!(patch.params.get("fx1_delay_feedback"), Some(&0.6));
        assert!(!patch.params.contains_key("fx1_reverb_size"));
        assert_eq!(patch.values()[key("fx1_delay_feedback")], 0.6);

        // Master FX work the same way and are saved with the session.
        let m = master::FX_BASE + fx::STRIDE; // master FX 2 type
        app.set_param(Target::Master, m, 8.0); // EQ
        assert!(app.session().master.contains_key("fx2_eq_low"));
        assert!(!app.session().master.contains_key("fx1_mix"));

        // MCP: types by name, settings appear once the type is set.
        app.run_tool("set_params", &serde_json::json!({ "slot": 1, "values": { "fx2_type": "reverb", "fx2_reverb_size": "90%" } }))
            .unwrap();
        let params = app
            .run_tool("get_params", &serde_json::json!({ "slot": 1 }))
            .unwrap();
        let keys: Vec<&str> = params["params"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["key"].as_str().unwrap())
            .collect();
        assert!(keys.contains(&"fx2_reverb_size") && !keys.contains(&"fx2_delay_time"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn physical_page_shows_only_the_selected_models_controls() {
        let dir = temp_dir("phys-ui");
        let mut app = test_app(&dir);
        app.add_synth(SynthKind::Physical);
        let i = app.selected_slot().unwrap();
        let t = Target::Slot(i);
        let key = |k: &str| SynthKind::Physical.index_of(k).unwrap();
        let shown = |app: &App, k: &str| app.param_visible(t, key(k));
        app.set_param(t, key("model"), 2.0); // Piano
        assert!(
            shown(&app, "unison")
                && shown(&app, "hardness")
                && !shown(&app, "bow_pressure")
                && !shown(&app, "material")
        );
        let s = screen(&mut app, 150, 50);
        assert!(
            s.contains("Unison Detune") && !s.contains("Bow Pressure"),
            "{s}"
        );
        app.set_param(t, key("model"), 3.0); // Bowed
        assert!(
            shown(&app, "bow_pressure")
                && shown(&app, "vibrato")
                && !shown(&app, "hardness")
                && !shown(&app, "decay")
        );
        let s = screen(&mut app, 150, 50);
        assert!(
            s.contains("Bow Pressure") && s.contains("Instrument") && !s.contains("Hardness"),
            "{s}"
        );
        // Saved patches leave out the other models' settings.
        let patch = app.current_patch(i).unwrap();
        assert!(
            patch.params.contains_key("bow_pressure") && !patch.params.contains_key("hardness")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_captures_the_master_output() {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        let dir = temp_dir("record");
        let mut app = test_app(&dir);
        app.default_rack();
        // A real engine on its own thread, fed by the app's command queue.
        let mut engine = crate::engine::Engine::new(
            48_000.0,
            app.commands.clone(),
            app.garbage.clone(),
            app.telemetry.clone(),
        );
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let audio = std::thread::spawn(move || {
            let mut buf = vec![0.0f32; 256 * 2];
            while flag.load(Relaxed) {
                engine.process(&mut buf, 2);
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        let started = app
            .run_tool("start_recording", &serde_json::json!({ "name": "take" }))
            .unwrap();
        assert!(
            started["recording"]
                .as_str()
                .unwrap()
                .ends_with("recordings/take.wav")
        );
        app.send(Command::NoteOn {
            slot: 0,
            note: 60,
            velocity: 1.0,
        });
        std::thread::sleep(Duration::from_millis(400));
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("● REC 0:00"), "{s}");
        assert!(app.run_tool("get_rack", &serde_json::json!({})).unwrap()["recording"].is_object());

        let stopped = app
            .run_tool("stop_recording", &serde_json::json!({}))
            .unwrap();
        assert_eq!(stopped["finalized"], true, "{stopped}");
        let path = PathBuf::from(stopped["saved"].as_str().unwrap());
        let mut wav = hound::WavReader::open(&path).unwrap();
        assert_eq!((wav.spec().channels, wav.spec().sample_rate), (2, 48_000));
        let samples: Vec<f32> = wav.samples::<f32>().map(Result::unwrap).collect();
        let secs = samples.len() as f64 / 2.0 / 48_000.0;
        assert!(
            (secs - stopped["seconds"].as_f64().unwrap()).abs() < 0.01,
            "{secs}s on disk vs {stopped}"
        );
        assert!(
            samples.iter().any(|v| v.abs() > 0.01),
            "the note isn't in the recording"
        );
        assert!(
            app.recording.is_none()
                && app
                    .run_tool("stop_recording", &serde_json::json!({}))
                    .is_err()
        );

        // Another take with the same name doesn't overwrite the first.
        let again = app.start_recording(Some("take")).unwrap();
        assert!(again.ends_with("take (2).wav"));
        app.finish_recording(Duration::from_secs(3));
        assert!(again.exists() && path.exists());

        running.store(false, Relaxed);
        audio.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_rack_resets_after_saving_the_old_one() {
        let dir = temp_dir("new-rack");
        let mut app = test_app(&dir);
        app.default_rack();
        app.set_param(Target::Master, master::VOLUME, 0.3);
        app.set_param(Target::Master, master::FX_BASE + fx::TYPE, 1.0); // master delay
        app.cc_map.push(CcMapping {
            channel: 0,
            cc: 74,
            slot: Some(0),
            param: "volume".into(),
        });

        // N defaults to saving first when the rack has synths.
        app.on_key(key(KeyCode::Char('N')));
        assert!(matches!(
            app.popup,
            Some(Popup::NewRack {
                cursor: 0,
                save: true
            })
        ));
        let s = screen(&mut app, 120, 40);
        assert!(
            s.contains("Starter rack") && s.contains("save current rack"),
            "{s}"
        );
        app.on_key(key(KeyCode::Enter)); // empty rack
        assert!(!app.has_synths() && app.cc_map.is_empty());
        assert_eq!(
            app.master[master::VOLUME],
            master::PARAMS[master::VOLUME].default
        );
        assert_eq!(app.master[master::FX_BASE + fx::TYPE], 0.0);
        let saved = app.storage.list_sessions();
        assert_eq!(saved.len(), 1);
        let old: Session = read_json(&saved[0]).unwrap();
        assert_eq!(old.slots.len(), 4);
        assert_eq!(old.cc_map.len(), 1);

        // Starter template, without saving the (empty) rack.
        app.on_key(key(KeyCode::Char('N')));
        assert!(
            matches!(app.popup, Some(Popup::NewRack { save: false, .. })),
            "nothing to save"
        );
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.rack_rows().len(), 5);
        assert_eq!(app.storage.list_sessions().len(), 1);

        // MCP: save under a name, start empty.
        let r = app
            .run_tool(
                "new_rack",
                &serde_json::json!({ "template": "empty", "save_as": "before" }),
            )
            .unwrap();
        assert!(r["slots"].as_array().unwrap().is_empty());
        assert!(
            r["previous_rack_saved_as"]
                .as_str()
                .unwrap()
                .ends_with("sessions/before.json")
        );
        assert!(
            app.run_tool("new_rack", &serde_json::json!({ "template": "huge" }))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_wav(path: &Path, freq: f32, seconds: f32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(44_100.0 * seconds) as usize {
            let t = i as f32 / 44_100.0;
            let v = (std::f32::consts::TAU * freq * t).sin() * (-t * 8.0).exp();
            w.write_sample((v * 30_000.0) as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn kits_load_folders_save_and_play() {
        let dir = temp_dir("kit");
        let mut app = test_app(&dir);
        let folder = dir.join("samples/My Kit");
        std::fs::create_dir_all(&folder).unwrap();
        for (name, f) in [
            ("BD 808.wav", 60.0),
            ("Snare.wav", 200.0),
            ("hh closed.wav", 900.0),
            ("Open HH.wav", 800.0),
            ("zap.wav", 400.0),
        ] {
            write_wav(&folder.join(name), f, 0.3);
        }
        app.add_synth(SynthKind::Kit);
        let i = app.selected_slot().unwrap();
        assert_eq!(
            app.slots[i].as_ref().unwrap().channel,
            Some(9),
            "kits default to channel 10"
        );

        // Browse to the folder and press K.
        app.on_key(key(KeyCode::Char('f')));
        app.open_files(folder.clone());
        let s = screen(&mut app, 140, 40);
        assert!(
            s.contains("Load into pad 1 (Kick)") && s.contains("K: load this whole folder"),
            "{s}"
        );
        app.on_key(key(KeyCode::Char('K')));
        assert!(app.popup.is_none());
        let slot = app.slots[i].as_ref().unwrap();
        let name = |p: usize| {
            slot.sample(p)
                .map(|l| l.path.file_name().unwrap().to_string_lossy().into_owned())
        };
        assert_eq!(name(0).as_deref(), Some("BD 808.wav"));
        assert_eq!(name(1).as_deref(), Some("Snare.wav"));
        assert_eq!(name(2).as_deref(), Some("hh closed.wav"));
        assert_eq!(name(3).as_deref(), Some("Open HH.wav"));
        assert_eq!(
            name(4).as_deref(),
            Some("zap.wav"),
            "unrecognised file fills the next free pad"
        );
        let s = screen(&mut app, 140, 60);
        assert!(
            s.contains("Pad 1 · 36 C2 · BD 808") && s.contains("Pad 10 · 49 C#3 · empty (Crash)"),
            "{s}"
        );

        // Patches remember every pad's sample and restore them.
        let patch = app.current_patch(i).unwrap();
        assert_eq!(patch.samples.len(), 5);
        app.storage
            .save_patch(&Patch {
                name: "My Kit".into(),
                ..patch.clone()
            })
            .unwrap();
        app.new_rack(false, None).unwrap();
        app.add_synth(SynthKind::Fm);
        let j = app.selected_slot().unwrap();
        app.run_tool(
            "load_patch",
            &serde_json::json!({ "slot": j + 1, "name": "My Kit" }),
        )
        .unwrap();
        let restored = app.slots[j].as_ref().unwrap();
        assert_eq!(restored.kind, SynthKind::Kit);
        assert_eq!(restored.samples.iter().flatten().count(), 5);

        // MCP: pads are listed, single-pad loads need a pad number.
        let rack = app.run_tool("get_rack", &serde_json::json!({})).unwrap();
        let pads = rack["slots"][0]["pads"].as_array().unwrap();
        assert_eq!(pads[2]["role"], "Closed Hat");
        assert!(
            pads[2]["sample"]
                .as_str()
                .unwrap()
                .ends_with("hh closed.wav")
        );
        let clap = folder.join("Snare.wav").to_string_lossy().into_owned();
        assert!(
            app.run_tool(
                "load_sample",
                &serde_json::json!({ "slot": j + 1, "path": clap })
            )
            .is_err()
        );
        app.run_tool(
            "load_sample",
            &serde_json::json!({ "slot": j + 1, "path": clap, "pad": 5 }),
        )
        .unwrap();
        let r = app
            .run_tool(
                "load_kit_folder",
                &serde_json::json!({ "slot": j + 1, "path": folder }),
            )
            .unwrap();
        assert_eq!(r["loaded"].as_array().unwrap().len(), 5);

        // And the kit plays through the engine.
        let mut engine = crate::engine::Engine::new(
            48_000.0,
            app.commands.clone(),
            app.garbage.clone(),
            app.telemetry.clone(),
        );
        app.send(Command::NoteOn {
            slot: j,
            note: 36,
            velocity: 1.0,
        });
        let mut out = vec![0.0f32; 2 * 9_600];
        engine.process(&mut out, 2);
        assert!(out.iter().any(|v| v.abs() > 0.05), "kick pad silent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vcsl_kits_explain_missing_files_then_load() {
        let dir = temp_dir("vcsl");
        let mut app = test_app(&dir);
        app.add_synth(SynthKind::Kit);
        let i = app.selected_slot().unwrap();
        let patch = crate::patch::factory_patches()
            .into_iter()
            .find(|p| p.name == "VCSL Acoustic Kit")
            .unwrap();

        // Not downloaded yet: the browser says so, and loading explains how to fix it.
        app.on_key(key(KeyCode::Char('l')));
        let s = screen(&mut app, 140, 60);
        assert!(
            s.contains("VCSL Acoustic Kit") && s.contains("download"),
            "{s}"
        );
        app.on_key(key(KeyCode::Esc));
        app.load_patch_into(i, patch.clone());
        let status = app
            .status
            .as_ref()
            .map(|s| s.text.clone())
            .unwrap_or_default();
        assert!(status.contains("fetch-kits"), "{status}");

        // Once the files exist (here: stand-ins), every pad loads from the relative paths.
        for path in patch.sample_paths().into_iter().flatten() {
            let full = app.storage.samples_dir().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            write_wav(&full, 220.0, 0.1);
        }
        app.load_patch_into(i, patch);
        assert_eq!(
            app.slots[i]
                .as_ref()
                .unwrap()
                .samples
                .iter()
                .flatten()
                .count(),
            16
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn midi_learn_maps_cc() {
        let dir = temp_dir("learn");
        let mut app = test_app(&dir);
        app.default_rack();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('c')));
        app.on_midi(MidiMsg {
            channel: 3,
            kind: MidiKind::Cc { cc: 74, value: 0 },
        });
        assert_eq!(app.cc_map.len(), 1);
        app.on_midi(MidiMsg {
            channel: 3,
            kind: MidiKind::Cc { cc: 74, value: 127 },
        });
        assert_eq!(app.param_value(Target::Slot(0), 0), 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
