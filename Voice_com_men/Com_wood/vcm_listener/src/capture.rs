#![allow(dead_code)]

// Захват звука с устройства записи через cpal 0.18: выбор устройства по подстроке
// имени, выбор канала и усиление, приведение к 16 кГц, передача кусков по каналу.

use std::error::Error;
use std::sync::mpsc::Sender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio_config::AudioConfig;
use crate::dsp::{MonoMixer, Resampler};

/// Что именно открыли (для вывода пользователю).
pub struct CaptureInfo {
    pub device_name: String,
    pub sample_format: String,
    pub device_rate: u32,
    pub channels: usize,
    pub target_rate: u32,
}

/// Пока объект жив, поток записи работает.
pub struct Capture {
    _stream: cpal::Stream,
    pub info: CaptureInfo,
}

/// Печатает все устройства записи с их форматом по умолчанию.
pub fn print_devices() -> Result<(), Box<dyn Error>> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().map(|d| d.to_string());
    println!("Устройства записи:");
    let devices = host
        .input_devices()
        .map_err(|e| format!("не удалось получить список устройств: {e}"))?;
    for device in devices {
        let name = device.to_string();
        let mark = if Some(&name) == default_name.as_ref() {
            " (по умолчанию)"
        } else {
            ""
        };
        match device.default_input_config() {
            Ok(c) => println!(
                "  {name}{mark}: {} Гц, {} кан., {}",
                c.sample_rate(),
                c.channels(),
                c.sample_format()
            ),
            Err(e) => println!("  {name}{mark}: формат недоступен ({e})"),
        }
    }
    Ok(())
}

fn pick_device(host: &cpal::Host, needle: &str) -> Result<cpal::Device, String> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| "в системе нет устройства записи по умолчанию".to_string());
    }
    let devices = host
        .input_devices()
        .map_err(|e| format!("не удалось получить список устройств: {e}"))?;
    let mut seen = Vec::new();
    for device in devices {
        let name = device.to_string();
        if name.to_lowercase().contains(&needle) {
            return Ok(device);
        }
        seen.push(name);
    }
    Err(format!(
        "устройство с «{needle}» в имени не найдено. Доступны: {}",
        if seen.is_empty() { "—".to_string() } else { seen.join("; ") }
    ))
}

fn on_error(err: cpal::Error) {
    eprintln!("Ошибка аудиопотока: {err}");
}

fn make_callback<T: Copy + 'static>(
    convert: fn(T) -> f32,
    mut mixer: MonoMixer,
    mut resampler: Resampler,
    tx: Sender<Vec<f32>>,
) -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static {
    move |data: &[T], _info: &cpal::InputCallbackInfo| {
        let interleaved: Vec<f32> = data.iter().map(|&s| convert(s)).collect();
        let mono = mixer.mix(&interleaved);
        let out = resampler.process(&mono);
        if !out.is_empty() {
            let _ = tx.send(out);
        }
    }
}

/// Открывает устройство и начинает слать в `tx` куски моно-звука с частотой
/// `cfg.target_sample_rate`.
pub fn start(cfg: &AudioConfig, tx: Sender<Vec<f32>>) -> Result<Capture, Box<dyn Error>> {
    let host = cpal::default_host();
    let device = pick_device(&host, &cfg.device)?;
    let device_name = device.to_string();

    let supported = device
        .default_input_config()
        .map_err(|e| format!("не удалось получить формат устройства «{device_name}»: {e}"))?;
    let channels = supported.channels() as usize;
    let device_rate = supported.sample_rate();
    let format = supported.sample_format();
    let config = supported.config();

    let mixer = MonoMixer::new(channels, device_rate, cfg.channel, cfg.gain_db);
    let resampler = Resampler::new(device_rate, cfg.target_sample_rate);

    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            make_callback::<f32>(|x| x, mixer, resampler, tx),
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            make_callback::<i16>(|x| x as f32 / 32_768.0, mixer, resampler, tx),
            on_error,
            None,
        ),
        cpal::SampleFormat::I32 => device.build_input_stream::<i32, _, _>(
            config,
            make_callback::<i32>(|x| x as f32 / 2_147_483_648.0, mixer, resampler, tx),
            on_error,
            None,
        ),
        other => {
            return Err(format!(
                "формат отсчётов {other} устройства «{device_name}» пока не поддержан (поддержаны F32, I16, I32)"
            )
            .into())
        }
    }
    .map_err(|e| format!("не удалось открыть устройство «{device_name}»: {e}"))?;

    stream
        .play()
        .map_err(|e| format!("не удалось запустить запись с «{device_name}»: {e}"))?;

    Ok(Capture {
        _stream: stream,
        info: CaptureInfo {
            device_name,
            sample_format: format.to_string(),
            device_rate,
            channels,
            target_rate: cfg.target_sample_rate,
        },
    })
}
