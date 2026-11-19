//! Microphone capture with cpal (its PipeWire backend when PipeWire runs, ALSA otherwise).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, SampleFormat, SizedSample};
use zora_plugin::clock::now_ns;
use zora_plugin::mic::{AudioInput, AudioSink};

#[derive(Default)]
pub struct CpalInput {
    stream: Option<cpal::Stream>,
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    sink: AudioSink,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let (channels, rate) = (config.channels, config.sample_rate);
    let errs = sink.clone();
    let mut buf: Vec<f32> = Vec::new();
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            buf.clear();
            buf.extend(
                data.iter()
                    .map(|s| <f32 as cpal::FromSample<T>>::from_sample_(*s)),
            );
            sink.push(&buf, channels, rate, now_ns());
        },
        move |err: cpal::Error| match err.kind() {
            ErrorKind::Xrun => errs.mark_lost(),
            _ => {
                eprintln!("zora-listen: audio stream error: {err}");
                errs.fail("microphone stream error");
            }
        },
        None,
    )
}

impl AudioInput for CpalInput {
    fn open(&mut self, sink: AudioSink) -> Result<(), String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("no default input device")?;
        let supported = device.default_input_config().map_err(|e| e.to_string())?;
        let config: cpal::StreamConfig = supported.config();
        let stream = match supported.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, config, sink),
            SampleFormat::I16 => build::<i16>(&device, config, sink),
            SampleFormat::I32 => build::<i32>(&device, config, sink),
            SampleFormat::U16 => build::<u16>(&device, config, sink),
            SampleFormat::I8 => build::<i8>(&device, config, sink),
            f => return Err(format!("unsupported sample format {f}")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        eprintln!(
            "zora-listen: microphone open ({:?} host, {} Hz, {} ch)",
            host.id(),
            supported.sample_rate(),
            supported.channels()
        );
        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) {
        if let Some(s) = self.stream.take() {
            let _ = s.pause();
            drop(s);
            eprintln!("zora-listen: microphone closed");
        }
    }
}
