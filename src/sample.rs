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
    /// Detected pitch as a fractional MIDI note, for loaded files that have a
    /// clear one (see `detect_pitch`).
    pub pitch: Option<f32>,
}

pub const OVERVIEW_BUCKETS: usize = 1024;
const ONSET_HOP: usize = 256;
const MAX_ONSETS: usize = 512;

impl Sample {
    /// Build a sample, normalising its peak to -1 dB and analysing it.
    pub fn new(
        name: impl Into<String>,
        mut data: Vec<f32>,
        mut right: Option<Vec<f32>>,
        sample_rate: f32,
    ) -> Self {
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
            pitch: None,
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
        let at = |buf: &[f32], k: isize| {
            if (0..len).contains(&k) {
                buf[k as usize]
            } else {
                0.0
            }
        };
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

    /// Estimate the sample's fundamental with the YIN algorithm over several
    /// windows. Returns a fractional MIDI note, or `None` when the sample
    /// has no clear, stable pitch (drums, noise, chords).
    pub fn detect_pitch(&self) -> Option<f32> {
        const W: usize = 1024; // integration window
        const WINDOWS: usize = 9;
        const THRESHOLD: f32 = 0.15;
        let sr = self.sample_rate;
        let tau_min = (sr / 2000.0) as usize; // up to ~2 kHz
        let tau_max = (sr / 40.0) as usize; // down to 40 Hz
        let span = W + tau_max + 2;
        let len = self.len();
        if len < span {
            return None;
        }
        // Skip the attack: analyse from 10% in, or from just after the
        // loudest point if that comes later.
        let peak_at = (0..len)
            .step_by(64)
            .max_by(|&a, &b| self.mono_at(a).abs().total_cmp(&self.mono_at(b).abs()))?;
        let from = (len / 10)
            .max(peak_at + (0.03 * sr) as usize)
            .min(len - span);
        let to = (len - span).min(from + (2.0 * sr) as usize).max(from);
        let peak = self.data.iter().fold(0.0f32, |m, v| m.max(v.abs()));

        let mut buf = vec![0.0f32; span];
        let mut d = vec![0.0f32; tau_max + 2];
        let mut notes: Vec<f32> = Vec::new();
        let mut analysed = 0;
        for k in 0..WINDOWS {
            let start = from + (to - from) * k / (WINDOWS - 1);
            for (j, b) in buf.iter_mut().enumerate() {
                *b = self.mono_at(start + j);
            }
            let rms = (buf[..W].iter().map(|v| v * v).sum::<f32>() / W as f32).sqrt();
            if rms < peak * 0.01 {
                continue; // too quiet to judge
            }
            analysed += 1;
            // Difference function and its cumulative-mean normalisation.
            d[0] = 1.0;
            let mut running = 0.0;
            for tau in 1..=tau_max + 1 {
                let mut sum = 0.0;
                for j in 0..W {
                    let x = buf[j] - buf[j + tau];
                    sum += x * x;
                }
                running += sum;
                d[tau] = if running > 0.0 {
                    sum * tau as f32 / running
                } else {
                    1.0
                };
            }
            // First dip under the threshold, then walk down to its minimum.
            let Some(mut tau) = (tau_min.max(2)..tau_max).find(|&t| d[t] < THRESHOLD) else {
                continue;
            };
            while tau + 1 < tau_max && d[tau + 1] < d[tau] {
                tau += 1;
            }
            // Parabolic interpolation for sub-sample accuracy.
            let (a, b, c) = (d[tau - 1], d[tau], d[tau + 1]);
            let denom = a - 2.0 * b + c;
            let shift = if denom.abs() > 1e-9 {
                0.5 * (a - c) / denom
            } else {
                0.0
            };
            let freq = sr / (tau as f32 + shift.clamp(-1.0, 1.0));
            notes.push(69.0 + 12.0 * (freq / 440.0).log2());
        }
        if analysed == 0 || notes.len() * 3 < analysed * 2 {
            return None; // most windows had no clear periodicity
        }
        notes.sort_by(f32::total_cmp);
        let median = notes[notes.len() / 2];
        // Require the estimates to agree; otherwise the pitch is unstable.
        let agreeing: Vec<f32> = notes
            .iter()
            .copied()
            .filter(|n| (n - median).abs() < 0.5)
            .collect();
        if agreeing.len() * 3 < analysed * 2 {
            return None;
        }
        Some(agreeing.iter().sum::<f32>() / agreeing.len() as f32)
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
                let e: f32 = (f * ONSET_HOP..(f + 1) * ONSET_HOP)
                    .map(|i| self.mono_at(i).powi(2))
                    .sum();
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
    let mut sample = Sample::new(name, left, right, sample_rate);
    sample.pitch = sample.detect_pitch();
    Ok(sample)
}

/// Interleaved samples, channel count and sample rate.
type Decoded = (Vec<f32>, usize, f32);

fn decode_wav(path: &Path) -> Result<Decoded> {
    match decode_wav_hound(path) {
        Ok(d) => Ok(d),
        // hound is strict about header details (e.g. 20-byte fmt chunks);
        // fall back to a lenient chunk walker before giving up.
        Err(strict) => decode_wav_lenient(path).map_err(|_| strict),
    }
}

/// Minimal, tolerant RIFF/WAVE reader: integer PCM (8/16/24/32-bit), float
/// (32/64-bit), any fmt chunk size >= 16, and WAVE_FORMAT_EXTENSIBLE.
fn decode_wav_lenient(path: &Path) -> Result<Decoded> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a RIFF/WAVE file");
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at =
        |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut fmt: Option<(u16, usize, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32_at(i + 4) as usize;
        let body = &bytes[i + 8..(i + 8 + size).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            let mut format = u16_at(i + 8);
            if format == 0xFFFE && body.len() >= 26 {
                // Extensible: the real format is the start of the sub-format GUID.
                format = u16_at(i + 8 + 24);
            }
            fmt = Some((
                format,
                u16_at(i + 10).max(1) as usize,
                u32_at(i + 12),
                u16_at(i + 22),
            ));
        } else if id == b"data" {
            data = Some(body);
        }
        i += 8 + size + (size & 1);
    }
    let (format, channels, sample_rate, bits) = fmt.context("no fmt chunk")?;
    let data = data.context("no data chunk")?;
    let samples: Vec<f32> = match (format, bits) {
        (1, 8) => data.iter().map(|&b| (b as f32 - 128.0) / 128.0).collect(),
        (1, 16) => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c) as f32 / 32_768.0)
            .collect(),
        (1, 24) => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|&[a, b, c]| i32::from_le_bytes([0, a, b, c]) as f32 / 2_147_483_648.0)
            .collect(),
        (1, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| i32::from_le_bytes(*c) as f32 / 2_147_483_648.0)
            .collect(),
        (3, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect(),
        (3, 64) => data
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| f64::from_le_bytes(*c) as f32)
            .collect(),
        _ => bail!("unsupported WAV format {format} / {bits}-bit"),
    };
    Ok((samples, channels, sample_rate as f32))
}

