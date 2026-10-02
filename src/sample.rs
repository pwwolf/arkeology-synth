//! Audio sources for the granular synth and sampler: WAV/FLAC loading, analysis,
//! and a handful of procedurally generated built-in sources so both make
//! sound out of the box.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::dsp::{FilterMode, Rng, Svf, SvfCoefs, sin_cycles};

/// A sample buffer (mono or stereo) plus analysis done once at load time: a
/// waveform overview for display and detected onsets for slicing.
///
/// Built-ins are tuned so MIDI note 60 plays at the recorded pitch; loaded
/// files are assumed to be at C4 too (use Transpose/Tune or Root to correct).
pub struct Sample {
    pub name: String,
    /// Left channel, or the only channel of a mono sample.
    pub data: Vec<f32>,
    pub right: Option<Vec<f32>>,
    pub sample_rate: f32,
    /// `OVERVIEW_BUCKETS` (min, max) pairs of the mono mix, for drawing.
    pub overview: Vec<(f32, f32)>,
    /// Onset positions (in frames) with strength 0..=1, strongest first.
    pub onsets: Vec<(usize, f32)>,
}

pub const OVERVIEW_BUCKETS: usize = 1024;
const ONSET_HOP: usize = 256;
const MAX_ONSETS: usize = 512;

impl Sample {
    /// Build a sample, normalising its peak to -1 dB and analysing it.
    pub fn new(name: impl Into<String>, mut data: Vec<f32>, mut right: Option<Vec<f32>>, sample_rate: f32) -> Self {
        let peak = data
            .iter()
            .chain(right.iter().flatten())
            .fold(0.0f32, |m, v| m.max(v.abs()));
        if peak > 1e-6 {
            let g = 0.9 / peak;
            data.iter_mut().for_each(|v| *v *= g);
            right.iter_mut().flatten().for_each(|v| *v *= g);
        }
        let mut s = Sample {
            name: name.into(),
            data,
            right,
            sample_rate,
            overview: Vec::new(),
            onsets: Vec::new(),
        };
        s.overview = s.compute_overview();
        s.onsets = s.detect_onsets();
        s
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn is_stereo(&self) -> bool {
        self.right.is_some()
    }

    pub fn duration(&self) -> f32 {
        self.data.len() as f32 / self.sample_rate
    }

    #[inline]
    fn mono_at(&self, i: usize) -> f32 {
        match &self.right {
            Some(r) => 0.5 * (self.data[i] + r[i]),
            None => self.data[i],
        }
    }

    /// Linear-interpolated mono read with wrap-around (used by grains).
    #[inline]
    pub fn read(&self, pos: f64) -> f32 {
        let len = self.data.len();
        let p = pos.rem_euclid(len as f64);
        let i = p as usize;
        let f = (p - i as f64) as f32;
        let a = self.mono_at(i % len);
        let b = self.mono_at((i + 1) % len);
        a + (b - a) * f
    }

    /// 4-point Hermite stereo read; positions outside the buffer read silence.
    #[inline]
    pub fn read_stereo(&self, pos: f64) -> (f32, f32) {
        let len = self.data.len() as isize;
        let i = pos.floor() as isize;
        let f = (pos - i as f64) as f32;
        let at = |buf: &[f32], k: isize| if (0..len).contains(&k) { buf[k as usize] } else { 0.0 };
        let interp = |buf: &[f32]| {
            let (xm1, x0, x1, x2) = (at(buf, i - 1), at(buf, i), at(buf, i + 1), at(buf, i + 2));
            let c1 = 0.5 * (x1 - xm1);
            let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
            let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
            ((c3 * f + c2) * f + c1) * f + x0
        };
        let l = interp(&self.data);
        match &self.right {
            Some(r) => (l, interp(r)),
            None => (l, l),
        }
    }

    fn compute_overview(&self) -> Vec<(f32, f32)> {
        let len = self.len();
        (0..OVERVIEW_BUCKETS)
            .map(|b| {
                let a = b * len / OVERVIEW_BUCKETS;
                let e = ((b + 1) * len / OVERVIEW_BUCKETS).max(a + 1).min(len);
                (a..e).fold((0.0f32, 0.0f32), |(lo, hi), i| {
                    let v = self.mono_at(i);
                    (lo.min(v), hi.max(v))
                })
            })
            .collect()
    }

    /// Energy-rise onset detector: frames whose level jumps well above the
    /// preceding frames. Strength is normalised to the strongest onset.
    fn detect_onsets(&self) -> Vec<(usize, f32)> {
        let frames = self.len() / ONSET_HOP;
        if frames < 3 {
            return Vec::new();
        }
        let db: Vec<f32> = (0..frames)
            .map(|f| {
                let e: f32 = (f * ONSET_HOP..(f + 1) * ONSET_HOP).map(|i| self.mono_at(i).powi(2)).sum();
                10.0 * (e / ONSET_HOP as f32 + 1e-10).log10()
            })
            .collect();
        let max_db = db.iter().fold(f32::MIN, |m, v| m.max(*v));
        let rise: Vec<f32> = (0..frames)
            .map(|f| {
                if f < 2 {
                    return 0.0;
                }
                (db[f] - db[f - 1].max(db[f - 2])).max(0.0)
            })
            .collect();
        let mut onsets: Vec<(usize, f32)> = (2..frames)
            .filter(|&f| {
                let next = rise.get(f + 1).copied().unwrap_or(0.0);
                rise[f] > 3.0 && rise[f] >= rise[f - 1] && rise[f] >= next && db[f] > max_db - 50.0
            })
            // Start a little before the frame so the attack isn't clipped.
            .map(|f| ((f * ONSET_HOP).saturating_sub(ONSET_HOP / 2), rise[f]))
            .collect();
        let strongest = onsets.iter().fold(0.0f32, |m, o| m.max(o.1));
        if strongest > 0.0 {
            onsets.iter_mut().for_each(|o| o.1 /= strongest);
        }
        onsets.sort_by(|a, b| b.1.total_cmp(&a.1));
        onsets.truncate(MAX_ONSETS);
        onsets
    }
}

/// File extensions the sample browser offers.
pub const AUDIO_EXTENSIONS: [&str; 3] = ["wav", "wave", "flac"];

pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|x| AUDIO_EXTENSIONS.iter().any(|e| x.eq_ignore_ascii_case(e)))
}

