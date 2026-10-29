//! The laptop's voice on macOS and Windows: `cpal` on the default output — CoreAudio and WASAPI,
//! which every such machine has, so nothing here is a library the binary might not find.
//!
//! The device's own rate and channel count are used as they are, so nothing converts but the
//! bank, once, when it is rendered (`Bank::render`).

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use super::Mixer;

/// The stream plays for as long as this is held.
pub struct Output(#[allow(dead_code)] cpal::Stream);

pub fn open() -> Result<(Output, u32, Arc<Mutex<Mixer>>), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no audio output on this machine")?;
    let config = device
        .default_output_config()
        .map_err(|e| format!("the audio output has no configuration: {e}"))?;
    let format = config.sample_format();
    let config: cpal::StreamConfig = config.into();
    let rate = config.sample_rate;
    let mixer = Arc::new(Mutex::new(Mixer::new(rate)));
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, mixer.clone()),
        SampleFormat::I16 => build::<i16>(&device, config, mixer.clone()),
        SampleFormat::I32 => build::<i32>(&device, config, mixer.clone()),
        other => return Err(format!("the audio output takes {other} samples")),
    }?;
    stream
        .play()
        .map_err(|e| format!("the audio output will not play: {e}"))?;
    Ok((Output(stream), rate, mixer))
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
) -> Result<cpal::Stream, String> {
    let channels = usize::from(config.channels);
    let mut mono = Vec::new();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                mono.resize(data.len() / channels, 0.0);
                match mixer.lock() {
                    Ok(mut m) => m.fill(&mut mono),
                    Err(_) => mono.fill(0.0),
                }
                for (frame, s) in data.chunks_mut(channels).zip(&mono) {
                    frame.fill(T::from_sample(*s));
                }
            },
            // An xrun or a device change would land on top of `udp-pad`'s screen: the sound
            // carries on or it does not, and the pad is what matters.
            |_| {},
            None,
        )
        .map_err(|e| format!("cannot open the audio output: {e}"))
}
