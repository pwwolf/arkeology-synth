//! UI-thread application state: the model of the rack, input handling, and
//! all communication with the audio engine and MIDI layer.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::engine::{self, Command, CommandQueue, GarbageQueue, MAX_SLOTS, Slot, Telemetry};
use crate::midi::{MidiKind, MidiManager, MidiMsg, note_name};
use crate::params::{ParamDesc, StepSize, master};
use crate::patch::{CcMapping, Patch, PatchEntry, Session, SessionSlot, Storage, master_index, read_json};
use crate::sample::{self, Builtins, Sample};
use crate::synth::{SynthKind, granular, sampler};

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
    pub sample_path: Option<PathBuf>,
    /// Kept so the sample is freed on this thread, never the audio thread.
    pub sample: Option<Arc<Sample>>,
    pub meter: f32,
    pub voices: u32,
    notes_seen: u32,
    pub activity: Option<Instant>,
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
    AddSynth { cursor: usize },
    ConfirmRemove { slot: usize },
    Text { title: String, hint: String, input: String, action: TextAction },
    Patches { all: Vec<PatchEntry>, filter: Option<SynthKind>, cursor: usize },
    Sessions { items: Vec<PathBuf>, cursor: usize },
    Ports { items: Vec<String>, cursor: usize },
    Files { dir: PathBuf, entries: Vec<FileEntry>, cursor: usize },
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
    held: HashMap<char, (usize, u8, Instant)>,
}