/// Load a WAV or FLAC file, chosen by its header rather than its extension.
pub fn load_file(path: &Path) -> Result<Sample> {
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
        .with_context(|| format!("opening {}", path.display()))?;
    let (interleaved, channels, sample_rate) = match &magic {
        b"fLaC" => decode_flac(path)?,
        b"RIFF" | b"RF64" => decode_wav(path)?,
        _ => bail!("{} isn't a WAV or FLAC file", path.display()),
    };
    // Keep the first two channels; anything beyond stereo is dropped.
    let left: Vec<f32> = interleaved.chunks(channels).map(|f| f[0]).collect();
    let right: Option<Vec<f32>> =
        (channels > 1).then(|| interleaved.chunks(channels).map(|f| f[1]).collect());
    if left.len() < 64 {
        bail!("{} is too short to use", path.display());
    }
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sample".into());
    Ok(Sample::new(name, left, right, sample_rate))
}

/// Interleaved samples, channel count and sample rate.
type Decoded = (Vec<f32>, usize, f32);

fn decode_wav(path: &Path) -> Result<Decoded> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
    };
    Ok((interleaved, spec.channels.max(1) as usize, spec.sample_rate as f32))
}

fn decode_flac(path: &Path) -> Result<Decoded> {
    let mut reader = claxon::FlacReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let info = reader.streaminfo();
    let scale = 1.0 / (1i64 << (info.bits_per_sample - 1)) as f32;
    let interleaved: Vec<f32> = reader
        .samples()
        .map(|s| s.map(|v| v as f32 * scale))
        .collect::<Result<_, _>>()
        .with_context(|| format!("decoding {}", path.display()))?;
    Ok((interleaved, info.channels.max(1) as usize, info.sample_rate as f32))
}

// ---------------------------------------------------------------------------
// Built-in sources
// ---------------------------------------------------------------------------

/// Names line up with the granular "Source" parameter (index 0 is "File").
pub const BUILTIN_NAMES: [&str; 5] = ["Choir", "Glass", "Saw", "Pluck", "Noise"];

