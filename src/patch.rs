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
    /// The sample file of a granular synth or sampler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<PathBuf>,
    /// Per-pad sample files of a drum kit, keyed "pad1".."pad16".
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub samples: BTreeMap<String, PathBuf>,
}

impl Patch {
    /// `samples` holds one optional path per sample slot of the synth type.
    pub fn from_values(
        name: &str,
        kind: SynthKind,
        values: &[f32],
        samples: &[Option<PathBuf>],
    ) -> Self {
        let (sample, pads) = if kind == SynthKind::Kit {
            let pads = samples
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.clone().map(|p| (format!("pad{}", i + 1), p)))
                .collect();
            (None, pads)
        } else {
            (samples.first().cloned().flatten(), BTreeMap::new())
        };
        Patch {
            samples: pads,
            name: name.to_string(),
            kind,
            // Settings of inactive FX types are left out to keep files readable.
            params: kind
                .params()
                .zip(values)
                .enumerate()
                .filter(|(i, _)| kind.param_visible(values, *i))
                .map(|(_, (p, v))| (p.key.to_string(), *v))
                .collect(),
            sample,
        }
    }

    /// One optional sample path per sample slot of the synth type.
    pub fn sample_paths(&self) -> Vec<Option<PathBuf>> {
        if self.kind == SynthKind::Kit {
            (1..=self.kind.sample_slots())
                .map(|i| self.samples.get(&format!("pad{i}")).cloned())
                .collect()
        } else {
            (0..self.kind.sample_slots())
                .map(|_| self.sample.clone())
                .collect()
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
        for dir in [
            s.patches_dir(),
            s.sessions_dir(),
            s.samples_dir(),
            s.recordings_dir(),
        ] {
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

    pub fn recordings_dir(&self) -> PathBuf {
        self.root.join("recordings")
    }

    pub fn autosave_path(&self) -> PathBuf {
        self.root.join("autosave.json")
    }

    pub fn save_patch(&self, patch: &Patch) -> Result<PathBuf> {
        let path = self
            .patches_dir()
            .join(format!("{}.json", file_stem(&patch.name)));
        write_json(&path, patch)?;
        Ok(path)
    }

    pub fn save_session(&self, name: &str, session: &Session) -> Result<PathBuf> {
        let path = self
            .sessions_dir()
            .join(format!("{}.json", file_stem(name)));
        write_json(&path, session)?;
        Ok(path)
    }

    /// Built-in factory patches plus the user's saved ones, grouped by synth
    /// kind with the user's patches first. Unreadable files are skipped.
    pub fn list_patches(&self) -> Vec<PatchEntry> {
        let user = json_files(&self.patches_dir()).into_iter().filter_map(|p| {
            read_json::<Patch>(&p).ok().map(|patch| PatchEntry {
                patch,
                user_path: Some(p),
            })
        });
        let factory = factory_patches().into_iter().map(|patch| PatchEntry {
            patch,
            user_path: None,
        });
        let mut out: Vec<PatchEntry> = user.chain(factory).collect();
        out.sort_by_key(|e| {
            (
                e.patch.kind.order(),
                e.is_factory(),
                e.patch.name.to_lowercase(),
            )
        });
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
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') {
                c
            } else {
                '_'
            }
        })
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
    Patch::from_values(name, kind, &values, &[])
}

