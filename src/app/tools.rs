//! MCP tool implementations. They run on the UI thread and go through the
//! same methods as keyboard edits, so the TUI always shows the result.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::{App, Target, channel_label};
use crate::engine::{Command, MAX_SLOTS};
use crate::midi::note_name;
use crate::fx;
use crate::params::{Kind, ParamDesc, Scale, Unit, master, parse_note_name};
use crate::patch::{CcMapping, file_stem, read_json};
use crate::synth::{self, SynthKind, physical};

type ToolResult = Result<Value, String>;

const MAX_NOTES: usize = 512;
const MAX_SCHEDULE: Duration = Duration::from_secs(60);

fn kind_name(kind: SynthKind) -> Value {
    serde_json::to_value(kind).unwrap_or(Value::Null)
}

fn parse_kind(v: &Value) -> Result<SynthKind, String> {
    serde_json::from_value(v.clone()).map_err(|_| {
        format!("unknown synth type {v}; use one of fm, analog, physical, granular, acid, drums, sampler")
    })
}

/// Parse a channel argument: 1-16 or "omni". Returns the 0-based channel.
fn parse_channel(v: &Value) -> Result<Option<u8>, String> {
    match v {
        Value::String(s) if s.eq_ignore_ascii_case("omni") => Ok(None),
        Value::Number(n) => match n.as_u64() {
            Some(c @ 1..=16) => Ok(Some(c as u8 - 1)),
            _ => Err(format!("channel must be 1-16 or \"omni\", got {n}")),
        },
        _ => Err(format!("channel must be 1-16 or \"omni\", got {v}")),
    }
}

fn parse_value(desc: &ParamDesc, v: &Value) -> Result<f32, String> {
    match v {
        Value::Number(n) => Ok(desc.clamp(n.as_f64().unwrap_or(f64::NAN) as f32)),
        Value::Bool(b) => Ok(if *b { desc.max } else { desc.min }),
        Value::String(s) => desc.parse(s).ok_or_else(|| match desc.kind {
            Kind::Enum(opts) => format!("'{s}' isn't one of {}", opts.join(", ")),
            _ => format!("couldn't read '{s}'"),
        }),
        _ => Err(format!("unsupported value {v}")),
    }
}

/// Static facts about a parameter, for clients deciding how to set or map it.
fn describe_param(d: &ParamDesc) -> Value {
    let kind = match d.kind {
        Kind::Float => "float",
        Kind::Int => "int",
        Kind::Enum(_) => "choice",
        Kind::Toggle => "toggle",
    };
    let unit = match d.unit {
        Unit::None => "none",
        Unit::Percent => "fraction (0-1, shown as %)",
        Unit::Seconds => "seconds",
        Unit::Hz => "Hz",
        Unit::Semitones => "semitones",
        Unit::Cents => "cents",
        Unit::Ratio => "ratio",
        Unit::Pan => "pan (-1 left .. 1 right)",
        Unit::Note => "MIDI note",
        Unit::Decibels => "dB",
    };
    let mut p = json!({
        "key": d.key,
        "name": d.name,
        "group": d.group,
        "kind": kind,
        "unit": unit,
        "scale": if d.scale == Scale::Exp { "log" } else { "linear" },
        "min": round4(d.min),
        "max": round4(d.max),
        "default": round4(d.default),
    });
    if let Kind::Enum(opts) = d.kind {
        p["options"] = json!(opts);
    }
    p
}

/// When a parameter only applies to some FX types or physical models,
/// describe the condition (e.g. "fx1_type is Delay", "model is Piano").
fn applies_when(kind: SynthKind, i: usize) -> Option<String> {
    let fx_base = kind.fx_base();
    let mut v = kind.defaults();
    if i >= fx_base {
        let rel = i - fx_base;
        if rel % fx::STRIDE == fx::TYPE {
            return None;
        }
        let type_i = fx_base + (rel / fx::STRIDE) * fx::STRIDE + fx::TYPE;
        let kinds: Vec<&str> = (1..fx::KIND_NAMES.len())
            .filter(|&k| {
                v[type_i] = k as f32;
                kind.param_visible(&v, i)
            })
            .map(|k| fx::KIND_NAMES[k])
            .collect();
        let key = kind.param(type_i).key;
        return Some(if kinds.len() == fx::KIND_NAMES.len() - 1 {
            format!("{key} is not Off")
        } else {
            format!("{key} is {}", kinds.join(" or "))
        });
    }
    let model_i = kind.index_of("model")?;
    if kind != SynthKind::Physical || i == model_i {
        return None;
    }
    let models: Vec<&str> = (0..physical::MODELS.len())
        .filter(|&m| {
            v[model_i] = m as f32;
            kind.param_visible(&v, i)
        })
        .map(|m| physical::MODELS[m])
        .collect();
    (models.len() < physical::MODELS.len()).then(|| format!("model is {}", models.join(" or ")))
}