const GEN_SR: f32 = 48_000.0;
const GEN_SECONDS: f32 = 4.0;
const C4: f32 = 261.625_58;

pub type Builtins = Arc<[Arc<Sample>]>;

pub fn builtins() -> Builtins {
    let gens: [fn() -> Vec<f32>; 5] = [gen_choir, gen_glass, gen_saw, gen_pluck, gen_noise];
    gens.iter()
        .zip(BUILTIN_NAMES)
        .map(|(g, name)| {
            Arc::new(Sample::new(name, g(), None, GEN_SR))
        })
        .collect::<Vec<_>>()
        .into()
}

fn frames() -> usize {
    (GEN_SR * GEN_SECONDS) as usize
}

/// Breathy vocal pad whose vowel morphs a → o → e → i over the buffer, so
/// scanning the position changes the timbre.
fn gen_choir() -> Vec<f32> {
    const VOWELS: [[f32; 3]; 4] = [
        [800.0, 1150.0, 2900.0],
        [450.0, 800.0, 2830.0],
        [400.0, 1700.0, 2600.0],
        [300.0, 2100.0, 3000.0],
    ];
    let n = frames();
    let harmonics = 40;
    let mut out = vec![0.0f32; n];
    let mut rng = Rng::new(7);
    let mut breath = Svf::default();
    let breath_c = SvfCoefs::new(FilterMode::BandPass, 2500.0, 0.3, GEN_SR);
    let mut phase = 0.0f64;
    let mut amps = vec![0.0f32; harmonics];
    for (i, o) in out.iter_mut().enumerate() {
        let t = i as f32 / GEN_SR;
        if i % 128 == 0 {
            let pos = (i as f32 / n as f32) * (VOWELS.len() - 1) as f32;
            let a = pos.floor() as usize;
            let b = (a + 1).min(VOWELS.len() - 1);
            let f = pos - a as f32;
            let formants: [f32; 3] = std::array::from_fn(|k| VOWELS[a][k] * (1.0 - f) + VOWELS[b][k] * f);
            for (h, amp) in amps.iter_mut().enumerate() {
                let hf = C4 * (h + 1) as f32;
                let mut g = 0.0;
                for (k, &fc) in formants.iter().enumerate() {
                    let bw = 90.0 + 40.0 * k as f32;
                    g += (-((hf - fc) / bw).powi(2)).exp() / (k + 1) as f32;
                }
                *amp = (g + 0.02) / (h + 1) as f32;
            }
        }
        let vib = 1.0 + 0.004 * sin_cycles(t * 5.2) + 0.002 * sin_cycles(t * 0.37);
        phase += (C4 * vib / GEN_SR) as f64;
        let p = phase.fract() as f32;
        let mut s = 0.0;
        for (h, amp) in amps.iter().enumerate() {
            s += sin_cycles(p * (h + 1) as f32) * amp;
        }
        s += breath.process(&breath_c, rng.bipolar()) * 0.04;
        *o = s;
    }
    out
}

/// Shimmering inharmonic bell partials with slow independent tremolos.
fn gen_glass() -> Vec<f32> {
    const PARTIALS: [(f32, f32, f32); 7] = [
        (1.0, 1.0, 0.13),
        (2.0, 0.5, 0.21),
        (3.01, 0.35, 0.34),
        (4.17, 0.3, 0.29),
        (5.43, 0.22, 0.47),
        (6.79, 0.16, 0.61),
        (8.21, 0.12, 0.83),
    ];
    let n = frames();
    let mut phases = [0.0f64; PARTIALS.len()];
    (0..n)
        .map(|i| {
            let t = i as f32 / GEN_SR;
            let mut s = 0.0;
            for (k, &(ratio, amp, trem)) in PARTIALS.iter().enumerate() {
                phases[k] += (C4 * ratio / GEN_SR) as f64;
                let tremolo = 0.6 + 0.4 * sin_cycles(t * trem + k as f32 * 0.17);
                s += sin_cycles(phases[k].fract() as f32) * amp * tremolo;
            }
            s
        })
        .collect()
}

