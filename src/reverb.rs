//! Stereo reverbs: the master send reverb and the Reverb insert effect.
//!
//! - **Classic** is the original Freeverb-style comb/all-pass reverb, kept
//!   exactly as it was so older sessions and patches sound the same.
//! - **Room, Chamber, Hall, Cathedral, Plate** share a feedback delay network:
//!   eight delay lines mixed through a Hadamard matrix, each with a damping
//!   filter so treble decays faster than mids, slowly modulated so tails
//!   don't ring at fixed pitches, fed through input diffusers, with each
//!   type's own early reflections, pre-delay, size range and tone.
//! - **Spring** models a guitar-amp spring tank: feedback loops through
//!   chains of stretched all-pass filters, whose dispersion makes the chirp.
//!
//! Every buffer is sized for the largest type when built (on the UI thread),
//! so switching type on the audio thread never allocates.

pub const REVERB_TYPES: [&str; 7] = [
    "Classic",
    "Room",
    "Chamber",
    "Hall",
    "Cathedral",
    "Plate",
    "Spring",
];

struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Comb {
            buf: vec![0.0; len.max(1)],
            idx: 0,
            store: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.idx];
        self.store = out * (1.0 - damp) + self.store * damp;
        self.buf[self.idx] = input + self.store * feedback;
        self.idx += 1;
        if self.idx == self.buf.len() {
            self.idx = 0;
        }
        out
    }
}

struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Allpass {
            buf: vec![0.0; len.max(1)],
            idx: 0,
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let delayed = self.buf[self.idx];
        let out = delayed - input;
        self.buf[self.idx] = input + delayed * 0.5;
        self.idx += 1;
        if self.idx == self.buf.len() {
            self.idx = 0;
        }
        out
    }
}

const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
const SPREAD: usize = 23;

struct Classic {
    combs_l: Vec<Comb>,
    combs_r: Vec<Comb>,
    ap_l: Vec<Allpass>,
    ap_r: Vec<Allpass>,
    feedback: f32,
    damp: f32,
    width: f32,
}

impl Classic {
    fn new(sample_rate: f32) -> Self {
        let scale = |n: usize| ((n as f32) * sample_rate / 44_100.0) as usize;
        Classic {
            combs_l: COMBS.iter().map(|&n| Comb::new(scale(n))).collect(),
            combs_r: COMBS
                .iter()
                .map(|&n| Comb::new(scale(n + SPREAD)))
                .collect(),
            ap_l: ALLPASSES.iter().map(|&n| Allpass::new(scale(n))).collect(),
            ap_r: ALLPASSES
                .iter()
                .map(|&n| Allpass::new(scale(n + SPREAD)))
                .collect(),
            feedback: 0.84,
            damp: 0.2,
            width: 1.0,
        }
    }

    /// `size`, `damp` and `width` are all 0..=1.
    fn set(&mut self, size: f32, damp: f32, width: f32) {
        self.feedback = 0.7 + 0.28 * size.clamp(0.0, 1.0);
        self.damp = 0.4 * damp.clamp(0.0, 1.0);
        self.width = width.clamp(0.0, 1.0);
    }

    /// Processes the send buffers in place, leaving the 100% wet signal in them.
    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let w1 = 0.5 + self.width * 0.5;
        let w2 = (1.0 - self.width) * 0.5;
        for (sl, sr) in l.iter_mut().zip(r.iter_mut()) {
            let input = (*sl + *sr) * 0.015;
            let mut ol = 0.0;
            let mut or = 0.0;
            for c in &mut self.combs_l {
                ol += c.process(input, self.feedback, self.damp);
            }
            for c in &mut self.combs_r {
                or += c.process(input, self.feedback, self.damp);
            }
            for a in &mut self.ap_l {
                ol = a.process(ol);
            }
            for a in &mut self.ap_r {
                or = a.process(or);
            }
            *sl = (ol * w1 + or * w2) * 3.0;
            *sr = (or * w1 + ol * w2) * 3.0;
        }
    }
}

// ---------------------------------------------------------------------------
// Feedback delay network (Room, Chamber, Hall, Cathedral, Plate)
// ---------------------------------------------------------------------------