/// How an instrument responds to performance MIDI (not counting learned CCs).
fn midi_behaviour(kind: SynthKind, values: &[f32]) -> Value {
    let get = |key: &str| kind.index_of(key).map_or(0.0, |i| values[i]);
    let bend = values[synth::BEND_RANGE].round();
    let model = physical::MODELS.get(get("model").round() as usize).copied().unwrap_or("String");
    let mod_wheel = match kind {
        SynthKind::Fm | SynthKind::Analog => {
            format!("adds vibrato, up to Wheel>Vibrato ({:.0} cents)", get("wheel_vib"))
        }
        SynthKind::Granular => format!("adds grain spray, up to Wheel>Spray ({:.0}%)", get("wheel_spray") * 100.0),
        SynthKind::Acid => format!("opens the filter, up to {:.1} octaves (Wheel>Cutoff)", get("wheel_cutoff") * 3.0),
        SynthKind::Physical if model == "Bowed" => "adds bow pressure (force) for swells".to_string(),
        _ => "no effect".to_string(),
    };
    let velocity = match kind {
        SynthKind::Fm => "level of each operator, scaled by its Vel Sens (on modulators this also changes brightness)".to_string(),
        SynthKind::Analog => "level (Vel>Amp) and filter cutoff (Vel>Cutoff)".to_string(),
        SynthKind::Granular => "level".to_string(),
        SynthKind::Acid => format!(
            "accent only: notes at or above {:.0} (Accent Vel) are accented; others play at normal level",
            get("accent_vel")
        ),
        SynthKind::Drums => "level, scaled by Vel Sens".to_string(),
        SynthKind::Sampler => "level (Vel>Amp) and filter cutoff (Vel>Cutoff)".to_string(),
        SynthKind::Physical if model == "Bowed" => "bow speed: louder notes with the same tone".to_string(),
        SynthKind::Physical => "level and brightness (Vel>Bright: harder strikes/plucks are brighter)".to_string(),
    };
    let sustain = match kind {
        SynthKind::Drums => "ignored (drums are one-shots)",
        SynthKind::Physical if model == "Piano" => "holds notes, like lifting the dampers",
        _ => "holds notes until the pedal is released",
    };
    let notes = match kind {
        SynthKind::Drums => "General MIDI drum map: 35/36 kick, 37 rim, 38/40 snare, 39 clap, 42/44 closed hat, \
46 open hat, 41/43 low tom, 45/47 mid tom, 48/50 high tom, 56 cowbell, 49/51/52/55/57/59 cymbal"
            .to_string(),
        SynthKind::Acid => {
            "monophonic: a note started while another is held slides to it without retriggering".to_string()
        }
        SynthKind::Sampler if get("mode").round() as usize == 2 => {
            let first = get("base_note").round() as i32;
            format!("Slice mode: one slice per note starting at {first} ({})", note_name(first.clamp(0, 127) as u8))
        }
        SynthKind::Sampler => format!(
            "chromatic around Root Note {} ({})",
            get("root").round(),
            note_name(get("root").round().clamp(0.0, 127.0) as u8)
        ),
        _ => "chromatic".to_string(),
    };
    json!({
        "pitch_bend": if kind == SynthKind::Drums {
            format!("retunes the whole kit by up to ±{bend} semitones")
        } else {
            format!("±{bend} semitones (Bend Range)")
        },
        "mod_wheel_cc1": mod_wheel,
        "sustain_cc64": sustain,
        "velocity": velocity,
        "notes": notes,
        "all_notes_off": "CC120 or CC123",
        "learned_ccs": "any other CC can drive any parameter; see map_cc and list_midi_mappings",
    })
}

fn round4(v: f32) -> f64 {
    (v as f64 * 10_000.0).round() / 10_000.0
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing '{key}'"))
}

