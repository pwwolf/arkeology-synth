//! Generated drum kits. Renders one-shots offline through the synth engines
//! (the 808/909 drum machine, physical-modelled membranes and bars, FM),
//! layering engines and running each layer's patch FX plus optional
//! per-hit FX, then writes trimmed 24-bit WAVs into `samples/kits/<Kit>/`.
//! Factory Kit patches reference these files by relative path.
//!
//! Kits are rendered on first launch and again whenever `KIT_VERSION`
//! changes, so updated recipes reach existing installs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::fx::{self, FX_UNITS, FxUnit};
use crate::patch::{Patch, factory_patches};
use crate::sample::Builtins;
use crate::synth::{Instrument, MAX_BLOCK, SynthKind, kit};

/// Bump when any recipe changes so installs re-render their kits.
pub const KIT_VERSION: u32 = 2;
const SR: f32 = 48_000.0;
const MARKER: &str = ".arkeology-kits-version";
/// Peak level each one-shot is normalised to (-1 dBFS).
const TARGET_PEAK: f32 = 0.89;

#[derive(Clone, Copy)]
enum Base {
    /// Start from a factory patch (its parameters and insert FX).
    Patch(&'static str),
    /// Start from a synth type's defaults.
    Kind(SynthKind),
}

type Overrides = Vec<(&'static str, f32)>;

struct Layer {
    base: Base,
    overrides: Overrides,
    note: u8,
    velocity: f32,
    gain: f32,
}

fn layer(base: Base, overrides: &[(&'static str, f32)], note: u8, gain: f32) -> Layer {
    Layer {
        base,
        overrides: overrides.to_vec(),
        note,
        velocity: 0.9,
        gain,
    }
}

struct Hit {
    file: &'static str,
    seconds: f32,
    layers: Vec<Layer>,
    /// Extra FX over the mixed layers, as fx1_/fx2_/fx3_ settings.
    fx: Overrides,
}

/// A pad in the kit's factory patch: (pad index, file, note, level, decay).
type PadSpec = (usize, &'static str, Option<u8>, f32, Option<f32>);

struct KitRecipe {
    name: &'static str,
    patch_name: &'static str,
    hits: Vec<Hit>,
    pads: Vec<PadSpec>,
}

const MEMBRANE: Base = Base::Kind(SynthKind::Physical);

/// The classic drum-machine hits, rendered from a Drums patch.
fn machine_hits(base: &'static str) -> Vec<Hit> {
    let d = Base::Patch(base);
    let hit = |file, seconds, note| Hit {
        file,
        seconds,
        layers: vec![layer(d, &[], note, 1.0)],
        fx: Vec::new(),
    };
    vec![
        hit("Kick.wav", 1.5, 36),
        hit("Snare.wav", 0.6, 38),
        hit("Clap.wav", 0.8, 39),
        hit("Rim.wav", 0.3, 37),
        hit("HH Closed.wav", 0.4, 42),
        hit("HH Open.wav", 1.2, 46),
        hit("Tom Low.wav", 1.5, 41),
        hit("Tom Mid.wav", 1.5, 45),
        hit("Tom High.wav", 1.5, 48),
        hit("Cowbell.wav", 1.0, 56),
        hit("Crash.wav", 3.0, 49),
        Hit {
            file: "Ride.wav",
            seconds: 2.5,
            layers: vec![layer(
                d,
                &[
                    ("cymbal_tune", 4.0),
                    ("cymbal_decay", 1.0),
                    ("cymbal_tone", 0.9),
                ],
                49,
                1.0,
            )],
            fx: Vec::new(),
        },
    ]
}

/// Standard pads for the machine kits (GM notes are the Kit defaults).
fn machine_pads() -> Vec<PadSpec> {
    vec![
        (0, "Kick.wav", None, 0.85, None),
        (1, "Snare.wav", None, 0.8, None),
        (2, "HH Closed.wav", None, 0.6, None),
        (3, "HH Open.wav", None, 0.55, None),
        (4, "Clap.wav", None, 0.75, None),
        (5, "Rim.wav", None, 0.65, None),
        (6, "Tom Low.wav", None, 0.75, None),
        (7, "Tom Mid.wav", None, 0.75, None),
        (8, "Tom High.wav", None, 0.75, None),
        (9, "Crash.wav", None, 0.55, None),
        (10, "Ride.wav", None, 0.5, None),
        (11, "HH Closed.wav", None, 0.5, Some(0.05)),
        (14, "Cowbell.wav", None, 0.6, None),
    ]
}

const MEMBRANE_SET: &[(&str, f32)] = &[
    ("model", 1.0),
    ("material", 5.0),
    ("body", 0.0),
    ("reverb_send", 0.0),
];

fn recipes() -> Vec<KitRecipe> {
    let k909 = Base::Patch("909 Kit");
    let room: &'static [(&str, f32)] = &[
        ("fx1_type", 2.0),
        ("fx1_mix", 0.18),
        ("fx1_reverb_size", 0.3),
        ("fx1_reverb_damp", 0.6),
        ("fx1_reverb_predelay", 0.01),
    ];
    let punch: &'static [(&str, f32)] = &[
        ("fx1_type", 9.0),
        ("fx1_mix", 1.0),
        ("fx1_comp_threshold", -18.0),
        ("fx1_comp_ratio", 4.0),
        ("fx1_comp_attack", 0.005),
        ("fx1_comp_release", 0.12),
        ("fx1_comp_makeup", 3.0),
        ("fx2_type", 6.0),
        ("fx2_mix", 1.0),
        ("fx2_drive_mode", 0.0),
        ("fx2_drive_amount", 0.3),
        ("fx2_drive_output", -3.0),
    ];
    let shaker = || Hit {
        file: "Shaker.wav",
        seconds: 0.3,
        layers: vec![layer(
            k909,
            &[("chat_decay", 0.12), ("chat_tone", 1.0)],
            42,
            1.0,
        )],
        fx: vec![
            ("fx1_type", 7.0),
            ("fx1_mix", 1.0),
            ("fx1_filter_type", 4.0),
            ("fx1_filter_cutoff", 4000.0),
        ],
    };
    let tambourine = || Hit {
        file: "Tambourine.wav",
        seconds: 0.5,
        layers: vec![
            layer(k909, &[("chat_tune", 5.0), ("chat_decay", 0.25)], 42, 1.0),
            layer(
                MEMBRANE,
                &[
                    ("model", 1.0),
                    ("material", 1.0),
                    ("decay", 0.2),
                    ("hardness", 0.9),
                    ("body", 0.0),
                ],
                96,
                0.4,
            ),
        ],
        fx: Vec::new(),
    };
    vec![
        KitRecipe {
            name: "808",
            patch_name: "808 Sampled",
            hits: machine_hits("808 Kit"),
            pads: machine_pads(),
        },
        KitRecipe {
            name: "909",
            patch_name: "909 Sampled",
            hits: machine_hits("909 Kit"),
            pads: machine_pads(),
        },
        KitRecipe {
            name: "Lo-Fi",
            patch_name: "Lo-Fi Sampled",
            hits: machine_hits("Lo-Fi Kit"),
            pads: machine_pads(),
        },
        KitRecipe {
            name: "Hybrid",
            patch_name: "Hybrid Kit",
            hits: vec![
                Hit {
                    file: "Kick.wav",
                    seconds: 1.2,
                    layers: vec![
                        layer(k909, &[], 36, 1.0),
                        layer(
                            MEMBRANE,
                            &[
                                ("model", 1.0),
                                ("material", 5.0),
                                ("decay", 0.5),
                                ("hardness", 0.7),
                                ("body", 0.0),
                            ],
                            28,
                            0.8,
                        ),
                    ],
                    fx: punch.to_vec(),
                },
                Hit {
                    file: "Snare.wav",
                    seconds: 1.0,
                    layers: vec![
                        layer(k909, &[("snare_tone", 0.8), ("snare_tune", 2.0)], 38, 1.0),
                        layer(
                            MEMBRANE,
                            &[
                                ("model", 1.0),
                                ("material", 5.0),
                                ("decay", 0.25),
                                ("hardness", 0.8),
                                ("body", 0.0),
                            ],
                            50,
                            0.6,
                        ),
                    ],
                    fx: room.to_vec(),
                },
                Hit {
                    file: "Clap.wav",
                    seconds: 1.2,
                    layers: vec![layer(k909, &[], 39, 1.0)],
                    fx: room.to_vec(),
                },
                Hit {
                    file: "HH Closed.wav",
                    seconds: 0.4,
                    layers: vec![layer(k909, &[("chat_tone", 0.7)], 42, 1.0)],
                    fx: vec![
                        ("fx1_type", 6.0),
                        ("fx1_mix", 1.0),
                        ("fx1_drive_amount", 0.2),
                        ("fx1_drive_output", -3.0),
                    ],
                },
                Hit {
                    file: "HH Open.wav",
                    seconds: 1.2,
                    layers: vec![layer(
                        k909,
                        &[("ohat_tone", 0.7), ("ohat_decay", 0.5)],
                        46,
                        1.0,
                    )],
                    fx: Vec::new(),
                },
                Hit {
                    file: "Rim.wav",
                    seconds: 0.4,
                    layers: vec![
                        layer(
                            MEMBRANE,
                            &[
                                ("model", 1.0),
                                ("material", 0.0),
                                ("decay", 0.15),
                                ("hardness", 0.9),
                                ("body", 0.0),
                            ],
                            81,
                            1.0,
                        ),
                        layer(k909, &[], 37, 0.5),
                    ],
                    fx: Vec::new(),
                },
                Hit {
                    file: "Tom Low.wav",
                    seconds: 1.5,
                    layers: tom_layers(40, 41),
                    fx: room.to_vec(),
                },
                Hit {
                    file: "Tom Mid.wav",
                    seconds: 1.5,
                    layers: tom_layers(45, 45),
                    fx: room.to_vec(),
                },
                Hit {
                    file: "Tom High.wav",
                    seconds: 1.5,
                    layers: tom_layers(50, 48),
                    fx: room.to_vec(),
                },
                Hit {
                    file: "Crash.wav",
                    seconds: 3.0,
                    layers: vec![layer(k909, &[("cymbal_decay", 2.5)], 49, 1.0)],
                    fx: room.to_vec(),
                },
                Hit {
                    file: "Ride.wav",
                    seconds: 2.5,
                    layers: vec![
                        layer(
                            MEMBRANE,
                            &[
                                ("model", 1.0),
                                ("material", 1.0),
                                ("decay", 2.0),
                                ("hardness", 0.9),
                                ("body", 0.0),
                            ],
                            84,
                            1.0,
                        ),
                        layer(
                            k909,
                            &[("cymbal_tune", 4.0), ("cymbal_decay", 1.0)],
                            49,
                            0.5,
                        ),
                    ],
                    fx: Vec::new(),
                },
                Hit {
                    file: "Cowbell.wav",
                    seconds: 1.0,
                    layers: vec![layer(k909, &[], 56, 1.0)],
                    fx: Vec::new(),
                },
                shaker(),
                tambourine(),
            ],
            pads: {
                let mut p = machine_pads();
                p.push((13, "Tambourine.wav", None, 0.55, None));
                p.push((15, "Shaker.wav", None, 0.55, None));
                p
            },
        },
        KitRecipe {
            name: "Hand Percussion",
            patch_name: "Hand Percussion",
            hits: vec![
                membrane_hit("Conga Low.wav", 47, 1.5, 0.5),
                membrane_hit("Conga High.wav", 54, 1.4, 0.5),
                membrane_hit("Bongo Low.wav", 61, 1.0, 0.7),
                membrane_hit("Bongo High.wav", 66, 0.9, 0.7),
                membrane_hit("Djembe Bass.wav", 38, 1.3, 0.3),
                bar_hit("Woodblock.wav", 0.0, 84, 0.12),
                bar_hit("Clave.wav", 3.0, 89, 0.2),
                bar_hit("Triangle.wav", 1.0, 100, 3.0),
                bar_hit("Agogo High.wav", 1.0, 79, 0.8),
                bar_hit("Agogo Low.wav", 1.0, 74, 0.8),
                Hit {
                    file: "Log Drum.wav",
                    seconds: 0.8,
                    layers: vec![layer(Base::Patch("Log Drum"), &[], 48, 1.0)],
                    fx: Vec::new(),
                },
                Hit {
                    file: "Cowbell.wav",
                    seconds: 1.0,
                    layers: vec![layer(k909, &[], 56, 1.0)],
                    fx: Vec::new(),
                },
                shaker(),
                tambourine(),
            ],
            // General MIDI percussion notes.
            pads: vec![
                (0, "Djembe Bass.wav", Some(36), 0.85, None),
                (1, "Conga Low.wav", Some(64), 0.75, None),
                (2, "Conga High.wav", Some(63), 0.75, None),
                (3, "Conga High.wav", Some(62), 0.7, Some(0.08)),
                (4, "Bongo High.wav", Some(60), 0.75, None),
                (5, "Bongo Low.wav", Some(61), 0.75, None),
                (6, "Agogo High.wav", Some(67), 0.6, None),
                (7, "Agogo Low.wav", Some(68), 0.6, None),
                (8, "Woodblock.wav", Some(76), 0.65, None),
                (9, "Log Drum.wav", Some(77), 0.7, None),
                (10, "Clave.wav", Some(75), 0.65, None),
                (11, "Triangle.wav", Some(81), 0.5, None),
                (12, "Triangle.wav", Some(80), 0.5, Some(0.1)),
                (13, "Tambourine.wav", Some(54), 0.55, None),
                (14, "Cowbell.wav", Some(56), 0.6, None),
                (15, "Shaker.wav", Some(70), 0.55, None),
            ],
        },
    ]
}

fn tom_layers(membrane_note: u8, drum_note: u8) -> Vec<Layer> {
    vec![
        layer(
            MEMBRANE,
            &[
                ("model", 1.0),
                ("material", 5.0),
                ("decay", 0.6),
                ("hardness", 0.6),
                ("body", 0.0),
            ],
            membrane_note,
            1.0,
        ),
        layer(Base::Patch("909 Kit"), &[], drum_note, 0.5),
    ]
}

fn membrane_hit(file: &'static str, note: u8, decay: f32, hardness: f32) -> Hit {
    let mut o = MEMBRANE_SET.to_vec();
    o.extend([("decay", decay), ("hardness", hardness), ("position", 0.3)]);
    Hit {
        file,
        seconds: 1.2,
        layers: vec![layer(MEMBRANE, &o, note, 1.0)],
        fx: Vec::new(),
    }
}

fn bar_hit(file: &'static str, material: f32, note: u8, decay: f32) -> Hit {
    let o = [
        ("model", 1.0),
        ("body", 0.0),
        ("reverb_send", 0.0),
        ("hardness", 0.95),
        ("material", material),
        ("decay", decay),
    ];
    Hit {
        file,
        seconds: (decay * 1.5).clamp(0.4, 4.0),
        layers: vec![layer(MEMBRANE, &o, note, 1.0)],
        fx: Vec::new(),
    }
}

fn render_layer(l: &Layer, frames: usize, builtins: &Builtins) -> Result<(Vec<f32>, Vec<f32>)> {
    let (kind, mut values) = match l.base {
        Base::Patch(name) => {
            let p = factory_patches()
                .into_iter()
                .find(|p| p.name == name)
                .with_context(|| format!("no patch {name}"))?;
            // Layers are balanced by their recipe gains, not by the patch's
            // rack volume, so trimming a patch's level never changes a kit.
            let mut values = p.values();
            values[crate::synth::VOLUME] = crate::synth::COMMON[crate::synth::VOLUME].default;
            (p.kind, values)
        }
        Base::Kind(k) => (k, k.defaults()),
    };
    for (key, v) in &l.overrides {
        let i = kind
            .index_of(key)
            .with_context(|| format!("{kind:?} has no parameter {key}"))?;
        values[i] = kind.param(i).clamp(*v);
    }
    let mut inst = Instrument::new(kind, SR, builtins);
    inst.update(&values);
    let mut chain: Vec<Box<FxUnit>> = (0..FX_UNITS)
        .map(|u| FxUnit::from_values(fx::unit_values(&values, kind.fx_base(), u), SR))
        .collect();
    let vol = values[crate::synth::VOLUME];
    let gain = vol * vol * l.gain;
    inst.note_on(l.note, l.velocity);
    let (mut out_l, mut out_r) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
    let release_at = (0.1 * SR) as usize;
    while out_l.len() < frames {
        if out_l.len() >= release_at && out_l.len() < release_at + MAX_BLOCK {
            inst.note_off(l.note);
        }
        let (mut bl, mut br) = ([0.0f32; MAX_BLOCK], [0.0f32; MAX_BLOCK]);
        inst.render(&mut bl, &mut br);
        for unit in &mut chain {
            unit.process(&mut bl, &mut br);
        }
        out_l.extend(bl.iter().map(|v| v * gain));
        out_r.extend(br.iter().map(|v| v * gain));
    }
    out_l.truncate(frames);
    out_r.truncate(frames);
    Ok((out_l, out_r))
}

fn render_hit(hit: &Hit, builtins: &Builtins) -> Result<(Vec<f32>, Vec<f32>)> {
    let frames = (hit.seconds * SR) as usize;
    let (mut l, mut r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    for layer in &hit.layers {
        let (a, b) = render_layer(layer, frames, builtins)?;
        l.iter_mut().zip(&a).for_each(|(x, y)| *x += y);
        r.iter_mut().zip(&b).for_each(|(x, y)| *x += y);
    }
    if !hit.fx.is_empty() {
        let mut values: Vec<f32> = fx::TABLE.iter().map(|d| d.default).collect();
        for (key, v) in &hit.fx {
            let i = fx::TABLE
                .iter()
                .position(|d| d.key == *key)
                .with_context(|| format!("no FX parameter {key}"))?;
            values[i] = fx::TABLE[i].clamp(*v);
        }
        let mut chain: Vec<Box<FxUnit>> = (0..FX_UNITS)
            .map(|u| FxUnit::from_values(fx::unit_values(&values, 0, u), SR))
            .collect();
        for start in (0..frames).step_by(MAX_BLOCK) {
            let end = (start + MAX_BLOCK).min(frames);
            for unit in &mut chain {
                unit.process(&mut l[start..end], &mut r[start..end]);
            }
        }
    }
    // Normalise, then trim the silent tail (with a short fade).
    let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
    anyhow::ensure!(peak > 1e-6, "{} rendered silent", hit.file);
    let g = TARGET_PEAK / peak;
    let threshold = 0.001 * TARGET_PEAK;
    let last = l
        .iter()
        .zip(&r)
        .rposition(|(a, b)| a.abs() * g > threshold || b.abs() * g > threshold)
        .unwrap_or(0);
    let end = (last + (0.01 * SR) as usize).min(frames);
    let fade = ((0.005 * SR) as usize).min(end);
    l.truncate(end);
    r.truncate(end);
    for (i, (a, b)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
        let f = if i + fade >= end {
            (end - i) as f32 / fade as f32
        } else {
            1.0
        };
        *a *= g * f;
        *b *= g * f;
    }
    Ok((l, r))
}

fn write_wav(path: &Path, l: &[f32], r: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR as u32,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .with_context(|| format!("writing {}", path.display()))?;
    let scale = 8_388_607.0;
    for (a, b) in l.iter().zip(r) {
        w.write_sample((a.clamp(-1.0, 1.0) * scale) as i32)?;
        w.write_sample((b.clamp(-1.0, 1.0) * scale) as i32)?;
    }
    w.finalize()?;
    Ok(())
}

pub fn kits_dir(samples_dir: &Path) -> PathBuf {
    samples_dir.join("kits")
}

/// Render the kits unless they're already present at the current version
/// (or `force`). Returns how many files were written.
pub fn ensure(samples_dir: &Path, builtins: &Builtins, force: bool) -> Result<usize> {
    let root = kits_dir(samples_dir);
    let marker = root.join(MARKER);
    let current = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    if !force && current == Some(KIT_VERSION) {
        return Ok(0);
    }
    let mut written = 0;
    for kit in recipes() {
        let dir = root.join(kit.name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for hit in &kit.hits {
            let (l, r) = render_hit(hit, builtins)
                .with_context(|| format!("{} / {}", kit.name, hit.file))?;
            write_wav(&dir.join(hit.file), &l, &r)?;
            written += 1;
        }
    }
    std::fs::write(&marker, KIT_VERSION.to_string())?;
    Ok(written)
}

/// Factory Kit patches pointing at the rendered kits (paths relative to the
/// samples folder).
pub fn factory_kit_patches() -> Vec<Patch> {
    recipes()
        .into_iter()
        .map(|kit_recipe| {
            let kind = SynthKind::Kit;
            let mut values = kind.defaults();
            let mut paths: Vec<Option<PathBuf>> = vec![None; kit::PADS];
            for (pad, file, note, level, decay) in &kit_recipe.pads {
                paths[*pad] = Some(PathBuf::from(format!("kits/{}/{file}", kit_recipe.name)));
                let n = pad + 1;
                let set = |values: &mut Vec<f32>, key: String, v: f32| {
                    let i = kind.index_of(&key).expect("kit pad parameter");
                    values[i] = kind.param(i).clamp(v);
                };
                set(&mut values, format!("pad{n}_level"), *level);
                if let Some(note) = note {
                    set(&mut values, format!("pad{n}_note"), *note as f32);
                }
                if let Some(decay) = decay {
                    set(&mut values, format!("pad{n}_decay"), *decay);
                }
            }
            values[crate::synth::REVERB_SEND] = 0.1;
            Patch::from_values(kit_recipe.patch_name, kind, &values, &paths)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kit_renders_and_matches_its_patch() {
        crate::dsp::init_tables();
        let dir = std::env::temp_dir().join(format!("arkeology-kitgen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let builtins = crate::sample::builtins();
        let t = std::time::Instant::now();
        let written = ensure(&dir, &builtins, false).unwrap();
        assert!(written >= 50, "{written} files");
        assert!(
            t.elapsed().as_secs_f32() < 20.0,
            "rendering took {:?}",
            t.elapsed()
        );
        // Second call is a no-op at the same version.
        assert_eq!(ensure(&dir, &builtins, false).unwrap(), 0);

        for patch in factory_kit_patches() {
            for path in patch.sample_paths().into_iter().flatten() {
                let full = dir.join(&path);
                let s = crate::sample::load_file(&full)
                    .unwrap_or_else(|e| panic!("{}: {e}", full.display()));
                assert!(
                    s.duration() > 0.02 && s.duration() < 4.0,
                    "{}: {}s",
                    full.display(),
                    s.duration()
                );
            }
        }
        // Kicks start with energy and decay; hats are short.
        let kick = crate::sample::load_file(&dir.join("kits/909/Kick.wav")).unwrap();
        let hat = crate::sample::load_file(&dir.join("kits/909/HH Closed.wav")).unwrap();
        assert!(hat.duration() < kick.duration());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
