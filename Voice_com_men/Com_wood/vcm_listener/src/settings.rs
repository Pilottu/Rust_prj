#![allow(dead_code)]

// Ядро окна настроек (без графики): чтение и проверка значений формы, точечная правка
// config.toml (комментарии остаются, прежний файл копируется в config.toml.bak),
// списки выбора. Само окно — ui/settings.slint и settings_ui.rs.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::audio_config::{AudioConfig, ChannelMode, VadConfig, VotingConfig};
use crate::capture::{self, InputDeviceInfo};
use crate::playback::{self, OutputDeviceInfo};
use crate::config_edit::{set_value, toml_float, toml_string};
use crate::sql_pool::SqlConfig;
use crate::tts::{self, SapiLists, TtsConfig};

/// Где лежит config.toml: рядом с программой, а если там нет — в папке проекта.
pub fn config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join("config.toml");
            if beside.exists() {
                return beside;
            }
        }
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("config.toml")
}

/// То, что окно настроек читает и пишет (остальное в файле не трогается).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ConfigView {
    pub models_dir: String,
    pub audio: AudioConfig,
    pub vad: VadConfig,
    pub voting: VotingConfig,
    pub sql: SqlConfig,
    pub tts: TtsConfig,
}

#[derive(Debug, Default, Clone)]
pub struct Devices {
    pub inputs: Vec<InputDeviceInfo>,
    /// Устройства воспроизведения (колонки, наушники): для обоих голосов.
    pub outputs: Vec<OutputDeviceInfo>,
    /// Голоса Windows (нужны только если выбран голос Windows).
    pub sapi: SapiLists,
    /// Найденные папки с нейросетевыми голосами внутри папки моделей.
    pub models: Vec<String>,
    pub notes: Vec<String>,
}

pub fn gather_devices(models_dir: &str) -> Devices {
    let mut d = Devices::default();
    d.models = tts::list_models(Path::new(models_dir));
    match playback::list_output_devices() {
        Ok(list) => d.outputs = list,
        Err(e) => d.notes.push(format!("Устройства воспроизведения: {e}")),
    }
    match capture::list_input_devices() {
        Ok(list) => d.inputs = list,
        Err(e) => d.notes.push(format!("Устройства записи: {e}")),
    }
    match tts::list_sapi() {
        Ok(l) => d.sapi = l,
        Err(e) => d.notes.push(format!("Голоса Windows: {e}")),
    }
    d
}

// ---------------------------------------------------------------------------
// Значения формы
// ---------------------------------------------------------------------------

fn text<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
    form.get(key).map(|s| s.trim()).unwrap_or("")
}

fn float(form: &HashMap<String, String>, key: &str, label: &str, min: f32, max: f32) -> Result<f32, String> {
    let raw = text(form, key).replace(',', ".");
    match raw.parse::<f32>() {
        Ok(v) if v.is_finite() && v >= min && v <= max => Ok(v),
        _ => Err(format!("Поле «{label}» должно быть числом от {min} до {max}.")),
    }
}

fn int(form: &HashMap<String, String>, key: &str, label: &str, min: i64, max: i64) -> Result<i64, String> {
    match text(form, key).parse::<i64>() {
        Ok(v) if v >= min && v <= max => Ok(v),
        _ => Err(format!("Поле «{label}» должно быть целым числом от {min} до {max}.")),
    }
}

fn flag(form: &HashMap<String, String>, key: &str) -> bool {
    form.get(key).map(|v| v == "on" || v == "1" || v == "true").unwrap_or(false)
}

pub fn channel_name(c: ChannelMode) -> &'static str {
    match c {
        ChannelMode::Auto => "auto",
        ChannelMode::Left => "left",
        ChannelMode::Right => "right",
        ChannelMode::Mix => "mix",
    }
}

