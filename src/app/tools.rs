//! MCP tool implementations. They run on the UI thread and go through the
//! same methods as keyboard edits, so the TUI always shows the result.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::{App, Target, channel_label};
use crate::engine::{Command, MAX_SLOTS};
use crate::midi::note_name;
use crate::params::{Kind, ParamDesc, master, parse_note_name};
use crate::patch::{file_stem, read_json};
use crate::synth::SynthKind;

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
            "virtual_midi_port": self.midi.has_virtual().then_some(crate::midi::CLIENT_NAME),
            "sample_rate": self.sample_rate,
            "data_dir": self.storage.root,
        })
    }

    fn tool_get_params(&self, args: &Value) -> ToolResult {
        let t = self.target_arg(args)?;
        let params: Vec<Value> = (0..self.param_count(t))
            .map(|i| {
                let d = self.param_desc(t, i);
                let v = self.param_value(t, i);
                let mut p = json!({
                    "key": d.key,
                    "name": d.name,
                    "group": d.group,
                    "value": round4(v),
                    "display": d.format(v),
                    "min": round4(d.min),
                    "max": round4(d.max),
                });
                if let Kind::Enum(opts) = d.kind {
                    p["options"] = json!(opts);
                }
                p
            })
            .collect();
        Ok(match t {
            Target::Master => json!({ "slot": "master", "params": params }),
            Target::Slot(i) => {
                let mut v = self.slot_json(i);
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
}