const LINES: usize = 8;
/// Delay-line lengths (ms) at scale 1; spread so their echoes don't line up.
const LINE_MS: [f32; LINES] = [31.3, 37.9, 43.1, 47.7, 53.3, 59.9, 66.1, 73.7];
const MOD_HZ: [f32; LINES] = [0.31, 0.43, 0.57, 0.71, 0.37, 0.53, 0.67, 0.83];
const DIFFUSER_MS: [[f32; 4]; 2] = [[4.77, 3.59, 12.73, 9.31], [5.03, 3.83, 11.9, 8.67]];
const MAX_SCALE: f32 = 1.9;
/// Longest early reflection plus pre-delay (ms), for buffer sizing.
const MAX_EARLY_MS: f32 = 230.0;

/// One room type.
struct Space {
    /// Delay-line scale at Size 0 and 1 (bigger rooms, longer lines).
    scale: (f32, f32),
    /// Mid-frequency decay time (s, to -60 dB) at Size 0 and 1.
    decay: (f32, f32),
    /// Treble decay as a fraction of the mid decay at Damping 0 and 1.
    hf: (f32, f32),
    predelay_ms: f32,
    /// Input diffuser all-pass gain: higher builds density faster.
    diffusion: f32,
    /// Delay modulation depth (ms).
    mod_ms: f32,
    /// Input low-pass (Hz): how bright the space is.
    bright: f32,
    /// Early reflections: (ms after pre-delay, gain, pan -1..1).
    early: &'static [(f32, f32, f32)],
    early_level: f32,
    /// Output level, matched across types by measurement.
    level: f32,
}

const SPACES: [Space; 5] = [
    // Room: fast, dense early reflections and a short, darker tail.
    Space {
        scale: (0.25, 0.45),
        decay: (0.35, 1.0),
        hf: (0.75, 0.35),
        predelay_ms: 2.0,
        diffusion: 0.6,
        mod_ms: 0.1,
        bright: 9_000.0,
        early: &[
            (2.1, 0.85, -0.6),
            (3.7, 0.7, 0.5),
            (5.3, 0.6, -0.2),
            (7.9, 0.55, 0.8),
            (9.4, 0.5, -0.9),
            (12.2, 0.42, 0.3),
            (14.6, 0.38, -0.4),
            (17.8, 0.33, 0.65),
            (21.3, 0.27, -0.7),
            (25.1, 0.22, 0.1),
            (29.0, 0.18, 0.9),
        ],
        early_level: 0.5,
        level: 4.17,
    },
    // Chamber: a thick, smooth build-up and a medium tail.
    Space {
        scale: (0.45, 0.75),
        decay: (0.9, 2.0),
        hf: (0.8, 0.45),
        predelay_ms: 8.0,
        diffusion: 0.7,
        mod_ms: 0.2,
        bright: 11_000.0,
        early: &[
            (3.0, 0.7, 0.4),
            (6.1, 0.62, -0.5),
            (9.7, 0.55, 0.7),
            (13.3, 0.5, -0.3),
            (17.9, 0.44, 0.2),
            (22.4, 0.38, -0.8),
            (28.6, 0.32, 0.6),
            (35.2, 0.27, -0.1),
            (43.0, 0.22, 0.85),
        ],
        early_level: 0.4,
        level: 3.85,
    },
    // Hall: sparse early reflections after a pre-delay, a long smooth tail.
    Space {
        scale: (0.8, 1.2),
        decay: (1.6, 4.0),
        hf: (0.7, 0.3),
        predelay_ms: 22.0,
        diffusion: 0.65,
        mod_ms: 0.35,
        bright: 9_000.0,
        early: &[
            (12.0, 0.6, -0.7),
            (19.0, 0.5, 0.8),
            (27.0, 0.45, -0.3),
            (36.0, 0.4, 0.5),
            (48.0, 0.33, -0.8),
            (61.0, 0.27, 0.2),
            (77.0, 0.22, 0.9),
        ],
        early_level: 0.35,
        level: 3.80,
    },
    // Cathedral: a very long, dark tail that builds slowly.
    Space {
        scale: (1.3, MAX_SCALE),
        decay: (4.0, 9.5),
        hf: (0.6, 0.25),
        predelay_ms: 35.0,
        diffusion: 0.7,
        mod_ms: 0.45,
        bright: 7_500.0,
        early: &[
            (20.0, 0.5, -0.6),
            (34.0, 0.45, 0.7),
            (51.0, 0.38, -0.2),
            (72.0, 0.32, 0.6),
            (95.0, 0.27, -0.8),
            (123.0, 0.22, 0.3),
        ],
        early_level: 0.3,
        level: 2.63,
    },
    // Plate: no early reflections, an instant dense, bright wash.
    Space {
        scale: (0.35, 0.55),
        decay: (1.0, 3.5),
        hf: (0.9, 0.5),
        predelay_ms: 0.0,
        diffusion: 0.78,
        mod_ms: 0.25,
        bright: 16_000.0,
        early: &[],
        early_level: 0.0,
        level: 2.51,
    },
];

