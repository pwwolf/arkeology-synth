//! Optional acoustic kits from the Versilian Community Sample Library (VCSL),
//! which is CC0 (public domain): https://github.com/sgossner/VCSL
//!
//! VCSL is ~4 GB, so rather than bundling it we download a curated handful of
//! one-shots (~10 MB) from a pinned commit into `samples/kits/`, with clean
//! names, and ship factory Kit patches that point at them. Downloads use the
//! system `curl`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::patch::Patch;
use crate::synth::{SynthKind, kit};

/// The VCSL commit the file paths below were taken from.
const COMMIT: &str = "c1ea7bcc3c7309650ab0da9d15c9cd1fbc4a4c7e";
const REPO: &str = "https://github.com/sgossner/VCSL";

/// One pad: (pad index, MIDI note, VCSL path, local file name, level, decay, tune).
type PadSource = (usize, u8, &'static str, &'static str, f32, Option<f32>, f32);

pub struct VcslKit {
    pub dir: &'static str,
    pub patch_name: &'static str,
    pads: &'static [PadSource],
}

macro_rules! m {
    ($p:literal) => {
        concat!("Membranophones/Struck Membranophones/", $p)
    };
}
macro_rules! i {
    ($p:literal) => {
        concat!("Idiophones/Struck Idiophones/", $p)
    };
}

