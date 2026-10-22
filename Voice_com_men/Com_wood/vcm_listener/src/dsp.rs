#![allow(dead_code)]

// Обработка звука без зависимостей от cpal и sherpa-onnx: выбор канала, усиление,
// ресемплинг. Вынесено отдельно, чтобы это можно было проверять тестами.

use crate::audio_config::ChannelMode;
use std::f64::consts::PI;

/// Потоковый ресемплер: оконный sinc-фильтр (низкочастотный срез чуть ниже
/// частоты Найквиста выходного сигнала) с интерполяцией в нужные моменты времени.
/// Для 96000 -> 16000 Гц шаг ровно 6 входных отсчётов на один выходной.
pub struct Resampler {
    step: f64,
    fc: f64,
    half: usize,
    buf: Vec<f32>,
    pos: f64,
    passthrough: bool,
}

impl Resampler {
    /// Число нулевых пересечений sinc по каждую сторону от центра.
    const ZERO_CROSSINGS: f64 = 8.0;
    /// Срез фильтра в долях частоты Найквиста выхода (запас против наложения спектра).
    const CUTOFF_MARGIN: f64 = 0.92;

    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let passthrough = in_rate == out_rate;
        let step = in_rate as f64 / out_rate as f64;
        let fc = (out_rate as f64 / in_rate as f64).min(1.0) * Self::CUTOFF_MARGIN;
        let half = (Self::ZERO_CROSSINGS / fc).ceil() as usize;
        Self {
            step,
            fc,
            half,
            buf: vec![0.0; half],
            pos: half as f64,
            passthrough,
        }
    }

    fn kernel(&self, x: f64) -> f64 {
        let half = self.half as f64;
        if x.abs() >= half {
            return 0.0;
        }
        let sinc = if x == 0.0 {
            1.0
        } else {
            let y = PI * self.fc * x;
            y.sin() / y
        };
        let window = 0.5 * (1.0 + (PI * x / half).cos());
        self.fc * sinc * window
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.passthrough {
            return input.to_vec();
        }
        self.buf.extend_from_slice(input);
        let h = self.half as isize;
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 1);
        loop {
            let center = self.pos;
            let base = center.floor() as isize;
            if (base + h) as usize >= self.buf.len() {
                break;
            }
            let mut acc = 0.0f64;
            let mut weights = 0.0f64;
            for k in (base - h + 1)..=(base + h) {
                let w = self.kernel(k as f64 - center);
                acc += self.buf[k as usize] as f64 * w;
                weights += w;
            }
            out.push(if weights.abs() > 1e-12 { (acc / weights) as f32 } else { 0.0 });
            self.pos += self.step;
        }
        let drop = (self.pos.floor() as isize - h).max(0) as usize;
        if drop > 0 {
            self.buf.drain(..drop);
            self.pos -= drop as f64;
        }
        out
    }
}

/// Сведение многоканального входа в моно: выбор канала, усиление.
pub struct MonoMixer {
    channels: usize,
    rate: u32,
    mode: ChannelMode,
    gain: f32,
    energy: Vec<f64>,
    current: usize,
    primed: bool,
}

impl MonoMixer {
    /// Во сколько раз второй канал должен быть громче, чтобы `Auto` переключился.
    const SWITCH_RATIO: f64 = 2.0;
    /// Постоянная времени сглаживания громкости каналов, секунды.
    const SMOOTHING_S: f64 = 1.5;

    pub fn new(channels: usize, rate: u32, mode: ChannelMode, gain_db: f32) -> Self {
        let channels = channels.max(1);
        Self {
            channels,
            rate,
            mode,
            gain: 10f32.powf(gain_db / 20.0),
            energy: vec![0.0; channels],
            current: 0,
            primed: false,
        }
    }

    /// Какой канал сейчас используется в режиме Auto (для диагностики).
    pub fn current_channel(&self) -> usize {
        self.current
    }

    pub fn mix(&mut self, interleaved: &[f32]) -> Vec<f32> {
        let ch = self.channels;
        let frames = interleaved.len() / ch;
        let mut out = Vec::with_capacity(frames);

        if ch == 1 {
            out.extend(interleaved.iter().map(|s| (s * self.gain).clamp(-1.0, 1.0)));
            return out;
        }

        let fixed: Option<usize> = match self.mode {
            ChannelMode::Left => Some(0),
            ChannelMode::Right => Some(1.min(ch - 1)),
            ChannelMode::Mix => None,
            ChannelMode::Auto => {
                self.update_energy(interleaved, frames);
                Some(self.current)
            }
        };

        for frame in interleaved.chunks_exact(ch) {
            let s = match fixed {
                Some(c) => frame[c],
                None => frame.iter().sum::<f32>() / ch as f32,
            };
            out.push((s * self.gain).clamp(-1.0, 1.0));
        }
        out
    }