struct Diffuser {
    buf: Vec<f32>,
    idx: usize,
    len: usize,
}

impl Diffuser {
    fn new(len: usize) -> Self {
        Diffuser {
            buf: vec![0.0; len.max(1)],
            idx: 0,
            len: len.max(1),
        }
    }

    #[inline]
    fn process(&mut self, x: f32, g: f32) -> f32 {
        let d = self.buf[self.idx];
        let v = x + g * d;
        self.buf[self.idx] = v;
        self.idx += 1;
        if self.idx == self.len {
            self.idx = 0;
        }
        d - g * v
    }
}

/// Frequency at which a type's treble decay is specified.
const DAMP_REF_HZ: f32 = 4_000.0;

/// Coefficient `d` of the one-pole low-pass `y = (1-d)x + d·y[-1]` (unity
/// gain at DC) whose gain at `w` radians/sample is `ratio` (0 < ratio <= 1).
fn one_pole_for(ratio: f32, w: f32) -> f32 {
    let r2 = (ratio * ratio).clamp(0.0, 1.0) as f64;
    if r2 > 0.999_999 {
        return 0.0;
    }
    // (1-d)^2 = r^2 (1 - 2d cos w + d^2): the root inside the unit circle.
    let c = w.cos() as f64;
    let b = 1.0 - r2 * c;
    let a = 1.0 - r2;
    ((b - (b * b - a * a).max(0.0).sqrt()) / a) as f32
}

/// In-place Hadamard transform of 8 values, scaled to stay energy-preserving.
#[inline]
fn hadamard(x: &mut [f32; LINES]) {
    let mut h = 1;
    while h < LINES {
        for i in (0..LINES).step_by(2 * h) {
            for j in i..i + h {
                let (a, b) = (x[j], x[j + h]);
                x[j] = a + b;
                x[j + h] = a - b;
            }
        }
        h *= 2;
    }
    let s = 1.0 / (LINES as f32).sqrt();
    for v in x.iter_mut() {
        *v *= s;
    }
}

struct Fdn {
    sr: f32,
    lines: [Vec<f32>; LINES],
    mask: usize,
    w: usize,
    delay: [f32; LINES],
    gain: [f32; LINES],
    damp: [f32; LINES],
    lp: [f32; LINES],
    mod_phase: [f32; LINES],
    mod_depth: f32,
    diffusers: [[Diffuser; 4]; 2],
    diffusion: f32,
    /// Pre-delay and early reflections share one buffer per channel.
    early_buf: [Vec<f32>; 2],
    early_mask: usize,
    early_w: usize,
    predelay: usize,
    taps: [(usize, f32, f32); 12],
    tap_count: usize,
    early_level: f32,
    in_lp: [f32; 2],
    in_coef: f32,
    level: f32,
    width: f32,
}

