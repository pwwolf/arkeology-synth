//! `--render-patches`: render every factory patch through a phrase that
//! suits it, one WAV each, plus a level summary that flags patches that are
//! silent, very quiet or close to clipping. For auditioning by ear.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use crossbeam_queue::ArrayQueue;

use crate::engine::{self, Command, CommandQueue, Engine, GarbageQueue, Telemetry};
use crate::params::master;
use crate::patch::{self, Patch};
use crate::sample::{self, Builtins};
use crate::synth::{SynthKind, kit};

const SR: f32 = 48_000.0;

/// (seconds, note, velocity 0..1; 0 = note off)
type Events = Vec<(f32, u8, f32)>;

pub struct Rendered {
    pub name: String,
    pub kind: SynthKind,
    pub file: Option<PathBuf>,
    pub peak: f32,
    pub rms: f32,
    pub note: String,
}

impl Rendered {
    /// Problems worth a listen: silence, very low level, or near clipping.
    pub fn flag(&self) -> Option<&'static str> {
        if self.file.is_none() {
            None
        } else if self.peak < 1e-3 {
            Some("SILENT")
        } else if self.peak > 0.95 {
            Some("HOT (near clipping)")
        } else if self.rms < 0.008 {
            Some("QUIET")
        } else {
            None
        }
    }
}

fn note(events: &mut Events, at: f32, len: f32, n: u8, vel: f32) {
    events.push((at, n, vel));
    events.push((at + len, n, 0.0));
}

fn is_bass(p: &Patch) -> bool {
    let n = p.name.to_lowercase();
    ["bass", "sub", "reese", "growl", "wobble"]
        .iter()
        .any(|w| n.contains(w))
}

/// The phrase a patch is auditioned with, and how long to render.
fn phrase(p: &Patch) -> (Events, f32) {
    let mut e = Events::new();
    let name = p.name.to_lowercase();
    match p.kind {
        SynthKind::Drums => {
            // GM groove: kick, snare + clap, 8th hats, an open hat, a fill.
            for beat in 0..8 {
                let t = beat as f32 * 0.6;
                note(&mut e, t, 0.1, 36, 0.95);
                if beat % 2 == 1 {
                    note(&mut e, t, 0.1, 38, 0.85);
                }
                note(&mut e, t, 0.1, 42, 0.6);
                note(
                    &mut e,
                    t + 0.3,
                    0.1,
                    if beat % 4 == 3 { 46 } else { 42 },
                    0.5,
                );
            }
            for (k, n) in [48u8, 45, 41].iter().enumerate() {
                note(&mut e, 4.8 + k as f32 * 0.15, 0.1, *n, 0.85);
            }
            note(&mut e, 5.4, 0.1, 49, 0.9);
            (e, 7.5)
        }
        SynthKind::Kit => {
            // Every pad in turn, then a groove on the first four.
            let values = p.values();
            let pad_note = |pad: usize| {
                let key = format!("pad{}_note", pad + 1);
                values[SynthKind::Kit.index_of(&key).expect("pad note")].round() as u8
            };
            for pad in 0..kit::PADS {
                note(&mut e, pad as f32 * 0.35, 0.1, pad_note(pad), 0.85);
            }
            let start = kit::PADS as f32 * 0.35 + 0.5;
            for beat in 0..8 {
                let t = start + beat as f32 * 0.6;
                note(&mut e, t, 0.1, pad_note(0), 0.95);
                if beat % 2 == 1 {
                    note(&mut e, t, 0.1, pad_note(1), 0.85);
                }
                note(&mut e, t, 0.1, pad_note(2), 0.6);
                note(
                    &mut e,
                    t + 0.3,
                    0.1,
                    pad_note(if beat % 4 == 3 { 3 } else { 2 }),
                    0.5,
                );
            }
            (e, start + 6.5)
        }
        SynthKind::Acid => {
            // Two bars of 16ths with accents (high velocity) and slides
            // (overlapping notes).
            let line: [(u8, f32, bool); 16] = [
                (36, 0.9, false),
                (36, 0.6, false),
                (48, 0.6, true),
                (36, 0.6, false),
                (39, 0.9, false),
                (36, 0.6, false),
                (46, 0.6, true),
                (48, 0.9, false),
                (36, 0.6, false),
                (36, 0.9, false),
                (43, 0.6, true),
                (41, 0.6, false),
                (39, 0.6, false),
                (36, 0.9, false),
                (48, 0.6, true),
                (36, 0.6, false),
            ];
            for rep in 0..2 {
                for (step, &(n, vel, slide)) in line.iter().enumerate() {
                    let t = (rep * 16 + step) as f32 * 0.15;
                    note(&mut e, t, if slide { 0.18 } else { 0.08 }, n, vel);
                }
            }
            (e, 6.0)
        }
        SynthKind::Sampler if name.contains("slice") => {
            for k in 0..8u8 {
                note(&mut e, k as f32 * 0.4, 0.3, 36 + k, 0.85);
            }
            (e, 5.0)
        }
        _ if is_bass(p) => {
            // A low riff, then a held note.
            for (k, &n) in [28u8, 28, 40, 28, 31, 33, 28, 35].iter().enumerate() {
                note(
                    &mut e,
                    k as f32 * 0.3,
                    0.22,
                    n,
                    if k % 4 == 0 { 0.95 } else { 0.75 },
                );
            }
            note(&mut e, 2.6, 1.6, 28, 0.85);
            (e, 6.5)
        }
        _ => {
            // A held chord, a melody, then the low and high ends.
            for n in [48u8, 52, 55, 60] {
                note(&mut e, 0.0, 2.2, n, 0.8);
            }
            for (k, &n) in [60u8, 62, 64, 67, 69, 67, 64, 62, 60].iter().enumerate() {
                note(&mut e, 3.2 + k as f32 * 0.24, 0.22, n, 0.75);
            }
            note(&mut e, 5.6, 1.2, 36, 0.8);
            note(&mut e, 7.0, 0.8, 84, 0.8);
            (e, 10.5)
        }
    }
}

