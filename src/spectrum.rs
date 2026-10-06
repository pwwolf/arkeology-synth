//! Spectrum analyzer for the master output, computed on the UI thread from
//! the samples the engine copies into `Telemetry::scope`.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Samples kept in the scope ring and analysed per frame (~85 ms at 48 kHz,
/// 12 Hz resolution, so low notes land in separate bands).
pub const FFT_SIZE: usize = 4096;
/// Display floor and ceiling in dBFS (after the tilt).
pub const FLOOR_DB: f32 = -66.0;
pub const CEIL_DB: f32 = 0.0;
const LOW_HZ: f32 = 30.0;
const HIGH_HZ: f32 = 16_000.0;
/// Typical music falls ~3 dB per octave; tilting by that much reads as flat.
const TILT_DB_PER_OCT: f32 = 3.0;
/// Bars fall at this rate; peak markers hold, then fall more slowly.
const FALL_DB_PER_S: f32 = 36.0;
const PEAK_HOLD_S: f32 = 0.8;
const PEAK_FALL_DB_PER_S: f32 = 15.0;

/// Lock-free ring of the most recent master output samples (mono). The
/// engine writes, the UI reads; a torn read only blurs one frame.
pub struct Scope {
    buf: Box<[AtomicU32]>,
    pos: AtomicUsize,
}

impl Default for Scope {
    fn default() -> Self {
        Scope {
            buf: (0..FFT_SIZE).map(|_| AtomicU32::new(0)).collect(),
            pos: AtomicUsize::new(0),
        }
    }
}

impl Scope {
    /// Append samples (audio thread; allocation-free).
    pub fn write(&self, l: &[f32], r: &[f32]) {
        let mut pos = self.pos.load(Ordering::Relaxed);
        for (a, b) in l.iter().zip(r) {
            self.buf[pos % FFT_SIZE].store((0.5 * (a + b)).to_bits(), Ordering::Relaxed);
            pos = pos.wrapping_add(1);
        }
        self.pos.store(pos, Ordering::Release);
    }

    /// Copy the latest `FFT_SIZE` samples, oldest first.
    pub fn read(&self, out: &mut [f32]) {
        let pos = self.pos.load(Ordering::Acquire);
        for (i, o) in out.iter_mut().enumerate().take(FFT_SIZE) {
            let idx = pos.wrapping_sub(FFT_SIZE).wrapping_add(i) % FFT_SIZE;
            *o = f32::from_bits(self.buf[idx].load(Ordering::Relaxed));
        }
    }
}

/// In-place iterative radix-2 FFT; `re.len()` must be a power of two.
pub fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f64::consts::TAU / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (ang * k as f64).sin_cos();
                let (wr, wi) = (c as f32, s as f32);
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * wr - im[b] * wi;
                let ti = re[b] * wi + im[b] * wr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

pub struct Analyzer {
    sample_rate: f32,
    window: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    /// Band edges in Hz (`bands + 1` values).
    edges: Vec<f32>,
    /// Displayed level per band, dB.
    pub levels: Vec<f32>,
    /// Peak-hold level per band, dB, and how long it has been held.
    pub peaks: Vec<f32>,
    held: Vec<f32>,
}

impl Analyzer {
    pub fn new(sample_rate: f32) -> Self {
        let window = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        let mut a = Analyzer {
            sample_rate,
            window,
            re: vec![0.0; FFT_SIZE],
            im: vec![0.0; FFT_SIZE],
            edges: Vec::new(),
            levels: Vec::new(),
            peaks: Vec::new(),
            held: Vec::new(),
        };
        // What the 48-column left panel fits (one column plus a gap per band).
        a.set_bands(23);
        a
    }

    pub fn bands(&self) -> usize {
        self.levels.len()
    }

    /// Log-spaced bands between 30 Hz and 16 kHz (or just under Nyquist).
    pub fn set_bands(&mut self, bands: usize) {
        if bands == self.levels.len() || bands == 0 {
            return;
        }
        let high = HIGH_HZ.min(self.sample_rate * 0.45);
        let ratio = (high / LOW_HZ).powf(1.0 / bands as f32);
        self.edges = (0..=bands).map(|i| LOW_HZ * ratio.powi(i as i32)).collect();
        self.levels = vec![FLOOR_DB; bands];
        self.peaks = vec![FLOOR_DB; bands];
        self.held = vec![0.0; bands];
    }

    /// Centre frequency of a band (geometric mean of its edges).
    pub fn centre(&self, band: usize) -> f32 {
        (self.edges[band] * self.edges[band + 1]).sqrt()
    }