impl App {
    /// Run an MCP tool call against the app state.
    pub fn run_tool(&mut self, name: &str, args: &Value) -> ToolResult {
        match name {
            "get_rack" => Ok(self.rack_json()),
            "describe_synth" => self.tool_describe_synth(args),
            "list_midi_mappings" => Ok(self.mappings_json()),
            "map_cc" => self.tool_map_cc(args),
            "clear_midi_mapping" => self.tool_clear_mapping(args),
            "get_params" => self.tool_get_params(args),
            "set_params" => self.tool_set_params(args),
            "add_synth" => self.tool_add_synth(args),
            "remove_synth" => {
                let i = self.slot_arg(args)?;
                self.remove_slot(i);
                self.mcp_note(format!("removed slot {}", i + 1));
                Ok(self.rack_json())
            }
            "set_slot" => self.tool_set_slot(args),
            "list_patches" => self.tool_list_patches(args),
            "load_patch" => self.tool_load_patch(args),
            "save_patch" => self.tool_save_patch(args),
            "list_sessions" => Ok(json!(
                self.storage
                    .list_sessions()
                    .iter()
                    .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .collect::<Vec<_>>()
            )),
            "save_session" => {
                let name = str_arg(args, "name")?.to_string();
                let path = self.storage.save_session(&name, &self.session()).map_err(|e| format!("{e:#}"))?;
                self.mcp_note(format!("saved session '{name}'"));
                Ok(json!({ "saved": path }))
            }
            "load_session" => self.tool_load_session(args),
            "load_sample" => self.tool_load_sample(args),
            "play_notes" => self.tool_play_notes(args),
            "panic" => {
                self.scheduled.clear();
                self.all_keyboard_notes_off();
                self.send(Command::Panic);
                self.mcp_note("all notes off".to_string());
                Ok(json!("all notes off"))
            }
            _ => Err(format!("unknown tool '{name}'")),
        }
    }

    /// Show what an MCP client changed in the status line.
    fn mcp_note(&mut self, text: String) {
        self.info(format!("MCP: {text}"));
    }

    fn slot_arg(&self, args: &Value) -> Result<usize, String> {
        let v = args.get("slot").ok_or("missing 'slot'")?;
        let n = v.as_u64().filter(|n| (1..=MAX_SLOTS as u64).contains(n));
        let i = n.ok_or_else(|| format!("slot must be 1-{MAX_SLOTS}, got {v}"))? as usize - 1;
        if self.slots[i].is_none() {
            return Err(format!("slot {} is empty; get_rack lists the occupied slots", i + 1));
        }
        Ok(i)
    }

    fn target_arg(&self, args: &Value) -> Result<Target, String> {
        match args.get("slot") {
            Some(Value::String(s)) if s.eq_ignore_ascii_case("master") => Ok(Target::Master),
            _ => self.slot_arg(args).map(Target::Slot),
        }
    }

    fn slot_json(&self, i: usize) -> Value {
        let Some(s) = self.slots[i].as_ref() else { return Value::Null };
        json!({
            "slot": i + 1,
            "kind": kind_name(s.kind),
            "name": s.name,
            "channel": s.channel.map_or(json!("omni"), |c| json!(c + 1)),
            "mute": s.mute,
            "solo": s.solo,
            "active_voices": s.voices,
            "sample": s.sample_path,
        })
    }

    fn rack_json(&self) -> Value {
        let master: Map<String, Value> = master::PARAMS
            .iter()
            .zip(&self.master)
            .map(|(p, v)| (p.key.to_string(), json!(p.format(*v))))
            .collect();
        json!({
            "slots": (0..MAX_SLOTS).filter(|&i| self.slots[i].is_some()).map(|i| self.slot_json(i)).collect::<Vec<_>>(),
            "free_slots": (0..MAX_SLOTS).filter(|&i| self.slots[i].is_none()).count(),
            "soloed": self.soloed_slots().iter().map(|i| i + 1).collect::<Vec<_>>(),
            "master": master,
            "midi_inputs": self.midi.connected_names(),
            "midi_mappings": self.cc_map.len(),
            "virtual_midi_port": self.midi.has_virtual().then_some(crate::midi::CLIENT_NAME),
            "sample_rate": self.sample_rate,
            "data_dir": self.storage.root,
        })
    }

    fn tool_get_params(&self, args: &Value) -> ToolResult {
        let t = self.target_arg(args)?;
        // Settings of inactive FX types are hidden, as in the TUI.
        let params: Vec<Value> = self
            .visible_params(t)
            .into_iter()
            .map(|i| {
                let d = self.param_desc(t, i);
                let v = self.param_value(t, i);
                let mut p = describe_param(d);
                p["value"] = json!(round4(v));
                p["display"] = json!(d.format(v));
                if let Some(m) = self.mapping_for(t, i) {
                    p["midi_cc"] = json!({ "channel": m.channel + 1, "cc": m.cc });
                }
                p
            })
            .collect();
        Ok(match t {
            Target::Master => json!({ "slot": "master", "params": params }),
            Target::Slot(i) => {
                let mut v = self.slot_json(i);
                let s = self.slots[i].as_ref().expect("checked");
                v["midi"] = midi_behaviour(s.kind, &s.params);
                v["params"] = json!(params);
                v
            }
        })
    }

