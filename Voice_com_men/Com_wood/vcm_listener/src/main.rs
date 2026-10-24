mod audio_config;
mod capture;
mod dialog;
mod dsp;
mod engines;
mod history;
mod listen;
mod sql_pool;
mod tts;
mod voting;

use audio_config::{AudioConfig, VadConfig, VotingConfig};
use sql_pool::SqlConfig;
use tts::TtsConfig;
use serde::Deserialize;
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OnlineRecognizer, OnlineRecognizerConfig, Wave,
};
use std::{
    error::Error,
    fs, io,
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Deserialize)]
struct Config {
    models_dir: PathBuf,
    test_wav: PathBuf,
    silero_vad: String,
    gigaam: ModelFiles,
    t_one: ModelFiles,
    vosk: TransducerFiles,
    #[serde(default)]
    audio: AudioConfig,
    #[serde(default)]
    voting: VotingConfig,
    #[serde(default)]
    vad: VadConfig,
    #[serde(default)]
    sql: SqlConfig,
    #[serde(default)]
    tts: TtsConfig,
}

#[derive(Deserialize)]
struct ModelFiles {
    model: String,
    tokens: String,
}

#[derive(Deserialize)]
struct TransducerFiles {
    encoder: String,
    decoder: String,
    joiner: String,
    tokens: String,
}

fn resolve(models_dir: &Path, relative_path: &str) -> String {
    models_dir
        .join(relative_path)
        .to_string_lossy()
        .into_owned()
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("devices") {
        return capture::print_devices();
    }

    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config_text = fs::read_to_string(crate_dir.join("config.toml"))?;
    let config: Config = toml::from_str(&config_text)?;

    if args.first().map(String::as_str) == Some("listen") {
        let paths = engines::EnginePaths {
            gigaam_model: resolve(&config.models_dir, &config.gigaam.model),
            gigaam_tokens: resolve(&config.models_dir, &config.gigaam.tokens),
            t_one_model: resolve(&config.models_dir, &config.t_one.model),
            t_one_tokens: resolve(&config.models_dir, &config.t_one.tokens),
            vosk_encoder: resolve(&config.models_dir, &config.vosk.encoder),
            vosk_decoder: resolve(&config.models_dir, &config.vosk.decoder),
            vosk_joiner: resolve(&config.models_dir, &config.vosk.joiner),
            vosk_tokens: resolve(&config.models_dir, &config.vosk.tokens),
        };
        let vad_model = resolve(&config.models_dir, &config.silero_vad);
        let wav = if args.get(1).map(String::as_str) == Some("--wav") {
            args.get(2).map(PathBuf::from)
        } else {
            None
        };
        return listen::run(
            &paths,
            &vad_model,
            &config.audio,
            &config.vad,
            &config.voting,
            &config.sql,
            &config.tts,
            wav.as_deref(),
        );
    }

    let wav_path = config.models_dir.join(&config.test_wav);
    let wave = Wave::read(&wav_path.to_string_lossy())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cannot read test WAV"))?;
    if wave.sample_rate() != 16_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("test WAV must be 16 kHz, got {} Hz", wave.sample_rate()),
        )
        .into());
    }

    let mut gigaam_config = OfflineRecognizerConfig::default();
    gigaam_config.model_config.nemo_ctc.model =
        Some(resolve(&config.models_dir, &config.gigaam.model));
    gigaam_config.model_config.tokens = Some(resolve(&config.models_dir, &config.gigaam.tokens));
    let gigaam = OfflineRecognizer::create(&gigaam_config).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "failed to create GigaAM recognizer",
        )
    })?;

    let mut t_one_config = OnlineRecognizerConfig::default();
    t_one_config.model_config.t_one_ctc.model =
        Some(resolve(&config.models_dir, &config.t_one.model));
    t_one_config.model_config.tokens = Some(resolve(&config.models_dir, &config.t_one.tokens));
    let t_one = OnlineRecognizer::create(&t_one_config).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "failed to create T-one recognizer",
        )
    })?;

    let mut vosk_config = OnlineRecognizerConfig::default();
    vosk_config.model_config.transducer.encoder =
        Some(resolve(&config.models_dir, &config.vosk.encoder));
    vosk_config.model_config.transducer.decoder =
        Some(resolve(&config.models_dir, &config.vosk.decoder));
    vosk_config.model_config.transducer.joiner =
        Some(resolve(&config.models_dir, &config.vosk.joiner));
    vosk_config.model_config.tokens = Some(resolve(&config.models_dir, &config.vosk.tokens));
    vosk_config.model_config.model_type = Some("zipformer2".to_owned());
    let vosk = OnlineRecognizer::create(&vosk_config).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "failed to create Vosk recognizer",
        )
    })?;

    let gigaam_stream = gigaam.create_stream();
    gigaam_stream.accept_waveform(wave.sample_rate(), wave.samples());
    let started = Instant::now();
    gigaam.decode(&gigaam_stream);
    let gigaam_elapsed = started.elapsed();
    let gigaam_result = gigaam_stream
        .get_result()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "GigaAM returned no result"))?;
    println!(
        "GigaAM v2: {}\nВремя декодирования: {:.3} с",
        gigaam_result.text,
        gigaam_elapsed.as_secs_f64()
    );

    let t_one_stream = t_one.create_stream();
    t_one_stream.accept_waveform(wave.sample_rate(), wave.samples());
    t_one_stream.input_finished();
    let started = Instant::now();
    while t_one.is_ready(&t_one_stream) {
        t_one.decode(&t_one_stream);
    }
    let t_one_elapsed = started.elapsed();
    let t_one_result = t_one
        .get_result(&t_one_stream)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "T-one returned no result"))?;
    println!(
        "T-one: {}\nВремя декодирования: {:.3} с",
        t_one_result.text,
        t_one_elapsed.as_secs_f64()
    );

    let vosk_stream = vosk.create_stream();
    vosk_stream.accept_waveform(wave.sample_rate(), wave.samples());
    vosk_stream.input_finished();
    let started = Instant::now();
    while vosk.is_ready(&vosk_stream) {
        vosk.decode(&vosk_stream);
    }
    let vosk_elapsed = started.elapsed();
    let vosk_result = vosk
        .get_result(&vosk_stream)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Vosk returned no result"))?;
    println!(
        "Vosk small ru: {}\nВремя декодирования: {:.3} с",
        vosk_result.text,
        vosk_elapsed.as_secs_f64()
    );

    let _silero_vad_path = config.models_dir.join(config.silero_vad);
    println!(
        "Пиковая память процесса: {:.2} MiB",
        peak_working_set_bytes()? as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

#[cfg(windows)]
fn peak_working_set_bytes() -> io::Result<usize> {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };

    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { zeroed() };
    counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let success = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(counters.PeakWorkingSetSize)
    }
}

#[cfg(not(windows))]
fn peak_working_set_bytes() -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peak working set is implemented for Windows only",
    ))
}