impl Fdn {
    fn new(sr: f32) -> Self {
        let max_line = ((LINE_MS[LINES - 1] * MAX_SCALE + 2.0) * 0.001 * sr) as usize;
        let line_len = max_line.next_power_of_two();
        let early_len = ((MAX_EARLY_MS * 0.001 * sr) as usize).next_power_of_two();
        let diffusers = std::array::from_fn(|ch| {
            std::array::from_fn(|k| Diffuser::new((DIFFUSER_MS[ch][k] * 0.001 * sr) as usize))
        });
        Fdn {
            sr,
            lines: std::array::from_fn(|_| vec![0.0; line_len]),
            mask: line_len - 1,
            w: 0,
            delay: [1.0; LINES],
            gain: [0.0; LINES],
            damp: [0.0; LINES],
            lp: [0.0; LINES],
            mod_phase: std::array::from_fn(|k| k as f32 / LINES as f32),
            mod_depth: 0.0,
            diffusers,
            diffusion: 0.6,
            early_buf: [vec![0.0; early_len], vec![0.0; early_len]],
            early_mask: early_len - 1,
            early_w: 0,
            predelay: 0,
            taps: [(0, 0.0, 0.0); 12],
            tap_count: 0,
            early_level: 0.0,
            in_lp: [0.0; 2],
            in_coef: 0.0,
            level: 1.0,
            width: 1.0,
        }
    }

    fn set(&mut self, space: &Space, size: f32, damp: f32, width: f32) {
        let sr = self.sr;
        let size = size.clamp(0.0, 1.0);
        let scale = space.scale.0 + (space.scale.1 - space.scale.0) * size;
        let t60 = space.decay.0 * (space.decay.1 / space.decay.0).powf(size);
        let hf = space.hf.0 + (space.hf.1 - space.hf.0) * damp.clamp(0.0, 1.0);
        for (k, ms) in LINE_MS.iter().enumerate() {
            let len = ms * scale * 0.001 * sr;
            self.delay[k] = len;
            // Loop gain for the mid decay, and a damping low-pass whose gain
            // at 4 kHz gives the (shorter) treble decay there.
            let g = 10f32.powf(-3.0 * len / (sr * t60));
            let g_hf = 10f32.powf(-3.0 * len / (sr * t60 * hf));
            self.gain[k] = g;
            self.damp[k] = one_pole_for(g_hf / g, std::f32::consts::TAU * DAMP_REF_HZ / sr);
        }
        self.mod_depth = space.mod_ms * 0.001 * sr;
        self.diffusion = space.diffusion;
        self.predelay = (space.predelay_ms * 0.001 * sr) as usize;
        self.tap_count = space.early.len().min(self.taps.len());
        let early_scale = 0.8 + 0.4 * size;
        for (t, &(ms, gain, pan)) in self.taps.iter_mut().zip(space.early) {
            let d = self.predelay + (ms * early_scale * 0.001 * sr) as usize;
            let (l, r) = crate::dsp::pan_gains(pan);
            *t = (d.min(self.early_mask), gain * l, gain * r);
        }
        self.early_level = space.early_level;
        self.in_coef = (-std::f32::consts::TAU * space.bright / sr).exp();
        self.level = space.level;
        self.width = width.clamp(0.0, 1.0);
    }

