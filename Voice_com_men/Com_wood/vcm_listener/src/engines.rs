#![allow(dead_code)]

// Три распознавателя, каждый в своём потоке. Модели создаются ВНУТРИ потока, поэтому
// от типов sherpa-onnx не требуется ни Send, ни Sync. Фраза уходит всем трём сразу,
// результаты собираются и возвращаются в порядке GigaAM, T-one, Vosk.

use std::error::Error;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OnlineRecognizer, OnlineRecognizerConfig};

const SAMPLE_RATE: i32 = 16_000;
/// Тишина в конце для потоковых моделей: чтобы декодер дообработал хвост фразы.
const TAIL_PADDING_S: f32 = 0.5;

pub struct EnginePaths {
    pub gigaam_model: String,
    pub gigaam_tokens: String,
    pub t_one_model: String,
    pub t_one_tokens: String,
    pub vosk_encoder: String,
    pub vosk_decoder: String,
    pub vosk_joiner: String,
    pub vosk_tokens: String,
}

#[derive(Debug, Clone)]
pub struct EngineText {
    pub index: usize,
    pub engine: &'static str,
    pub text: String,
    pub elapsed: Duration,
}

pub const ENGINE_NAMES: [&str; 3] = ["GigaAM", "T-one", "Vosk"];

type Job = Arc<Vec<f32>>;

pub struct EngineBank {
    jobs: Vec<Sender<Job>>,
    results: Receiver<EngineText>,
    _threads: Vec<JoinHandle<()>>,
}

impl EngineBank {
    pub fn start(paths: &EnginePaths) -> Result<Self, Box<dyn Error>> {
        let (result_tx, results) = mpsc::channel::<EngineText>();
        let mut jobs = Vec::new();
        let mut threads = Vec::new();

        for index in 0..ENGINE_NAMES.len() {
            let (job_tx, job_rx) = mpsc::channel::<Job>();
            let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
            let result_tx = result_tx.clone();
            let paths = EnginePaths {
                gigaam_model: paths.gigaam_model.clone(),
                gigaam_tokens: paths.gigaam_tokens.clone(),
                t_one_model: paths.t_one_model.clone(),
                t_one_tokens: paths.t_one_tokens.clone(),
                vosk_encoder: paths.vosk_encoder.clone(),
                vosk_decoder: paths.vosk_decoder.clone(),
                vosk_joiner: paths.vosk_joiner.clone(),
                vosk_tokens: paths.vosk_tokens.clone(),
            };
            let handle = thread::spawn(move || {
                let decode: Box<dyn Fn(&[f32]) -> String> = match build(index, &paths) {
                    Ok(f) => {
                        let _ = ready_tx.send(Ok(()));
                        f
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                while let Ok(samples) = job_rx.recv() {
                    let started = Instant::now();
                    let text = decode(samples.as_slice());
                    let _ = result_tx.send(EngineText {
                        index,
                        engine: ENGINE_NAMES[index],
                        text: text.trim().to_string(),
                        elapsed: started.elapsed(),
                    });
                }
            });
            match ready_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(format!("{}: {e}", ENGINE_NAMES[index]).into()),
                Err(_) => {
                    return Err(format!("{}: поток распознавателя завершился при запуске", ENGINE_NAMES[index]).into())
                }
            }
            jobs.push(job_tx);
            threads.push(handle);
        }
        Ok(Self { jobs, results, _threads: threads })
    }

    /// Отдаёт фразу всем трём распознавателям и ждёт все ответы.
    pub fn recognize(&self, samples: Arc<Vec<f32>>) -> Vec<EngineText> {
        let mut sent = 0;
        for tx in &self.jobs {
            if tx.send(Arc::clone(&samples)).is_ok() {
                sent += 1;
            }
        }
        let mut out: Vec<EngineText> = Vec::with_capacity(sent);
        for _ in 0..sent {
            match self.results.recv() {
                Ok(r) => out.push(r),
                Err(_) => break,
            }
        }
        out.sort_by_key(|r| r.index);
        out
    }
}

fn tail_padding() -> Vec<f32> {
    vec![0.0; (SAMPLE_RATE as f32 * TAIL_PADDING_S) as usize]
}

/// Создаёт распознаватель с номером `index` и возвращает функцию «фраза -> текст».
fn build(index: usize, p: &EnginePaths) -> Result<Box<dyn Fn(&[f32]) -> String>, String> {
    match index {
        0 => {
            let mut config = OfflineRecognizerConfig::default();
            config.model_config.nemo_ctc.model = Some(p.gigaam_model.clone());
            config.model_config.tokens = Some(p.gigaam_tokens.clone());
            let recognizer = OfflineRecognizer::create(&config)
                .ok_or_else(|| "не удалось создать распознаватель".to_string())?;
            Ok(Box::new(move |samples: &[f32]| {
                let stream = recognizer.create_stream();
                stream.accept_waveform(SAMPLE_RATE, samples);
                recognizer.decode(&stream);
                stream.get_result().map(|r| r.text).unwrap_or_default()
            }))
        }
        1 => {
            let mut config = OnlineRecognizerConfig::default();
            config.model_config.t_one_ctc.model = Some(p.t_one_model.clone());
            config.model_config.tokens = Some(p.t_one_tokens.clone());
            let recognizer = OnlineRecognizer::create(&config)
                .ok_or_else(|| "не удалось создать распознаватель".to_string())?;
            Ok(Box::new(move |samples: &[f32]| {
                let stream = recognizer.create_stream();
                stream.accept_waveform(SAMPLE_RATE, samples);
                stream.accept_waveform(SAMPLE_RATE, &tail_padding());
                stream.input_finished();
                while recognizer.is_ready(&stream) {
                    recognizer.decode(&stream);
                }
                recognizer.get_result(&stream).map(|r| r.text).unwrap_or_default()
            }))
        }
        _ => {
            let mut config = OnlineRecognizerConfig::default();
            config.model_config.transducer.encoder = Some(p.vosk_encoder.clone());
            config.model_config.transducer.decoder = Some(p.vosk_decoder.clone());
            config.model_config.transducer.joiner = Some(p.vosk_joiner.clone());
            config.model_config.tokens = Some(p.vosk_tokens.clone());
            config.model_config.model_type = Some("zipformer2".to_owned());
            let recognizer = OnlineRecognizer::create(&config)
                .ok_or_else(|| "не удалось создать распознаватель".to_string())?;
            Ok(Box::new(move |samples: &[f32]| {
                let stream = recognizer.create_stream();
                stream.accept_waveform(SAMPLE_RATE, samples);
                stream.accept_waveform(SAMPLE_RATE, &tail_padding());
                stream.input_finished();
                while recognizer.is_ready(&stream) {
                    recognizer.decode(&stream);
                }
                recognizer.get_result(&stream).map(|r| r.text).unwrap_or_default()
            }))
        }
    }
}
