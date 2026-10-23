#![allow(dead_code)]

// Режим прослушивания: микрофон (или WAV-файл) -> детектор речи Silero -> фраза
// уходит трём распознавателям -> голосование. Пока без базы данных: результат
// печатается на экран. Ключ сравнения здесь ЧЕРНОВОЙ (draft_norm), настоящий
// нормализатор со стеммером и словарём команд будет позже.

use std::error::Error;
use std::io::Write;
use std::path::Path;
use std::sync::{mpsc, Arc};

use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector, Wave};

use crate::audio_config::{AudioConfig, VadConfig, VotingConfig};
use crate::capture;
use crate::draft_norm::draft_key;
use crate::dsp::Resampler;
use crate::engines::{EngineBank, EnginePaths};
use crate::history::History;
use crate::voting::{vote, EngineVote, Interpretation, Verdict};

const SAMPLE_RATE: u32 = 16_000;
/// Тишина перед фразой: у части моделей первый звук «съедается».
const LEAD_PADDING_S: f32 = 0.2;
const TAIL_PADDING_S: f32 = 0.5;

struct Pipeline {
    vad: VoiceActivityDetector,
    bank: EngineBank,
    min_agree: usize,
    counter: usize,
    history: History,
    pre_roll: usize,
    post_roll: usize,
}

fn create_vad(model: &str, cfg: &VadConfig) -> Result<VoiceActivityDetector, Box<dyn Error>> {
    let config = VadModelConfig {
        silero_vad: SileroVadModelConfig {
            model: Some(model.to_owned()),
            threshold: cfg.threshold,
            min_silence_duration: cfg.min_silence_s,
            min_speech_duration: cfg.min_speech_s,
            max_speech_duration: cfg.max_speech_s,
            window_size: 512,
            ..SileroVadModelConfig::default()
        },
        sample_rate: SAMPLE_RATE as i32,
        num_threads: 1,
        provider: Some("cpu".to_owned()),
        debug: false,
        ..VadModelConfig::default()
    };
    VoiceActivityDetector::create(&config, 30.0)
        .ok_or_else(|| "не удалось создать детектор речи (проверьте silero_vad.onnx)".into())
}

fn interpretation_of(text: &str) -> Option<Interpretation> {
    let key = draft_key(text);
    if key.is_empty() {
        None
    } else {
        Some(Interpretation { cmd_key: key, number: None })
    }
}

impl Pipeline {
    fn feed(&mut self, chunk: &[f32]) {
        self.history.push(chunk);
        self.vad.accept_waveform(chunk);
        self.drain_segments();
    }

    fn flush(&mut self) {
        self.vad.flush();
        self.drain_segments();
    }

    fn drain_segments(&mut self) {
        while !self.vad.is_empty() {
            let Some(segment) = self.vad.front() else {
                break;
            };
            self.vad.pop();
            let samples = segment.samples();
            if samples.is_empty() {
                continue;
            }
            self.handle_segment(samples);
        }
    }

    fn handle_segment(&mut self, samples: &[f32]) {
        self.counter += 1;
        let seconds = samples.len() as f32 / SAMPLE_RATE as f32;

        // Реальный звук до и после фразы: детектор речи начинает с опозданием и
        // отдаёт сегмент без начала. Находим сегмент в истории и берём контекст.
        let (core, pre_s, post_s): (Vec<f32>, f32, f32) = match self.history.find(samples) {
            Some(start) => {
                let end = start + samples.len();
                let (from, slice) = self
                    .history
                    .range(start.saturating_sub(self.pre_roll), end + self.post_roll);
                let pre = start - from;
                let post = slice.len().saturating_sub(pre + samples.len());
                (
                    slice.to_vec(),
                    pre as f32 / SAMPLE_RATE as f32,
                    post as f32 / SAMPLE_RATE as f32,
                )
            }
            None => (samples.to_vec(), 0.0, 0.0), // в истории не нашли — как раньше
        };

        let lead = (SAMPLE_RATE as f32 * LEAD_PADDING_S) as usize;
        let tail = (SAMPLE_RATE as f32 * TAIL_PADDING_S) as usize;
        let mut padded = Vec::with_capacity(lead + core.len() + tail);
        padded.extend(std::iter::repeat(0.0f32).take(lead));
        padded.extend_from_slice(&core);
        padded.extend(std::iter::repeat(0.0f32).take(tail));

        let texts = self.bank.recognize(Arc::new(padded));

        println!(
            "\n[#{}] фраза {:.1} с (контекст: {:.2} с до, {:.2} с после)",
            self.counter, seconds, pre_s, post_s
        );
        for t in &texts {
            println!(
                "  {:<7} {:>5.2} с  {}",
                t.engine,
                t.elapsed.as_secs_f64(),
                if t.text.is_empty() { "—" } else { t.text.as_str() }
            );
        }

        let votes: Vec<EngineVote> = texts
            .iter()
            .map(|t| EngineVote { engine: t.engine, interpretation: interpretation_of(&t.text) })
            .collect();

        match vote(&votes, self.min_agree) {
            Verdict::Accepted { interpretation, agreed, dissent } => {
                let against = if dissent.is_empty() {
                    String::new()
                } else {
                    format!("; не согласны: {}", dissent.join(", "))
                };
                println!(
                    "  Голосование: ПРИНЯТО {}/{} — ключ «{}»; согласны: {}{}",
                    agreed.len(),
                    votes.len(),
                    interpretation.cmd_key,
                    agreed.join(", "),
                    against
                );
            }
            Verdict::NeedNumber { cmd_key } => {
                println!("  Голосование: команда «{cmd_key}» ясна, число расходится — переспросить число");
            }
            Verdict::NoAgreement => println!("  Голосование: НЕТ СОГЛАСИЯ — переспросить"),
            Verdict::NothingRecognized => println!("  Голосование: ничего не распознано"),
        }
    }
}