    fn clear(&mut self) {
        for l in &mut self.lines {
            l.fill(0.0);
        }
        for ch in &mut self.diffusers {
            for d in ch {
                d.buf.fill(0.0);
            }
        }
        for b in &mut self.early_buf {
            b.fill(0.0);
        }
        self.lp = [0.0; LINES];
        self.in_lp = [0.0; 2];
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let w1 = 0.5 + self.width * 0.5;
        let w2 = (1.0 - self.width) * 0.5;
        let mod_inc: [f32; LINES] = std::array::from_fn(|k| MOD_HZ[k] / self.sr);
        for (sl, sr) in l.iter_mut().zip(r.iter_mut()) {
            // Tone, then pre-delay and early reflections.
            let c = self.in_coef;
            self.in_lp[0] = *sl + (self.in_lp[0] - *sl) * c;
            self.in_lp[1] = *sr + (self.in_lp[1] - *sr) * c;
            let em = self.early_mask;
            self.early_buf[0][self.early_w] = self.in_lp[0];
            self.early_buf[1][self.early_w] = self.in_lp[1];
            let pre = (self.early_w + em + 1 - self.predelay) & em;
            let (mut el, mut er) = (0.0, 0.0);
            for &(d, gl, gr) in &self.taps[..self.tap_count] {
                let i = (self.early_w + em + 1 - d) & em;
                let x = 0.5 * (self.early_buf[0][i] + self.early_buf[1][i]);
                el += x * gl;
                er += x * gr;
            }
            let (mut il, mut ir) = (self.early_buf[0][pre], self.early_buf[1][pre]);
            self.early_w = (self.early_w + 1) & em;

            // Diffuse the input so the tail is dense from the start.
            let g = self.diffusion;
            for (k, d) in self.diffusers[0].iter_mut().enumerate() {
                il = d.process(il, if k < 2 { g } else { g * 0.85 });
            }
            for (k, d) in self.diffusers[1].iter_mut().enumerate() {
                ir = d.process(ir, if k < 2 { g } else { g * 0.85 });
            }

            // Read the (modulated) lines, damp, mix, write back.
            let mut y = [0.0f32; LINES];
            for k in 0..LINES {
                let m = self.mod_depth * (1.0 + crate::dsp::sin_cycles(self.mod_phase[k]));
                self.mod_phase[k] = (self.mod_phase[k] + mod_inc[k]).fract();
                let pos = self.w as f32 + (self.mask + 1) as f32 - self.delay[k] - m;
                let i = pos as usize;
                let f = pos - i as f32;
                let a = self.lines[k][i & self.mask];
                let b = self.lines[k][(i + 1) & self.mask];
                let out = a + (b - a) * f;
                self.lp[k] = (1.0 - self.damp[k]) * out + self.damp[k] * self.lp[k];
                y[k] = self.lp[k] * self.gain[k];
            }
            let (mut ol, mut or) = (0.0, 0.0);
            // Two orthogonal sign patterns for the left and right outputs.
            for (k, &v) in y.iter().enumerate() {
                ol += if k % 2 == 0 { v } else { -v };
                or += if k & 2 == 0 { v } else { -v };
            }
            let mut v = y;
            hadamard(&mut v);
            for (k, (line, mixed)) in self.lines.iter_mut().zip(v).enumerate() {
                let input = if k % 2 == 0 { il } else { ir };
                line[self.w] = mixed + input * 0.5;
            }
            self.w = (self.w + 1) & self.mask;

            let s = 0.35 * self.level;
            ol = ol * s + el * self.early_level;
            or = or * s + er * self.early_level;
            *sl = ol * w1 + or * w2;
            *sr = or * w1 + ol * w2;
        }
    }
}

// ---------------------------------------------------------------------------
// Spring tank
// ---------------------------------------------------------------------------

/// All-pass stages per spring, the stretch factor and coefficient: delays
/// pile up around 2 kHz, so transients smear into the spring's chirp.
const SPRING_STAGES: usize = 48;
const SPRING_STRETCH: usize = 12;
const SPRING_LOOP_MS: [f32; 2] = [27.0, 33.0];

struct Spring {
    /// Each stage's last `SPRING_STRETCH` inputs and outputs.
    ap_x: Vec<[f32; SPRING_STRETCH]>,
    ap_y: Vec<[f32; SPRING_STRETCH]>,
    ap_i: usize,
    a: f32,
    line: Vec<f32>,
    len: usize,
    w: usize,
    feedback: f32,
    lp: [f32; 2],
    lp_coef: f32,
    hp: f32,
}

impl Spring {
    fn new(sr: f32, loop_ms: f32, a: f32) -> Self {
        let len = (loop_ms * 0.001 * sr) as usize;
        Spring {
            ap_x: vec![[0.0; SPRING_STRETCH]; SPRING_STAGES],
            ap_y: vec![[0.0; SPRING_STRETCH]; SPRING_STAGES],
            ap_i: 0,
            a,
            line: vec![0.0; len.max(1)],
            len: len.max(1),
            w: 0,
            feedback: 0.0,
            lp: [0.0; 2],
            lp_coef: 0.0,
            hp: 0.0,
        }
    }