/// Значения из присланной формы с проверкой диапазонов.
pub fn read_form(form: &HashMap<String, String>) -> Result<ConfigView, String> {
    let channel = match text(form, "audio.channel") {
        "auto" => ChannelMode::Auto,
        "left" => ChannelMode::Left,
        "right" => ChannelMode::Right,
        "mix" => ChannelMode::Mix,
        _ => return Err("Канал должен быть auto, left, right или mix.".to_string()),
    };
    let sql_server = text(form, "sql.server").to_string();
    if sql_server.is_empty() {
        return Err("Поле «Сервер SQL» не должно быть пустым.".to_string());
    }
    Ok(ConfigView {
        models_dir: text(form, "models_dir").to_string(),
        audio: AudioConfig {
            device: text(form, "audio.device").to_string(),
            channel,
            gain_db: float(form, "audio.gain_db", "Усиление, дБ", -30.0, 60.0)?,
            target_sample_rate: 16_000,
            buffer_frames: int(form, "audio.buffer_frames", "Буфер, кадров", 0, 65_536)? as u32,
        },
        vad: VadConfig {
            threshold: float(form, "vad.threshold", "Порог детектора речи", 0.05, 0.99)?,
            min_silence_s: float(form, "vad.min_silence_s", "Тишина в конце фразы, с", 0.05, 5.0)?,
            min_speech_s: float(form, "vad.min_speech_s", "Минимальная фраза, с", 0.05, 5.0)?,
            max_speech_s: float(form, "vad.max_speech_s", "Максимальная фраза, с", 1.0, 30.0)?,
            pre_roll_s: float(form, "vad.pre_roll_s", "Звук до фразы, с", 0.0, 2.0)?,
            post_roll_s: float(form, "vad.post_roll_s", "Звук после фразы, с", 0.0, 2.0)?,
        },
        voting: VotingConfig { min_agree: int(form, "voting.min_agree", "Согласных движков", 1, 3)? as usize },
        sql: SqlConfig {
            server: sql_server,
            database: text(form, "sql.database").to_string(),
            driver: text(form, "sql.driver").to_string(),
            trusted_connection: flag(form, "sql.trusted_connection"),
        },
        tts: TtsConfig {
            enabled: flag(form, "tts.enabled"),
            engine: match text(form, "tts.engine") {
                e @ ("auto" | "neural" | "windows") => e.to_string(),
                _ => return Err("Голос должен быть auto, neural или windows.".to_string()),
            },
            model_dir: text(form, "tts.model_dir").to_string(),
            speaker_id: int(form, "tts.speaker_id", "Номер голоса в модели", 0, 1000)? as u32,
            speed: float(form, "tts.speed", "Скорость нейросетевого голоса", 0.3, 3.0)?,
            say_not_a_command: flag(form, "tts.say_not_a_command"),
            say_unclear: flag(form, "tts.say_unclear"),
            voice: text(form, "tts.voice").to_string(),
            output_device: text(form, "tts.output_device").to_string(),
            rate: int(form, "tts.rate", "Скорость речи", -10, 10)? as i32,
            volume: int(form, "tts.volume", "Громкость голоса", 0, 100)? as u32,
            confirm: flag(form, "tts.confirm"),
            greeting: text(form, "tts.greeting").to_string(),
            answer_window_s: float(form, "tts.answer_window_s", "Ждать ответ-число, с", 1.0, 60.0)?,
            echo_tail_s: float(form, "tts.echo_tail_s", "Пауза после речи, с", 0.0, 3.0)?,
        },
    })
}