    fn update_energy(&mut self, interleaved: &[f32], frames: usize) {
        if frames == 0 {
            return;
        }
        let alpha = if self.primed {
            (frames as f64 / self.rate as f64 / Self::SMOOTHING_S).min(1.0)
        } else {
            1.0
        };
        for c in 0..self.channels {
            let mean_sq = interleaved
                .chunks_exact(self.channels)
                .map(|f| (f[c] as f64) * (f[c] as f64))
                .sum::<f64>()
                / frames as f64;
            self.energy[c] = (1.0 - alpha) * self.energy[c] + alpha * mean_sq;
        }
        self.primed = true;

        let best = (0..self.channels)
            .max_by(|&a, &b| self.energy[a].total_cmp(&self.energy[b]))
            .unwrap_or(0);
        if best != self.current
            && (self.energy[self.current] < 1e-12
                || self.energy[best] > self.energy[self.current] * Self::SWITCH_RATIO)
        {
            self.current = best;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn same_rate_is_passthrough() {
        let mut r = Resampler::new(16_000, 16_000);
        let x = sine(440.0, 16_000, 500, 0.5);
        assert_eq!(r.process(&x), x);
    }

    #[test]
    fn length_ratio_for_96k_to_16k() {
        let mut r = Resampler::new(96_000, 16_000);
        let out = r.process(&sine(300.0, 96_000, 96_000, 0.5));
        // секунда входа даёт около 16000 выходных отсчётов (минус задержка фильтра)
        assert!((15_900..=16_000).contains(&out.len()), "len = {}", out.len());
    }

    #[test]
    fn speech_band_tone_keeps_amplitude() {
        let mut r = Resampler::new(96_000, 16_000);
        let out = r.process(&sine(1_000.0, 96_000, 48_000, 0.5));
        let body = &out[100..out.len() - 100];
        let expected = 0.5 / 2f32.sqrt();
        assert!((rms(body) - expected).abs() < 0.01 * expected, "rms = {}", rms(body));
    }

    #[test]
    fn tone_above_output_nyquist_is_suppressed() {
        let mut r = Resampler::new(96_000, 16_000);
        // 20 кГц без фильтра легла бы в полосу речи как ложный тон
        let out = r.process(&sine(20_000.0, 96_000, 48_000, 0.5));
        let body = &out[100..out.len() - 100];
        assert!(rms(body) < 0.02, "rms = {}", rms(body));
    }

    #[test]
    fn chunked_processing_equals_one_shot() {
        let x = sine(700.0, 96_000, 9_600, 0.4);
        let mut whole = Resampler::new(96_000, 16_000);
        let a = whole.process(&x);
        let mut parts = Resampler::new(96_000, 16_000);
        let mut b = Vec::new();
        for chunk in x.chunks(137) {
            b.extend(parts.process(chunk));
        }
        assert_eq!(a.len(), b.len());
        for (p, q) in a.iter().zip(&b) {
            assert!((p - q).abs() < 1e-5);
        }
    }

    #[test]
    fn other_rates_work_too() {
        let mut r = Resampler::new(44_100, 16_000);
        let out = r.process(&sine(1_000.0, 44_100, 44_100, 0.5));
        assert!((15_800..=16_000).contains(&out.len()), "len = {}", out.len());
        let body = &out[100..out.len() - 100];
        assert!((rms(body) - 0.3536).abs() < 0.01);
    }

    fn stereo(left: &[f32], right: &[f32]) -> Vec<f32> {
        left.iter().zip(right).flat_map(|(l, r)| [*l, *r]).collect()
    }

    #[test]
    fn auto_picks_the_louder_channel() {
        let l = vec![0.0; 480];
        let r = sine(500.0, 48_000, 480, 0.5);
        let mut m = MonoMixer::new(2, 48_000, ChannelMode::Auto, 0.0);
        let out = m.mix(&stereo(&l, &r));
        assert_eq!(m.current_channel(), 1);
        for (o, s) in out.iter().zip(&r) {
            assert!((o - s).abs() < 1e-6);
        }
    }

    #[test]
    fn auto_does_not_flip_on_small_difference() {
        let a = sine(500.0, 48_000, 480, 0.50);
        let b = sine(500.0, 48_000, 480, 0.55);
        let mut m = MonoMixer::new(2, 48_000, ChannelMode::Auto, 0.0);
        m.mix(&stereo(&a, &vec![0.0; 480])); // сначала громче левый
        assert_eq!(m.current_channel(), 0);
        for _ in 0..50 {
            m.mix(&stereo(&a, &b)); // правый чуть громче, но не вдвое по мощности
        }
        assert_eq!(m.current_channel(), 0);
    }

    #[test]
    fn auto_follows_after_real_change() {
        let sig = sine(500.0, 48_000, 4_800, 0.5);
        let silence = vec![0.0; 4_800];
        let mut m = MonoMixer::new(2, 48_000, ChannelMode::Auto, 0.0);
        m.mix(&stereo(&sig, &silence));
        assert_eq!(m.current_channel(), 0);
        for _ in 0..30 {
            m.mix(&stereo(&silence, &sig)); // сигнал «переехал» на правый
        }
        assert_eq!(m.current_channel(), 1);
    }

    #[test]
    fn fixed_modes_and_mix() {
        let l = vec![0.2; 100];
        let r = vec![0.6; 100];
        let data = stereo(&l, &r);
        let left = MonoMixer::new(2, 48_000, ChannelMode::Left, 0.0).mix(&data);
        let right = MonoMixer::new(2, 48_000, ChannelMode::Right, 0.0).mix(&data);
        let mix = MonoMixer::new(2, 48_000, ChannelMode::Mix, 0.0).mix(&data);
        assert!((left[0] - 0.2).abs() < 1e-6);
        assert!((right[0] - 0.6).abs() < 1e-6);
        assert!((mix[0] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn gain_in_db_and_clipping() {
        let mut m = MonoMixer::new(1, 48_000, ChannelMode::Auto, 6.0206);
        let out = m.mix(&[0.1, 0.3, 0.9]);
        assert!((out[0] - 0.2).abs() < 1e-4);
        assert!((out[1] - 0.6).abs() < 1e-4);
        assert_eq!(out[2], 1.0); // не выходим за диапазон
    }

    #[test]
    fn mono_input_is_unchanged_without_gain() {
        let mut m = MonoMixer::new(1, 16_000, ChannelMode::Mix, 0.0);
        assert_eq!(m.mix(&[0.1, -0.2, 0.3]), vec![0.1, -0.2, 0.3]);
    }
}
