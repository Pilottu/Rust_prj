#![allow(dead_code)]

// Воспроизведение готового звука на выбранное устройство через cpal 0.18.
// Нужен нейросетевому голосу: он выдаёт отсчёты, а проигрывает их эта программа сама,
// поэтому колонки или наушники выбираются по имени, как и микрофон.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::dsp::Resampler;

/// Тишина перед звуком: у части устройств первые миллисекунды «съедаются» при запуске потока.
const LEAD_S: f32 = 0.15;
/// Тишина после звука: для сброса хвоста фильтра ресемплера.
const TAIL_S: f32 = 0.05;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDeviceInfo {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub is_default: bool,
}

pub fn list_output_devices() -> Result<Vec<OutputDeviceInfo>, String> {
    let host = cpal::default_host();
    let default_name = host.default_output_device().map(|d| d.to_string());
    let devices = host
        .output_devices()
        .map_err(|e| format!("не удалось получить список устройств воспроизведения: {e}"))?;
    let mut out = Vec::new();
    for device in devices {
        let name = device.to_string();
        let (rate, channels) = match device.default_output_config() {
            Ok(c) => (c.sample_rate(), c.channels()),
            Err(_) => (0, 0),
        };
        out.push(OutputDeviceInfo { is_default: Some(&name) == default_name.as_ref(), name, rate, channels });
    }
    Ok(out)
}

fn pick_output(host: &cpal::Host, needle: &str) -> Result<cpal::Device, String> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return host
            .default_output_device()
            .ok_or_else(|| "в системе нет устройства воспроизведения по умолчанию".to_string());
    }
    let devices = host
        .output_devices()
        .map_err(|e| format!("не удалось получить список устройств воспроизведения: {e}"))?;
    let mut seen = Vec::new();
    for device in devices {
        let name = device.to_string();
        if name.to_lowercase().contains(&needle) {
            return Ok(device);
        }
        seen.push(name);
    }
    Err(format!(
        "устройство воспроизведения с «{needle}» в имени не найдено. Доступны: {}",
        if seen.is_empty() { "—".to_string() } else { seen.join("; ") }
    ))
}

/// Готовит звук к проигрыванию: тишина в начале, перевод в частоту устройства, громкость, тишина в конце.
pub fn prepare(samples: &[f32], src_rate: u32, dev_rate: u32, volume: f32) -> Vec<f32> {
    let lead = (src_rate as f32 * LEAD_S) as usize;
    let tail = (src_rate as f32 * TAIL_S) as usize;
    let mut input = Vec::with_capacity(lead + samples.len() + tail);
    input.extend(std::iter::repeat(0.0f32).take(lead));
    input.extend_from_slice(samples);
    input.extend(std::iter::repeat(0.0f32).take(tail));
    let mut out = if src_rate == dev_rate {
        input
    } else {
        Resampler::new(src_rate, dev_rate).process(&input)
    };
    for s in &mut out {
        *s = (*s * volume).clamp(-1.0, 1.0);
    }
    out
}

/// Заполняет буфер устройства кадрами из моно-звука (один и тот же звук во все каналы).
/// Когда звук кончился — тишина. Возвращает true, если звук проигран до конца.
pub fn fill_frames<T: Copy>(out: &mut [T], channels: usize, mono: &[f32], pos: &mut usize, convert: fn(f32) -> T, silence: T) -> bool {
    for frame in out.chunks_mut(channels.max(1)) {
        let sample = mono.get(*pos).copied();
        *pos += 1;
        let value = sample.map(convert).unwrap_or(silence);
        for slot in frame.iter_mut() {
            *slot = value;
        }
    }
    *pos >= mono.len()
}