/// Новый текст config.toml: значения из `v` внесены в `old`, остальное сохранено.
pub fn apply(v: &ConfigView, old: &str) -> String {
    let mut t = old.to_string();
    let mut set = |section: &str, key: &str, value: String| t = set_value(&t, section, key, &value);
    set("", "models_dir", toml_string(&v.models_dir));
    set("audio", "device", toml_string(&v.audio.device));
    set("audio", "channel", toml_string(channel_name(v.audio.channel)));
    set("audio", "gain_db", toml_float(v.audio.gain_db as f64));
    set("audio", "buffer_frames", v.audio.buffer_frames.to_string());
    set("audio", "target_sample_rate", v.audio.target_sample_rate.to_string());
    set("vad", "threshold", toml_float(v.vad.threshold as f64));
    set("vad", "min_silence_s", toml_float(v.vad.min_silence_s as f64));
    set("vad", "min_speech_s", toml_float(v.vad.min_speech_s as f64));
    set("vad", "max_speech_s", toml_float(v.vad.max_speech_s as f64));
    set("vad", "pre_roll_s", toml_float(v.vad.pre_roll_s as f64));
    set("vad", "post_roll_s", toml_float(v.vad.post_roll_s as f64));
    set("voting", "min_agree", v.voting.min_agree.to_string());
    set("sql", "server", toml_string(&v.sql.server));
    set("sql", "database", toml_string(&v.sql.database));
    set("sql", "driver", toml_string(&v.sql.driver));
    set("sql", "trusted_connection", v.sql.trusted_connection.to_string());
    set("tts", "enabled", v.tts.enabled.to_string());
    set("tts", "engine", toml_string(&v.tts.engine));
    set("tts", "model_dir", toml_string(&v.tts.model_dir));
    set("tts", "speaker_id", v.tts.speaker_id.to_string());
    set("tts", "speed", toml_float(v.tts.speed as f64));
    set("tts", "say_not_a_command", v.tts.say_not_a_command.to_string());
    set("tts", "say_unclear", v.tts.say_unclear.to_string());
    set("tts", "voice", toml_string(&v.tts.voice));
    set("tts", "output_device", toml_string(&v.tts.output_device));
    set("tts", "rate", v.tts.rate.to_string());
    set("tts", "volume", v.tts.volume.to_string());
    set("tts", "confirm", v.tts.confirm.to_string());
    set("tts", "greeting", toml_string(&v.tts.greeting));
    set("tts", "answer_window_s", toml_float(v.tts.answer_window_s as f64));
    set("tts", "echo_tail_s", toml_float(v.tts.echo_tail_s as f64));
    t
}

pub fn load_view(path: &Path) -> Result<(ConfigView, String), String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("не удалось прочитать {}: {e}", path.display()))?;
    let view: ConfigView = toml::from_str(&text)
        .map_err(|e| format!("в {} ошибка: {e}", path.display()))?;
    Ok((view, text))
}

/// Проверяет, что итоговый файл читается, делает копию .bak и записывает.
pub fn save(path: &Path, v: &ConfigView) -> Result<(), String> {
    let old = fs::read_to_string(path).unwrap_or_default();
    let new = apply(v, &old);
    toml::from_str::<ConfigView>(&new).map_err(|e| format!("после правки файл бы не читался: {e}"))?;
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        fs::copy(path, PathBuf::from(bak)).map_err(|e| format!("не удалось сделать копию .bak: {e}"))?;
    }
    fs::write(path, new).map_err(|e| format!("не удалось записать {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Списки выбора
// ---------------------------------------------------------------------------

/// Подпись для пустого значения «по умолчанию в Windows».
pub const DEFAULT_LABEL: &str = "(по умолчанию в Windows)";
pub const FIRST_RUSSIAN_LABEL: &str = "(первый русский)";

/// Варианты выпадающего списка: подпись для человека и значение для config.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices {
    items: Vec<(String, String)>,
}

impl Choices {
    /// `empty_label` — подпись варианта с пустым значением (если он нужен), `items` —
    /// (подпись, значение), `current` — значение из конфига. Возвращает варианты и подпись
    /// того, что надо показать выбранным. Если значения из конфига на этом компьютере нет
    /// (например, другой микрофон), оно остаётся в списке с пометкой — при сохранении
    /// оно не потеряется.
    pub fn build(empty_label: Option<&str>, items: Vec<(String, String)>, current: &str) -> (Choices, String) {
        let mut all: Vec<(String, String)> = Vec::new();
        if let Some(label) = empty_label {
            all.push((label.to_string(), String::new()));
        }
        all.extend(items);
        let cur_l = current.to_lowercase();
        let selected = if current.is_empty() {
            all.iter().find(|(_, v)| v.is_empty()).or_else(|| all.first()).map(|(l, _)| l.clone())
        } else {
            all.iter()
                .find(|(_, v)| v == current)
                .or_else(|| all.iter().find(|(_, v)| !v.is_empty() && v.to_lowercase().contains(&cur_l)))
                .map(|(l, _)| l.clone())
        };
        let selected = match selected {
            Some(label) => label,
            None => {
                let label = format!("{current} — не найдено на этом компьютере");
                all.push((label.clone(), current.to_string()));
                label
            }
        };
        (Choices { items: all }, selected)
    }

    pub fn labels(&self) -> Vec<String> {
        self.items.iter().map(|(l, _)| l.clone()).collect()
    }

    /// Значение для config.toml по выбранной подписи.
    pub fn value_of(&self, label: &str) -> String {
        self.items
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| label.to_string())
    }
}

