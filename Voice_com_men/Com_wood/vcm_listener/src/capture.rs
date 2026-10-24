#![allow(dead_code)]

// Захват звука с устройства записи через cpal 0.18: выбор устройства по подстроке
// имени, выбор канала и усиление, приведение к 16 кГц, передача кусков по каналу.

use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

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

/// Устройство записи для списка в окне настроек.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceInfo {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub format: String,
    pub is_default: bool,
}

pub fn list_input_devices() -> Result<Vec<InputDeviceInfo>, String> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().map(|d| d.to_string());
    let devices = host
        .input_devices()
        .map_err(|e| format!("не удалось получить список устройств: {e}"))?;
    let mut out = Vec::new();
    for device in devices {
        let name = device.to_string();
        let (rate, channels, format) = match device.default_input_config() {
            Ok(c) => (c.sample_rate(), c.channels(), c.sample_format().to_string()),
            Err(_) => (0, 0, "?".to_string()),
        };
        out.push(InputDeviceInfo {
            is_default: Some(&name) == default_name.as_ref(),
            name,
            rate,
            channels,
            format,
        });
    }
    Ok(out)
}

/// Результат короткого замера уровня микрофона.
#[derive(Debug, Clone, PartialEq)]
pub struct MicReport {
    pub rms_db: f64,
    pub peak_db: f64,
    pub seconds: f32,
    pub glitches: u64,
}

fn dbfs(x: f64) -> f64 {
    20.0 * x.max(1e-9).log10()
}

/// Считает уровень по готовым отсчётам (после выбора канала, усиления и 16 кГц).
pub fn level_of(samples: &[f32]) -> (f64, f64) {
    if samples.is_empty() {
        return (-120.0, -120.0);
    }
    let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let peak = samples.iter().fold(0.0f64, |m, s| m.max((*s as f64).abs()));
    (dbfs((sum_sq / samples.len() as f64).sqrt()), dbfs(peak))
}

/// Открывает устройство на `seconds` секунд и измеряет уровень сигнала.
pub fn measure(cfg: &AudioConfig, seconds: f32) -> Result<MicReport, String> {
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let _capture = start(cfg, tx).map_err(|e| e.to_string())?;
    let _ = take_glitches();
    let deadline = Instant::now() + Duration::from_secs_f32(seconds.max(0.5));
    let mut all: Vec<f32> = Vec::new();
    while Instant::now() < deadline {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(200)) {
            all.extend(chunk);
        }
    }
    if all.is_empty() {
        return Err("устройство открылось, но звука нет (проверьте, что оно не занято другой программой)".to_string());
    }
    let (rms_db, peak_db) = level_of(&all);
    Ok(MicReport { rms_db, peak_db, seconds, glitches: take_glitches() })
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

static GLITCHES: AtomicU64 = AtomicU64::new(0);
static FIRST_ERROR_SHOWN: AtomicBool = AtomicBool::new(false);

/// Ошибки потока (чаще всего «buffer underrun/overrun» — потеря кусочка звука)
/// не печатаем каждую отдельно: считаем, а счётчик показывает индикатор уровня.
fn on_error(err: cpal::Error) {
    GLITCHES.fetch_add(1, Ordering::Relaxed);
    if !FIRST_ERROR_SHOWN.swap(true, Ordering::Relaxed) {
        eprintln!("\nОшибка аудиопотока (дальше только счётчик): {err}");
    }
}

/// Сколько ошибок потока было с прошлого вызова.
pub fn take_glitches() -> u64 {
    GLITCHES.swap(0, Ordering::Relaxed)
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
    let mut config = supported.config();
    if cfg.buffer_frames > 0 {
        config.buffer_size = cpal::BufferSize::Fixed(cfg.buffer_frames);
    }

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
    .map_err(|e| {
        format!(
            "не удалось открыть устройство «{device_name}»: {e} (если задан buffer_frames, попробуйте 0)"
        )
    })?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_of_silence_and_full_scale() {
        assert_eq!(level_of(&[]), (-120.0, -120.0));
        let (rms, peak) = level_of(&[1.0, -1.0, 1.0, -1.0]);
        assert!(rms.abs() < 1e-6 && peak.abs() < 1e-6); // 0 dBFS
        let (rms, peak) = level_of(&[0.1, -0.1, 0.1, -0.1]);
        assert!((rms + 20.0).abs() < 1e-6 && (peak + 20.0).abs() < 1e-6);
    }
}