    fn tool_set_params(&mut self, args: &Value) -> ToolResult {
        let t = self.target_arg(args)?;
        let values = args.get("values").and_then(Value::as_object).ok_or("missing 'values' object")?;
        let mut applied = Map::new();
        let mut errors = Map::new();
        for (key, v) in values {
            let index = (0..self.param_count(t)).find(|&i| self.param_key(t, i) == key);
            let Some(i) = index else {
                errors.insert(key.clone(), json!("no such parameter; see get_params"));
                continue;
            };
            let desc = self.param_desc(t, i);
            match parse_value(desc, v) {
                Ok(x) => {
                    self.set_param(t, i, x);
                    applied.insert(key.clone(), json!(desc.format(self.param_value(t, i))));
                }
                Err(e) => {
                    errors.insert(key.clone(), json!(e));
                }
            }
        }
        if applied.is_empty() {
            return Err(format!("nothing applied: {}", Value::Object(errors)));
        }
        let what = match t {
            Target::Master => "master".to_string(),
            Target::Slot(i) => format!("slot {}", i + 1),
        };
        self.mcp_note(format!("set {} on {what}", applied.keys().cloned().collect::<Vec<_>>().join(", ")));
        let mut out = json!({ "applied": applied });
        if !errors.is_empty() {
            out["errors"] = Value::Object(errors);
        }
        Ok(out)
    }

    fn tool_add_synth(&mut self, args: &Value) -> ToolResult {
        let kind = parse_kind(args.get("kind").ok_or("missing 'kind'")?)?;
        let channel = args.get("channel").map(parse_channel).transpose()?;
        if self.free_slot().is_none() {
            return Err(format!("all {MAX_SLOTS} slots are in use"));
        }
        self.add_synth(kind);
        let i = self.selected_slot().ok_or("couldn't add the synth")?;
        if let Some(ch) = channel {
            self.set_channel(i, ch);
        }
        if let Some(name) = args.get("patch").and_then(Value::as_str) {
            let patch = self.find_patch(name, Some(kind))?;
            self.load_patch_into(i, patch);
        }
        let s = self.slots[i].as_ref().expect("just added");
        let note = format!("added {} '{}' in slot {} on {}", kind.long_name(), s.name, i + 1, channel_label(s.channel));
        self.mcp_note(note);
        Ok(self.slot_json(i))
    }

    fn tool_describe_synth(&self, args: &Value) -> ToolResult {
        let kind = parse_kind(args.get("kind").ok_or("missing 'kind'")?)?;
        let include_fx = args.get("include_fx").and_then(Value::as_bool).unwrap_or(false);
        let end = if include_fx { kind.param_count() } else { kind.fx_base() };
        let params: Vec<Value> = (0..end)
            .map(|i| {
                let mut p = describe_param(kind.param(i));
                if let Some(cond) = applies_when(kind, i) {
                    p["applies_when"] = json!(cond);
                }
                p
            })
            .collect();
        // The insert FX are the same on every synth: summarise them compactly.
        let fx_types: Map<String, Value> = (1..fx::KIND_NAMES.len())
            .map(|k| {
                let mut v = kind.defaults();
                let base = kind.fx_base();
                v[base + fx::TYPE] = k as f32;
                let keys: Vec<&str> = (base + fx::MIX + 1..base + fx::STRIDE)
                    .filter(|&i| kind.param_visible(&v, i))
                    .map(|i| kind.param(i).key.trim_start_matches("fx1_"))
                    .collect();
                (fx::KIND_NAMES[k].to_string(), json!(keys))
            })
            .collect();
        Ok(json!({
            "kind": kind_name(kind),
            "name": kind.long_name(),
            "midi": midi_behaviour(kind, &kind.defaults()),
            "params": params,
            "insert_fx": {
                "units": fx::FX_UNITS,
                "keys": "fxN_type and fxN_mix for N = 1-3, plus fxN_<setting> for the selected type",
                "settings_by_type": fx_types,
                "full_details": if include_fx { "included in params" } else { "pass include_fx: true" },
            },
        }))
    }

    fn mapping_json(&self, m: &CcMapping) -> Value {
        let (slot, target) = match m.slot {
            None => (json!("master"), Some(Target::Master)),
            Some(s) => (json!(s + 1), self.slots.get(s).and_then(|x| x.as_ref()).map(|_| Target::Slot(s))),
        };
        let index = target.and_then(|t| (0..self.param_count(t)).find(|&i| self.param_key(t, i) == m.param).map(|i| (t, i)));
        let mut v = json!({ "channel": m.channel + 1, "cc": m.cc, "slot": slot, "param": m.param });
        match index {
            Some((t, i)) => {
                v["name"] = json!(self.param_desc(t, i).name);
                v["group"] = json!(self.param_desc(t, i).group);
                v["active"] = json!(self.param_visible(t, i));
            }
            None => v["active"] = json!(false),
        }
        v
    }