/// Строка-индикатор уровня сигнала раз в секунду (чтобы видеть, что микрофон слышен).
struct Meter {
    sum_sq: f64,
    count: usize,
}

impl Meter {
    fn new() -> Self {
        Self { sum_sq: 0.0, count: 0 }
    }

    fn update(&mut self, chunk: &[f32]) {
        self.sum_sq += chunk.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>();
        self.count += chunk.len();
        if self.count >= SAMPLE_RATE as usize {
            let rms = (self.sum_sq / self.count as f64).sqrt().max(1e-9);
            let db = 20.0 * rms.log10();
            let bars = (((db + 60.0) / 60.0) * 30.0).clamp(0.0, 30.0) as usize;
            print!("\r  уровень {:>6.1} dBFS |{:<30}|", db, "#".repeat(bars));
            let _ = std::io::stdout().flush();
            self.sum_sq = 0.0;
            self.count = 0;
        }
    }
}

pub fn run(
    paths: &EnginePaths,
    vad_model: &str,
    audio: &AudioConfig,
    vad_cfg: &VadConfig,
    voting: &VotingConfig,
    wav: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    println!("Загружаю модели (три распознавателя и детектор речи)...");
    let bank = EngineBank::start(paths)?;
    let vad = create_vad(vad_model, vad_cfg)?;
    let mut pipeline = Pipeline {
        vad,
        bank,
        min_agree: voting.min_agree,
        counter: 0,
        history: History::new(20 * SAMPLE_RATE as usize),
        pre_roll: (vad_cfg.pre_roll_s.max(0.0) * SAMPLE_RATE as f32) as usize,
        post_roll: (vad_cfg.post_roll_s.max(0.0) * SAMPLE_RATE as f32) as usize,
    };

    match wav {
        Some(path) => {
            let wave = Wave::read(&path.to_string_lossy())
                .ok_or("не удалось прочитать WAV-файл")?;
            let rate = wave.sample_rate() as u32;
            let mut samples = wave.samples().to_vec();
            if rate != SAMPLE_RATE {
                samples = Resampler::new(rate, SAMPLE_RATE).process(&samples);
            }
            println!("Файл: {} ({} Гц), {:.1} с", path.display(), rate, samples.len() as f32 / SAMPLE_RATE as f32);
            for chunk in samples.chunks(1_600) {
                pipeline.feed(chunk);
            }
            pipeline.feed(&vec![0.0f32; SAMPLE_RATE as usize]); // секунда тишины закрывает последнюю фразу
            pipeline.flush();
            Ok(())
        }
        None => {
            let (tx, rx) = mpsc::channel::<Vec<f32>>();
            let capture = capture::start(audio, tx)?;
            let i = &capture.info;
            println!(
                "Устройство: {} — {} Гц, {} кан., {}; в распознавание идёт {} Гц моно.",
                i.device_name, i.device_rate, i.channels, i.sample_format, i.target_rate
            );
            println!("Говорите. Выход — Ctrl+C.");
            let mut meter = Meter::new();
            for chunk in rx {
                meter.update(&chunk);
                pipeline.feed(&chunk);
            }
            Ok(())
        }
    }
}
