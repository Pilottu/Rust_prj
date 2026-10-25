#![allow(dead_code)]

// Голосовая обратная связь. Синтезатор — встроенный в Windows (System.Speech) через
// один постоянно работающий дочерний процесс PowerShell: он один раз загружает голос и
// потом читает вслух строки со стандартного ввода. Так не тратится время на запуск
// PowerShell перед каждой фразой, и в программе нет COM-кода.
//
// Протокол: программа пишет строку с текстом, PowerShell произносит её и отвечает «done».
// Это же позволяет знать, когда речь закончилась (нужно, чтобы не слушать самих себя).

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use sherpa_onnx::{GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsModelConfig, OfflineTtsVitsModelConfig};

use crate::playback;

/// Секция [tts] файла config.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct TtsConfig {
    pub enabled: bool,
    /// "auto" — нейросетевой голос, если модель найдена, иначе голос Windows;
    /// "neural" — только нейросетевой; "windows" — только встроенный голос Windows.
    pub engine: String,
    /// Папка модели нейросетевого голоса (Piper): в ней .onnx, tokens.txt и espeak-ng-data.
    /// Относительный путь отсчитывается от папки с моделями.
    pub model_dir: String,
    /// Номер голоса в модели (у русских моделей Piper он один — 0).
    pub speaker_id: u32,
    /// Скорость нейросетевого голоса: 1.0 — обычная, больше — быстрее.
    pub speed: f32,
    /// Говорить «Такой команды нет.» (в шумной комнате лучше выключить).
    pub say_not_a_command: bool,
    /// Говорить «Не понял. Повторите, пожалуйста.».
    pub say_unclear: bool,
    /// Часть имени голоса. Пусто или не найдено — первый русский голос.
    pub voice: String,
    /// Часть имени устройства воспроизведения (колонки, наушники). Пусто — по умолчанию в Windows.
    pub output_device: String,
    /// Скорость речи, от -10 до 10.
    pub rate: i32,
    /// Громкость, 0..100.
    pub volume: u32,
    /// Говорить «Принято.» после каждой выполненной команды.
    pub confirm: bool,
    /// Фраза при запуске (пусто — молчать). Помогает проверить колонки и голос.
    pub greeting: String,
    /// Сколько секунд ждать ответ-число после вопроса «какое значение?».
    pub answer_window_s: f32,
    /// Сколько секунд после окончания речи не слушать микрофон (эхо колонок).
    pub echo_tail_s: f32,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: "auto".to_string(),
            model_dir: "vits-piper-ru_RU-irina-medium".to_string(),
            speaker_id: 0,
            speed: 1.0,
            say_not_a_command: false,
            say_unclear: true,
            voice: "Irina".to_string(),
            output_device: String::new(),
            rate: 0,
            volume: 100,
            confirm: false,
            greeting: "Готов.".to_string(),
            answer_window_s: 8.0,
            echo_tail_s: 0.4,
        }
    }
}

enum Backend {
    Disabled,
    Process {
        child: Child,
        stdin: ChildStdin,
        replies: Receiver<String>,
    },
    Neural(Neural),
}

/// Нейросетевой голос: sherpa-onnx синтезирует, cpal проигрывает.
struct Neural {
    tts: OfflineTts,
    /// Уже озвученные фразы (подсказки повторяются, второй раз они звучат мгновенно).
    cache: HashMap<String, (Vec<f32>, u32)>,
    output: String,
    volume: f32,
    sid: i32,
    speed: f32,
}

impl Neural {
    fn say(&mut self, text: &str) -> Result<(), String> {
        let key = text.trim().to_string();
        if key.is_empty() {
            return Ok(());
        }
        if !self.cache.contains_key(&key) {
            let gen = GenerationConfig { sid: self.sid, speed: self.speed, ..Default::default() };
            let audio = self
                .tts
                .generate_with_config(&key, &gen, None::<fn(&[f32], f32) -> bool>)
                .ok_or_else(|| "синтезатор не смог озвучить фразу".to_string())?;
            let samples = audio.samples().to_vec();
            let rate = audio.sample_rate() as u32;
            if self.cache.len() >= 64 {
                self.cache.clear();
            }
            self.cache.insert(key.clone(), (samples, rate));
        }
        let (samples, rate) = &self.cache[&key];
        playback::play_blocking(&self.output, samples, *rate, self.volume)
    }
}