    fn set(&mut self, sr: f32, size: f32, damp: f32) {
        let t60 = 1.5 * (4.0f32 / 1.5).powf(size.clamp(0.0, 1.0));
        // Round trip: the loop line plus the chain's low-frequency delay.
        let low_delay =
            SPRING_STAGES as f32 * SPRING_STRETCH as f32 * (1.0 - self.a) / (1.0 + self.a);
        let trip = self.len as f32 + low_delay;
        self.feedback = 10f32.powf(-3.0 * trip / (sr * t60));
        let fc = 5_000.0 * (0.5f32).powf(damp.clamp(0.0, 1.0));
        self.lp_coef = (-std::f32::consts::TAU * fc / sr).exp();
    }

    fn clear(&mut self) {
        for s in self.ap_x.iter_mut().chain(self.ap_y.iter_mut()) {
            *s = [0.0; SPRING_STRETCH];
        }
        self.line.fill(0.0);
        self.lp = [0.0; 2];
        self.hp = 0.0;
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let out = self.line[self.w];
        let mut v = x + out * self.feedback;
        let i = self.ap_i;
        let a = self.a;
        for (xs, ys) in self.ap_x.iter_mut().zip(self.ap_y.iter_mut()) {
            let y = a * v + xs[i] - a * ys[i];
            xs[i] = v;
            ys[i] = y;
            v = y;
        }
        self.ap_i = (i + 1) % SPRING_STRETCH;
        // Two-pole low-pass: springs don't carry much treble, and it hides
        // the stretched chain's repeating response above it.
        let c = self.lp_coef;
        self.lp[0] = v + (self.lp[0] - v) * c;
        self.lp[1] = self.lp[0] + (self.lp[1] - self.lp[0]) * c;
        self.line[self.w] = self.lp[1];
        self.w += 1;
        if self.w == self.len {
            self.w = 0;
        }
        // Springs are thin: high-pass the output around 300 Hz.
        self.hp += (out - self.hp) * 0.04;
        out - self.hp
    }
}

const SPRING_LEVEL: f32 = 3.01;

// ---------------------------------------------------------------------------
// The reverb as used by the engine
// ---------------------------------------------------------------------------

pub struct Reverb {
    sample_rate: f32,
    kind: usize,
    classic: Classic,
    fdn: Box<Fdn>,
    springs: [Spring; 2],
    width: f32,
}

impl Reverb {
    pub fn new(sample_rate: f32) -> Self {
        Reverb {
            sample_rate,
            kind: 0,
            classic: Classic::new(sample_rate),
            fdn: Box::new(Fdn::new(sample_rate)),
            springs: [
                Spring::new(sample_rate, SPRING_LOOP_MS[0], 0.6),
                Spring::new(sample_rate, SPRING_LOOP_MS[1], 0.57),
            ],
            width: 1.0,
        }
    }

    /// `kind` indexes `REVERB_TYPES`; `size`, `damp` and `width` are 0..=1.
    /// Allocation-free.
    pub fn set(&mut self, kind: usize, size: f32, damp: f32, width: f32) {
        let kind = kind.min(REVERB_TYPES.len() - 1);
        if kind != self.kind {
            // Start the new space empty rather than ringing with the old one.
            self.fdn.clear();
            for s in &mut self.springs {
                s.clear();
            }
            self.kind = kind;
        }
        self.width = width.clamp(0.0, 1.0);
        match kind {
            0 => self.classic.set(size, damp, width),
            6 => {
                for s in &mut self.springs {
                    s.set(self.sample_rate, size, damp);
                }
            }
            k => self.fdn.set(&SPACES[k - 1], size, damp, width),
        }
    }

