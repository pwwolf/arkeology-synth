//! Patch and session persistence (JSON files), plus the factory patch set.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::params::{self, master};
use crate::synth::SynthKind;

/// A single synth's sound. Parameters are stored by key so patches survive
/// parameters being added or reordered later.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Patch {
    pub name: String,
    pub kind: SynthKind,
    pub params: BTreeMap<String, f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<PathBuf>,
}

impl Patch {
    pub fn from_values(name: &str, kind: SynthKind, values: &[f32], sample: Option<PathBuf>) -> Self {
        Patch {
            name: name.to_string(),
            kind,
            params: kind.params().zip(values).map(|(p, v)| (p.key.to_string(), *v)).collect(),
            sample,
        }
    }

    /// Values in parameter order; unknown keys are ignored, missing ones defaulted.
    pub fn values(&self) -> Vec<f32> {
        self.kind
            .params()
            .map(|p| p.clamp(self.params.get(p.key).copied().unwrap_or(p.default)))
            .collect()
    }
}

/// An entry in the patch browser: built into the app, or saved by the user.
#[derive(Clone, Debug)]
pub struct PatchEntry {
    pub patch: Patch,
    /// `None` for factory patches.
    pub user_path: Option<PathBuf>,
}

impl PatchEntry {
    pub fn is_factory(&self) -> bool {
        self.user_path.is_none()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CcMapping {
    /// 0-based MIDI channel.
    pub channel: u8,
    pub cc: u8,
    /// Slot index, or `None` for the master section.
    pub slot: Option<usize>,
    pub param: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSlot {
    pub index: usize,
    /// 1-based MIDI channel, `None` = omni.
    pub channel: Option<u8>,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
    pub patch: Patch,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub master: BTreeMap<String, f32>,
    #[serde(default)]
    pub slots: Vec<SessionSlot>,
    #[serde(default)]
    pub cc_map: Vec<CcMapping>,
    #[serde(default)]
    pub midi_ports: Vec<String>,
}

impl Session {
    pub fn master_values(&self) -> Vec<f32> {
        master::PARAMS
            .iter()
            .map(|p| p.clamp(self.master.get(p.key).copied().unwrap_or(p.default)))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Storage {
    pub root: PathBuf,
}

impl Storage {
    pub fn new(root: PathBuf) -> Result<Self> {
        let s = Storage { root };
        for dir in [s.patches_dir(), s.sessions_dir(), s.samples_dir()] {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(s)
    }

    pub fn default_root() -> PathBuf {
        dirs::data_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("arkeology-synth")
    }

    pub fn patches_dir(&self) -> PathBuf {
        self.root.join("patches")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn samples_dir(&self) -> PathBuf {
        self.root.join("samples")
    }

    pub fn autosave_path(&self) -> PathBuf {
        self.root.join("autosave.json")
    }

    pub fn save_patch(&self, patch: &Patch) -> Result<PathBuf> {
        let path = self.patches_dir().join(format!("{}.json", file_stem(&patch.name)));
        write_json(&path, patch)?;
        Ok(path)
    }

    pub fn save_session(&self, name: &str, session: &Session) -> Result<PathBuf> {
        let path = self.sessions_dir().join(format!("{}.json", file_stem(name)));
        write_json(&path, session)?;
        Ok(path)
    }

    /// Built-in factory patches plus the user's saved ones, grouped by synth
    /// kind with the user's patches first. Unreadable files are skipped.
    pub fn list_patches(&self) -> Vec<PatchEntry> {
        let user = json_files(&self.patches_dir()).into_iter().filter_map(|p| {
            read_json::<Patch>(&p).ok().map(|patch| PatchEntry { patch, user_path: Some(p) })
        });
        let factory = factory_patches().into_iter().map(|patch| PatchEntry { patch, user_path: None });
        let mut out: Vec<PatchEntry> = user.chain(factory).collect();
        out.sort_by_key(|e| (e.patch.kind.order(), e.is_factory(), e.patch.name.to_lowercase()));
        out
    }

    pub fn list_sessions(&self) -> Vec<PathBuf> {
        let mut v = json_files(&self.sessions_dir());
        v.sort();
        v
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect()
        })
        .unwrap_or_default()
}

/// Turn a display name into a safe file name.
pub fn file_stem(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') { c } else { '_' })
        .collect();
    let s = s.trim_matches('.').trim().to_string();
    if s.is_empty() { "untitled".into() } else { s }
}

// ---------------------------------------------------------------------------
// Factory patches
// ---------------------------------------------------------------------------

fn make(name: &str, kind: SynthKind, overrides: &[(&str, f32)]) -> Patch {
    let mut values = kind.defaults();
    for (key, v) in overrides {
        match kind.index_of(key) {
            Some(i) => values[i] = *v,
            None => panic!("factory patch '{name}' sets unknown param '{key}'"),
        }
    }
    Patch::from_values(name, kind, &values, None)
}

pub fn factory_patches() -> Vec<Patch> {
    use SynthKind::{Acid, Drums, Fm, Granular, Sampler};
    vec![
        make("FM Init", Fm, &[]),
        make(
            "E.Piano",
            Fm,
            &[
                ("algorithm", 4.0),
                ("op1_level", 0.8),
                ("op1_decay", 2.5),
                ("op1_sustain", 0.0),
                ("op1_release", 0.4),
                ("op2_ratio", 14.0),
                ("op2_level", 0.35),
                ("op2_decay", 0.35),
                ("op2_sustain", 0.0),
                ("op2_vel", 0.9),
                ("op3_ratio", 1.0),
                ("op3_level", 0.75),
                ("op3_detune", 3.0),
                ("op3_decay", 3.0),
                ("op3_sustain", 0.0),
                ("op4_ratio", 1.0),
                ("op4_level", 0.55),
                ("op4_decay", 1.2),
                ("op4_sustain", 0.1),
                ("op4_vel", 0.8),
                ("reverb_send", 0.25),
            ],
        ),
        make(
            "Glass Bell",
            Fm,
            &[
                ("algorithm", 4.0),
                ("op1_decay", 4.0),
                ("op1_sustain", 0.0),
                ("op1_release", 2.0),
                ("op2_ratio", 3.5),
                ("op2_level", 0.55),
                ("op2_decay", 3.0),
                ("op2_sustain", 0.0),
                ("op3_ratio", 2.0),
                ("op3_level", 0.6),
                ("op3_decay", 5.0),
                ("op3_sustain", 0.0),
                ("op3_release", 2.0),
                ("op4_ratio", 7.0),
                ("op4_detune", 7.0),
                ("op4_level", 0.4),
                ("op4_decay", 2.0),
                ("op4_sustain", 0.0),
                ("reverb_send", 0.45),
            ],
        ),
        make(
            "Solid Bass",
            Fm,
            &[
                ("algorithm", 0.0),
                ("voices", 1.0),
                ("glide", 0.04),
                ("transpose", -12.0),
                ("feedback", 0.35),
                ("op1_decay", 0.8),
                ("op1_sustain", 0.7),
                ("op1_release", 0.08),
                ("op2_ratio", 1.0),
                ("op2_level", 0.6),
                ("op2_decay", 0.25),
                ("op2_sustain", 0.25),
                ("op3_ratio", 2.0),
                ("op3_level", 0.3),
                ("op3_decay", 0.15),
                ("op3_sustain", 0.0),
                ("op4_ratio", 1.0),
                ("op4_level", 0.4),
                ("op4_decay", 0.2),
                ("op4_sustain", 0.0),
                ("reverb_send", 0.0),
            ],
        ),
        make(
            "Brass Stack",
            Fm,
            &[
                ("algorithm", 1.0),
                ("feedback", 0.5),
                ("vib_depth", 8.0),
                ("op1_attack", 0.06),
                ("op1_sustain", 0.85),
                ("op1_release", 0.25),
                ("op2_ratio", 1.0),
                ("op2_level", 0.55),
                ("op2_attack", 0.09),
                ("op2_sustain", 0.6),
                ("op3_ratio", 1.0),
                ("op3_level", 0.35),
                ("op3_attack", 0.12),
                ("op3_sustain", 0.5),
                ("op4_ratio", 1.0),
                ("op4_level", 0.3),
                ("op4_attack", 0.1),
                ("op4_sustain", 0.5),
                ("reverb_send", 0.2),
            ],
        ),
        make(
            "Organ",
            Fm,
            &[
                ("algorithm", 7.0),
                ("op1_ratio", 0.5),
                ("op1_level", 0.7),
                ("op1_sustain", 1.0),
                ("op1_release", 0.05),
                ("op1_vel", 0.0),
                ("op2_ratio", 1.0),
                ("op2_level", 0.7),
                ("op2_sustain", 1.0),
                ("op2_release", 0.05),
                ("op2_vel", 0.0),
                ("op3_ratio", 2.0),
                ("op3_level", 0.5),
                ("op3_sustain", 1.0),
                ("op3_release", 0.05),
                ("op3_vel", 0.0),
                ("op4_ratio", 4.0),
                ("op4_level", 0.35),
                ("op4_sustain", 1.0),
                ("op4_release", 0.05),
                ("op4_vel", 0.0),
                ("vib_depth", 6.0),
                ("vib_rate", 6.5),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "FM Strings",
            Fm,
            &[
                ("algorithm", 4.0),
                ("feedback", 0.2),
                ("vib_rate", 5.5),
                ("vib_depth", 7.0),
                ("op1_detune", -6.0),
                ("op1_level", 0.75),
                ("op1_attack", 0.35),
                ("op1_decay", 2.0),
                ("op1_sustain", 0.85),
                ("op1_release", 0.9),
                ("op1_vel", 0.3),
                ("op2_ratio", 1.0),
                ("op2_level", 0.38),
                ("op2_attack", 0.6),
                ("op2_decay", 3.0),
                ("op2_sustain", 0.7),
                ("op2_release", 1.0),
                ("op2_vel", 0.4),
                ("op3_ratio", 1.0),
                ("op3_detune", 7.0),
                ("op3_level", 0.75),
                ("op3_attack", 0.4),
                ("op3_decay", 2.0),
                ("op3_sustain", 0.85),
                ("op3_release", 0.9),
                ("op3_vel", 0.3),
                ("op4_ratio", 2.0),
                ("op4_level", 0.25),
                ("op4_attack", 0.5),
                ("op4_decay", 3.0),
                ("op4_sustain", 0.6),
                ("op4_release", 1.0),
                ("op4_vel", 0.4),
                ("reverb_send", 0.45),
            ],
        ),
        make(
            "Soft Pad",
            Fm,
            &[
                ("algorithm", 6.0),
                ("vib_depth", 4.0),
                ("op1_attack", 1.2),
                ("op1_level", 0.7),
                ("op1_sustain", 0.9),
                ("op1_release", 2.5),
                ("op2_ratio", 2.0),
                ("op2_level", 0.25),
                ("op2_attack", 1.5),
                ("op2_sustain", 0.8),
                ("op2_release", 2.5),
                ("op3_ratio", 2.0),
                ("op3_detune", 5.0),
                ("op3_level", 0.35),
                ("op3_attack", 1.4),
                ("op3_sustain", 0.9),
                ("op3_release", 2.5),
                ("op4_ratio", 0.5),
                ("op4_level", 0.5),
                ("op4_attack", 1.0),
                ("op4_sustain", 0.9),
                ("op4_release", 2.5),
                ("reverb_send", 0.6),
            ],
        ),
        make(
            "Marimba",
            Fm,
            &[
                ("algorithm", 4.0),
                ("op1_decay", 0.6),
                ("op1_sustain", 0.0),
                ("op1_release", 0.3),
                ("op1_vel", 0.7),
                ("op2_ratio", 4.0),
                ("op2_level", 0.4),
                ("op2_decay", 0.08),
                ("op2_sustain", 0.0),
                ("op2_vel", 0.9),
                ("op3_ratio", 4.0),
                ("op3_level", 0.25),
                ("op3_decay", 0.3),
                ("op3_sustain", 0.0),
                ("op3_release", 0.2),
                ("op4_ratio", 1.0),
                ("op4_level", 0.2),
                ("op4_decay", 0.05),
                ("op4_sustain", 0.0),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Mono Lead",
            Fm,
            &[
                ("algorithm", 0.0),
                ("voices", 1.0),
                ("glide", 0.06),
                ("feedback", 0.55),
                ("wheel_vib", 40.0),
                ("op1_sustain", 0.9),
                ("op1_release", 0.15),
                ("op2_ratio", 1.0),
                ("op2_level", 0.5),
                ("op2_sustain", 0.6),
                ("op3_ratio", 2.0),
                ("op3_level", 0.3),
                ("op3_sustain", 0.5),
                ("op4_ratio", 1.0),
                ("op4_level", 0.35),
                ("op4_sustain", 0.4),
                ("reverb_send", 0.25),
            ],
        ),
        make("Granular Init", Granular, &[]),
        make(
            "Choir Cloud",
            Granular,
            &[
                ("source", 1.0),
                ("position", 0.1),
                ("spray", 0.15),
                ("speed", 0.1),
                ("size", 0.25),
                ("density", 30.0),
                ("pitch_spray", 0.12),
                ("spread", 0.9),
                ("attack", 0.8),
                ("release", 3.0),
                ("cutoff", 6000.0),
                ("reverb_send", 0.6),
            ],
        ),
        make(
            "Shimmer Pad",
            Granular,
            &[
                ("source", 2.0),
                ("spray", 0.4),
                ("size", 0.4),
                ("density", 40.0),
                ("pitch_spray", 0.08),
                ("spread", 1.0),
                ("reverse", 0.4),
                ("attack", 1.5),
                ("release", 4.0),
                ("reverb_send", 0.7),
            ],
        ),
        make(
            "Grain Swarm",
            Granular,
            &[
                ("source", 3.0),
                ("spray", 0.6),
                ("size", 0.03),
                ("density", 120.0),
                ("jitter", 0.9),
                ("pitch_spray", 0.3),
                ("shape", 3.0),
                ("attack", 0.05),
                ("release", 0.8),
                ("filter_type", 0.0),
                ("cutoff", 3500.0),
                ("resonance", 0.35),
                ("reverb_send", 0.35),
            ],
        ),
        make(
            "Pluck Stretch",
            Granular,
            &[
                ("source", 4.0),
                ("position", 0.0),
                ("spray", 0.02),
                ("speed", 0.25),
                ("size", 0.09),
                ("density", 45.0),
                ("attack", 0.005),
                ("decay", 2.0),
                ("sustain", 0.4),
                ("release", 1.2),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Wind Texture",
            Granular,
            &[
                ("source", 5.0),
                ("keytrack", 0.0),
                ("spray", 1.0),
                ("speed", 0.2),
                ("size", 0.5),
                ("density", 20.0),
                ("spread", 1.0),
                ("attack", 2.0),
                ("release", 4.0),
                ("filter_type", 1.0),
                ("cutoff", 1200.0),
                ("resonance", 0.4),
                ("reverb_send", 0.6),
            ],
        ),
        make("Acid Classic", Acid, &[]),
        make(
            "Acid Squelch",
            Acid,
            &[
                ("cutoff", 260.0),
                ("resonance", 0.88),
                ("env_mod", 0.75),
                ("decay", 0.3),
                ("accent", 0.85),
                ("drive", 0.5),
            ],
        ),
        make(
            "Acid Square",
            Acid,
            &[("wave", 1.0), ("cutoff", 250.0), ("resonance", 0.6), ("env_mod", 0.5), ("decay", 0.6)],
        ),
        make(
            "Acid Sub Bass",
            Acid,
            &[
                ("cutoff", 180.0),
                ("resonance", 0.2),
                ("env_mod", 0.15),
                ("decay", 1.0),
                ("accent", 0.3),
                ("drive", 0.1),
                ("reverb_send", 0.0),
            ],
        ),
        make(
            "Acid Screamer",
            Acid,
            &[
                ("cutoff", 500.0),
                ("resonance", 0.95),
                ("env_mod", 0.85),
                ("decay", 0.5),
                ("accent", 1.0),
                ("drive", 0.85),
                ("slide", 0.09),
                ("reverb_send", 0.15),
            ],
        ),
        make("Drums Init", Drums, &[]),
        make(
            "808 Kit",
            Drums,
            &[
                ("kick_tune", -2.0),
                ("kick_decay", 0.9),
                ("kick_tone", 0.3),
                ("snare_tone", 0.45),
                ("snare_decay", 0.22),
                ("clap_decay", 0.3),
                ("chat_tone", 0.35),
                ("ohat_tone", 0.35),
                ("ltom_decay", 0.7),
                ("mtom_decay", 0.6),
                ("htom_decay", 0.5),
                ("drive", 0.1),
                ("reverb_send", 0.1),
            ],
        ),
        make(
            "909 Kit",
            Drums,
            &[
                ("kick_tune", 1.0),
                ("kick_decay", 0.35),
                ("kick_tone", 0.85),
                ("snare_tune", 2.0),
                ("snare_tone", 0.8),
                ("snare_decay", 0.16),
                ("chat_tone", 0.8),
                ("chat_decay", 0.04),
                ("ohat_tone", 0.8),
                ("ohat_decay", 0.3),
                ("cymbal_tone", 0.8),
                ("drive", 0.35),
                ("reverb_send", 0.08),
            ],
        ),
        make(
            "Lo-Fi Kit",
            Drums,
            &[
                ("transpose", -3.0),
                ("kick_decay", 0.4),
                ("kick_tone", 0.6),
                ("snare_tone", 0.7),
                ("chat_tone", 0.1),
                ("chat_decay", 0.03),
                ("ohat_tone", 0.1),
                ("ohat_decay", 0.25),
                ("cymbal_tone", 0.1),
                ("drive", 0.75),
                ("reverb_send", 0.2),
            ],
        ),
        make("Sampler Init", Sampler, &[]),
        make(
            "Choir Loop",
            Sampler,
            &[
                ("source", 1.0),
                ("loop_start", 0.15),
                ("loop_end", 0.85),
                ("crossfade", 0.3),
                ("attack", 0.25),
                ("release", 1.2),
                ("cutoff", 7000.0),
                ("reverb_send", 0.45),
            ],
        ),
        make(
            "Pluck Slices",
            Sampler,
            &[
                ("source", 4.0),
                ("mode", 2.0),
                ("slices", 8.0),
                ("slice_by", 1.0),
                ("sensitivity", 0.8),
                ("reverb_send", 0.25),
            ],
        ),
        make(
            "Saw Stab",
            Sampler,
            &[
                ("source", 3.0),
                ("mode", 1.0),
                ("start", 0.55),
                ("end", 0.62),
                ("filter_type", 0.0),
                ("cutoff", 3000.0),
                ("resonance", 0.3),
                ("vel_cutoff", 0.6),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Reverse Glass",
            Sampler,
            &[
                ("source", 2.0),
                ("mode", 1.0),
                ("reverse", 1.0),
                ("start", 0.0),
                ("end", 0.5),
                ("attack", 0.4),
                ("reverb_send", 0.5),
            ],
        ),
    ]
}

/// Lookup helper for master params by key (used by CC mappings).
pub fn master_index(key: &str) -> Option<usize> {
    params::index_of(&master::PARAMS, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_patches_are_valid() {
        for p in factory_patches() {
            assert_eq!(p.values().len(), p.kind.param_count());
        }
    }

    #[test]
    fn patch_round_trip() {
        let p = &factory_patches()[1];
        let text = serde_json::to_string(p).unwrap();
        let back: Patch = serde_json::from_str(&text).unwrap();
        assert_eq!(back.values(), p.values());
    }
}