/// Файлы нейросетевой модели голоса (формат Piper в сборке sherpa-onnx).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeuralFiles {
    pub model: PathBuf,
    pub tokens: PathBuf,
    pub data_dir: PathBuf,
}

/// Ищет в папке модель: первый .onnx, tokens.txt и папку espeak-ng-data.
pub fn find_model(dir: &Path) -> Option<NeuralFiles> {
    let tokens = dir.join("tokens.txt");
    let data_dir = dir.join("espeak-ng-data");
    if !tokens.is_file() || !data_dir.is_dir() {
        return None;
    }
    let mut onnx: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().map(|e| e == "onnx").unwrap_or(false))
        .collect();
    onnx.sort();
    Some(NeuralFiles { model: onnx.into_iter().next()?, tokens, data_dir })
}

/// Имена подпапок папки с моделями, в которых лежит голосовая модель.
pub fn list_models(models_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(models_dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir() && find_model(&e.path()).is_some())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Neural,
    Windows,
}

/// Какой голос использовать по настройке и наличию модели.
pub fn choose_engine(setting: &str, model_found: bool) -> Engine {
    match setting.trim().to_lowercase().as_str() {
        "windows" => Engine::Windows,
        "neural" => Engine::Neural,
        _ => {
            if model_found {
                Engine::Neural
            } else {
                Engine::Windows
            }
        }
    }
}

pub struct Speaker {
    backend: Backend,
    voice_name: String,
}

/// Ожидание ответа от синтезатора на одну фразу.
const SPEAK_TIMEOUT: Duration = Duration::from_secs(40);
const START_TIMEOUT: Duration = Duration::from_secs(20);

