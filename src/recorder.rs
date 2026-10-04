//! Recording the master output to a WAV file.
//!
//! The audio thread copies each rendered block into a pre-allocated buffer
//! from a free pool and hands it to a writer thread through a lock-free
//! queue; the writer appends it to the file and returns the buffer. Nothing
//! on the audio side allocates, blocks or touches the disk. If the writer
//! falls behind by more than the pool holds, blocks are dropped and counted.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossbeam_queue::ArrayQueue;

use crate::synth::MAX_BLOCK;

/// ~5.5 s of audio at 48 kHz in 64-frame blocks.
const POOL_BLOCKS: usize = 4096;

pub struct RecBlock {
    len: usize,
    data: [f32; MAX_BLOCK * 2],
}

/// Progress shared between the engine, the writer and the UI.
#[derive(Default)]
pub struct RecShared {
    pub frames: AtomicU64,
    pub dropped_frames: AtomicU64,
    done: AtomicBool,
}

/// The engine's end: owned by the audio thread while recording.
pub struct RecordTap {
    free: Arc<ArrayQueue<Box<RecBlock>>>,
    full: Arc<ArrayQueue<Box<RecBlock>>>,
    shared: Arc<RecShared>,
}

impl RecordTap {
    /// Queue one block of stereo output. Allocation-free.
    pub fn write(&self, l: &[f32], r: &[f32]) {
        let n = l.len().min(MAX_BLOCK);
        let Some(mut block) = self.free.pop() else {
            self.shared
                .dropped_frames
                .fetch_add(n as u64, Ordering::Relaxed);
            return;
        };
        for i in 0..n {
            block.data[2 * i] = l[i];
            block.data[2 * i + 1] = r[i];
        }
        block.len = n;
        // The pool and queue are the same size, so this can't fail.
        let _ = self.full.push(block);
    }

    /// Called by the engine after its last `write`.
    pub fn finish(&self) {
        self.shared.done.store(true, Ordering::Release);
    }
}

/// The UI's end: the file being written and its writer thread.
pub struct Recording {
    pub path: PathBuf,
    pub sample_rate: f32,
    pub shared: Arc<RecShared>,
    writer: Option<JoinHandle<Result<()>>>,
}

impl Recording {
    /// Create the file and writer thread; returns the tap to hand to the engine.
    pub fn start(path: PathBuf, sample_rate: f32) -> Result<(Recording, Box<RecordTap>)> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: sample_rate as u32,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut wav = hound::WavWriter::create(&path, spec)
            .with_context(|| format!("creating {}", path.display()))?;
        let free = Arc::new(ArrayQueue::new(POOL_BLOCKS));
        let full = Arc::new(ArrayQueue::new(POOL_BLOCKS));
        for _ in 0..POOL_BLOCKS {
            let _ = free.push(Box::new(RecBlock {
                len: 0,
                data: [0.0; MAX_BLOCK * 2],
            }));
        }
        let shared = Arc::new(RecShared::default());
        let tap = Box::new(RecordTap {
            free: free.clone(),
            full: full.clone(),
            shared: shared.clone(),
        });
        let thread_shared = shared.clone();
        let writer =
            std::thread::Builder::new()
                .name("recorder".into())
                .spawn(move || -> Result<()> {
                    loop {
                        // Read `done` before draining so a block pushed just before
                        // the engine finished is never missed.
                        let done = thread_shared.done.load(Ordering::Acquire);
                        let mut wrote = false;
                        while let Some(block) = full.pop() {
                            for &s in &block.data[..block.len * 2] {
                                wav.write_sample(s)?;
                            }
                            thread_shared
                                .frames
                                .fetch_add(block.len as u64, Ordering::Relaxed);
                            let _ = free.push(block);
                            wrote = true;
                        }
                        if done {
                            break;
                        }
                        if !wrote {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    wav.finalize()?;
                    Ok(())
                })?;
        Ok((
            Recording {
                path,
                sample_rate,
                shared,
                writer: Some(writer),
            },
            tap,
        ))
    }

    pub fn seconds(&self) -> f64 {
        self.shared.frames.load(Ordering::Relaxed) as f64 / self.sample_rate as f64
    }

    pub fn dropped_seconds(&self) -> f64 {
        self.shared.dropped_frames.load(Ordering::Relaxed) as f64 / self.sample_rate as f64
    }

    /// True once the writer has finalized the file (after the engine stopped).
    pub fn is_finished(&self) -> bool {
        self.writer.as_ref().is_none_or(|w| w.is_finished())
    }

    /// Wait for the file to be finalized.
    pub fn join(mut self) -> Result<PathBuf> {
        if let Some(w) = self.writer.take() {
            w.join()
                .map_err(|_| anyhow!("recorder thread panicked"))??;
        }
        Ok(self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_exactly_what_it_was_given() {
        let path = std::env::temp_dir().join(format!("arkeology-rec-{}.wav", std::process::id()));
        let (rec, tap) = Recording::start(path.clone(), 48_000.0).unwrap();
        let mut expected = Vec::new();
        for b in 0..100 {
            let l: Vec<f32> = (0..64)
                .map(|i| ((b * 64 + i) as f32 * 0.001).sin())
                .collect();
            let r: Vec<f32> = l.iter().map(|v| -v).collect();
            tap.write(&l, &r);
            for (a, c) in l.iter().zip(&r) {
                expected.extend([*a, *c]);
            }
        }
        tap.finish();
        let path = rec.join().unwrap();
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        let got: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(got, expected);
        let _ = std::fs::remove_file(&path);
    }
}