    fn mappings_json(&self) -> Value {
        json!(self.cc_map.iter().map(|m| self.mapping_json(m)).collect::<Vec<_>>())
    }

    fn tool_map_cc(&mut self, args: &Value) -> ToolResult {
        let channel = match args.get("channel").and_then(Value::as_u64) {
            Some(c @ 1..=16) => c as u8 - 1,
            _ => return Err("channel must be 1-16 (mappings are per channel)".into()),
        };
        let cc = match args.get("cc").and_then(Value::as_u64) {
            Some(c @ 0..=119) => c as u8,
            Some(c @ 120..=127) => return Err(format!("CC{c} is a channel mode message and can't be mapped")),
            _ => return Err("cc must be 0-119".into()),
        };
        let t = self.target_arg(args)?;
        let key = str_arg(args, "param")?;
        let i = (0..self.param_count(t))
            .find(|&i| self.param_key(t, i) == key)
            .ok_or_else(|| format!("no parameter '{key}' here; get_params lists the keys"))?;
        let slot = match t {
            Target::Master => None,
            Target::Slot(s) => Some(s),
        };
        // One CC per parameter (as with MIDI learn); one CC may drive several.
        self.cc_map.retain(|m| !(m.slot == slot && m.param == key));
        let mapping = CcMapping { channel, cc, slot, param: key.to_string() };
        let mut out = self.mapping_json(&mapping);
        self.cc_map.push(mapping);
        let mut notes = Vec::new();
        match cc {
            1 => notes.push("CC1 is also the mod wheel, so it keeps its built-in effect too"),
            64 => notes.push("CC64 is also the sustain pedal, so it keeps holding notes too"),
            _ => {}
        }
        if !self.param_visible(t, i) {
            notes.push("this parameter is inactive right now (its FX type or model isn't selected)");
        }
        if !notes.is_empty() {
            out["notes"] = json!(notes);
        }
        let name = self.param_desc(t, i).name;
        self.mcp_note(format!("mapped CC{cc} (ch {}) to {name}", channel + 1));
        Ok(out)
    }

    fn tool_clear_mapping(&mut self, args: &Value) -> ToolResult {
        let before = self.cc_map.len();
        if let Some(key) = args.get("param").and_then(Value::as_str) {
            let slot = match self.target_arg(args)? {
                Target::Master => None,
                Target::Slot(s) => Some(s),
            };
            self.cc_map.retain(|m| !(m.slot == slot && m.param == key));
        } else if let (Some(ch), Some(cc)) = (args.get("channel").and_then(Value::as_u64), args.get("cc").and_then(Value::as_u64)) {
            self.cc_map.retain(|m| !(m.channel as u64 + 1 == ch && m.cc as u64 == cc));
        } else {
            return Err("pass slot + param, or channel + cc".into());
        }
        let removed = before - self.cc_map.len();
        if removed > 0 {
            self.mcp_note(format!("removed {removed} MIDI mapping(s)"));
        }
        Ok(json!({ "removed": removed, "mappings": self.mappings_json() }))
    }

    pub(super) fn set_channel(&mut self, index: usize, channel: Option<u8>) {
        if let Some(s) = self.slots[index].as_mut() {
            s.channel = channel;
            self.send(Command::SetChannel { slot: index, channel });
        }
    }

    fn tool_set_slot(&mut self, args: &Value) -> ToolResult {
        let i = self.slot_arg(args)?;
        let mut changed = Vec::new();
        if let Some(v) = args.get("channel") {
            let ch = parse_channel(v)?;
            self.set_channel(i, ch);
            changed.push(format!("channel {}", channel_label(ch)));
        }
        for (key, is_mute) in [("mute", true), ("solo", false)] {
            if let Some(on) = args.get(key).and_then(Value::as_bool) {
                let s = self.slots[i].as_mut().expect("checked");
                if is_mute {
                    s.mute = on;
                    self.send(Command::SetMute { slot: i, on });
                } else {
                    s.solo = on;
                    self.send(Command::SetSolo { slot: i, on });
                }
                changed.push(format!("{key} {}", if on { "on" } else { "off" }));
            }
        }
        if let Some(name) = args.get("name").and_then(Value::as_str) {
            self.slots[i].as_mut().expect("checked").name = name.to_string();
            changed.push(format!("name '{name}'"));
        }
        if changed.is_empty() {
            return Err("nothing to change; pass channel, mute, solo or name".into());
        }
        self.mcp_note(format!("slot {}: {}", i + 1, changed.join(", ")));
        Ok(self.slot_json(i))
    }