fn decode_wav_hound(path: &Path) -> Result<Decoded> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
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
    Ok((
        interleaved,
        spec.channels.max(1) as usize,
        spec.sample_rate as f32,
    ))
}

fn decode_flac(path: &Path) -> Result<Decoded> {
    let mut reader =
        claxon::FlacReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let info = reader.streaminfo();
    let scale = 1.0 / (1i64 << (info.bits_per_sample - 1)) as f32;
    let interleaved: Vec<f32> = reader
        .samples()
        .map(|s| s.map(|v| v as f32 * scale))
        .collect::<Result<_, _>>()
        .with_context(|| format!("decoding {}", path.display()))?;
    Ok((
        interleaved,
        info.channels.max(1) as usize,
        info.sample_rate as f32,
    ))
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
        .map(|(g, name)| Arc::new(Sample::new(name, g(), None, GEN_SR)))
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
            let formants: [f32; 3] =
                std::array::from_fn(|k| VOWELS[a][k] * (1.0 - f) + VOWELS[b][k] * f);
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
                coefs = SvfCoefs::new(
                    FilterMode::BandPass,
                    200.0 * 30f32.powf(sweep),
                    0.75,
                    GEN_SR,
                );
            }
            f.process(&coefs, rng.bipolar())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
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

    fn tone(freq: f32, seconds: f32) -> Sample {
        let sr = 48_000.0;
        let data = (0..(sr * seconds) as usize)
            .map(|i| {
                let t = i as f32 / sr;
                // A few harmonics, like a real instrument.
                (1..=5)
                    .map(|h| (std::f32::consts::TAU * freq * h as f32 * t).sin() / h as f32)
                    .sum::<f32>()
            })
            .collect();
        Sample::new("tone", data, None, sr)
    }

    #[test]
    fn detects_pitch_to_within_a_few_cents() {
        crate::dsp::init_tables();
        for (freq, note) in [
            (440.0, 69.0),
            (55.0, 33.0),
            (1046.5, 84.0),
            (440.0 * 2f32.powf(0.3 / 12.0), 69.3),
        ] {
            let p = tone(freq, 1.0)
                .detect_pitch()
                .unwrap_or_else(|| panic!("{freq} Hz: no pitch"));
            assert!(
                (p - note).abs() < 0.05,
                "{freq} Hz: detected {p}, expected {note}"
            );
        }
    }

    #[test]
    fn builtin_sources_read_as_c4_and_noise_has_no_pitch() {
        crate::dsp::init_tables();
        let b = builtins();
        for s in b
            .iter()
            .filter(|s| matches!(s.name.as_str(), "Choir" | "Saw" | "Pluck"))
        {
            let p = s
                .detect_pitch()
                .unwrap_or_else(|| panic!("{}: no pitch", s.name));
            assert!((p - 60.0).abs() < 0.1, "{}: {p}", s.name);
        }
        let noise = b.iter().find(|s| s.name == "Noise").unwrap();
        assert_eq!(noise.detect_pitch(), None);
    }

    /// WAVs with a 20-byte fmt chunk (valid, but rejected by hound) still load.
    #[test]
    fn lenient_reader_handles_odd_fmt_chunks() {
        let path =
            std::env::temp_dir().join(format!("arkeology-oddfmt-{}.wav", std::process::id()));
        let frames: Vec<i16> = (0..400)
            .map(|i| ((i as f32 * 0.05).sin() * 20_000.0) as i16)
            .collect();
        let mut b = Vec::new();
        let data_len = (frames.len() * 2) as u32;
        b.extend(b"RIFF");
        b.extend((4 + 8 + 20 + 8 + data_len).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(20u32.to_le_bytes());
        b.extend(1u16.to_le_bytes()); // PCM
        b.extend(1u16.to_le_bytes()); // mono
        b.extend(44_100u32.to_le_bytes());
        b.extend((44_100u32 * 2).to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend([0u8; 4]); // cbSize + padding
        b.extend(b"data");
        b.extend(data_len.to_le_bytes());
        for f in &frames {
            b.extend(f.to_le_bytes());
        }
        std::fs::write(&path, &b).unwrap();
        assert!(
            hound::WavReader::open(&path).is_err(),
            "hound accepts it now; test is moot"
        );
        let s = load_file(&path).unwrap();
        assert_eq!(s.len(), 400);
        assert_eq!(s.sample_rate, 44_100.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_non_audio() {
        let err = load_file(&fixture("../../Cargo.toml")).err().unwrap();
        assert!(err.to_string().contains("isn't a WAV or FLAC"), "{err}");
    }
}