pub fn factory_patches() -> Vec<Patch> {
    use SynthKind::{Acid, Analog, Drums, Fm, Granular, Kit, Physical, Sampler};
    let mut patches = vec![
        make("FM Init", Fm, &[]),
        make(
            "E.Piano",
            Fm,
            &[
                ("fx1_type", 11.0),
                ("fx1_mix", 1.0),
                ("fx1_trem_rate", 4.0),
                ("fx1_trem_depth", 0.35),
                ("fx1_trem_pan", 1.0),
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
                ("volume", 0.95),
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
                ("volume", 0.6),
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
                ("volume", 0.6),
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
                ("volume", 0.62),
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
                ("volume", 0.95),
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
                ("volume", 0.95),
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
                ("fx1_type", 1.0),
                ("fx1_mix", 0.25),
                ("fx1_delay_time", 0.28),
                ("fx1_delay_feedback", 0.45),
                ("fx1_delay_tone", 3000.0),
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
            &[
                ("wave", 1.0),
                ("cutoff", 250.0),
                ("resonance", 0.6),
                ("env_mod", 0.5),
                ("decay", 0.6),
            ],
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
        make("Kit Init", Kit, &[]),
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
                ("fx1_type", 9.0),
                ("fx1_mix", 1.0),
                ("fx1_comp_threshold", -20.0),
                ("fx1_comp_ratio", 4.0),
                ("fx1_comp_attack", 0.01),
                ("fx1_comp_release", 0.1),
                ("fx1_comp_makeup", 1.0),
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
                ("fx1_type", 10.0),
                ("fx1_mix", 1.0),
                ("fx1_crush_bits", 10.0),
                ("fx1_crush_downsample", 3.0),
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
        make("Analog Init", Analog, &[]),
        make(
            "Warm Pad",
            Analog,
            &[
                ("osc2_detune", 9.0),
                ("cutoff", 900.0),
                ("resonance", 0.15),
                ("filter_env", 0.25),
                ("key_track", 0.3),
                ("f_attack", 1.2),
                ("f_decay", 2.0),
                ("f_sustain", 0.5),
                ("f_release", 2.5),
                ("attack", 0.8),
                ("sustain", 1.0),
                ("release", 2.5),
                ("lfo_rate", 0.3),
                ("lfo_filter", 0.1),
                ("chorus", 2.0),
                ("reverb_send", 0.5),
            ],
        ),
        make(
            "Poly Brass",
            Analog,
            &[
                ("osc2_detune", 5.0),
                ("cutoff", 550.0),
                ("resonance", 0.1),
                ("filter_env", 0.55),
                ("f_attack", 0.08),
                ("f_decay", 0.5),
                ("f_sustain", 0.45),
                ("f_release", 0.3),
                ("vel_filter", 0.5),
                ("attack", 0.04),
                ("sustain", 0.9),
                ("release", 0.3),
                ("wheel_vib", 20.0),
                ("chorus", 1.0),
                ("reverb_send", 0.25),
            ],
        ),
        make(
            "Juno Strings",
            Analog,
            &[
                ("volume", 0.55),
                ("osc1_wave", 1.0),
                ("osc_mix", 0.35),
                ("pulse_width", 0.5),
                ("lfo_wave", 1.0),
                ("lfo_rate", 0.7),
                ("lfo_pw", 0.6),
                ("poles", 0.0),
                ("cutoff", 3500.0),
                ("resonance", 0.05),
                ("filter_env", 0.0),
                ("attack", 0.35),
                ("sustain", 1.0),
                ("release", 1.2),
                ("chorus", 1.0),
                ("reverb_send", 0.4),
            ],
        ),
        make(
            "Supersaw Lead",
            Analog,
            &[
                ("fx1_type", 1.0),
                ("fx1_mix", 0.2),
                ("fx1_delay_time", 0.33),
                ("fx1_delay_feedback", 0.35),
                ("voices", 4.0),
                ("glide", 0.03),
                ("unison", 7.0),
                ("unison_detune", 0.5),
                ("unison_spread", 0.85),
                ("cutoff", 6000.0),
                ("resonance", 0.2),
                ("filter_env", 0.2),
                ("sustain", 0.9),
                ("release", 0.35),
                ("wheel_vib", 40.0),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Poly Stab",
            Analog,
            &[
                ("osc2_wave", 1.0),
                ("osc2_semi", 12.0),
                ("cutoff", 400.0),
                ("resonance", 0.35),
                ("filter_env", 0.7),
                ("f_decay", 0.25),
                ("f_sustain", 0.0),
                ("vel_filter", 0.6),
                ("decay", 0.5),
                ("sustain", 0.2),
                ("release", 0.2),
                ("chorus", 1.0),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Soft Bass",
            Analog,
            &[
                ("volume", 1.0),
                ("osc2_wave", 2.0),
                ("osc2_semi", -12.0),
                ("osc2_detune", 0.0),
                ("sub", 0.6),
                ("drift", 0.15),
                ("cutoff", 300.0),
                ("resonance", 0.2),
                ("filter_env", 0.4),
                ("f_decay", 0.3),
                ("f_sustain", 0.2),
                ("key_track", 0.3),
                ("sustain", 0.9),
                ("release", 0.1),
                ("reverb_send", 0.0),
            ],
        ),
        make("Physical Init", Physical, &[]),
        make(
            "Nylon Guitar",
            Physical,
            &[
                ("hardness", 0.35),
                ("position", 0.2),
                ("decay", 3.5),
                ("hf_damp", 0.55),
                ("body", 0.6),
                ("release_damp", 0.7),
                ("width", 0.3),
                ("reverb_send", 0.25),
            ],
        ),
        make(
            "Steel Guitar",
            Physical,
            &[
                ("hardness", 0.7),
                ("position", 0.12),
                ("decay", 6.0),
                ("hf_damp", 0.25),
                ("body", 0.4),
                ("release_damp", 0.6),
                ("reverb_send", 0.25),
            ],
        ),
        make(
            "Harp",
            Physical,
            &[
                ("hardness", 0.4),
                ("position", 0.45),
                ("decay", 5.0),
                ("hf_damp", 0.45),
                ("body", 0.2),
                ("release_damp", 0.0),
                ("width", 0.6),
                ("reverb_send", 0.35),
            ],
        ),
        make(
            "Clav",
            Physical,
            &[
                ("fx1_type", 7.0),
                ("fx1_mix", 1.0),
                ("fx1_filter_cutoff", 1200.0),
                ("fx1_filter_resonance", 0.6),
                ("fx1_filter_lfo_rate", 2.0),
                ("fx1_filter_lfo_depth", 0.5),
                ("hardness", 0.85),
                ("position", 0.08),
                ("decay", 1.2),
                ("hf_damp", 0.3),
                ("body", 0.0),
                ("release_damp", 1.0),
                ("tone", 7000.0),
                ("reverb_send", 0.1),
            ],
        ),
        make(
            "Koto",
            Physical,
            &[
                ("hardness", 0.8),
                ("position", 0.1),
                ("decay", 4.0),
                ("hf_damp", 0.35),
                ("stiffness", 0.15),
                ("body", 0.35),
                ("release_damp", 0.4),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Hammered Wire",
            Physical,
            &[
                ("hardness", 0.6),
                ("position", 0.12),
                ("decay", 8.0),
                ("hf_damp", 0.4),
                ("stiffness", 0.45),
                ("body", 0.2),
                ("release_damp", 0.9),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Modelled Marimba",
            Physical,
            &[
                ("model", 1.0),
                ("material", 0.0),
                ("hardness", 0.45),
                ("position", 0.2),
                ("decay", 2.4),
                ("hf_damp", 0.5),
                ("release_damp", 0.0),
                ("body", 0.2),
                ("reverb_send", 0.25),
            ],
        ),
        make(
            "Vibraphone",
            Physical,
            &[
                ("model", 1.0),
                ("material", 1.0),
                ("hardness", 0.55),
                ("decay", 2.5),
                ("hf_damp", 0.3),
                ("release_damp", 0.8),
                ("body", 0.0),
                ("width", 0.5),
                ("reverb_send", 0.35),
            ],
        ),
        make(
            "Glass Bowl",
            Physical,
            &[
                ("model", 1.0),
                ("material", 2.0),
                ("hardness", 0.7),
                ("decay", 3.0),
                ("hf_damp", 0.2),
                ("release_damp", 0.0),
                ("body", 0.0),
                ("reverb_send", 0.5),
            ],
        ),
        make(
            "Xylophone",
            Physical,
            &[
                ("model", 1.0),
                ("material", 3.0),
                ("hardness", 0.9),
                ("decay", 0.9),
                ("hf_damp", 0.6),
                ("release_damp", 0.0),
                ("body", 0.1),
                ("reverb_send", 0.2),
            ],
        ),
        make(
            "Church Bell",
            Physical,
            &[
                ("model", 1.0),
                ("material", 4.0),
                ("transpose", -12.0),
                ("voices", 8.0),
                ("hardness", 0.8),
                ("decay", 3.0),
                ("hf_damp", 0.25),
                ("release_damp", 0.0),
                ("body", 0.0),
                ("reverb_send", 0.6),
            ],
        ),
        make(
            "Tuned Drum",
            Physical,
            &[
                ("model", 1.0),
                ("material", 5.0),
                ("hardness", 0.6),
                ("decay", 1.5),
                ("hf_damp", 0.5),
                ("release_damp", 0.0),
                ("body", 0.0),
                ("reverb_send", 0.15),
            ],
        ),
        make(
            "Kalimba",
            Physical,
            &[
                ("model", 1.0),
                ("material", 6.0),
                ("hardness", 0.6),
                ("position", 0.15),
                ("decay", 2.0),
                ("hf_damp", 0.4),
                ("release_damp", 0.3),
                ("body", 0.3),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Dub Chord",
            Analog,
            &[
                ("volume", 0.95),
                ("osc2_detune", 6.0),
                ("cutoff", 700.0),
                ("resonance", 0.25),
                ("filter_env", 0.5),
                ("f_decay", 0.18),
                ("f_sustain", 0.0),
                ("decay", 0.3),
                ("sustain", 0.0),
                ("release", 0.15),
                ("reverb_send", 0.0),
                ("fx1_type", 1.0),
                ("fx1_mix", 0.35),
                ("fx1_delay_time", 0.375),
                ("fx1_delay_feedback", 0.6),
                ("fx1_delay_tone", 2500.0),
                ("fx2_type", 2.0),
                ("fx2_mix", 0.25),
                ("fx2_reverb_size", 0.8),
            ],
        ),
        make(
            "Flanged Pad",
            Analog,
            &[
                ("osc2_detune", 9.0),
                ("cutoff", 1500.0),
                ("filter_env", 0.1),
                ("attack", 0.6),
                ("sustain", 1.0),
                ("release", 2.0),
                ("reverb_send", 0.4),
                ("fx1_type", 4.0),
                ("fx1_mix", 0.5),
                ("fx1_flanger_rate", 0.12),
                ("fx1_flanger_depth", 0.8),
                ("fx1_flanger_feedback", 0.7),
            ],
        ),
        make(
            "Grand Piano",
            Physical,
            &[
                ("model", 2.0),
                ("hardness", 0.45),
                ("position", 0.12),
                ("vel_bright", 0.8),
                ("decay", 9.0),
                ("hf_damp", 0.35),
                ("stiffness", 0.3),
                ("unison", 0.8),
                ("body", 0.25),
                ("release_damp", 0.95),
                ("width", 0.6),
                ("reverb_send", 0.25),
            ],
        ),
        make(
            "Upright Piano",
            Physical,
            &[
                ("model", 2.0),
                ("hardness", 0.55),
                ("position", 0.12),
                ("vel_bright", 0.7),
                ("decay", 6.0),
                ("hf_damp", 0.45),
                ("stiffness", 0.4),
                ("unison", 1.6),
                ("body", 0.4),
                ("release_damp", 0.9),
                ("width", 0.4),
                ("tone", 9000.0),
                ("reverb_send", 0.15),
            ],
        ),
        make(
            "Honky-Tonk",
            Physical,
            &[
                ("model", 2.0),
                ("hardness", 0.7),
                ("position", 0.12),
                ("decay", 5.0),
                ("hf_damp", 0.35),
                ("stiffness", 0.4),
                ("unison", 7.0),
                ("body", 0.3),
                ("release_damp", 0.9),
                ("tone", 8000.0),
                ("reverb_send", 0.15),
            ],
        ),
        make(
            "Felt Piano",
            Physical,
            &[
                ("model", 2.0),
                ("hardness", 0.1),
                ("position", 0.12),
                ("vel_bright", 0.3),
                ("decay", 7.0),
                ("hf_damp", 0.65),
                ("stiffness", 0.25),
                ("unison", 1.0),
                ("body", 0.2),
                ("release_damp", 0.7),
                ("tone", 3500.0),
                ("width", 0.6),
                ("reverb_send", 0.45),
            ],
        ),
        make(
            "Solo Violin",
            Physical,
            &[
                ("model", 3.0),
                ("voices", 6.0),
                ("bowed_body", 0.0),
                ("bow_pressure", 0.55),
                ("bow_position", 0.13),
                ("bow_attack", 0.12),
                ("vibrato", 18.0),
                ("vibrato_rate", 5.6),
                ("hf_damp", 0.4),
                ("body", 0.9),
                ("width", 0.2),
                ("reverb_send", 0.35),
            ],
        ),
        make(
            "Viola",
            Physical,
            &[
                ("model", 3.0),
                ("voices", 6.0),
                ("bowed_body", 1.0),
                ("bow_attack", 0.14),
                ("vibrato", 15.0),
                ("vibrato_rate", 5.3),
                ("hf_damp", 0.45),
                ("body", 0.9),
                ("width", 0.2),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Cello",
            Physical,
            &[
                ("model", 3.0),
                ("voices", 6.0),
                ("bowed_body", 2.0),
                ("bow_attack", 0.18),
                ("vibrato", 14.0),
                ("vibrato_rate", 5.0),
                ("hf_damp", 0.5),
                ("body", 0.9),
                ("width", 0.2),
                ("reverb_send", 0.3),
            ],
        ),
        make(
            "Double Bass",
            Physical,
            &[
                ("model", 3.0),
                ("voices", 4.0),
                ("bowed_body", 3.0),
                ("bow_pressure", 0.7),
                ("bow_attack", 0.2),
                ("vibrato", 8.0),
                ("vibrato_rate", 4.8),
                ("hf_damp", 0.6),
                ("body", 0.9),
                ("width", 0.1),
                ("reverb_send", 0.2),
            ],
        ),
        make(
            "String Section",
            Physical,
            &[
                ("model", 3.0),
                ("voices", 16.0),
                ("bowed_body", 0.0),
                ("bow_attack", 0.3),
                ("vibrato", 10.0),
                ("vibrato_rate", 5.4),
                ("hf_damp", 0.45),
                ("body", 0.8),
                ("width", 0.7),
                ("reverb_send", 0.5),
                ("fx1_type", 3.0),
                ("fx1_mix", 0.45),
                ("fx1_chorus_rate", 0.4),
                ("fx1_chorus_depth", 0.4),
            ],
        ),
        make(
            "DX Bass",
            Fm,
            &[
                ("volume", 0.85),
                ("algorithm", 4.0),
                ("voices", 2.0),
                ("feedback", 0.35),
                ("reverb_send", 0.0),
                ("op1_ratio", 1.0),
                ("op1_level", 0.85),
                ("op1_decay", 1.2),
                ("op1_sustain", 0.35),
                ("op1_release", 0.08),
                ("op1_vel", 0.3),
                ("op2_ratio", 1.0),
                ("op2_level", 0.65),
                ("op2_decay", 0.22),
                ("op2_sustain", 0.12),
                ("op2_release", 0.08),
                ("op2_vel", 0.8),
                ("op3_ratio", 1.0),
                ("op3_detune", 5.0),
                ("op3_level", 0.55),
                ("op3_decay", 1.0),
                ("op3_sustain", 0.3),
                ("op3_release", 0.08),
                ("op3_vel", 0.3),
                ("op4_ratio", 1.0),
                ("op4_level", 0.5),
                ("op4_decay", 0.3),
                ("op4_sustain", 0.1),
                ("op4_release", 0.08),
                ("op4_vel", 0.7),
            ],
        ),
        make(
            "Tubular Bells",
            Fm,
            &[
                ("volume", 0.62),
                ("algorithm", 4.0),
                ("voices", 8.0),
                ("reverb_send", 0.45),
                ("op1_ratio", 1.0),
                ("op1_level", 0.8),
                ("op1_decay", 7.0),
                ("op1_sustain", 0.0),
                ("op1_release", 3.0),
                ("op1_vel", 0.4),
                ("op2_ratio", 3.5),
                ("op2_level", 0.5),
                ("op2_decay", 4.0),
                ("op2_sustain", 0.0),
                ("op2_release", 3.0),
                ("op2_vel", 0.6),
                ("op3_ratio", 1.0),
                ("op3_detune", 4.0),
                ("op3_level", 0.6),
                ("op3_decay", 6.0),
                ("op3_sustain", 0.0),
                ("op3_release", 3.0),
                ("op4_ratio", 7.07),
                ("op4_level", 0.35),
                ("op4_decay", 3.0),
                ("op4_sustain", 0.0),
                ("op4_release", 2.0),
            ],
        ),
        make(
            "Bright EP",
            Fm,
            &[
                ("algorithm", 4.0),
                ("reverb_send", 0.25),
                ("fx1_type", 3.0),
                ("fx1_mix", 0.35),
                ("fx1_chorus_rate", 0.5),
                ("fx1_chorus_depth", 0.45),
                ("op1_ratio", 1.0),
                ("op1_level", 0.8),
                ("op1_decay", 2.5),
                ("op1_sustain", 0.0),
                ("op1_release", 0.4),
                ("op2_ratio", 14.0),
                ("op2_level", 0.45),
                ("op2_decay", 0.3),
                ("op2_sustain", 0.0),
                ("op2_vel", 0.95),
                ("op3_ratio", 1.0),
                ("op3_detune", 3.0),
                ("op3_level", 0.75),
                ("op3_decay", 3.0),
                ("op3_sustain", 0.0),
                ("op4_ratio", 1.0),
                ("op4_level", 0.62),
                ("op4_decay", 1.0),
                ("op4_sustain", 0.1),
                ("op4_vel", 0.85),
            ],
        ),
        make(
            "FM Clav",
            Fm,
            &[
                ("algorithm", 4.0),
                ("voices", 8.0),
                ("reverb_send", 0.1),
                ("op1_ratio", 1.0),
                ("op1_level", 0.75),
                ("op1_decay", 0.9),
                ("op1_sustain", 0.1),
                ("op1_release", 0.04),
                ("op1_vel", 0.7),
                ("op2_ratio", 1.0),
                ("op2_level", 0.7),
                ("op2_decay", 0.25),
                ("op2_sustain", 0.3),
                ("op2_release", 0.04),
                ("op2_vel", 0.8),
                ("op3_ratio", 3.0),
                ("op3_level", 0.45),
                ("op3_decay", 0.6),
                ("op3_sustain", 0.05),
                ("op3_release", 0.04),
                ("op3_vel", 0.6),
                ("op4_ratio", 6.0),
                ("op4_level", 0.35),
                ("op4_decay", 0.15),
                ("op4_sustain", 0.0),
                ("op4_release", 0.04),
                ("op4_vel", 0.8),
            ],
        ),
        make(
            "Saw Lead",
            Fm,
            &[
                ("algorithm", 0.0),
                ("voices", 1.0),
                ("glide", 0.04),
                ("feedback", 0.95),
                ("wheel_vib", 40.0),
                ("reverb_send", 0.2),
                ("fx1_type", 1.0),
                ("fx1_mix", 0.2),
                ("fx1_delay_time", 0.3),
                ("fx1_delay_feedback", 0.3),
                ("op1_ratio", 1.0),
                ("op1_level", 0.8),
                ("op1_decay", 1.0),
                ("op1_sustain", 0.9),
                ("op1_release", 0.15),
                ("op2_ratio", 1.0),
                ("op2_level", 0.55),
                ("op2_sustain", 0.8),
                ("op3_ratio", 1.0),
                ("op3_level", 0.45),
                ("op3_sustain", 0.8),
                ("op4_ratio", 1.0),
                ("op4_level", 0.6),
                ("op4_sustain", 0.9),
            ],
        ),
        make(
            "Steel Pan",
            Fm,
            &[
                ("algorithm", 4.0),
                ("voices", 8.0),
                ("reverb_send", 0.35),
                ("op1_ratio", 1.0),
                ("op1_level", 0.8),
                ("op1_decay", 1.6),
                ("op1_sustain", 0.0),
                ("op1_release", 0.8),
                ("op1_vel", 0.5),
                ("op2_ratio", 2.0),
                ("op2_level", 0.4),
                ("op2_decay", 0.6),
                ("op2_sustain", 0.0),
                ("op2_vel", 0.7),
                ("op3_ratio", 2.0),
                ("op3_detune", 3.0),
                ("op3_level", 0.45),
                ("op3_decay", 1.0),
                ("op3_sustain", 0.0),
                ("op3_release", 0.6),
                ("op4_ratio", 4.5),
                ("op4_level", 0.25),
                ("op4_decay", 0.3),
                ("op4_sustain", 0.0),
            ],
        ),
        make(
            "Log Drum",
            Fm,
            &[
                ("volume", 0.95),
                ("algorithm", 4.0),
                ("voices", 6.0),
                ("reverb_send", 0.25),
                ("op1_ratio", 1.0),
                ("op1_level", 0.85),
                ("op1_decay", 0.4),
                ("op1_sustain", 0.0),
                ("op1_release", 0.3),
                ("op2_ratio", 1.41),
                ("op2_level", 0.45),
                ("op2_decay", 0.05),
                ("op2_sustain", 0.0),
                ("op2_vel", 0.8),
                ("op3_ratio", 2.92),
                ("op3_level", 0.35),
                ("op3_decay", 0.2),
                ("op3_sustain", 0.0),
                ("op3_release", 0.2),
                ("op4_ratio", 1.0),
                ("op4_level", 0.2),
                ("op4_decay", 0.03),
                ("op4_sustain", 0.0),
            ],
        ),
        make(
            "Ice Pad",
            Fm,
            &[
                ("algorithm", 6.0),
                ("voices", 10.0),
                ("vib_depth", 4.0),
                ("reverb_send", 0.6),
                ("fx1_type", 3.0),
                ("fx1_mix", 0.4),
                ("op1_ratio", 1.0),
                ("op1_level", 0.6),
                ("op1_attack", 1.2),
                ("op1_decay", 3.0),
                ("op1_sustain", 0.8),
                ("op1_release", 3.0),
                ("op2_ratio", 7.0),
                ("op2_level", 0.18),
                ("op2_attack", 2.0),
                ("op2_decay", 4.0),
                ("op2_sustain", 0.6),
                ("op2_release", 3.0),
                ("op2_vel", 0.3),
                ("op3_ratio", 2.0),
                ("op3_detune", 6.0),
                ("op3_level", 0.35),
                ("op3_attack", 1.5),
                ("op3_sustain", 0.8),
                ("op3_release", 3.0),
                ("op4_ratio", 3.0),
                ("op4_detune", -5.0),
                ("op4_level", 0.2),
                ("op4_attack", 1.8),
                ("op4_sustain", 0.8),
                ("op4_release", 3.0),
            ],
        ),
        make(
            "FM Flute",
            Fm,
            &[
                ("volume", 0.6),
                ("algorithm", 0.0),
                ("voices", 6.0),
                ("feedback", 1.0),
                ("vib_rate", 5.0),
                ("vib_depth", 6.0),
                ("wheel_vib", 25.0),
                ("reverb_send", 0.35),
                ("op1_ratio", 1.0),
                ("op1_level", 0.8),
                ("op1_attack", 0.07),
                ("op1_decay", 1.0),
                ("op1_sustain", 0.85),
                ("op1_release", 0.15),
                ("op1_vel", 0.4),
                ("op2_ratio", 1.0),
                ("op2_level", 0.2),
                ("op2_attack", 0.12),
                ("op2_decay", 1.0),
                ("op2_sustain", 0.6),
                ("op2_release", 0.15),
                ("op3_ratio", 2.0),
                ("op3_level", 0.1),
                ("op3_attack", 0.1),
                ("op3_sustain", 0.5),
                ("op4_ratio", 1.0),
                ("op4_level", 0.25),
                ("op4_attack", 0.02),
                ("op4_decay", 0.08),
                ("op4_sustain", 0.05),
            ],
        ),
        make(
            "Harpsichord",
            Fm,
            &[
                ("algorithm", 4.0),
                ("voices", 12.0),
                ("reverb_send", 0.2),
                ("op1_ratio", 1.0),
                ("op1_level", 0.8),
                ("op1_decay", 3.0),
                ("op1_sustain", 0.0),
                ("op1_release", 0.15),
                ("op1_vel", 0.2),
                ("op2_ratio", 5.0),
                ("op2_level", 0.55),
                ("op2_decay", 0.9),
                ("op2_sustain", 0.0),
                ("op2_vel", 0.3),
                ("op3_ratio", 2.0),
                ("op3_level", 0.6),
                ("op3_decay", 2.0),
                ("op3_sustain", 0.0),
                ("op3_release", 0.15),
                ("op3_vel", 0.2),
                ("op4_ratio", 9.0),
                ("op4_level", 0.3),
                ("op4_decay", 0.5),
                ("op4_sustain", 0.0),
            ],
        ),
        make(
            "Frozen Choir",
            Granular,
            &[
                ("source", 1.0),
                ("position", 0.5),
                ("speed", 0.0),
                ("spray", 0.04),
                ("size", 0.35),
                ("density", 40.0),
                ("jitter", 0.5),
                ("spread", 1.0),
                ("attack", 1.2),
                ("release", 4.0),
                ("cutoff", 7000.0),
                ("reverb_send", 0.5),
            ],
        ),
        make(
            "Vowel Morph",
            Granular,
            &[
                ("source", 1.0),
                ("position", 0.0),
                ("speed", 0.3),
                ("spray", 0.02),
                ("size", 0.15),
                ("density", 50.0),
                ("jitter", 0.3),
                ("spread", 0.7),
                ("attack", 0.4),
                ("release", 2.0),
                ("reverb_send", 0.35),
            ],
        ),
        make(
            "Glass Shimmer",
            Granular,
            &[
                ("source", 2.0),
                ("spray", 0.3),
                ("size", 0.5),
                ("density", 30.0),
                ("reverse", 0.3),
                ("spread", 1.0),
                ("attack", 1.0),
                ("release", 3.5),
                ("reverb_send", 0.2),
                ("fx1_type", 1.0),
                ("fx1_mix", 0.3),
                ("fx1_delay_time", 0.43),
                ("fx1_delay_feedback", 0.55),
                ("fx1_delay_tone", 5000.0),
                ("fx2_type", 2.0),
                ("fx2_mix", 0.35),
                ("fx2_reverb_size", 0.85),
            ],
        ),
        make(
            "Grain Bass",
            Granular,
            &[
                ("source", 3.0),
                ("voices", 4.0),
                ("position", 0.5),
                ("spray", 0.01),
                ("size", 0.08),
                ("density", 60.0),
                ("jitter", 0.15),
                ("spread", 0.2),
                ("attack", 0.005),
                ("decay", 0.5),
                ("sustain", 0.6),
                ("release", 0.2),
                ("cutoff", 900.0),
                ("resonance", 0.3),
                ("reverb_send", 0.0),
            ],
        ),
        make(
            "Stutter Pluck",
            Granular,
            &[
                ("source", 4.0),
                ("position", 0.0),
                ("spray", 0.0),
                ("speed", 0.5),
                ("size", 0.04),
                ("density", 12.0),
                ("jitter", 0.0),
                ("shape", 3.0),
                ("spread", 0.8),
                ("attack", 0.005),
                ("release", 0.6),
                ("reverb_send", 0.25),
            ],
        ),
        make(
            "Reverse Swell",
            Granular,
            &[
                ("source", 1.0),
                ("reverse", 1.0),
                ("size", 0.6),
                ("density", 15.0),
                ("spray", 0.2),
                ("attack", 2.0),
                ("sustain", 1.0),
                ("release", 3.0),
                ("spread", 1.0),
                ("reverb_send", 0.3),
                ("fx1_type", 2.0),
                ("fx1_mix", 0.4),
            ],
        ),
        make(
            "Rain",
            Granular,
            &[
                ("source", 5.0),
                ("keytrack", 0.0),
                ("spray", 1.0),
                ("size", 0.01),
                ("density", 150.0),
                ("jitter", 1.0),
                ("shape", 3.0),
                ("spread", 1.0),
                ("filter_type", 2.0),
                ("cutoff", 2500.0),
                ("resonance", 0.2),
                ("attack", 0.3),
                ("release", 1.5),
                ("reverb_send", 0.4),
            ],
        ),
        make(
            "Saw Cloud",
            Granular,
            &[
                ("source", 3.0),
                ("spray", 0.15),
                ("size", 0.25),
                ("density", 60.0),
                ("jitter", 0.6),
                ("pitch_spray", 0.15),
                ("spread", 1.0),
                ("attack", 1.0),
                ("release", 3.0),
                ("cutoff", 3000.0),
                ("reverb_send", 0.45),
                ("fx1_type", 3.0),
                ("fx1_mix", 0.35),
            ],
        ),
        make(
            "Bowed Glass",
            Granular,
            &[
                ("volume", 0.6),
                ("source", 2.0),
                ("position", 0.3),
                ("speed", 0.05),
                ("spray", 0.05),
                ("size", 0.9),
                ("density", 12.0),
                ("jitter", 0.2),
                ("spread", 0.8),
                ("attack", 0.6),
                ("sustain", 1.0),
                ("release", 2.5),
                ("reverb_send", 0.45),
            ],
        ),
        make(
            "Lo-Fi Grains",
            Granular,
            &[
                ("volume", 0.9),
                ("source", 4.0),
                ("speed", 0.2),
                ("size", 0.07),
                ("density", 35.0),
                ("spray", 0.3),
                ("shape", 3.0),
                ("reverb_send", 0.2),
                ("fx1_type", 10.0),
                ("fx1_mix", 1.0),
                ("fx1_crush_bits", 8.0),
                ("fx1_crush_downsample", 6.0),
            ],
        ),
        make(
            "DnB Sub",
            Analog,
            &[
                ("volume", 0.8),
                ("fx1_type", 8.0),
                ("fx1_mix", 1.0),
                ("fx1_eq_low", 12.0),
                ("voices", 1.0),
                ("reverb_send", 0.0),
                ("wheel_vib", 0.0),
                ("glide", 0.03),
                ("osc1_wave", 2.0),
                ("osc_mix", 0.0),
                ("sub", 0.0),
                ("noise", 0.0),
                ("drift", 0.05),
                ("unison", 1.0),
                ("cutoff", 400.0),
                ("resonance", 0.0),
                ("poles", 1.0),
                ("filter_env", 0.0),
                ("key_track", 1.0),
                ("vel_filter", 0.0),
                ("attack", 0.003),
                ("decay", 0.5),
                ("sustain", 1.0),
                ("release", 0.08),
                ("vel_amp", 0.1),
                ("lfo_filter", 0.0),
                ("chorus", 0.0),
            ],
        ),
        make(
            "Reese",
            Analog,
            &[
                ("voices", 1.0),
                ("reverb_send", 0.0),
                ("wheel_vib", 0.0),
                ("glide", 0.03),
                ("osc1_wave", 0.0),
                ("osc2_wave", 0.0),
                ("osc2_detune", 18.0),
                ("osc_mix", 0.5),
                ("unison", 3.0),
                ("unison_detune", 0.25),
                ("unison_spread", 0.0),
                ("drift", 0.2),
                ("cutoff", 900.0),
                ("resonance", 0.25),
                ("poles", 1.0),
                ("filter_env", 0.15),
                ("f_decay", 0.3),
                ("f_sustain", 0.5),
                ("key_track", 0.3),
                ("vel_filter", 0.0),
                ("attack", 0.003),
                ("sustain", 1.0),
                ("release", 0.12),
                ("vel_amp", 0.2),
                ("lfo_wave", 0.0),
                ("lfo_rate", 0.3),
                ("lfo_filter", 0.15),
                ("chorus", 0.0),
                ("fx1_type", 6.0),
                ("fx1_mix", 1.0),
                ("fx1_drive_mode", 3.0),
                ("fx1_drive_amount", 0.55),
                ("fx1_drive_tone", 6000.0),
                ("fx1_drive_output", -6.0),
                ("fx2_type", 9.0),
                ("fx2_mix", 1.0),
                ("fx2_comp_threshold", -20.0),
                ("fx2_comp_ratio", 5.0),
                ("fx2_comp_attack", 0.003),
                ("fx2_comp_release", 0.1),
                ("fx2_comp_makeup", 3.0),
            ],
        ),
        make(
            "Reese Mid",
            Analog,
            &[
                ("voices", 1.0),
                ("reverb_send", 0.0),
                ("wheel_vib", 0.0),
                ("glide", 0.03),
                ("osc1_wave", 0.0),
                ("osc2_wave", 0.0),
                ("osc2_detune", 18.0),
                ("osc_mix", 0.5),
                ("unison", 3.0),
                ("unison_detune", 0.25),
                ("unison_spread", 0.0),
                ("drift", 0.2),
                ("cutoff", 900.0),
                ("resonance", 0.25),
                ("poles", 1.0),
                ("filter_env", 0.15),
                ("f_decay", 0.3),
                ("f_sustain", 0.5),
                ("key_track", 0.3),
                ("vel_filter", 0.0),
                ("attack", 0.003),
                ("sustain", 1.0),
                ("release", 0.12),
                ("vel_amp", 0.2),
                ("lfo_wave", 0.0),
                ("lfo_rate", 0.3),
                ("lfo_filter", 0.15),
                ("chorus", 0.0),
                ("fx1_type", 6.0),
                ("fx1_mix", 1.0),
                ("fx1_drive_mode", 3.0),
                ("fx1_drive_amount", 0.55),
                ("fx1_drive_tone", 6000.0),
                ("fx1_drive_output", -6.0),
                ("fx2_type", 7.0),
                ("fx2_mix", 1.0),
                ("fx2_filter_type", 4.0),
                ("fx2_filter_cutoff", 140.0),
                ("fx2_filter_resonance", 0.1),
                ("fx2_filter_lfo_depth", 0.0),
                ("fx3_type", 9.0),
                ("fx3_mix", 1.0),
                ("fx3_comp_threshold", -20.0),
                ("fx3_comp_ratio", 5.0),
                ("fx3_comp_attack", 0.003),
                ("fx3_comp_release", 0.1),
                ("fx3_comp_makeup", 3.0),
            ],
        ),
        make(
            "Neuro Growl",
            Fm,
            &[
                ("volume", 0.55),
                ("voices", 1.0),
                ("reverb_send", 0.0),
                ("wheel_vib", 0.0),
                ("algorithm", 0.0),
                ("glide", 0.02),
                ("feedback", 0.8),
                ("brightness", 1.6),
                ("op1_ratio", 1.0),
                ("op1_level", 0.85),
                ("op1_decay", 1.0),
                ("op1_sustain", 1.0),
                ("op1_release", 0.1),
                ("op1_vel", 0.1),
                ("op2_ratio", 1.0),
                ("op2_level", 0.7),
                ("op2_decay", 0.5),
                ("op2_sustain", 0.8),
                ("op2_vel", 0.1),
                ("op3_ratio", 2.0),
                ("op3_detune", 7.0),
                ("op3_level", 0.5),
                ("op3_sustain", 0.7),
                ("op3_vel", 0.1),
                ("op4_ratio", 1.0),
                ("op4_level", 0.6),
                ("op4_sustain", 0.8),
                ("op4_vel", 0.1),
                ("fx1_type", 7.0),
                ("fx1_mix", 1.0),
                ("fx1_filter_type", 3.0),
                ("fx1_filter_cutoff", 1800.0),
                ("fx1_filter_resonance", 0.45),
                ("fx1_filter_lfo_rate", 2.0),
                ("fx1_filter_lfo_depth", 0.45),
                ("fx2_type", 6.0),
                ("fx2_mix", 1.0),
                ("fx2_drive_mode", 3.0),
                ("fx2_drive_amount", 0.6),
                ("fx2_drive_tone", 7000.0),
                ("fx2_drive_output", -6.0),
                ("fx3_type", 7.0),
                ("fx3_mix", 1.0),
                ("fx3_filter_type", 4.0),
                ("fx3_filter_cutoff", 110.0),
                ("fx3_filter_resonance", 0.1),
                ("fx3_filter_lfo_depth", 0.0),
            ],
        ),
        make(
            "Wobble Bass",
            Analog,
            &[
                ("voices", 1.0),
                ("reverb_send", 0.0),
                ("wheel_vib", 0.0),
                ("glide", 0.02),
                ("osc1_wave", 0.0),
                ("osc2_wave", 1.0),
                ("osc2_detune", 7.0),
                ("osc_mix", 0.5),
                ("sub", 0.4),
                ("cutoff", 300.0),
                ("resonance", 0.5),
                ("poles", 1.0),
                ("filter_env", 0.0),
                ("lfo_wave", 1.0),
                ("lfo_rate", 3.0),
                ("lfo_filter", 0.8),
                ("sustain", 1.0),
                ("release", 0.1),
                ("vel_amp", 0.2),
                ("chorus", 0.0),
                ("fx1_type", 6.0),
                ("fx1_mix", 1.0),
                ("fx1_drive_mode", 0.0),
                ("fx1_drive_amount", 0.4),
                ("fx1_drive_output", -6.0),
                ("fx2_type", 9.0),
                ("fx2_mix", 1.0),
                ("fx2_comp_threshold", -20.0),
                ("fx2_comp_ratio", 5.0),
                ("fx2_comp_attack", 0.003),
                ("fx2_comp_release", 0.1),
                ("fx2_comp_makeup", 3.0),
            ],
        ),
    ];
    // Sample kits rendered from the engines (see kitgen).
    patches.extend(crate::kitgen::factory_kit_patches());
    // Acoustic kits from VCSL (CC0), downloaded on request with --fetch-kits.
    patches.extend(crate::vcsl::factory_kit_patches());
    patches
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