pub fn input_items(devices: &Devices) -> Vec<(String, String)> {
    devices
        .inputs
        .iter()
        .map(|d| {
            let mark = if d.is_default { " — по умолчанию" } else { "" };
            (format!("{} ({} Гц, {} кан., {}){}", d.name, d.rate, d.channels, d.format, mark), d.name.clone())
        })
        .collect()
}

pub fn output_items(devices: &Devices) -> Vec<(String, String)> {
    devices
        .outputs
        .iter()
        .map(|d| {
            let mark = if d.is_default { " — по умолчанию" } else { "" };
            (format!("{} ({} Гц, {} кан.){}", d.name, d.rate, d.channels, mark), d.name.clone())
        })
        .collect()
}

pub fn engine_items() -> Vec<(String, String)> {
    [
        ("auto — нейросетевой, если модель найдена", "auto"),
        ("neural — только нейросетевой", "neural"),
        ("windows — только голос Windows", "windows"),
    ]
    .iter()
    .map(|(l, v)| (l.to_string(), v.to_string()))
    .collect()
}

pub fn model_items(devices: &Devices) -> Vec<(String, String)> {
    devices.models.iter().map(|m| (m.clone(), m.clone())).collect()
}

pub fn channel_items() -> Vec<(String, String)> {
    [
        ("auto — самый громкий канал", "auto"),
        ("left — левый", "left"),
        ("right — правый", "right"),
        ("mix — среднее", "mix"),
    ]
    .iter()
    .map(|(l, v)| (l.to_string(), v.to_string()))
    .collect()
}

pub fn describe_level(r: &capture::MicReport) -> String {
    let verdict = if r.rms_db < -60.0 {
        "сигнала почти нет — проверьте устройство, канал и усиление"
    } else if r.peak_db > -1.0 {
        "есть перегрузка — уменьшите усиление"
    } else if r.rms_db < -45.0 {
        "тихо — добавьте усиление"
    } else {
        "уровень в порядке"
    };
    format!(
        "Микрофон: уровень {:.1} dBFS, пик {:.1} dBFS за {:.0} с, пропусков звука: {} — {}.",
        r.rms_db, r.peak_db, r.seconds, r.glitches, verdict
    )
}