    /// Processes the send buffers in place, leaving the 100% wet signal in them.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        match self.kind {
            0 => self.classic.process(l, r),
            6 => {
                let w1 = 0.5 + self.width * 0.5;
                let w2 = (1.0 - self.width) * 0.5;
                for (sl, sr) in l.iter_mut().zip(r.iter_mut()) {
                    let x = 0.5 * (*sl + *sr);
                    let a = self.springs[0].process(x) * SPRING_LEVEL;
                    let b = self.springs[1].process(x) * SPRING_LEVEL;
                    *sl = a * w1 + b * w2;
                    *sr = b * w1 + a * w2;
                }
            }
            _ => self.fdn.process(l, r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SR: f32 = 48_000.0;
    const CLASSIC: usize = 0;
    const HALL: usize = 3;
    const PLATE: usize = 5;
    const SPRING: usize = 6;

    fn impulse_response(kind: usize, size: f32, damp: f32, secs: f32) -> Vec<f32> {
        crate::dsp::init_tables();
        let mut r = Reverb::new(SR);
        r.set(kind, size, damp, 1.0);
        let n = (secs * SR) as usize;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let (mut l, mut rr) = ([0.0f32; 64], [0.0f32; 64]);
            if out.is_empty() {
                (l[0], rr[0]) = (1.0, 1.0);
            }
            r.process(&mut l, &mut rr);
            out.extend_from_slice(&l);
        }
        out
    }

    /// RT60 from the -5..-35 dB slope of the Schroeder energy decay curve.
    fn rt60(x: &[f32]) -> f32 {
        let mut edc = vec![0.0f64; x.len()];
        let mut acc = 0.0;
        for i in (0..x.len()).rev() {
            acc += (x[i] as f64).powi(2);
            edc[i] = acc;
        }
        let at = |db: f64| {
            (0..x.len())
                .find(|&i| 10.0 * (edc[i] / edc[0]).log10() < db)
                .unwrap_or(x.len() - 1)
        };
        2.0 * (at(-35.0) - at(-5.0)) as f32 / SR
    }

    /// Normalized echo density around `t` s: ~1 once the tail is noise-like.
    fn echo_density(x: &[f32], t: f32) -> f32 {
        let c = (t * SR) as usize;
        let w = &x[c - 480..c + 480];
        let sd = (w.iter().map(|v| v * v).sum::<f32>() / w.len() as f32).sqrt();
        w.iter().filter(|v| v.abs() > sd).count() as f32 / w.len() as f32 / 0.3173
    }

    #[test]
    fn each_type_decays_in_its_range() {
        let ranges = [
            (1.5, 2.5),
            (0.4, 1.0),
            (1.0, 2.0),
            (2.0, 4.0),
            (5.0, 9.5),
            (1.5, 3.0),
            (1.8, 3.5),
        ];
        for (kind, (lo, hi)) in ranges.iter().enumerate() {
            let t = rt60(&impulse_response(kind, 0.75, 0.4, 14.0));
            assert!((*lo..*hi).contains(&t), "{}: {t:.2} s", REVERB_TYPES[kind]);
        }
        let small = rt60(&impulse_response(HALL, 0.0, 0.4, 8.0));
        let large = rt60(&impulse_response(HALL, 1.0, 0.4, 8.0));
        assert!(large > 2.0 * small, "hall size {small:.2} -> {large:.2} s");
    }

    #[test]
    fn damping_shortens_the_treble() {
        let band_rt = |damp: f32, f: f32| {
            let mut bp = crate::dsp::Biquad::band_pass(f, 2.0, SR);
            let x: Vec<f32> = impulse_response(HALL, 0.75, damp, 8.0)
                .iter()
                .map(|v| bp.process(*v))
                .collect();
            rt60(&x)
        };
        // Band-passed decays blur slightly (neighbouring bands leak into the
        // late tail), so check the ordering and the effect of the knob.
        let (mid, upper, treble) = (
            band_rt(1.0, 500.0),
            band_rt(1.0, 2_000.0),
            band_rt(1.0, 6_000.0),
        );
        assert!(
            treble < upper && upper < mid,
            "{mid:.2} / {upper:.2} / {treble:.2} s"
        );
        assert!(
            treble < 0.6 * mid,
            "damped hall: {mid:.2} s at 500 Hz, {treble:.2} s at 6 kHz"
        );
        let open = band_rt(0.0, 6_000.0);
        assert!(
            treble < 0.85 * open,
            "6 kHz: {open:.2} s undamped, {treble:.2} s damped"
        );
    }