pub const KITS: [VcslKit; 2] = [
    VcslKit {
        dir: "VCSL Acoustic",
        patch_name: "VCSL Acoustic Kit",
        pads: &[
            // The library's bass drum is a concert bass drum: shortened to sit in a kit.
            (0, 36, m!("Bass Drum 1/BDrumNew_hit_v5_rr1_Sum.wav"), "Kick.wav", 0.9, Some(0.7), 0.0),
            (1, 38, m!("Snare Drum, Modern 1/Snare2_HitSN_v7_rr1_Mid.wav"), "Snare.wav", 0.8, None, 0.0),
            (2, 42, i!("Hi-Hat Cymbal/HiHat_HitC_v3_rr1_Mid.wav"), "HH Closed.wav", 0.6, None, 0.0),
            (3, 46, i!("Hi-Hat Cymbal/HiHat_HitO_rr1_Mid.wav"), "HH Open.wav", 0.55, None, 0.0),
            (4, 39, i!("Claps/Clap_rr1.wav"), "Clap.wav", 0.7, None, 0.0),
            (5, 37, m!("Snare Drum, Modern 3/Snare4_Xstick_v2_rr1_Mid.wav"), "Cross Stick.wav", 0.7, None, 0.0),
            (6, 41, m!("Tom 2/Stick/TomL_HitS_v4_rr1_Mid.wav"), "Tom Low.wav", 0.75, None, 0.0),
            // Only two toms in the library: the mid tom is the high tom tuned down.
            (7, 45, m!("Tom 1/Stick/TomH_HitS_v3_rr1_Mid.wav"), "Tom Mid.wav", 0.75, None, -3.0),
            (8, 48, m!("Tom 1/Stick/TomH_HitS_v4_rr1_Mid.wav"), "Tom High.wav", 0.75, None, 0.0),
            (9, 49, i!("Suspended Cymbal 1/susCymb1_hit_stick_f1.wav"), "Crash.wav", 0.55, None, 0.0),
            (10, 51, i!("Suspended Cymbal 2/susCymb2_hit_stick_mp1.wav"), "Ride.wav", 0.5, Some(2.5), 0.0),
            (11, 44, i!("Hi-Hat Cymbal/HiHat_Close_rr1_Mid.wav"), "HH Pedal.wav", 0.5, None, 0.0),
            (12, 40, m!("Snare Drum, Modern 3/Snare4_rimshot_v4_rr1_Mid.wav"), "Rimshot.wav", 0.75, None, 0.0),
            (13, 54, i!("Tambourine 1/Tamb1_Hit_v2_rr1_Mid.wav"), "Tambourine.wav", 0.55, None, 0.0),
            (14, 56, i!("Cowbells/Cowbell1_Hit_v3_rr1_Mid.wav"), "Cowbell.wav", 0.6, None, 0.0),
            (15, 70, i!("Shaker, Small/Mid_Shaker_Slap_rr1.wav"), "Shaker.wav", 0.55, None, 0.0),
        ],
    },
    VcslKit {
        dir: "VCSL Percussion",
        patch_name: "VCSL Percussion",
        // General MIDI percussion notes.
        pads: &[
            (0, 64, m!("Conga/Tumba_HitN_v3_rr1_Sum.wav"), "Conga Low.wav", 0.75, None, 0.0),
            (1, 63, m!("Conga/Conga_HitN_v3_rr1_Sum.wav"), "Conga Open.wav", 0.75, None, 0.0),
            (2, 62, m!("Conga/Conga_HitFM_v2_rr1_Sum.wav"), "Conga Mute.wav", 0.7, None, 0.0),
            (3, 60, m!("Bongos/BongoH_Hit1_v3_rr1_Mid.wav"), "Bongo High.wav", 0.75, None, 0.0),
            (4, 61, m!("Bongos/BongoL_Hit1_v3_rr1_Mid.wav"), "Bongo Low.wav", 0.75, None, 0.0),
            (5, 67, i!("Agogo Bells/Agogo_High_v2_rr1_Mid.wav"), "Agogo High.wav", 0.6, None, 0.0),
            (6, 68, i!("Agogo Bells/Agogo_Low_v2_rr1_Mid.wav"), "Agogo Low.wav", 0.6, None, 0.0),
            (7, 75, i!("Claves/Claves1_Hit_v2_rr1_Mid.wav"), "Claves.wav", 0.65, None, 0.0),
            (8, 76, i!("Woodblock/wood_click_f_rr1.wav"), "Woodblock High.wav", 0.65, None, 0.0),
            (9, 77, i!("Woodblock/wood_click3_vl2.wav"), "Woodblock Low.wav", 0.65, None, 0.0),
            (10, 69, i!("Cabasa/Cabasa1_Hit_rr1_Mid.wav"), "Cabasa.wav", 0.55, None, 0.0),
            (11, 70, i!("Shaker, Small/Mid_ShakerDouble_Down_rr1.wav"), "Maracas.wav", 0.55, None, 0.0),
            (12, 81, i!("Triangles/Triangle1_Hit_v2_rr1_Mid.wav"), "Triangle Open.wav", 0.5, None, 0.0),
            (13, 80, i!("Triangles/Triangle1_HitM_v1_rr2_Mid.wav"), "Triangle Mute.wav", 0.5, None, 0.0),
            (14, 54, i!("Tambourine 1/Tamb1_Hit_v1_rr1_Mid.wav"), "Tambourine.wav", 0.55, None, 0.0),
            (15, 56, i!("Cowbells/Cowbell1_Normal_v3_rr1_Mid.wav"), "Cowbell.wav", 0.6, None, 0.0),
        ],
    },
];

/// Percent-encode a repository path for a raw.githubusercontent.com URL.
fn encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn url(path: &str) -> String {
    format!("https://raw.githubusercontent.com/sgossner/VCSL/{COMMIT}/{}", encode(path))
}