/// Render one patch to stereo samples.
fn render(p: &Patch, samples_dir: &Path, builtins: &Builtins) -> Result<Vec<f32>, String> {
    if p.kind == SynthKind::Kit && p.sample_paths().iter().all(Option::is_none) {
        return Err("no samples (an empty kit to load your own into)".into());
    }
    let mut loaded = Vec::new();
    for path in p.sample_paths() {
        loaded.push(match path {
            None => None,
            Some(path) => {
                let full = if path.is_relative() {
                    samples_dir.join(&path)
                } else {
                    path.clone()
                };
                if !full.exists() {
                    let hint = if crate::vcsl::is_vcsl_path(&path) {
                        " (download with --fetch-kits)"
                    } else {
                        ""
                    };
                    return Err(format!("missing {}{hint}", full.display()));
                }
                Some(Arc::new(
                    sample::load_file(&full).map_err(|e| format!("{e:#}"))?,
                ))
            }
        });
    }
    let commands: CommandQueue = Arc::new(ArrayQueue::new(1024));
    let garbage: GarbageQueue = Arc::new(ArrayQueue::new(64));
    let mut engine = Engine::new(
        SR,
        commands.clone(),
        garbage,
        Arc::new(Telemetry::default()),
    );
    // The starter rack's hall, as the patch would usually be heard.
    let hall = crate::reverb::REVERB_TYPES
        .iter()
        .position(|t| *t == "Hall")
        .unwrap_or(0);
    let _ = commands.push(Command::SetMaster {
        index: master::REVERB_TYPE,
        value: hall as f32,
    });
    let slot = engine::Slot::new(p.kind, p.values(), None, SR, builtins, &loaded);
    let _ = commands.push(Command::InstallSlot {
        slot: 0,
        data: slot,
    });

    let (mut events, secs) = phrase(p);
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total = (secs * SR) as usize;
    let mut out = vec![0.0f32; total * 2];
    let mut next = events.iter().peekable();
    let mut frame = 0;
    while frame < total {
        let now = frame as f32 / SR;
        while let Some(&&(t, n, vel)) = next.peek() {
            if t > now {
                break;
            }
            let cmd = if vel > 0.0 {
                Command::NoteOn {
                    slot: 0,
                    note: n,
                    velocity: vel,
                }
            } else {
                Command::NoteOff { slot: 0, note: n }
            };
            let _ = commands.push(cmd);
            next.next();
        }
        // Stop at the next event so notes start on time (to 1 ms).
        let until = next
            .peek()
            .map_or(total, |e| ((e.0 * SR) as usize).max(frame + 1));
        let n = (until - frame).clamp(1, 48).min(total - frame);
        engine.process(&mut out[frame * 2..(frame + n) * 2], 2);
        frame += n;
    }
    Ok(out)
}