    #[test]
    fn spaces_build_up_like_their_type() {
        let plate = impulse_response(PLATE, 0.75, 0.4, 1.0);
        let hall = impulse_response(HALL, 0.75, 0.4, 1.0);
        assert!(echo_density(&plate, 0.05) > 0.8, "a plate is dense at once");
        assert!(
            echo_density(&hall, 0.05) < 0.2,
            "a hall's tail comes after its pre-delay"
        );
        for (kind, name) in REVERB_TYPES.iter().enumerate().take(SPRING).skip(1) {
            let x = impulse_response(kind, 0.75, 0.4, 1.0);
            let d = echo_density(&x, 0.3);
            assert!(d > 0.85, "{name} tail isn't dense: {d:.2}");
        }
    }

    /// Pink noise (Paul Kellet's filter): equal energy per octave, like music.
    fn pink_noise(n: usize) -> Vec<f32> {
        let mut rng = crate::dsp::Rng::new(7);
        let mut b = [0.0f32; 3];
        (0..n)
            .map(|_| {
                let w = rng.bipolar();
                b[0] = 0.99765 * b[0] + w * 0.0990460;
                b[1] = 0.96300 * b[1] + w * 0.2965164;
                b[2] = 0.57000 * b[2] + w * 1.0526913;
                (b[0] + b[1] + b[2] + w * 0.1848) * 0.03
            })
            .collect()
    }

    /// Switching type keeps the send level: steady-state output on pink
    /// noise within 1.5 dB of Classic.
    #[test]
    fn types_are_level_matched() {
        let noise = pink_noise(96_000);
        let level = |kind: usize| {
            let mut r = Reverb::new(SR);
            r.set(kind, 0.75, 0.4, 1.0);
            let mut out = Vec::new();
            for c in noise.chunks(64) {
                let (mut a, mut b) = (c.to_vec(), c.to_vec());
                r.process(&mut a, &mut b);
                out.extend(a);
            }
            (out[48_000..].iter().map(|v| v * v).sum::<f32>() / 48_000.0).sqrt()
        };
        let base = level(CLASSIC);
        for (kind, name) in REVERB_TYPES.iter().enumerate().skip(1) {
            let db = 20.0 * (level(kind) / base).log10();
            assert!(db.abs() < 1.5, "{name}: {db:+.1} dB");
        }
    }

    /// The spring's all-pass chains delay its upper mids: the chirp.
    #[test]
    fn spring_chirps() {
        let x = impulse_response(SPRING, 0.75, 0.4, 0.1);
        let centroid = |f: f32| {
            let mut bp = crate::dsp::Biquad::band_pass(f, 6.0, SR);
            let (mut num, mut den) = (0.0f64, 0.0f64);
            for (i, v) in x[..(0.06 * SR) as usize].iter().enumerate() {
                let e = (bp.process(*v) as f64).powi(2);
                if i > (0.02 * SR) as usize {
                    num += e * i as f64;
                    den += e;
                }
            }
            num / den / SR as f64 * 1000.0
        };
        let (low, high) = (centroid(300.0), centroid(2_000.0));
        assert!(
            high > low + 6.0,
            "300 Hz at {low:.1} ms, 2 kHz at {high:.1} ms"
        );
    }

    /// Switching type mid-stream doesn't ring on with the old space or blow up.
    #[test]
    fn switching_type_is_clean() {
        crate::dsp::init_tables();
        let mut r = Reverb::new(SR);
        let mut rng = crate::dsp::Rng::new(3);
        for kind in [3, 6, 4, 0, 5, 1, 2, 3, 6] {
            r.set(kind, 1.0, 0.0, 1.0);
            for _ in 0..200 {
                let mut l: Vec<f32> = (0..64).map(|_| rng.bipolar()).collect();
                let mut rr = l.clone();
                r.process(&mut l, &mut rr);
                assert!(l.iter().chain(&rr).all(|v| v.is_finite() && v.abs() < 50.0));
            }
        }
        r.set(HALL, 0.75, 0.4, 1.0);
        let (mut l, mut rr) = ([0.0f32; 64], [0.0f32; 64]);
        r.process(&mut l, &mut rr);
        assert!(l.iter().all(|v| *v == 0.0), "the new space starts empty");
    }
}