    fn find_patch(&self, name: &str, kind: Option<SynthKind>) -> Result<crate::patch::Patch, String> {
        let all = self.storage.list_patches();
        let matches: Vec<_> = all
            .iter()
            .filter(|e| e.patch.name.eq_ignore_ascii_case(name) && kind.is_none_or(|k| e.patch.kind == k))
            .collect();
        // User patches win over factory ones with the same name.
        matches
            .iter()
            .find(|e| !e.is_factory())
            .or_else(|| matches.first())
            .map(|e| e.patch.clone())
            .ok_or_else(|| format!("no patch named '{name}'; list_patches shows what's available"))
    }

    fn tool_list_patches(&self, args: &Value) -> ToolResult {
        let kind = args.get("kind").map(parse_kind).transpose()?;
        let list: Vec<Value> = self
            .storage
            .list_patches()
            .iter()
            .filter(|e| kind.is_none_or(|k| e.patch.kind == k))
            .map(|e| {
                json!({
                    "name": e.patch.name,
                    "kind": kind_name(e.patch.kind),
                    "source": if e.is_factory() { "factory" } else { "user" },
                })
            })
            .collect();
        Ok(json!(list))
    }

    fn tool_load_patch(&mut self, args: &Value) -> ToolResult {
        let i = self.slot_arg(args)?;
        let patch = self.find_patch(str_arg(args, "name")?, None)?;
        let name = patch.name.clone();
        self.load_patch_into(i, patch);
        self.mcp_note(format!("loaded '{name}' into slot {}", i + 1));
        Ok(self.slot_json(i))
    }

    fn tool_save_patch(&mut self, args: &Value) -> ToolResult {
        let i = self.slot_arg(args)?;
        if let Some(name) = args.get("name").and_then(Value::as_str) {
            self.slots[i].as_mut().expect("checked").name = name.to_string();
        }
        let patch = self.current_patch(i).ok_or("slot is empty")?;
        let path = self.storage.save_patch(&patch).map_err(|e| format!("{e:#}"))?;
        self.mcp_note(format!("saved patch '{}'", patch.name));
        Ok(json!({ "saved": path, "name": patch.name }))
    }