/// Проигрывает моно-звук на устройство, имя которого содержит `device` (пусто — по умолчанию),
/// и возвращается, когда звук закончился.
pub fn play_blocking(device: &str, samples: &[f32], src_rate: u32, volume: f32) -> Result<(), String> {
    let host = cpal::default_host();
    let dev = pick_output(&host, device)?;
    let name = dev.to_string();
    let supported = dev
        .default_output_config()
        .map_err(|e| format!("не удалось получить формат устройства воспроизведения «{name}»: {e}"))?;
    let channels = supported.channels() as usize;
    let rate = supported.sample_rate();
    let format = supported.sample_format();
    let config = supported.config();

    let mono = Arc::new(prepare(samples, src_rate, rate, volume));
    let pos = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));

    fn on_error(err: cpal::Error) {
        eprintln!("Ошибка воспроизведения: {err}");
    }

    // Один обработчик на формат: общая логика в fill_frames.
    macro_rules! stream_for {
        ($t:ty, $convert:expr, $silence:expr) => {{
            let (mono, pos, finished) = (Arc::clone(&mono), Arc::clone(&pos), Arc::clone(&finished));
            dev.build_output_stream::<$t, _, _>(
                config,
                move |data: &mut [$t], _info: &cpal::OutputCallbackInfo| {
                    let mut p = pos.load(Ordering::Relaxed);
                    let done = fill_frames(data, channels, &mono, &mut p, $convert, $silence);
                    pos.store(p, Ordering::Relaxed);
                    if done {
                        finished.store(true, Ordering::Relaxed);
                    }
                },
                on_error,
                None,
            )
        }};
    }

    let stream = match format {
        cpal::SampleFormat::F32 => stream_for!(f32, |x| x, 0.0f32),
        cpal::SampleFormat::I16 => stream_for!(i16, |x| (x * 32_767.0) as i16, 0i16),
        cpal::SampleFormat::I32 => stream_for!(i32, |x| (x * 2_147_483_647.0) as i32, 0i32),
        other => {
            return Err(format!(
                "формат отсчётов {other} устройства «{name}» для воспроизведения пока не поддержан (поддержаны F32, I16, I32)"
            ))
        }
    }
    .map_err(|e| format!("не удалось открыть устройство воспроизведения «{name}»: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("не удалось начать воспроизведение на «{name}»: {e}"))?;

    let limit = Instant::now() + Duration::from_secs_f32(mono.len() as f32 / rate as f32 + 3.0);
    while !finished.load(Ordering::Relaxed) && Instant::now() < limit {
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(150)); // дать устройству доиграть свой буфер
    drop(stream);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_adds_silence_resamples_and_applies_volume() {
        let tone: Vec<f32> = (0..22_050).map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 22_050.0).sin()).collect();
        let out = prepare(&tone, 22_050, 48_000, 1.0);
        // 1 с звука + 0,15 с + 0,05 с тишины при 48 кГц ≈ 57 600 отсчётов (минус задержка фильтра)
        assert!((57_000..=57_700).contains(&out.len()), "len = {}", out.len());
        // начало — тишина
        assert!(out[..5_000].iter().all(|s| s.abs() < 1e-3));
        // громкость 50% вдвое тише
        let half = prepare(&tone, 22_050, 48_000, 0.5);
        let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak(&half) - peak(&out) / 2.0).abs() < 0.01);
    }

    #[test]
    fn prepare_same_rate_keeps_samples() {
        let out = prepare(&[0.1, 0.2, 0.3], 16_000, 16_000, 1.0);
        assert_eq!(out.len(), 2_400 + 3 + 800);
        assert_eq!(&out[2_400..2_403], &[0.1, 0.2, 0.3]);
    }

    #[test]
    fn volume_is_clamped_to_full_scale() {
        let out = prepare(&[0.9; 10], 16_000, 16_000, 3.0);
        assert!(out.iter().all(|s| s.abs() <= 1.0));
    }

    #[test]
    fn fill_frames_duplicates_to_all_channels_and_ends_with_silence() {
        let mono = [0.5f32, -0.5, 0.25];
        let mut out = [9.0f32; 10]; // 5 кадров по 2 канала
        let mut pos = 0usize;
        let done = fill_frames(&mut out, 2, &mono, &mut pos, |x| x, 0.0);
        assert!(done);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5, 0.25, 0.25, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(pos, 5);
    }

    #[test]
    fn fill_frames_continues_across_buffers() {
        let mono: Vec<f32> = (0..6).map(|i| i as f32 / 10.0).collect();
        let mut pos = 0usize;
        let mut a = [0i16; 4];
        let done_a = fill_frames(&mut a, 1, &mono, &mut pos, |x| (x * 10.0) as i16, 0);
        assert!(!done_a);
        assert_eq!(a, [0, 1, 2, 3]);
        let mut b = [0i16; 4];
        let done_b = fill_frames(&mut b, 1, &mono, &mut pos, |x| (x * 10.0) as i16, 0);
        assert!(done_b);
        assert_eq!(b, [4, 5, 0, 0]);
    }
}