#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "models_dir = \"E:\\\\Prog\\\\models\"\ntest_wav = \"x.wav\"\n\n# звук\n[audio]\ndevice = \"UR28M\"\nchannel = \"auto\"\ngain_db = 0.0\n\n[gigaam]\nmodel = \"a\\\\b\"\ntokens = \"t\"\n";

    fn temp_config(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vcm_settings_test_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        fs::write(&p, text).unwrap();
        p
    }

    fn devices() -> Devices {
        Devices {
            inputs: vec![
                InputDeviceInfo { name: "Микрофон (Realtek)".into(), rate: 44100, channels: 2, format: "f32".into(), is_default: false },
                InputDeviceInfo { name: "Line (2- Steinberg UR28M)".into(), rate: 96000, channels: 2, format: "f32".into(), is_default: true },
            ],
            ..Devices::default()
        }
    }

    /// Форма, как её собирает окно: все поля с заданными изменениями.
    fn full_form(changes: &[(&str, &str)]) -> HashMap<String, String> {
        let mut f: HashMap<String, String> = [
            ("models_dir", "E:\\Prog\\models"), ("audio.device", "Line (2- Steinberg UR28M)"),
            ("audio.channel", "auto"), ("audio.gain_db", "0"), ("audio.buffer_frames", "0"),
            ("vad.threshold", "0.5"), ("vad.min_silence_s", "0.5"), ("vad.min_speech_s", "0.25"),
            ("vad.max_speech_s", "8"), ("vad.pre_roll_s", "0.3"), ("vad.post_roll_s", "0.2"),
            ("voting.min_agree", "2"), ("tts.enabled", "on"), ("tts.voice", "Microsoft Irina Desktop"),
            ("tts.output_device", ""), ("tts.engine", "auto"), ("tts.model_dir", "vits-piper-ru_RU-irina-medium"),
            ("tts.speaker_id", "0"), ("tts.speed", "1"), ("tts.say_unclear", "on"), ("tts.rate", "0"), ("tts.volume", "100"), ("tts.greeting", "Готов."),
            ("tts.answer_window_s", "8"), ("tts.echo_tail_s", "0.4"), ("sql.server", "localhost\\TEW_SQLEXPRESS"),
            ("sql.database", "vcm_state"), ("sql.driver", "ODBC Driver 17 for SQL Server"), ("sql.trusted_connection", "on"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in changes {
            f.insert(k.to_string(), v.to_string());
        }
        f
    }

    #[test]
    fn valid_form_is_read_with_commas_and_checkboxes() {
        let v = read_form(&full_form(&[("audio.gain_db", "12,5"), ("tts.confirm", "on")])).unwrap();
        assert_eq!(v.audio.gain_db, 12.5);
        assert!(v.tts.confirm);
        assert!(v.sql.trusted_connection);
        assert_eq!(v.voting.min_agree, 2);
        let mut form = full_form(&[]);
        form.remove("tts.enabled");
        assert!(!read_form(&form).unwrap().tts.enabled);
    }

    #[test]
    fn invalid_values_are_rejected_with_a_clear_message() {
        for (key, bad, label) in [
            ("audio.gain_db", "очень много", "Усиление"),
            ("vad.threshold", "5", "Порог"),
            ("voting.min_agree", "7", "Согласных"),
            ("tts.volume", "-1", "Громкость"),
            ("audio.channel", "center", "Канал"),
            ("sql.server", "", "Сервер SQL"),
        ] {
            let err = read_form(&full_form(&[(key, bad)])).unwrap_err();
            assert!(err.contains(label), "{key}: {err}");
        }
    }

    #[test]
    fn new_voice_settings_are_read_and_validated() {
        let v = read_form(&full_form(&[("tts.engine", "neural"), ("tts.speed", "1,2"), ("tts.say_not_a_command", "on")])).unwrap();
        assert_eq!(v.tts.engine, "neural");
        assert_eq!(v.tts.speed, 1.2);
        assert!(v.tts.say_not_a_command);
        assert!(v.tts.say_unclear);
        assert_eq!(v.tts.model_dir, "vits-piper-ru_RU-irina-medium");
        // по умолчанию болтовне не отвечаем
        assert!(!read_form(&full_form(&[])).unwrap().tts.say_not_a_command);
        for (key, bad, label) in [("tts.engine", "robot", "Голос"), ("tts.speed", "9", "Скорость"), ("tts.speaker_id", "-1", "Номер голоса")] {
            let err = read_form(&full_form(&[(key, bad)])).unwrap_err();
            assert!(err.contains(label), "{key}: {err}");
        }
    }

    #[test]
    fn voice_settings_survive_saving_and_old_config_gets_safe_defaults() {
        let path = temp_config("voice", SAMPLE);
        save(&path, &read_form(&full_form(&[("tts.engine", "neural"), ("tts.output_device", "Динамики (Realtek)")])).unwrap()).unwrap();
        let back: ConfigView = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.tts.engine, "neural");
        assert_eq!(back.tts.model_dir, "vits-piper-ru_RU-irina-medium");
        // старый конфиг без новых ключей: болтовне не отвечаем, «не понял» остаётся
        let old: ConfigView = toml::from_str("[tts]\nenabled = true\n").unwrap();
        assert!(!old.tts.say_not_a_command);
        assert!(old.tts.say_unclear);
        assert_eq!(old.tts.engine, "auto");
    }

    #[test]
    fn engine_output_and_model_lists() {
        let d = Devices {
            outputs: vec![OutputDeviceInfo { name: "Динамики (Realtek)".into(), rate: 48000, channels: 2, is_default: true }],
            models: vec!["vits-piper-ru_RU-irina-medium".into()],
            ..Devices::default()
        };
        let (_, sel) = Choices::build(Some(DEFAULT_LABEL), output_items(&d), "realtek");
        assert_eq!(sel, "Динамики (Realtek) (48000 Гц, 2 кан.) — по умолчанию");
        let (c, sel) = Choices::build(None, engine_items(), "neural");
        assert_eq!(c.value_of(&sel), "neural");
        let (_, sel) = Choices::build(None, model_items(&d), "vits-piper-ru_RU-irina-medium");
        assert_eq!(sel, "vits-piper-ru_RU-irina-medium");
        // модели нет на этом компьютере — остаётся в списке с пометкой
        let (_, sel) = Choices::build(None, model_items(&Devices::default()), "vits-piper-ru_RU-ruslan-medium");
        assert!(sel.ends_with("не найдено на этом компьютере"));
    }

    #[test]
    fn save_changes_only_the_edited_values_and_makes_a_backup() {
        let path = temp_config("save", SAMPLE);
        let view = read_form(&full_form(&[("audio.device", "Микрофон (Realtek)"), ("audio.gain_db", "12"), ("tts.output_device", "Динамики (Realtek)")])).unwrap();
        save(&path, &view).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("device = \"Микрофон (Realtek)\""));
        assert!(after.contains("gain_db = 12.0"));
        assert!(after.contains("# звук"), "комментарий потерян");
        assert!(after.contains("[gigaam]\nmodel = \"a\\\\b\""), "чужая секция изменилась");
        assert!(after.contains("test_wav = \"x.wav\""));
        let back: ConfigView = toml::from_str(&after).unwrap();
        assert_eq!(back.tts.output_device, "Динамики (Realtek)");
        assert_eq!(back.audio.device, "Микрофон (Realtek)");
        assert_eq!(back.models_dir, "E:\\Prog\\models");
        let bak = fs::read_to_string(format!("{}.bak", path.display())).unwrap();
        assert_eq!(bak, SAMPLE);
    }

    #[test]
    fn bad_values_never_touch_the_file() {
        let path = temp_config("bad", SAMPLE);
        assert!(read_form(&full_form(&[("vad.threshold", "abc")])).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), SAMPLE);
        assert!(!Path::new(&format!("{}.bak", path.display())).exists());
    }

    #[test]
    fn config_file_loads_into_the_view() {
        let (view, _) = load_view(&temp_config("load", SAMPLE)).unwrap();
        assert_eq!(view.audio.device, "UR28M");
        assert_eq!(view.models_dir, "E:\\Prog\\models");
        assert!(load_view(Path::new("/нет/такого/файла.toml")).is_err());
    }

    // --- списки выбора -----------------------------------------------------------------

    #[test]
    fn config_substring_selects_the_full_device() {
        let (choices, selected) = Choices::build(Some(DEFAULT_LABEL), input_items(&devices()), "ur28m");
        assert_eq!(selected, "Line (2- Steinberg UR28M) (96000 Гц, 2 кан., f32) — по умолчанию");
        assert_eq!(choices.value_of(&selected), "Line (2- Steinberg UR28M)");
        assert_eq!(choices.labels()[0], DEFAULT_LABEL);
        assert_eq!(choices.labels().len(), 3);
    }

    #[test]
    fn empty_config_value_selects_the_default_entry() {
        let (choices, selected) = Choices::build(Some(DEFAULT_LABEL), input_items(&devices()), "");
        assert_eq!(selected, DEFAULT_LABEL);
        assert_eq!(choices.value_of(DEFAULT_LABEL), "");
    }

    #[test]
    fn unknown_device_is_kept_and_survives_saving() {
        let (choices, selected) = Choices::build(Some(DEFAULT_LABEL), input_items(&Devices::default()), "Steinberg ZZ99");
        assert_eq!(selected, "Steinberg ZZ99 — не найдено на этом компьютере");
        assert_eq!(choices.value_of(&selected), "Steinberg ZZ99");
    }

    #[test]
    fn list_without_empty_entry_selects_by_value() {
        let (choices, selected) = Choices::build(None, channel_items(), "mix");
        assert_eq!(selected, "mix — среднее");
        assert_eq!(choices.value_of("left — левый"), "left");
        assert_eq!(choices.value_of("что-то вписанное руками"), "что-то вписанное руками");
    }

    #[test]
    fn level_description_gives_advice() {
        let mk = |rms, peak| capture::MicReport { rms_db: rms, peak_db: peak, seconds: 2.0, glitches: 0 };
        assert!(describe_level(&mk(-80.0, -70.0)).contains("почти нет"));
        assert!(describe_level(&mk(-30.0, 0.0)).contains("перегрузка"));
        assert!(describe_level(&mk(-50.0, -30.0)).contains("тихо"));
        assert!(describe_level(&mk(-28.0, -12.0)).contains("в порядке"));
    }
}