    /// Analyse the latest samples and advance the bar ballistics by `dt` seconds.
    pub fn update(&mut self, samples: &[f32], dt: f32) {
        for i in 0..FFT_SIZE {
            self.re[i] = samples.get(i).copied().unwrap_or(0.0) * self.window[i];
            self.im[i] = 0.0;
        }
        fft(&mut self.re, &mut self.im);
        // A full-scale sine reads 0 dB: a Hann window's peak bin is N/4.
        let scale = 4.0 / FFT_SIZE as f32;
        let bin_hz = self.sample_rate / FFT_SIZE as f32;
        let mag = |re: &[f32], im: &[f32], k: usize| (re[k] * re[k] + im[k] * im[k]).sqrt() * scale;
        for b in 0..self.levels.len() {
            let (lo, hi) = (self.edges[b] / bin_hz, self.edges[b + 1] / bin_hz);
            let (first, last) = (lo.ceil() as usize, (hi.floor() as usize).min(FFT_SIZE / 2));
            // The band's loudest bin, or (for bass bands narrower than a bin)
            // the bin nearest its centre.
            let m = if first <= last {
                (first..=last)
                    .map(|k| mag(&self.re, &self.im, k))
                    .fold(0.0, f32::max)
            } else {
                mag(
                    &self.re,
                    &self.im,
                    (self.centre(b) / bin_hz).round() as usize,
                )
            };
            let tilt = TILT_DB_PER_OCT * (self.centre(b) / 1_000.0).log2();
            let db = (20.0 * (m + 1e-9).log10() + tilt).clamp(FLOOR_DB, CEIL_DB);
            let level = &mut self.levels[b];
            *level = if db > *level {
                db
            } else {
                (*level - FALL_DB_PER_S * dt).max(db)
            };
            if *level >= self.peaks[b] {
                self.peaks[b] = *level;
                self.held[b] = 0.0;
            } else {
                self.held[b] += dt;
                if self.held[b] > PEAK_HOLD_S {
                    self.peaks[b] = (self.peaks[b] - PEAK_FALL_DB_PER_S * dt).max(*level);
                }
            }
        }
    }

    /// Height of a level in 0..=1 of the display.
    pub fn norm(db: f32) -> f32 {
        ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_a_sine() {
        let n = 1024;
        let mut re: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 37.0 * i as f32 / n as f32).cos())
            .collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let mags: Vec<f32> = (0..n / 2)
            .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
            .collect();
        let top = (0..n / 2)
            .max_by(|a, b| mags[*a].total_cmp(&mags[*b]))
            .unwrap();
        assert_eq!(top, 37);
        assert!((mags[37] - n as f32 / 2.0).abs() < 0.01 * n as f32);
        assert!(
            mags.iter()
                .enumerate()
                .all(|(k, m)| k == 37 || *m < 1e-2 * n as f32)
        );
    }

    #[test]
    fn analyzer_shows_a_tone_in_its_band() {
        let sr = 48_000.0;
        let mut a = Analyzer::new(sr);
        a.set_bands(24);
        let samples: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 * (std::f32::consts::TAU * 1_000.0 * i as f32 / sr).sin())
            .collect();
        a.update(&samples, 1.0 / 60.0);
        let band = (0..a.bands())
            .find(|&b| a.edges[b] <= 1_000.0 && 1_000.0 < a.edges[b + 1])
            .unwrap();
        // -6 dBFS at 1 kHz, where the tilt is zero.
        assert!((a.levels[band] + 6.0).abs() < 1.5, "{}", a.levels[band]);
        for b in (0..a.bands()).filter(|b| b.abs_diff(band) > 2) {
            assert!(
                a.levels[b] < a.levels[band] - 30.0,
                "band {b}: {}",
                a.levels[b]
            );
        }
        // Silence: bars fall, peaks hold before falling.
        let silence = vec![0.0; FFT_SIZE];
        a.update(&silence, 0.5);
        assert!(a.levels[band] < -20.0 && a.peaks[band] > -7.5);
        a.update(&silence, 1.0);
        assert!(a.peaks[band] < -7.5);
    }

    #[test]
    fn scope_returns_the_latest_samples_in_order() {
        let s = Scope::default();
        let block: Vec<f32> = (0..5000).map(|i| i as f32).collect();
        s.write(&block, &block);
        let mut out = vec![0.0; FFT_SIZE];
        s.read(&mut out);
        assert_eq!(out[0], (5000 - FFT_SIZE) as f32);
        assert_eq!(out[FFT_SIZE - 1], 4999.0);
    }
}
