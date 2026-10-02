//! cpal output stream wiring.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample, StreamConfig};
use crossbeam_queue::ArrayQueue;

use crate::engine::Engine;

pub struct AudioOut {
    // Kept alive for as long as audio should play.
    _stream: cpal::Stream,
    pub sample_rate: f32,
    pub device_name: String,
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown device".into())
}

pub fn list_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    Ok(host.output_devices()?.map(|d| device_name(&d)).collect())
}

/// Open the output device and start the stream. `make_engine` is called with
/// the device sample rate once it is known.
pub fn start(
    device: Option<&str>,
    buffer: Option<u32>,
    errors: Arc<ArrayQueue<String>>,
    make_engine: impl FnOnce(f32) -> Engine,
) -> Result<AudioOut> {
    let host = cpal::default_host();
    let device = match device {
        Some(want) => host
            .output_devices()?
            .find(|d| device_name(d).to_lowercase().contains(&want.to_lowercase()))
            .ok_or_else(|| anyhow!("no output device matching '{want}' (try --list-devices)"))?,
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default audio output device"))?,
    };
    let name = device_name(&device);
    let supported = device
        .default_output_config()
        .with_context(|| format!("querying config for {name}"))?;
    let format = supported.sample_format();
    let mut config: StreamConfig = supported.config();
    if let Some(frames) = buffer {
        config.buffer_size = BufferSize::Fixed(frames);
    }
    let sample_rate = config.sample_rate as f32;
    let engine = make_engine(sample_rate);

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, engine, errors)?,
        SampleFormat::I16 => build::<i16>(&device, config, engine, errors)?,
        SampleFormat::I32 => build::<i32>(&device, config, engine, errors)?,
        SampleFormat::U16 => build::<u16>(&device, config, engine, errors)?,
        other => bail!("unsupported sample format {other:?}"),
    };
    stream.play().context("starting audio stream")?;
    Ok(AudioOut {
        _stream: stream,
        sample_rate,
        device_name: name,
    })
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut engine: Engine,
    errors: Arc<ArrayQueue<String>>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    // Scratch space for non-f32 devices; sized generously up front so the
    // callback normally never allocates.
    let mut scratch: Vec<f32> = vec![0.0; 8192 * channels.max(1)];
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
            if scratch.len() < data.len() {
                scratch.resize(data.len(), 0.0);
            }
            let buf = &mut scratch[..data.len()];
            engine.process(buf, channels);
            for (o, s) in data.iter_mut().zip(buf.iter()) {
                *o = T::from_sample(*s);
            }
        },
        move |err| {
            let _ = errors.push(format!("audio: {err}"));
        },
        None,
    )?;
    Ok(stream)
}