/// Three detuned PolyBLEP saws through a slowly opening low-pass filter.
fn gen_saw() -> Vec<f32> {
    fn blep(t: f32, dt: f32) -> f32 {
        if t < dt {
            let x = t / dt;
            x + x - x * x - 1.0
        } else if t > 1.0 - dt {
            let x = (t - 1.0) / dt;
            x * x + x + x + 1.0
        } else {
            0.0
        }
    }
    let n = frames();
    let detunes = [0.0f32, 0.07, -0.08];
    let mut phases = [0.0f32, 0.33, 0.71];
    let mut f = Svf::default();
    let mut coefs = SvfCoefs::default();
    (0..n)
        .map(|i| {
            if i % 64 == 0 {
                let x = i as f32 / n as f32;
                coefs = SvfCoefs::new(FilterMode::LowPass, 300.0 * 40f32.powf(x), 0.35, GEN_SR);
            }
            let mut s = 0.0;
            for (p, d) in phases.iter_mut().zip(detunes) {
                let dt = C4 * (d / 12.0).exp2() / GEN_SR;
                *p += dt;
                if *p >= 1.0 {
                    *p -= 1.0;
                }
                s += 2.0 * *p - 1.0 - blep(*p, dt);
            }
            f.process(&coefs, s)
        })
        .collect()
}

/// Karplus-Strong plucks of varying brightness, one per second.
fn gen_pluck() -> Vec<f32> {
    let n = frames();
    let period = GEN_SR / C4;
    let len = period.ceil() as usize + 2;
    let mut line = vec![0.0f32; len];
    let mut w = 0usize;
    let mut rng = Rng::new(99);
    let mut last = 0.0f32;
    let mut out = Vec::with_capacity(n);
    let pluck_every = GEN_SR as usize;
    for i in 0..n {
        if i % pluck_every == 0 {
            let brightness = 0.3 + 0.7 * ((i / pluck_every) as f32 / 4.0);
            let mut lp = 0.0;
            for k in 0..len {
                lp += (rng.bipolar() - lp) * brightness;
                line[(w + k) % len] = lp;
            }
        }
        // Fractional read `period` samples behind the write head.
        let rp = (w as f32 + len as f32 - period) % len as f32;
        let ri = rp as usize;
        let rf = rp - ri as f32;
        let a = line[ri % len];
        let b = line[(ri + 1) % len];
        let y = a + (b - a) * rf;
        let filtered = 0.5 * (y + last) * 0.998;
        last = y;
        line[w] = filtered;
        w = (w + 1) % len;
        out.push(y);
    }
    out
}

/// Noise through a resonant band-pass sweeping up and down.
fn gen_noise() -> Vec<f32> {
    let n = frames();
    let mut rng = Rng::new(1234);
    let mut f = Svf::default();
    let mut coefs = SvfCoefs::default();
    (0..n)
        .map(|i| {
            if i % 64 == 0 {
                let t = i as f32 / n as f32;
                let sweep = 0.5 - 0.5 * sin_cycles(t + 0.25);
                coefs = SvfCoefs::new(FilterMode::BandPass, 200.0 * 30f32.powf(sweep), 0.75, GEN_SR);
            }
            f.process(&coefs, rng.bipolar())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    /// The FLAC fixtures were encoded from the WAV fixtures with the reference
    /// `flac` encoder, so both decoders must produce identical samples.
    #[test]
    fn flac_matches_wav() {
        for bits in [16, 24] {
            let wav = load_file(&fixture(&format!("stereo{bits}.wav"))).unwrap();
            let flac = load_file(&fixture(&format!("stereo{bits}.flac"))).unwrap();
            assert!(flac.is_stereo(), "{bits}-bit");
            assert_eq!(flac.sample_rate, wav.sample_rate);
            assert_eq!(flac.data, wav.data, "{bits}-bit left");
            assert_eq!(flac.right, wav.right, "{bits}-bit right");
            // Left and right hold different signals, so channels weren't mixed.
            assert_ne!(flac.data, *flac.right.as_ref().unwrap());
        }
    }

    #[test]
    fn rejects_non_audio() {
        let err = load_file(&fixture("../../Cargo.toml")).err().unwrap();
        assert!(err.to_string().contains("isn't a WAV or FLAC"), "{err}");
    }
}