fn write_wav(path: &Path, out: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR as u32,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    for s in out {
        w.write_sample((s.clamp(-1.0, 1.0) * 8_388_607.0) as i32)?;
    }
    w.finalize()?;
    Ok(())
}

/// Render the factory patches whose name or type contains `only` (all if
/// `None`) into `dir`, in parallel. Returns one entry per patch, in order.
pub fn render_patches(
    dir: &Path,
    only: Option<&str>,
    samples_dir: &Path,
    builtins: &Builtins,
) -> Result<Vec<Rendered>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let filter = only.map(str::to_lowercase);
    let mut patches: Vec<Patch> = patch::factory_patches()
        .into_iter()
        .filter(|p| {
            filter.as_ref().is_none_or(|f| {
                p.name.to_lowercase().contains(f) || p.kind.long_name().to_lowercase().contains(f)
            })
        })
        .collect();
    patches.sort_by_key(|p| (p.kind.order(), p.name.to_lowercase()));

    let next = AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Rendered>>> = patches
        .iter()
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(p) = patches.get(i) else { break };
                    let mut r = Rendered {
                        name: p.name.clone(),
                        kind: p.kind,
                        file: None,
                        peak: 0.0,
                        rms: 0.0,
                        note: String::new(),
                    };
                    match render(p, samples_dir, builtins) {
                        Ok(out) => {
                            r.peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                            r.rms =
                                (out.iter().map(|v| v * v).sum::<f32>() / out.len() as f32).sqrt();
                            let file = dir.join(format!(
                                "{:02} {} - {}.wav",
                                p.kind.order() + 1,
                                p.kind.label(),
                                patch::file_stem(&p.name)
                            ));
                            match write_wav(&file, &out) {
                                Ok(()) => r.file = Some(file),
                                Err(e) => r.note = format!("{e:#}"),
                            }
                        }
                        Err(e) => r.note = e,
                    }
                    *results[i].lock().expect("result slot") = Some(r);
                }
            });
        }
    });
    let results: Vec<Rendered> = results
        .into_iter()
        .filter_map(|m| m.into_inner().ok().flatten())
        .collect();

    // A summary table next to the files.
    let mut tsv = String::from("synth\tpatch\tpeak\trms_db\tflag\tfile\n");
    for r in &results {
        tsv += &format!(
            "{}\t{}\t{:.3}\t{:.1}\t{}\t{}\n",
            r.kind.label(),
            r.name,
            r.peak,
            20.0 * (r.rms + 1e-9).log10(),
            r.flag()
                .unwrap_or(if r.file.is_none() { "SKIPPED" } else { "" }),
            r.file.as_ref().map_or(r.note.clone(), |f| f
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned())
        );
    }
    std::fs::write(dir.join("levels.tsv"), tsv)?;
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_summarises_a_filtered_set() {
        crate::dsp::init_tables();
        let dir = std::env::temp_dir().join(format!("arkeology-audition-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let builtins = sample::builtins();
        let results = render_patches(&dir, Some("organ"), &dir.join("samples"), &builtins).unwrap();
        assert!(results.len() >= 6, "{} organ patches", results.len());
        for r in &results {
            let file = r
                .file
                .as_ref()
                .unwrap_or_else(|| panic!("{}: {}", r.name, r.note));
            assert!(file.exists());
            assert_eq!(r.flag(), None, "{}: peak {} rms {}", r.name, r.peak, r.rms);
        }
        let summary = std::fs::read_to_string(dir.join("levels.tsv")).unwrap();
        assert!(summary.contains("Jazz Organ") && summary.lines().count() == results.len() + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_kit_samples_are_skipped_with_a_reason() {
        let dir =
            std::env::temp_dir().join(format!("arkeology-audition-vcsl-{}", std::process::id()));
        let builtins = sample::builtins();
        let results =
            render_patches(&dir, Some("VCSL Acoustic"), &dir.join("samples"), &builtins).unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].file.is_none() && results[0].note.contains("fetch-kits"),
            "{}",
            results[0].note
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