/// Legacy terminals don't report key-up, so notes are released after this
/// long without a key repeat.
const LEGACY_NOTE_HOLD: Duration = Duration::from_millis(600);

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
        }
    }

    // -----------------------------------------------------------------------
    // Messaging helpers
    // -----------------------------------------------------------------------

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), error: false, at: Instant::now() });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), error: true, at: Instant::now() });
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
        if src == 0 { s.sample.clone() } else { self.builtins.get(src - 1).cloned() }
    }

    /// Lowest playable note for synths that aren't laid out chromatically.
    fn fixed_keyboard_base(&self, index: usize) -> Option<i32> {
        let s = self.slots[index].as_ref()?;
        match s.kind {
            SynthKind::Drums => Some(36),
            SynthKind::Sampler => {
                let p = &s.params[crate::synth::COMMON.len()..];
                (p[sampler::MODE].round() as usize == 2).then(|| p[sampler::BASE_NOTE].round() as i32)
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
        match t {
            Target::Master => {
                self.master[i] = value;
                self.send(Command::SetMaster { index: i, value });
            }
            Target::Slot(s) => {
                if let Some(slot) = self.slots[s].as_mut() {
                    slot.params[i] = value;
                    self.send(Command::SetParam { slot: s, index: i, value });
                }
            }
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
        self.cc_map.iter().find(|m| m.slot == slot && m.param == key)
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
        sample_path: Option<PathBuf>,
        mute: bool,
        solo: bool,
    ) {
        let mut sample = None;
        if matches!(kind, SynthKind::Granular | SynthKind::Sampler)
            && let Some(path) = &sample_path
        {
            match sample::load_file(path) {
                Ok(s) => sample = Some(Arc::new(s)),
                Err(e) => self.error(format!("sample: {e:#}")),
            }
        }
        let data = Slot::new(kind, values.clone(), channel, self.sample_rate, &self.builtins, sample.clone())
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
            sample_path: if sample.is_some() { sample_path } else { None },
            sample,
            meter: 0.0,
            voices: 0,
            notes_seen,
            activity: None,
        });
    }

    pub fn add_synth(&mut self, kind: SynthKind) {
        let Some(index) = self.free_slot() else {
            self.error(format!("all {MAX_SLOTS} slots are in use"));
            return;
        };
        let ch10_free = !self.slots.iter().flatten().any(|s| s.channel == Some(9));
        let channel = if kind == SynthKind::Drums && ch10_free { Some(9) } else { self.free_channel() };
        let name = format!("{} {}", kind.label(), index + 1);
        self.install(index, kind, &name, kind.defaults(), channel, None, false, false);
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
        self.keyboard.held.retain(|_, (s, _, _)| *s != index);
        let rows = self.rack_rows().len();
        self.rack_cursor = self.rack_cursor.min(rows - 1);
        self.param_cursor = 0;
        self.param_scroll = 0;
        self.info(format!("removed slot {}", index + 1));
    }

    fn change_channel(&mut self, index: usize, dir: i32) {
        let Some(slot) = self.slots[index].as_mut() else { return };
        // Cycle: omni, 1..16
        let pos = slot.channel.map_or(0, |c| c as i32 + 1);
        let next = (pos + dir).rem_euclid(17);
        slot.channel = if next == 0 { None } else { Some((next - 1) as u8) };
        let channel = slot.channel;
        self.send(Command::SetChannel { slot: index, channel });
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
        Some(Patch::from_values(&s.name, s.kind, &s.params, s.sample_path.clone()))
    }

    fn load_patch_into(&mut self, index: usize, patch: Patch) {
        let (channel, mute, solo) = self.slots[index]
            .as_ref()
            .map_or((self.free_channel(), false, false), |s| (s.channel, s.mute, s.solo));
        let values = patch.values();
        self.install(index, patch.kind, &patch.name, values, channel, patch.sample.clone(), mute, solo);
        self.info(format!("loaded patch '{}' into slot {}", patch.name, index + 1));
    }

    fn save_patch(&mut self, name: &str) {
        let Some(index) = self.selected_slot() else { return };
        if let Some(s) = self.slots[index].as_mut() {
            s.name = name.to_string();
        }
        let Some(patch) = self.current_patch(index) else { return };
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
                .map(|(p, v)| (p.key.to_string(), *v))
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
            self.install(
                s.index,
                s.patch.kind,
                &s.patch.name,
                values,
                channel,
                s.patch.sample.clone(),
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
        let rack = [("E.Piano", 0u8), ("Choir Cloud", 1), ("Acid Classic", 2), ("808 Kit", 9)];
        for (i, (name, channel)) in rack.iter().enumerate() {
            if let Some(p) = find(name) {
                let values = p.values();
                self.install(i, p.kind, &p.name, values, Some(*channel), None, false, false);
            }
        }
        self.rack_cursor = 1;
    }

    fn load_sample(&mut self, index: usize, path: &Path) {
        match sample::load_file(path) {
            Ok(s) => {
                let dur = s.duration();
                let pitch = s.pitch;
                let arc = Arc::new(s);
                self.send(Command::SetSample { slot: index, sample: Some(arc.clone()) });
                if let Some(slot) = self.slots[index].as_mut() {
                    slot.sample = Some(arc);
                    slot.sample_path = Some(path.to_path_buf());
                }
                // Switch the source to "File" so the new sample is heard.
                let kind = self.slots[index].as_ref().map(|s| s.kind);
                if let Some(src) = kind.and_then(|k| k.index_of("source")) {
                    let file = granular::SOURCES.iter().position(|s| *s == "File").unwrap_or(0);
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
            format!(" · pitch {name} {cents:+.0} ct: Root Note {name}, Tune {:+.0} ct", -cents)
        }
    }

    fn open_files(&mut self, dir: PathBuf) {
        let mut entries = Vec::new();
        if let Some(parent) = dir.parent() {
            entries.push(FileEntry { name: "..".into(), path: parent.to_path_buf(), is_dir: true });
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
                        (is_dir || sample::is_audio_file(&path)).then_some(FileEntry { name, path, is_dir })
                    })
                    .collect()
            })
            .unwrap_or_default();
        list.sort_by_key(|e| (!e.is_dir, e.name.to_lowercase()));
        entries.extend(list);
        self.popup = Some(Popup::Files { dir, entries, cursor: 0 });
    }

    // -----------------------------------------------------------------------
    // Periodic work
    // -----------------------------------------------------------------------

    pub fn tick(&mut self) {
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

        if !self.keyboard.has_release {
            let now = Instant::now();
            let expired: Vec<char> = self
                .keyboard
                .held
                .iter()
                .filter(|(_, (_, _, at))| now.duration_since(*at) > LEGACY_NOTE_HOLD)
                .map(|(c, _)| *c)
                .collect();
            for c in expired {
                if let Some((slot, note, _)) = self.keyboard.held.remove(&c) {
                    self.send(Command::NoteOff { slot, note });
                }
            }
        }

        if let Some(s) = &self.status
            && s.at.elapsed() > Duration::from_secs(if s.error { 10 } else { 5 })
        {
            self.status = None;
        }
    }

    fn on_midi(&mut self, msg: MidiMsg) {
        self.midi_log.push_front(msg.to_string());
        self.midi_log.truncate(64);
        let MidiKind::Cc { cc, value } = msg.kind else { return };
        if let Some((target, index)) = self.learning.take() {
            let slot = match target {
                Target::Master => None,
                Target::Slot(s) => Some(s),
            };
            let param = self.param_key(target, index).to_string();
            self.cc_map.retain(|m| !(m.slot == slot && m.param == param));
            self.cc_map.push(CcMapping { channel: msg.channel, cc, slot, param });
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
                && let Some((slot, note, _)) = self.keyboard.held.remove(&c.to_ascii_lowercase())
            {
                self.send(Command::NoteOff { slot, note });
            }
            return;
        }

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }

        if self.popup.is_some() {
            if key.kind == KeyEventKind::Press {
                self.on_popup_key(key);
            }
            return;
        }

        if self.keyboard.enabled && self.on_play_key(key) {
            return;
        }
        if key.kind == KeyEventKind::Repeat && !matches!(key.code, KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Char(',') | KeyCode::Char('.')) {
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
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
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
            let Ok(note) = u8::try_from(note) else { return true };
            if note > 127 {
                return true;
            }
            let now = Instant::now();
            if let Some(entry) = self.keyboard.held.get_mut(&c) {
                // Key repeat: just keep the note alive.
                entry.2 = now;
                return true;
            }
            self.keyboard.held.insert(c, (slot, note, now));
            let velocity = self.keyboard.velocity as f32 / 127.0;
            self.send(Command::NoteOn { slot, note, velocity });
            return true;
        }
        if key.kind == KeyEventKind::Repeat {
            return c.is_ascii_lowercase();
        }
        match c {
            'z' => {
                self.keyboard.octave = (self.keyboard.octave - 1).max(-1);
                self.info(format!("octave {} (C = {})", self.keyboard.octave, note_name(((self.keyboard.octave + 1) * 12).clamp(0, 127) as u8)));
            }
            'x' => {
                self.keyboard.octave = (self.keyboard.octave + 1).min(8);
                self.info(format!("octave {} (C = {})", self.keyboard.octave, note_name(((self.keyboard.octave + 1) * 12).clamp(0, 127) as u8)));
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
        let held: Vec<_> = self.keyboard.held.drain().map(|(_, v)| v).collect();
        for (slot, note, _) in held {
            self.send(Command::NoteOff { slot, note });
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
                self.focus = if self.focus == Focus::Rack { Focus::Params } else { Focus::Rack };
            }
            KeyCode::Char(' ') => {
                self.all_keyboard_notes_off();
                self.send(Command::Panic);
                self.info("all notes off");
            }
            KeyCode::Char('k') => {
                self.keyboard.enabled = true;
                let how = if self.keyboard.has_release { "" } else { " (no key-up events in this terminal: notes auto-release)" };
                self.info(format!("keyboard play mode: a-' play notes, z/x octave, c/v velocity, Esc exits{how}"));
            }
            KeyCode::Char('a') => self.popup = Some(Popup::AddSynth { cursor: 0 }),
            KeyCode::Char('l') => {
                if slot.is_none() {
                    self.error("select a synth slot to load a patch into (or press 'a' to add one)");
                } else {
                    let all = self.storage.list_patches();
                    let current = slot.and_then(|i| self.slots[i].as_ref());
                    let filter = current.map(|s| s.kind);
                    let cursor = current
                        .and_then(|s| patch_view(&all, filter).iter().position(|e| e.patch.name == s.name))
                        .unwrap_or(0);
                    self.popup = Some(Popup::Patches { all, filter, cursor });
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
                Some((_, s)) if matches!(s.kind, SynthKind::Granular | SynthKind::Sampler) => {
                    let dir = s
                        .sample_path
                        .as_ref()
                        .and_then(|p| p.parent().map(Path::to_path_buf))
                        .unwrap_or_else(|| self.storage.samples_dir());
                    self.open_files(dir);
                }
                _ => self.error("select a granular synth or sampler to load a sample"),
            },
            KeyCode::Char('c') => {
                if self.param_count(target) > 0 {
                    let i = self.param_cursor.min(self.param_count(target) - 1);
                    self.learning = Some((target, i));
                    self.focus = Focus::Params;
                    self.info(format!("MIDI learn: move a knob/fader to map it to {}", self.param_desc(target, i).name));
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
            KeyCode::Left | KeyCode::Right | KeyCode::Char('-') | KeyCode::Char('=') | KeyCode::Char('+') => {
                if let Some(i) = self.selected_slot() {
                    let dir = if matches!(key.code, KeyCode::Left | KeyCode::Char('-')) { -1 } else { 1 };
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
        let cur = self.param_cursor.min(count - 1);
        let group_of = |app: &App, i: usize| app.param_desc(target, i).group;
        match key.code {
            KeyCode::Up => self.param_cursor = cur.saturating_sub(1),
            KeyCode::Down => self.param_cursor = (cur + 1).min(count - 1),
            KeyCode::Home => self.param_cursor = 0,
            KeyCode::End => self.param_cursor = count - 1,
            KeyCode::PageDown => {
                let g = group_of(self, cur);
                self.param_cursor = (cur..count).find(|&i| group_of(self, i) != g).unwrap_or(cur);
            }
            KeyCode::PageUp => {
                // Start of this group, or of the previous one if already at the start.
                let g = group_of(self, cur);
                let start = (0..=cur).rev().take_while(|&i| group_of(self, i) == g).last().unwrap_or(cur);
                self.param_cursor = if start == cur && cur > 0 {
                    let pg = group_of(self, cur - 1);
                    (0..cur).rev().take_while(|&i| group_of(self, i) == pg).last().unwrap_or(0)
                } else {
                    start
                };
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(',') | KeyCode::Char('.') => {
                let dir = if matches!(key.code, KeyCode::Left | KeyCode::Char(',')) { -1.0 } else { 1.0 };
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
        let Some(mut popup) = self.popup.take() else { return };
        let keep = match &mut popup {
            Popup::Help => false,
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
                            if let Some(s) = self.selected_slot().and_then(|i| self.slots[i].as_mut()) {
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
            Popup::Patches { all, filter, cursor } => {
                let chosen = patch_view(all, *filter).get(*cursor).map(|e| e.patch.clone());
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
            Popup::Files { entries, cursor, .. } => match list_nav(key.code, cursor, entries.len()) {
                ListAction::Select | ListAction::Preview => {
                    if let Some(e) = entries.get(*cursor) {
                        let path = e.path.clone();
                        if e.is_dir {
                            self.open_files(path);
                            return;
                        }
                        if let Some(i) = self.selected_slot() {
                            self.load_sample(i, &path);
                        }
                    }
                    false
                }
                ListAction::Close => false,
                ListAction::Stay => {
                    if key.code == KeyCode::Backspace
                        && let Some(up) = entries.first().filter(|e| e.name == "..").map(|e| e.path.clone())
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
    all.iter().filter(|e| filter.is_none_or(|k| e.patch.kind == k)).collect()
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

pub fn channel_label(ch: Option<u8>) -> String {
    match ch {
        None => "omni".into(),
        Some(c) => format!("ch {}", c + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Telemetry;
    use crate::midi::Sink;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }

    fn test_app(dir: &Path) -> App {
        let commands: CommandQueue = Arc::new(ArrayQueue::new(4096));
        let midi_in = Arc::new(ArrayQueue::new(64));
        let midi = MidiManager::new(Sink { engine: commands.clone(), ui: midi_in.clone() });
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

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arkeology-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
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
        assert!(s.contains("Pluck") && s.contains("slices · notes 36"), "{s}");
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
        assert_eq!(app.slots[3].as_ref().unwrap().channel, Some(9), "drums default to channel 10");

        // The browser lists built-in patches for the slot's synth type, plus
        // the copy saved above; tab cycles the type filter.
        app.on_key(key(KeyCode::Char('l')));
        let Some(Popup::Patches { all, filter, .. }) = &app.popup else { panic!("no browser") };
        assert_eq!(*filter, Some(SynthKind::Fm));
        assert!(all.iter().any(|e| e.is_factory() && e.patch.name == "FM Strings"));
        assert!(all.iter().any(|e| !e.is_factory() && e.patch.name == "Choir Cloud"));
        let fm_count = patch_view(all, *filter).len();
        app.on_key(key(KeyCode::Tab));
        let Some(Popup::Patches { all, filter, .. }) = &app.popup else { panic!("no browser") };
        assert_eq!(*filter, Some(SynthKind::Granular));
        assert_ne!(patch_view(all, *filter).len(), fm_count);
        // Tab on to "all synths", then load "FM Strings" into slot 1.
        while !matches!(&app.popup, Some(Popup::Patches { filter: None, .. })) {
            app.on_key(key(KeyCode::Tab));
        }
        let Some(Popup::Patches { all, filter, .. }) = &app.popup else { panic!("no browser") };
        let pos = patch_view(all, *filter).iter().position(|e| e.patch.name == "FM Strings").unwrap();
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
        let spec = hound::WavSpec { channels: 1, sample_rate: 44_100, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        let freq = 440.0 * 2f32.powf(-0.25 / 12.0);
        for i in 0..44_100 {
            let t = i as f32 / 44_100.0;
            let v = (std::f32::consts::TAU * freq * t).sin() * 0.5 + (std::f32::consts::TAU * 2.0 * freq * t).sin() * 0.2;
            w.write_sample((v * 32767.0) as i16).unwrap();
        }
        w.finalize().unwrap();

        app.load_sample(slot, &path);
        let p = |key: &str| app.param_value(Target::Slot(slot), SynthKind::Sampler.index_of(key).unwrap());
        assert_eq!(p("root"), 69.0);
        assert!((p("tune") - 25.0).abs() <= 1.0, "tune {}", p("tune"));
        assert_eq!(p("source"), 0.0, "source switched to File");
        let status = app.status.as_ref().unwrap().text.clone();
        assert!(status.contains("Root Note A4"), "{status}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn midi_learn_maps_cc() {
        let dir = temp_dir("learn");
        let mut app = test_app(&dir);
        app.default_rack();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('c')));
        app.on_midi(MidiMsg { channel: 3, kind: MidiKind::Cc { cc: 74, value: 0 } });
        assert_eq!(app.cc_map.len(), 1);
        app.on_midi(MidiMsg { channel: 3, kind: MidiKind::Cc { cc: 74, value: 127 } });
        assert_eq!(app.param_value(Target::Slot(0), 0), 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
