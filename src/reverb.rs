//! A compact Freeverb-style stereo reverb used as the master send effect.

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

pub struct Reverb {
    combs_l: Vec<Comb>,
    combs_r: Vec<Comb>,
    ap_l: Vec<Allpass>,
    ap_r: Vec<Allpass>,
    feedback: f32,
    damp: f32,
    width: f32,
}

impl Reverb {
    pub fn new(sample_rate: f32) -> Self {
        let scale = |n: usize| ((n as f32) * sample_rate / 44_100.0) as usize;
        Reverb {
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
    pub fn set(&mut self, size: f32, damp: f32, width: f32) {
        self.feedback = 0.7 + 0.28 * size.clamp(0.0, 1.0);
        self.damp = 0.4 * damp.clamp(0.0, 1.0);
        self.width = width.clamp(0.0, 1.0);
    }

    /// Processes the send buffers in place, leaving the 100% wet signal in them.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
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
