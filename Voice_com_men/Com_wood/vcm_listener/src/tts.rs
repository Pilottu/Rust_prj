#![allow(dead_code)]

// Голосовая обратная связь. Синтезатор — встроенный в Windows (System.Speech) через
// один постоянно работающий дочерний процесс PowerShell: он один раз загружает голос и
// потом читает вслух строки со стандартного ввода. Так не тратится время на запуск
// PowerShell перед каждой фразой, и в программе нет COM-кода.
//
// Протокол: программа пишет строку с текстом, PowerShell произносит её и отвечает «done».
// Это же позволяет знать, когда речь закончилась (нужно, чтобы не слушать самих себя).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use serde::Deserialize;

/// Секция [tts] файла config.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct TtsConfig {
    pub enabled: bool,
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

    /// Запускает синтезатор Windows. При `enabled = false` возвращает «немого» диктора.
    pub fn start(cfg: &TtsConfig) -> Result<Self, String> {
        if !cfg.enabled {
            return Ok(Self::disabled());
        }
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
        let Backend::Process { stdin, replies, .. } = &mut self.backend else {
            return Ok(());
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
}