impl Speaker {
    pub fn disabled() -> Self {
        Self { backend: Backend::Disabled, voice_name: String::new() }
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self.backend, Backend::Disabled)
    }

    /// Имя выбранного голоса (для вывода пользователю).
    pub fn voice_name(&self) -> &str {
        &self.voice_name
    }

    /// Запускает голос по настройкам. При `enabled = false` возвращает «немого» диктора.
    pub fn start(cfg: &TtsConfig) -> Result<Self, String> {
        if !cfg.enabled {
            return Ok(Self::disabled());
        }
        let found = find_model(Path::new(&cfg.model_dir));
        match choose_engine(&cfg.engine, found.is_some()) {
            Engine::Neural => Self::start_neural(cfg, found),
            Engine::Windows => Self::start_windows(cfg).map_err(|e| {
                if cfg.engine.trim().eq_ignore_ascii_case("windows") {
                    e
                } else {
                    format!("{e}; нейросетевая модель голоса тоже не найдена в «{}»", cfg.model_dir)
                }
            }),
        }
    }

    fn start_neural(cfg: &TtsConfig, found: Option<NeuralFiles>) -> Result<Self, String> {
        let files = found.ok_or_else(|| {
            format!(
                "не найдена модель голоса в «{}» (нужны файл .onnx, tokens.txt и папка espeak-ng-data)",
                cfg.model_dir
            )
        })?;
        let config = OfflineTtsConfig {
            model: OfflineTtsModelConfig {
                vits: OfflineTtsVitsModelConfig {
                    model: Some(files.model.to_string_lossy().into_owned()),
                    tokens: Some(files.tokens.to_string_lossy().into_owned()),
                    data_dir: Some(files.data_dir.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                num_threads: 2,
                debug: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let tts = OfflineTts::create(&config)
            .ok_or_else(|| format!("не удалось загрузить модель голоса из «{}»", cfg.model_dir))?;
        let name = Path::new(&cfg.model_dir)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cfg.model_dir.clone());
        Ok(Self {
            backend: Backend::Neural(Neural {
                tts,
                cache: HashMap::new(),
                output: cfg.output_device.trim().to_string(),
                volume: cfg.volume.min(100) as f32 / 100.0,
                sid: cfg.speaker_id as i32,
                speed: cfg.speed.clamp(0.3, 3.0),
            }),
            voice_name: name,
        })
    }

    fn start_windows(cfg: &TtsConfig) -> Result<Self, String> {
        let script = if cfg.output_device.trim().is_empty() {
            powershell_script(cfg)
        } else {
            powershell_script_sapi(cfg)
        };
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encode_command(&script),
        ]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        Self::with_command(cmd)
    }

    /// Запускает произвольную команду, говорящую на том же протоколе (нужна и для тестов).
    pub fn with_command(mut cmd: Command) -> Result<Self, String> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("не удалось запустить синтезатор речи: {e}"))?;
        let stdin = child.stdin.take().ok_or("нет входного потока синтезатора")?;
        let stdout = child.stdout.take().ok_or("нет выходного потока синтезатора")?;

        // Читаем ответы в отдельном потоке, чтобы ждать их с тайм-аутом.
        let (tx, replies) = mpsc::channel::<String>();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l.trim_start_matches('\u{feff}').trim().to_string()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let first = replies
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| "синтезатор речи не ответил при запуске".to_string())?;
        let voice_name = if let Some(name) = first.strip_prefix("ready:") {
            name.to_string()
        } else if let Some(err) = first.strip_prefix("error:") {
            let _ = child.kill();
            return Err(format!("синтезатор речи: {err}"));
        } else {
            let _ = child.kill();
            return Err(format!("синтезатор речи ответил неожиданно: {first}"));
        };
        Ok(Self { backend: Backend::Process { child, stdin, replies }, voice_name })
    }

    /// Произносит фразу и возвращается, когда она закончилась.
    pub fn say(&mut self, text: &str) -> Result<(), String> {
        let (stdin, replies) = match &mut self.backend {
            Backend::Disabled => return Ok(()),
            Backend::Neural(n) => return n.say(text),
            Backend::Process { stdin, replies, .. } => (stdin, replies),
        };
        let line: String = text.chars().map(|c| if c == '\r' || c == '\n' { ' ' } else { c }).collect();
        writeln!(stdin, "{line}")
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("не удалось отправить текст синтезатору: {e}"))?;
        match replies.recv_timeout(SPEAK_TIMEOUT) {
            Ok(r) if r == "done" => Ok(()),
            Ok(r) => Err(format!("синтезатор ответил: {r}")),
            Err(_) => Err("синтезатор не закончил фразу вовремя".to_string()),
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        if let Backend::Process { child, .. } = &mut self.backend {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Текст PowerShell-скрипта: выбирает голос и в цикле читает строки вслух.
pub fn powershell_script(cfg: &TtsConfig) -> String {
    let voice = cfg.voice.replace('\'', "''");
    let rate = cfg.rate.clamp(-10, 10);
    let volume = cfg.volume.min(100);
    format!(
        r#"$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
try {{
  Add-Type -AssemblyName System.Speech
  $s = New-Object System.Speech.Synthesis.SpeechSynthesizer
  $s.Rate = {rate}
  $s.Volume = {volume}
  $wanted = '{voice}'
  $v = $null
  if ($wanted -ne '') {{ $v = $s.GetInstalledVoices() | Where-Object {{ $_.Enabled -and $_.VoiceInfo.Name -like "*$wanted*" }} | Select-Object -First 1 }}
  if ($v -eq $null) {{ $v = $s.GetInstalledVoices() | Where-Object {{ $_.Enabled -and $_.VoiceInfo.Culture.Name -like 'ru*' }} | Select-Object -First 1 }}
  if ($v -eq $null) {{ [Console]::Out.WriteLine('error:не найден русский голос'); [Console]::Out.Flush(); exit 2 }}
  $s.SelectVoice($v.VoiceInfo.Name)
  [Console]::Out.WriteLine('ready:' + $v.VoiceInfo.Name); [Console]::Out.Flush()
  while ($true) {{
    $line = [Console]::In.ReadLine()
    if ($line -eq $null) {{ break }}
    if ($line.Trim() -ne '') {{ $s.Speak($line) }}
    [Console]::Out.WriteLine('done'); [Console]::Out.Flush()
  }}
}} catch {{
  [Console]::Out.WriteLine('error:' + $_.Exception.Message); [Console]::Out.Flush(); exit 1
}}
"#
    )
}

/// Скрипт с выбором устройства воспроизведения: System.Speech его выбрать не умеет,
/// поэтому здесь SAPI.SpVoice (COM), где у голоса можно задать AudioOutput.
pub fn powershell_script_sapi(cfg: &TtsConfig) -> String {
    let voice = cfg.voice.replace('\'', "''");
    let output = cfg.output_device.trim().replace('\'', "''");
    let rate = cfg.rate.clamp(-10, 10);
    let volume = cfg.volume.min(100);
    format!(
        r#"$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
try {{
  $v = New-Object -ComObject SAPI.SpVoice
  $wanted = '{voice}'
  $tok = $null
  if ($wanted -ne '') {{ $tok = $v.GetVoices() | Where-Object {{ $_.GetDescription() -like "*$wanted*" }} | Select-Object -First 1 }}
  if ($tok -eq $null) {{ $tok = $v.GetVoices() | Where-Object {{ $_.GetAttribute('Language') -like '419*' }} | Select-Object -First 1 }}
  if ($tok -eq $null) {{ [Console]::Out.WriteLine('error:не найден русский голос'); [Console]::Out.Flush(); exit 2 }}
  $v.Voice = $tok
  $out = '{output}'
  $o = $v.GetAudioOutputs() | Where-Object {{ $_.GetDescription() -like "*$out*" }} | Select-Object -First 1
  if ($o -eq $null) {{ [Console]::Out.WriteLine('error:не найдено устройство воспроизведения «' + $out + '»'); [Console]::Out.Flush(); exit 3 }}
  $v.AudioOutput = $o
  $v.Rate = {rate}
  $v.Volume = {volume}
  [Console]::Out.WriteLine('ready:' + $tok.GetDescription() + ' → ' + $o.GetDescription()); [Console]::Out.Flush()
  while ($true) {{
    $line = [Console]::In.ReadLine()
    if ($line -eq $null) {{ break }}
    if ($line.Trim() -ne '') {{ $null = $v.Speak($line, 0) }}
    [Console]::Out.WriteLine('done'); [Console]::Out.Flush()
  }}
}} catch {{
  [Console]::Out.WriteLine('error:' + $_.Exception.Message); [Console]::Out.Flush(); exit 1
}}
"#
    )
}

/// Голоса и устройства воспроизведения, известные Windows (SAPI).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SapiLists {
    pub voices: Vec<String>,
    pub outputs: Vec<String>,
}

/// «Microsoft Irina Desktop - Russian» -> «Microsoft Irina Desktop» (так имя подходит и
/// для поиска по подстроке в System.Speech, и в SAPI).
pub fn short_voice_name(description: &str) -> String {
    description.split(" - ").next().unwrap_or(description).trim().to_string()
}

pub fn parse_sapi_lists(text: &str) -> SapiLists {
    let mut lists = SapiLists::default();
    let mut section = 0;
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        match line {
            "" => {}
            "VOICES" => section = 1,
            "OUTPUTS" => section = 2,
            other => match section {
                1 => lists.voices.push(short_voice_name(other)),
                2 => lists.outputs.push(other.to_string()),
                _ => {}
            },
        }
    }
    lists.voices.dedup();
    lists
}

