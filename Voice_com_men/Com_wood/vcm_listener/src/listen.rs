#![allow(dead_code)]

// Режим прослушивания: микрофон (или WAV-файл) -> детектор речи Silero -> фраза
// уходит трём распознавателям -> голосование. Пока без базы данных: результат
// печатается на экран. Ключ сравнения здесь ЧЕРНОВОЙ (draft_norm), настоящий
// нормализатор со стеммером и словарём команд будет позже.

use std::collections::HashMap;
use std::error::Error;
use std::io::Write;
use std::path::Path;
use std::sync::{mpsc, Arc};

use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector, Wave};

use crate::audio_config::{AudioConfig, VadConfig, VotingConfig};
use crate::capture;
use crate::dsp::Resampler;
use crate::engines::{EngineBank, EnginePaths};
use crate::history::History;
use crate::sql_pool::{self, SqlConfig};
use vcm_norm::{speech_form, CommandIndex, Match};
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
    index: CommandIndex,
    reference: HashMap<String, String>,
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


impl Pipeline {
    fn reference_of(&self, cmd_key: &str) -> String {
        self.reference
            .get(cmd_key)
            .cloned()
            .unwrap_or_else(|| format!("команда {cmd_key}"))
    }

    /// Разбор текста одного движка: что он «услышал» как команду.
    fn interpret(&self, text: &str) -> (Option<Interpretation>, String) {
        if text.trim().is_empty() {
            return (None, String::new());
        }
        match self.index.lookup(&speech_form(text)) {
            Match::Exact { cmd_key, number } => {
                let note = match number {
                    Some(n) => format!("→ {} [{}]", self.reference_of(&cmd_key), n),
                    None => format!("→ {}", self.reference_of(&cmd_key)),
                };
                (Some(Interpretation { cmd_key, number }), note)
            }
            Match::NeedNumber { cmd_key } => {
                let note = format!("→ {} (число не услышано)", self.reference_of(&cmd_key));
                (Some(Interpretation { cmd_key, number: None }), note)
            }
            Match::OutOfRange { cmd_key, number, slot } => (
                None,
                format!(
                    "→ {}: число {} вне диапазона {}–{}",
                    self.reference_of(&cmd_key),
                    number,
                    slot.min,
                    slot.max
                ),
            ),
            Match::Ambiguous(keys) => (None, format!("→ неоднозначно ({} команд)", keys.len())),
            Match::None => (None, "→ такой команды нет".to_string()),
        }
    }

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

        let parsed: Vec<(Option<Interpretation>, String)> =
            texts.iter().map(|t| self.interpret(&t.text)).collect();

        println!(
            "\n[#{}] фраза {:.1} с (контекст: {:.2} с до, {:.2} с после)",
            self.counter, seconds, pre_s, post_s
        );
        for (t, (_, note)) in texts.iter().zip(&parsed) {
            println!(
                "  {:<7} {:>5.2} с  {}  {}",
                t.engine,
                t.elapsed.as_secs_f64(),
                if t.text.is_empty() { "—" } else { t.text.as_str() },
                note
            );
        }

        let votes: Vec<EngineVote> = texts
            .iter()
            .zip(&parsed)
            .map(|(t, (interp, _))| EngineVote { engine: t.engine, interpretation: interp.clone() })
            .collect();

        match vote(&votes, self.min_agree) {
            Verdict::Accepted { interpretation, agreed, dissent } => {
                let against = if dissent.is_empty() {
                    String::new()
                } else {
                    format!("; не согласны: {}", dissent.join(", "))
                };
                let command = self.reference_of(&interpretation.cmd_key);
                match (self.index.slot(&interpretation.cmd_key), interpretation.number) {
                    (Some(slot), None) => println!(
                        "  Голосование: команда «{}» ясна ({}/{}), но число {}–{} не услышано — переспросить число",
                        command,
                        agreed.len(),
                        votes.len(),
                        slot.min,
                        slot.max
                    ),
                    (_, number) => println!(
                        "  Голосование: ПРИНЯТО {}/{} — «{}»{}; согласны: {}{}",
                        agreed.len(),
                        votes.len(),
                        command,
                        number.map(|n| format!(", значение {n}")).unwrap_or_default(),
                        agreed.join(", "),
                        against
                    ),
                }
            }
            Verdict::NeedNumber { cmd_key } => {
                println!(
                    "  Голосование: команда «{}» ясна, число расходится — переспросить число",
                    self.reference_of(&cmd_key)
                );
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
            let glitches = capture::take_glitches();
            let note = if glitches > 0 {
                format!(" пропусков звука: {glitches}")
            } else {
                String::new()
            };
            print!("\r  уровень {:>6.1} dBFS |{:<30}|{:<24}", db, "#".repeat(bars), note);
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
    sql: &SqlConfig,
    wav: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    println!("Читаю пул команд из SQL Server ({} / {})...", sql.server, sql.database);
    let pool = sql_pool::load_pool(sql)
        .map_err(|e| format!("не удалось прочитать dbo.CommandPool: {e}"))?;
    println!("Команд: {}, вариантов фраз: {}", pool.reference.len(), pool.index.len());
    if pool.index.is_empty() {
        println!("ВНИМАНИЕ: пул пуст. Запустите основную программу, чтобы она записала пул, и перезапустите слушатель.");
    }

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
        index: pool.index,
        reference: pool.reference,
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
