//! Audio sources for the granular engine: WAV loading plus a handful of
//! procedurally generated built-in sources so it makes sound out of the box.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::dsp::{FilterMode, Rng, Svf, SvfCoefs, sin_cycles};

/// A mono sample buffer. Built-ins are tuned so MIDI note 60 plays at the
/// recorded pitch; loaded files are assumed to be at C4 too (use Transpose
/// and Fine to correct).
pub struct Sample {
    pub name: String,
    pub data: Vec<f32>,
    pub sample_rate: f32,
}

impl Sample {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn duration(&self) -> f32 {
        self.data.len() as f32 / self.sample_rate
    }

    /// Linear-interpolated read with wrap-around.
    #[inline]
    pub fn read(&self, pos: f64) -> f32 {
        let len = self.data.len();
        let p = pos.rem_euclid(len as f64);
        let i = p as usize;
        let f = (p - i as f64) as f32;
        let a = self.data[i % len];
        let b = self.data[(i + 1) % len];
        a + (b - a) * f
    }
}

pub fn load_wav(path: &Path) -> Result<Sample> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
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
    let mut data: Vec<f32> = interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    if data.len() < 64 {
        bail!("{} is too short to granulate", path.display());
    }
    // Normalise so quiet recordings are usable.
    let peak = data.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak > 1e-6 {
        let g = 0.9 / peak;
        data.iter_mut().for_each(|v| *v *= g);
    }
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sample".into());
    Ok(Sample {
        name,
        data,
        sample_rate: spec.sample_rate as f32,
    })
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
            let mut data = g();
            normalize(&mut data);
            Arc::new(Sample {
                name: name.to_string(),
                data,
                sample_rate: GEN_SR,
            })
        })
        .collect::<Vec<_>>()
        .into()
}

fn normalize(data: &mut [f32]) {
    let peak = data.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak > 0.0 {
        let g = 0.9 / peak;
        data.iter_mut().for_each(|v| *v *= g);
    }
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