    fn tool_load_session(&mut self, args: &Value) -> ToolResult {
        let name = str_arg(args, "name")?;
        let path = self
            .storage
            .list_sessions()
            .into_iter()
            .find(|p| p.file_stem().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(&file_stem(name))))
            .ok_or_else(|| format!("no session named '{name}'; list_sessions shows what's saved"))?;
        let session = read_json(&path).map_err(|e| format!("{e:#}"))?;
        self.apply_session(session);
        self.mcp_note(format!("loaded session '{name}'"));
        Ok(self.rack_json())
    }

    fn tool_load_sample(&mut self, args: &Value) -> ToolResult {
        let i = self.slot_arg(args)?;
        let kind = self.slots[i].as_ref().expect("checked").kind;
        if !matches!(kind, SynthKind::Granular | SynthKind::Sampler) {
            return Err(format!("slot {} is {}; samples load into granular or sampler slots", i + 1, kind.long_name()));
        }
        let raw = str_arg(args, "path")?;
        let path = match raw.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None => PathBuf::from(raw),
        };
        let path = if path.is_relative() { self.storage.samples_dir().join(path) } else { path };
        if !path.is_file() {
            return Err(format!("{} doesn't exist", path.display()));
        }
        self.load_sample(i, &path);
        if let Some(s) = self.status.as_ref().filter(|s| s.error) {
            return Err(s.text.clone());
        }
        let status = self.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        self.mcp_note(status.clone());
        let mut out = self.slot_json(i);
        out["result"] = json!(status);
        Ok(out)
    }

    fn tool_play_notes(&mut self, args: &Value) -> ToolResult {
        let i = self.slot_arg(args)?;
        let notes = args.get("notes").and_then(Value::as_array).ok_or("missing 'notes' array")?;
        if notes.len() > MAX_NOTES {
            return Err(format!("at most {MAX_NOTES} notes per call"));
        }
        let now = Instant::now();
        let mut events = Vec::with_capacity(notes.len() * 2);
        let mut end = Duration::ZERO;
        for n in notes {
            let note = match n.get("note") {
                Some(Value::Number(x)) => x.as_u64().filter(|v| *v <= 127).map(|v| v as i32),
                Some(Value::String(s)) => parse_note_name(&s.to_ascii_lowercase()),
                _ => None,
            }
            .filter(|v| (0..=127).contains(v))
            .ok_or_else(|| format!("bad note in {n}"))? as u8;
            let velocity = n.get("velocity").and_then(Value::as_u64).unwrap_or(100).clamp(1, 127) as f32 / 127.0;
            let start = Duration::from_millis(n.get("start_ms").and_then(Value::as_u64).unwrap_or(0));
            let length = Duration::from_millis(n.get("duration_ms").and_then(Value::as_u64).unwrap_or(400).max(1));
            if start + length > MAX_SCHEDULE {
                return Err(format!("notes must finish within {} s", MAX_SCHEDULE.as_secs()));
            }
            end = end.max(start + length);
            events.push((now + start, Command::NoteOn { slot: i, note, velocity }));
            events.push((now + start + length, Command::NoteOff { slot: i, note }));
        }
        self.scheduled.extend(events);
        self.mcp_note(format!("playing {} notes on slot {}", notes.len(), i + 1));
        Ok(json!({
            "scheduled": notes.len(),
            "slot": i + 1,
            "ends_after_ms": end.as_millis() as u64,
            "first_note": notes.first().and_then(|n| n.get("note")).map(|v| match v {
                Value::Number(x) => note_name(x.as_u64().unwrap_or(0).min(127) as u8),
                other => other.to_string(),
            }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests_support::{temp_dir, test_app};
    use super::*;

    #[test]
    fn configure_a_rack_through_tools() {
        let dir = temp_dir("mcp-tools");
        let mut app = test_app(&dir);

        let added = app.run_tool("add_synth", &json!({ "kind": "acid", "patch": "Acid Squelch", "channel": 5 })).unwrap();
        assert_eq!(added["slot"], 1);
        assert_eq!(added["name"], "Acid Squelch");
        assert_eq!(added["channel"], 5);
        let drums = app.run_tool("add_synth", &json!({ "kind": "drums" })).unwrap();
        assert_eq!(drums["channel"], 10);

        let r = app
            .run_tool("set_params", &json!({ "slot": 1, "values": { "cutoff": "1.2k", "wave": "square", "decay": "300ms", "nope": 1 } }))
            .unwrap();
        assert_eq!(r["applied"]["cutoff"], "1.20 kHz");
        assert_eq!(r["applied"]["wave"], "Square");
        assert_eq!(r["applied"]["decay"], "300 ms");
        assert!(r["errors"]["nope"].is_string());
        assert!(app.run_tool("set_params", &json!({ "slot": 1, "values": { "wave": "triangle" } })).is_err());

        let params = app.run_tool("get_params", &json!({ "slot": 1 })).unwrap();
        let cutoff = params["params"].as_array().unwrap().iter().find(|p| p["key"] == "cutoff").unwrap();
        assert_eq!(cutoff["value"], 1200.0);

        app.run_tool("set_params", &json!({ "slot": "master", "values": { "volume": "50%" } })).unwrap();
        assert_eq!(app.master[master::VOLUME], 0.5);

        let s = app.run_tool("set_slot", &json!({ "slot": 2, "solo": true, "name": "Beat" })).unwrap();
        assert_eq!(s["solo"], true);
        assert_eq!(app.run_tool("get_rack", &json!({})).unwrap()["soloed"], json!([2]));

        app.run_tool("load_patch", &json!({ "slot": 1, "name": "fm strings" })).unwrap();
        assert_eq!(app.slots[0].as_ref().unwrap().kind, SynthKind::Fm);
        app.run_tool("save_patch", &json!({ "slot": 1, "name": "My Strings" })).unwrap();
        let fm = app.run_tool("list_patches", &json!({ "kind": "fm" })).unwrap();
        assert!(fm.as_array().unwrap().iter().any(|p| p["name"] == "My Strings" && p["source"] == "user"));

        app.run_tool("save_session", &json!({ "name": "song" })).unwrap();
        app.run_tool("remove_synth", &json!({ "slot": 2 })).unwrap();
        assert!(app.run_tool("get_params", &json!({ "slot": 2 })).unwrap_err().contains("empty"));
        let rack = app.run_tool("load_session", &json!({ "name": "song" })).unwrap();
        assert_eq!(rack["slots"].as_array().unwrap().len(), 2);

        let played = app
            .run_tool("play_notes", &json!({ "slot": 2, "notes": [{ "note": 36 }, { "note": "d2", "start_ms": 250, "duration_ms": 100 }] }))
            .unwrap();
        assert_eq!(played["ends_after_ms"], 400);
        assert_eq!(app.scheduled.len(), 4);
        app.run_tool("panic", &json!({})).unwrap();
        assert!(app.scheduled.is_empty());

        assert!(app.run_tool("load_sample", &json!({ "slot": 1, "path": "x.wav" })).unwrap_err().contains("granular or sampler"));
        assert!(app.run_tool("add_synth", &json!({ "kind": "theremin" })).is_err());
        assert!(app.run_tool("bogus", &json!({})).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn midi_mappings_and_descriptions_over_mcp() {
        let dir = temp_dir("mcp-midi");
        let mut app = test_app(&dir);
        app.run_tool("add_synth", &json!({ "kind": "analog" })).unwrap();
        app.run_tool("add_synth", &json!({ "kind": "physical" })).unwrap();

        // get_params carries static facts and the instrument's MIDI behaviour.
        let p = app.run_tool("get_params", &json!({ "slot": 1 })).unwrap();
        let cutoff = p["params"].as_array().unwrap().iter().find(|x| x["key"] == "cutoff").unwrap().clone();
        assert_eq!(cutoff["scale"], "log");
        assert_eq!(cutoff["unit"], "Hz");
        assert_eq!(cutoff["default"], 2000.0);
        assert!(cutoff.get("midi_cc").is_none());
        assert!(p["midi"]["mod_wheel_cc1"].as_str().unwrap().contains("vibrato"));
        assert!(p["midi"]["velocity"].as_str().unwrap().contains("Vel>Cutoff"));

        // Map a knob, then a real CC message moves the parameter.
        let m = app.run_tool("map_cc", &json!({ "channel": 1, "cc": 74, "slot": 1, "param": "cutoff" })).unwrap();
        assert_eq!(m["name"], "Cutoff");
        assert_eq!(m["active"], true);
        app.on_midi(crate::midi::MidiMsg { channel: 0, kind: crate::midi::MidiKind::Cc { cc: 74, value: 127 } });
        let i = SynthKind::Analog.index_of("cutoff").unwrap();
        assert_eq!(app.param_value(Target::Slot(0), i), 20_000.0);
        let p = app.run_tool("get_params", &json!({ "slot": 1 })).unwrap();
        let cutoff = p["params"].as_array().unwrap().iter().find(|x| x["key"] == "cutoff").unwrap().clone();
        assert_eq!(cutoff["midi_cc"], json!({ "channel": 1, "cc": 74 }));

        // Remapping the same parameter moves it; one CC can drive several (a macro).
        app.run_tool("map_cc", &json!({ "channel": 1, "cc": 71, "slot": 1, "param": "cutoff" })).unwrap();
        app.run_tool("map_cc", &json!({ "channel": 1, "cc": 71, "slot": "master", "param": "reverb_return" })).unwrap();
        let list = app.run_tool("list_midi_mappings", &json!({})).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 2);
        assert!(list.as_array().unwrap().iter().all(|m| m["cc"] == 71));

        // Inactive parameters and built-in CCs are flagged; mode messages refused.
        let bowed = app.run_tool("map_cc", &json!({ "channel": 2, "cc": 1, "slot": 2, "param": "bow_pressure" })).unwrap();
        let notes = bowed["notes"].to_string();
        assert!(notes.contains("mod wheel") && notes.contains("inactive"), "{notes}");
        assert!(app.run_tool("map_cc", &json!({ "channel": 1, "cc": 123, "slot": 1, "param": "cutoff" })).is_err());
        assert!(app.run_tool("map_cc", &json!({ "channel": 1, "cc": 20, "slot": 1, "param": "nope" })).is_err());

        let cleared = app.run_tool("clear_midi_mapping", &json!({ "channel": 1, "cc": 71 })).unwrap();
        assert_eq!(cleared["removed"], 2);
        app.run_tool("clear_midi_mapping", &json!({ "slot": 2, "param": "bow_pressure" })).unwrap();
        assert!(app.cc_map.is_empty());

        // describe_synth works without a slot and explains conditional controls.
        let d = app.run_tool("describe_synth", &json!({ "kind": "physical" })).unwrap();
        let find = |key: &str| d["params"].as_array().unwrap().iter().find(|x| x["key"] == key).unwrap().clone();
        assert_eq!(find("bow_pressure")["applies_when"], "model is Bowed");
        assert_eq!(find("unison")["applies_when"], "model is Piano");
        assert!(find("decay").get("applies_when").is_some(), "decay doesn't apply to Bowed");
        assert!(find("width").get("applies_when").is_none());
        let fx_delay = d["insert_fx"]["settings_by_type"]["Delay"].as_array().unwrap();
        assert!(fx_delay.iter().any(|k| k == "delay_feedback"));
        let full = app.run_tool("describe_synth", &json!({ "kind": "drums", "include_fx": true })).unwrap();
        let fx2 = full["params"].as_array().unwrap().iter().find(|x| x["key"] == "fx2_reverb_size").unwrap().clone();
        assert_eq!(fx2["applies_when"], "fx2_type is Reverb");
        assert!(full["midi"]["notes"].as_str().unwrap().contains("36 kick"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