/// Path of a kit's folder under the samples folder.
pub fn kit_dir(samples_dir: &Path, kit: &VcslKit) -> PathBuf {
    crate::kitgen::kits_dir(samples_dir).join(kit.dir)
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let tmp = dest.with_extension("part");
    let status = Command::new("curl")
        .args(["--fail", "--location", "--silent", "--show-error", "--retry", "2", "-o"])
        .arg(&tmp)
        .arg(url)
        .status()
        .context("running curl (is it installed?)")?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("download failed: {url}");
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Download any missing VCSL kit files. Returns (downloaded, already present).
/// `progress` is called with each file as it's fetched.
pub fn fetch(samples_dir: &Path, mut progress: impl FnMut(&str)) -> Result<(usize, usize)> {
    let (mut fetched, mut present) = (0, 0);
    for kit in &KITS {
        let dir = kit_dir(samples_dir, kit);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for &(_, _, source, file, ..) in kit.pads {
            let dest = dir.join(file);
            if dest.is_file() && crate::sample::load_file(&dest).is_ok() {
                present += 1;
                continue;
            }
            progress(&format!("{}/{file}", kit.dir));
            download(&url(source), &dest)?;
            crate::sample::load_file(&dest).with_context(|| format!("checking {}", dest.display()))?;
            fetched += 1;
        }
        let credit = format!(
            "These samples are from the Versilian Community Sample Library (VCSL)\n\
             {REPO} (commit {COMMIT}), released under CC0 1.0 Universal (public domain).\n\
             Downloaded by Arkeology Synth with --fetch-kits and renamed; otherwise unmodified.\n"
        );
        std::fs::write(dir.join("SOURCE.txt"), credit)?;
    }
    Ok((fetched, present))
}

/// Whether a (relative) sample path belongs to a downloadable VCSL kit.
pub fn is_vcsl_path(path: &Path) -> bool {
    KITS.iter().any(|k| path.starts_with(Path::new("kits").join(k.dir)))
}

/// Factory Kit patches for the VCSL kits (paths relative to the samples folder).
pub fn factory_kit_patches() -> Vec<Patch> {
    KITS.iter()
        .map(|k| {
            let kind = SynthKind::Kit;
            let mut values = kind.defaults();
            let mut paths: Vec<Option<PathBuf>> = vec![None; kit::PADS];
            let set = |values: &mut Vec<f32>, key: String, v: f32| {
                let i = kind.index_of(&key).expect("kit pad parameter");
                values[i] = kind.param(i).clamp(v);
            };
            for &(pad, note, _, file, level, decay, tune) in k.pads {
                paths[pad] = Some(PathBuf::from(format!("kits/{}/{file}", k.dir)));
                let n = pad + 1;
                set(&mut values, format!("pad{n}_note"), note as f32);
                set(&mut values, format!("pad{n}_level"), level);
                set(&mut values, format!("pad{n}_tune"), tune);
                if let Some(d) = decay {
                    set(&mut values, format!("pad{n}_decay"), d);
                }
                // Hand percussion in the percussion kit has no hi-hat choke.
                if k.dir == "VCSL Percussion" {
                    set(&mut values, format!("pad{n}_choke"), 0.0);
                }
            }
            values[crate::synth::REVERB_SEND] = 0.12;
            Patch::from_values(k.patch_name, kind, &values, &paths)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_encoded() {
        assert_eq!(
            url("Idiophones/Struck Idiophones/Shaker, Small/a.wav"),
            format!("https://raw.githubusercontent.com/sgossner/VCSL/{COMMIT}/Idiophones/Struck%20Idiophones/Shaker%2C%20Small/a.wav")
        );
    }

    #[test]
    fn kits_are_consistent() {
        for k in &KITS {
            let mut pads: Vec<usize> = k.pads.iter().map(|p| p.0).collect();
            let mut files: Vec<&str> = k.pads.iter().map(|p| p.3).collect();
            pads.sort_unstable();
            pads.dedup();
            files.sort_unstable();
            files.dedup();
            assert_eq!(pads.len(), k.pads.len(), "{}: duplicate pad", k.dir);
            assert_eq!(files.len(), k.pads.len(), "{}: duplicate file name", k.dir);
            assert!(k.pads.iter().all(|p| p.0 < kit::PADS && p.2.ends_with(".wav")));
        }
        let patches = factory_kit_patches();
        assert_eq!(patches.len(), KITS.len());
        assert!(patches.iter().all(|p| p.sample_paths().iter().flatten().all(|path| is_vcsl_path(path))));
    }
}