/// Запрашивает у Windows список голосов и устройств воспроизведения (1–2 секунды).
pub fn list_sapi() -> Result<SapiLists, String> {
    let script = r#"[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$v = New-Object -ComObject SAPI.SpVoice
'VOICES'
$v.GetVoices() | ForEach-Object { $_.GetDescription() }
'OUTPUTS'
$v.GetAudioOutputs() | ForEach-Object { $_.GetDescription() }
"#;
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encode_command(script)]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("не удалось запустить PowerShell: {e}"))?;
    Ok(parse_sapi_lists(&String::from_utf8_lossy(&out.stdout)))
}

/// Скрипт -> значение параметра -EncodedCommand (base64 от UTF-16LE).
pub fn encode_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    base64(&bytes)
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Many hands make light work."), "TWFueSBoYW5kcyBtYWtlIGxpZ2h0IHdvcmsu");
    }

    #[test]
    fn encoded_command_is_utf16le_base64() {
        // "A" в UTF-16LE: 41 00 -> "QQA="
        assert_eq!(encode_command("A"), "QQA=");
        // кириллица кодируется двумя байтами: «Я» = U+042F -> 2F 04 -> "LwQ="
        assert_eq!(encode_command("Я"), "LwQ=");
    }

    #[test]
    fn script_contains_settings_and_escapes_quotes() {
        let cfg = TtsConfig { voice: "Iri'na".into(), rate: 99, volume: 250, ..TtsConfig::default() };
        let s = powershell_script(&cfg);
        assert!(s.contains("$wanted = 'Iri''na'"));
        assert!(s.contains("$s.Rate = 10")); // ограничено диапазоном
        assert!(s.contains("$s.Volume = 100"));
        assert!(s.contains("System.Speech"));
    }

    #[test]
    fn disabled_speaker_is_silent_and_ok() {
        let mut s = Speaker::disabled();
        assert!(!s.is_enabled());
        assert!(s.say("привет").is_ok());
        let cfg = TtsConfig { enabled: false, ..TtsConfig::default() };
        assert!(!Speaker::start(&cfg).unwrap().is_enabled());
    }

    #[cfg(unix)]
    fn fake(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[cfg(unix)]
    #[test]
    fn protocol_ready_say_done() {
        // «синтезатор»: сообщает о готовности, на каждую строку отвечает done
        let mut sp = Speaker::with_command(fake(
            "echo 'ready:Тестовый голос'; while IFS= read -r l; do echo done; done",
        ))
        .unwrap();
        assert!(sp.is_enabled());
        assert_eq!(sp.voice_name(), "Тестовый голос");
        assert!(sp.say("Не понял. Повторите, пожалуйста.").is_ok());
        assert!(sp.say("Второй раз").is_ok());
        assert!(sp.say("с\nпереносом").is_ok()); // переносы строк не ломают протокол
    }

    #[cfg(unix)]
    #[test]
    fn startup_error_is_reported() {
        let r = Speaker::with_command(fake("echo 'error:не найден русский голос'; exit 2"));
        assert!(r.err().unwrap().contains("не найден русский голос"));
    }

    #[cfg(unix)]
    #[test]
    fn bom_before_ready_is_tolerated() {
        let sp = Speaker::with_command(fake("printf '\\357\\273\\277ready:Ирина\\n'; cat >/dev/null")).unwrap();
        assert_eq!(sp.voice_name(), "Ирина");
    }

    #[cfg(unix)]
    #[test]
    fn dead_process_gives_error_not_hang() {
        let mut sp = Speaker::with_command(fake("echo 'ready:x'; exit 0")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(sp.say("привет").is_err());
    }

    #[test]
    fn sapi_script_selects_output_device_and_escapes() {
        let cfg = TtsConfig {
            voice: "Iri'na".into(),
            output_device: "Динамики (Realtek)".into(),
            rate: -3,
            ..TtsConfig::default()
        };
        let s = powershell_script_sapi(&cfg);
        assert!(s.contains("SAPI.SpVoice"));
        assert!(s.contains("$wanted = 'Iri''na'"));
        assert!(s.contains("$out = 'Динамики (Realtek)'"));
        assert!(s.contains("$v.AudioOutput = $o"));
        assert!(s.contains("$v.Rate = -3"));
    }

    #[test]
    fn sapi_lists_are_parsed() {
        let text = "\u{feff}VOICES\r\nMicrosoft Irina Desktop - Russian\r\nMicrosoft David Desktop - English (United States)\r\nOUTPUTS\r\nДинамики (Realtek High Definition Audio)\r\nVoicemeeter Input (VB-Audio Voicemeeter VAIO)\r\n";
        let l = parse_sapi_lists(text);
        assert_eq!(l.voices, vec!["Microsoft Irina Desktop", "Microsoft David Desktop"]);
        assert_eq!(l.outputs.len(), 2);
        assert_eq!(l.outputs[0], "Динамики (Realtek High Definition Audio)");
        assert_eq!(short_voice_name("Microsoft Irina Desktop - Russian"), "Microsoft Irina Desktop");
        assert_eq!(parse_sapi_lists(""), SapiLists::default());
    }

    // --- нейросетевой голос: поиск модели и выбор движка -----------------------------------

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vcm_tts_test_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_voice(dir: &Path, onnx: &[&str]) {
        fs::create_dir_all(dir.join("espeak-ng-data")).unwrap();
        fs::write(dir.join("tokens.txt"), "a 1").unwrap();
        for f in onnx {
            fs::write(dir.join(f), "x").unwrap();
        }
    }

    #[test]
    fn model_is_found_by_its_three_parts() {
        let d = temp_dir("find");
        make_voice(&d, &["ru_RU-irina-medium.onnx", "ru_RU-irina-medium.onnx.json"]);
        let f = find_model(&d).unwrap();
        assert!(f.model.ends_with("ru_RU-irina-medium.onnx"));
        assert!(f.tokens.ends_with("tokens.txt"));
        assert!(f.data_dir.ends_with("espeak-ng-data"));
    }

    #[test]
    fn incomplete_model_folders_are_rejected() {
        let no_espeak = temp_dir("noespeak");
        fs::write(no_espeak.join("tokens.txt"), "a").unwrap();
        fs::write(no_espeak.join("m.onnx"), "x").unwrap();
        assert!(find_model(&no_espeak).is_none());
        let no_onnx = temp_dir("noonnx");
        make_voice(&no_onnx, &[]);
        assert!(find_model(&no_onnx).is_none());
        let only_json = temp_dir("onlyjson");
        make_voice(&only_json, &["m.onnx.json"]);
        assert!(find_model(&only_json).is_none());
        assert!(find_model(Path::new("/нет/такой/папки")).is_none());
    }

    #[test]
    fn models_dir_listing_finds_voices_but_not_recognition_models() {
        let base = temp_dir("list");
        make_voice(&base.join("vits-piper-ru_RU-irina-medium"), &["a.onnx"]);
        make_voice(&base.join("vits-piper-ru_RU-denis-medium"), &["b.onnx"]);
        // модель распознавания: есть tokens.txt и .onnx, но нет espeak-ng-data
        let asr = base.join("sherpa-onnx-nemo-ctc-giga-am");
        fs::create_dir_all(&asr).unwrap();
        fs::write(asr.join("tokens.txt"), "a").unwrap();
        fs::write(asr.join("model.int8.onnx"), "x").unwrap();
        assert_eq!(
            list_models(&base),
            vec!["vits-piper-ru_RU-denis-medium", "vits-piper-ru_RU-irina-medium"]
        );
        assert!(list_models(Path::new("/нет/такой/папки")).is_empty());
    }

    #[test]
    fn engine_choice_follows_the_setting_and_model_presence() {
        assert_eq!(choose_engine("auto", true), Engine::Neural);
        assert_eq!(choose_engine("auto", false), Engine::Windows);
        assert_eq!(choose_engine("neural", false), Engine::Neural);
        assert_eq!(choose_engine("windows", true), Engine::Windows);
        assert_eq!(choose_engine("WINDOWS ", true), Engine::Windows);
        assert_eq!(choose_engine("что-то непонятное", true), Engine::Neural);
    }

    #[test]
    fn defaults_keep_the_room_quiet() {
        let c = TtsConfig::default();
        assert_eq!(c.engine, "auto");
        assert!(!c.say_not_a_command, "на болтовню отвечать не должен");
        assert!(c.say_unclear);
        assert_eq!(c.model_dir, "vits-piper-ru_RU-irina-medium");
        assert_eq!(c.speaker_id, 0);
        assert_eq!(c.speed, 1.0);
    }

    #[test]
    fn neural_voice_without_a_model_explains_what_is_missing() {
        let cfg = TtsConfig { engine: "neural".into(), model_dir: "/нет/модели".into(), ..TtsConfig::default() };
        let err = Speaker::start(&cfg).err().unwrap();
        assert!(err.contains("не найдена модель голоса"), "{err}");
        assert!(err.contains("espeak-ng-data"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn auto_without_model_and_without_windows_voice_mentions_both() {
        let cfg = TtsConfig { engine: "auto".into(), model_dir: "/нет/модели".into(), ..TtsConfig::default() };
        let err = Speaker::start(&cfg).err().unwrap();
        assert!(err.contains("нейросетевая модель голоса тоже не найдена"), "{err}");
    }
}
