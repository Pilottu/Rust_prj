#![allow(dead_code)]

// Окно настроек: `vcm_listener settings` поднимает маленький веб-сервер ТОЛЬКО на этом
// компьютере (127.0.0.1) и открывает страницу в браузере. На ней выбираются устройство
// записи и устройство воспроизведения, голос, канал, усиление, пороги и всё остальное
// из config.toml; есть проверка микрофона и голоса. Сохранение меняет в файле только
// нужные значения (комментарии остаются), прежний файл копируется в config.toml.bak.

use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;

use crate::audio_config::{AudioConfig, ChannelMode, VadConfig, VotingConfig};
use crate::capture::{self, InputDeviceInfo};
use crate::config_edit::{set_value, toml_float, toml_string};
use crate::sql_pool::SqlConfig;
use crate::tts::{self, SapiLists, Speaker, TtsConfig};

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
    pub sapi: SapiLists,
    pub notes: Vec<String>,
}

pub fn gather_devices() -> Devices {
    let mut d = Devices::default();
    match capture::list_input_devices() {
        Ok(list) => d.inputs = list,
        Err(e) => d.notes.push(format!("Устройства записи: {e}")),
    }
    match tts::list_sapi() {
        Ok(l) => d.sapi = l,
        Err(e) => d.notes.push(format!("Голоса и устройства воспроизведения: {e}")),
    }
    d
}

// ---------------------------------------------------------------------------
// Формы и значения
// ---------------------------------------------------------------------------

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() && hex(bytes[i + 1]).is_some() && hex(bytes[i + 2]).is_some() => {
                out.push(hex(bytes[i + 1]).unwrap() << 4 | hex(bytes[i + 2]).unwrap());
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (url_decode(k), url_decode(v))
        })
        .collect()
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

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

fn load_view(path: &Path) -> Result<(ConfigView, String), String> {
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
// Страница
// ---------------------------------------------------------------------------

type Opt = (String, String); // (значение, подпись)

fn select(name: &str, options: &[Opt], current: &str, empty_label: Option<&str>) -> String {
    let cur_l = current.to_lowercase();
    let selected_idx = if current.is_empty() {
        None
    } else {
        options
            .iter()
            .position(|(v, _)| v == current)
            .or_else(|| options.iter().position(|(v, _)| v.to_lowercase().contains(&cur_l)))
    };
    let mut html = format!("<select name=\"{}\">", esc(name));
    if let Some(label) = empty_label {
        let sel = if current.is_empty() { " selected" } else { "" };
        html += &format!("<option value=\"\"{sel}>{}</option>", esc(label));
    }
    for (i, (value, label)) in options.iter().enumerate() {
        let sel = if Some(i) == selected_idx { " selected" } else { "" };
        html += &format!("<option value=\"{}\"{sel}>{}</option>", esc(value), esc(label));
    }
    if !current.is_empty() && selected_idx.is_none() {
        html += &format!(
            "<option value=\"{0}\" selected>{0} — не найдено на этом компьютере</option>",
            esc(current)
        );
    }
    html + "</select>"
}

fn field(label: &str, hint: &str, control: String) -> String {
    format!(
        "<div class=\"f\"><label>{}</label><div>{}<small>{}</small></div></div>",
        esc(label),
        control,
        esc(hint)
    )
}

fn input(name: &str, value: &str, kind: &str) -> String {
    format!("<input type=\"{kind}\" name=\"{}\" value=\"{}\">", esc(name), esc(value))
}

fn number(name: &str, value: f32, step: &str) -> String {
    format!(
        "<input type=\"text\" inputmode=\"decimal\" name=\"{}\" value=\"{}\" size=\"8\" title=\"шаг {step}\">",
        esc(name),
        value
    )
}

fn checkbox(name: &str, on: bool) -> String {
    format!("<input type=\"checkbox\" name=\"{}\"{}>", esc(name), if on { " checked" } else { "" })
}

pub fn render(view: &ConfigView, dev: &Devices, token: &str, message: Option<(bool, String)>) -> String {
    let inputs: Vec<Opt> = dev
        .inputs
        .iter()
        .map(|d| {
            let mark = if d.is_default { " — по умолчанию" } else { "" };
            (d.name.clone(), format!("{} ({} Гц, {} кан., {}){}", d.name, d.rate, d.channels, d.format, mark))
        })
        .collect();
    let voices: Vec<Opt> = dev.sapi.voices.iter().map(|v| (v.clone(), v.clone())).collect();
    let outputs: Vec<Opt> = dev.sapi.outputs.iter().map(|o| (o.clone(), o.clone())).collect();
    let channels: Vec<Opt> = [
        ("auto", "auto — самый громкий канал"),
        ("left", "left — левый"),
        ("right", "right — правый"),
        ("mix", "mix — среднее"),
    ]
    .iter()
    .map(|(v, l)| (v.to_string(), l.to_string()))
    .collect();

    let mut body = String::new();
    if let Some((ok, text)) = message {
        body += &format!("<div class=\"msg {}\">{}</div>", if ok { "ok" } else { "bad" }, esc(&text));
    }
    for note in &dev.notes {
        body += &format!("<div class=\"msg warn\">{}</div>", esc(note));
    }

    body += &format!("<input type=\"hidden\" name=\"token\" value=\"{}\">", esc(token));

    body += "<fieldset><legend>Звук с микрофона</legend>";
    body += &field(
        "Устройство записи",
        "Откуда слушать команды. Выбирайте ПОЛНОЕ имя устройства.",
        select("audio.device", &inputs, &view.audio.device, Some("(устройство по умолчанию в Windows)")),
    );
    body += &field("Канал", "Если микрофон подключён к одному гнезду, auto сам возьмёт нужный.", select("audio.channel", &channels, channel_name(view.audio.channel), None));
    body += &field("Усиление, дБ", "Для линейного входа обычно 0…+20. Речь должна давать −30…−20 dBFS.", number("audio.gain_db", view.audio.gain_db, "0.5"));
    body += &field("Буфер, кадров", "0 — по умолчанию. При «пропусках звука» попробуйте 2048 или 4096.", number("audio.buffer_frames", view.audio.buffer_frames as f32, "1"));
    body += "</fieldset>";

    body += "<fieldset><legend>Детектор речи</legend>";
    body += &field("Порог", "0.05–0.99. Меньше — чувствительнее (ловит тихое начало фразы).", number("vad.threshold", view.vad.threshold, "0.05"));
    body += &field("Тишина в конце фразы, с", "Сколько тишины считать концом фразы.", number("vad.min_silence_s", view.vad.min_silence_s, "0.1"));
    body += &field("Минимальная фраза, с", "Короче — отбрасывается как щелчок.", number("vad.min_speech_s", view.vad.min_speech_s, "0.05"));
    body += &field("Максимальная фраза, с", "Длиннее — режется принудительно.", number("vad.max_speech_s", view.vad.max_speech_s, "1"));
    body += &field("Звук до фразы, с", "Берётся реальный звук перед фразой (детектор начинает с опозданием).", number("vad.pre_roll_s", view.vad.pre_roll_s, "0.05"));
    body += &field("Звук после фразы, с", "То же после фразы.", number("vad.post_roll_s", view.vad.post_roll_s, "0.05"));
    body += &field("Согласных движков", "Сколько из трёх должны дать одинаковый разбор (обычно 2).", number("voting.min_agree", view.voting.min_agree as f32, "1"));
    body += "</fieldset>";

    body += "<fieldset><legend>Голос</legend>";
    body += &field("Озвучка", "Включить голосовые подсказки.", checkbox("tts.enabled", view.tts.enabled));
    body += &field("Голос", "Русский голос Windows.", select("tts.voice", &voices, &view.tts.voice, Some("(первый русский)")));
    body += &field("Устройство воспроизведения", "Куда говорить: колонки, наушники. Пусто — по умолчанию в Windows.", select("tts.output_device", &outputs, &view.tts.output_device, Some("(по умолчанию в Windows)")));
    body += &field("Скорость речи", "От −10 до 10.", number("tts.rate", view.tts.rate as f32, "1"));
    body += &field("Громкость голоса", "От 0 до 100.", number("tts.volume", view.tts.volume as f32, "5"));
    body += &field("Говорить «Принято.»", "После каждой выполненной команды.", checkbox("tts.confirm", view.tts.confirm));
    body += &field("Фраза при запуске", "Пусто — молчать.", input("tts.greeting", &view.tts.greeting, "text"));
    body += &field("Ждать ответ-число, с", "После вопроса «Какое значение?».", number("tts.answer_window_s", view.tts.answer_window_s, "1"));
    body += &field("Пауза после речи, с", "Столько микрофон не слушает после голоса (эхо колонок).", number("tts.echo_tail_s", view.tts.echo_tail_s, "0.1"));
    body += "</fieldset>";

    body += "<fieldset><legend>Модели и база данных</legend>";
    body += &field("Папка с моделями", "Где лежат модели распознавания.", input("models_dir", &view.models_dir, "text"));
    body += &field("Сервер SQL", "Например localhost\\TEW_SQLEXPRESS.", input("sql.server", &view.sql.server, "text"));
    body += &field("База данных", "", input("sql.database", &view.sql.database, "text"));
    body += &field("Драйвер ODBC", "", input("sql.driver", &view.sql.driver, "text"));
    body += &field("Вход Windows", "Включено — вход под текущим пользователем Windows.", checkbox("sql.trusted_connection", view.sql.trusted_connection));
    body += "</fieldset>";

    format!(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\"><title>Настройки слушателя</title><style>\
body{{font-family:Segoe UI,Arial,sans-serif;max-width:900px;margin:20px auto;padding:0 12px;color:#222}}\
fieldset{{margin:14px 0;border:1px solid #bbb;border-radius:6px}}legend{{font-weight:600;padding:0 6px}}\
.f{{display:grid;grid-template-columns:240px 1fr;gap:8px;padding:5px 0;align-items:start}}\
.f label{{padding-top:4px}}small{{display:block;color:#666}}select,input[type=text]{{max-width:100%;padding:3px}}\
select{{min-width:380px}}.msg{{padding:8px 12px;border-radius:6px;margin:8px 0}}\
.ok{{background:#dff5df}}.bad{{background:#fadada}}.warn{{background:#fff3cd}}\
button{{padding:7px 14px;margin-right:8px;font-size:14px}}</style></head><body>\
<h2>Настройки слушателя команд</h2>\
<form method=\"post\" action=\"/save\">{body}\
<p><button type=\"submit\">Сохранить</button>\
<button type=\"submit\" formaction=\"/test-mic\">Проверить микрофон (2 с)</button>\
<button type=\"submit\" formaction=\"/test-voice\">Проверить голос</button></p>\
<p><small>Сохранение меняет config.toml (прежний файл копируется в config.toml.bak). Изменения вступают в силу после перезапуска «listen».</small></p>\
</form></body></html>"
    )
}

// ---------------------------------------------------------------------------
// Обработка запросов
// ---------------------------------------------------------------------------

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

pub struct Ctx {
    pub config_path: PathBuf,
    pub token: String,
    pub port: u16,
    pub devices: Devices,
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

fn page(ctx: &Ctx, view: &ConfigView, message: Option<(bool, String)>) -> Response {
    Response { status: 200, body: render(view, &ctx.devices, &ctx.token, message) }
}

fn plain(status: u16, text: &str) -> Response {
    Response { status, body: format!("<!doctype html><meta charset=\"utf-8\"><p>{}</p>", esc(text)) }
}

fn describe_level(r: &capture::MicReport) -> String {
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

pub fn handle(ctx: &Ctx, req: &Request) -> Response {
    // Защита от запросов с чужих страниц и от подмены имени узла (DNS rebinding).
    let host = req.headers.get("host").map(String::as_str).unwrap_or("");
    if host != format!("127.0.0.1:{}", ctx.port) && host != format!("localhost:{}", ctx.port) {
        return plain(403, "Недопустимый адрес.");
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => match load_view(&ctx.config_path) {
            Ok((view, _)) => page(ctx, &view, None),
            Err(e) => plain(500, &e),
        },
        ("POST", path @ ("/save" | "/test-mic" | "/test-voice")) => {
            let form = parse_form(&req.body);
            if form.get("token").map(String::as_str) != Some(ctx.token.as_str()) {
                return plain(403, "Неверный токен. Откройте окно настроек заново.");
            }
            let view = match read_form(&form) {
                Ok(v) => v,
                Err(e) => {
                    // показываем страницу с прежними значениями из файла и ошибкой
                    return match load_view(&ctx.config_path) {
                        Ok((old, _)) => page(ctx, &old, Some((false, e))),
                        Err(_) => plain(400, &e),
                    };
                }
            };
            let message = match path {
                "/save" => match save(&ctx.config_path, &view) {
                    Ok(()) => (true, "Сохранено. Перезапустите «listen», чтобы применить.".to_string()),
                    Err(e) => (false, e),
                },
                "/test-mic" => match capture::measure(&view.audio, 2.0) {
                    Ok(r) => (r.rms_db > -60.0, describe_level(&r)),
                    Err(e) => (false, format!("Микрофон: {e}")),
                },
                _ => {
                    let cfg = TtsConfig { enabled: true, ..view.tts.clone() };
                    match Speaker::start(&cfg).and_then(|mut s| s.say("Проверка голоса. Вы меня слышите?")) {
                        Ok(()) => (true, "Голос проверен: если вы ничего не услышали, выберите другое устройство воспроизведения.".to_string()),
                        Err(e) => (false, format!("Голос: {e}")),
                    }
                }
            };
            page(ctx, &view, Some(message))
        }
        _ => plain(404, "Не найдено."),
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err("слишком длинный заголовок".into());
        }
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("соединение закрыто".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let first = lines.next().ok_or("пустой запрос")?;
    let mut parts = first.split_whitespace();
    let method = parts.next().ok_or("нет метода")?.to_string();
    let path = parts.next().ok_or("нет пути")?.split('?').next().unwrap_or("/").to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if length > 1024 * 1024 {
        return Err("слишком большое тело запроса".into());
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Ok(Request { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

fn write_response(stream: &mut TcpStream, r: &Response) {
    let reason = match r.status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Bad Request",
    };
    let bytes = r.body.as_bytes();
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        r.status, reason, bytes.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(bytes);
    let _ = stream.flush();
}

pub fn serve(listener: TcpListener, ctx: Arc<Ctx>) {
    for stream in listener.incoming().flatten() {
        let ctx = Arc::clone(&ctx);
        thread::spawn(move || {
            let mut stream = stream;
            if let Ok(req) = read_request(&mut stream) {
                let resp = handle(&ctx, &req);
                write_response(&mut stream, &resp);
            }
        });
    }
}

fn new_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    (0..2)
        .map(|i| {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(nanos.wrapping_add(i));
            format!("{:016x}", h.finish())
        })
        .collect()
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = url;
    }
}

pub fn run(port: u16, config_path: &Path) -> Result<(), Box<dyn Error>> {
    load_view(config_path).map_err(|e| format!("{e}"))?; // сразу сообщаем, если конфиг не читается
    println!("Собираю список устройств (1–2 секунды)...");
    let devices = gather_devices();
    let (listener, port) = (0..10u16)
        .find_map(|i| TcpListener::bind(("127.0.0.1", port + i)).ok().map(|l| (l, port + i)))
        .ok_or("не удалось занять порт для окна настроек")?;
    let ctx = Arc::new(Ctx { config_path: config_path.to_path_buf(), token: new_token(), port, devices });
    let url = format!("http://127.0.0.1:{port}/");
    println!("Файл настроек: {}", config_path.display());
    println!("Окно настроек: {url}  (закрыть — Ctrl+C)");
    open_browser(&url);
    serve(listener, ctx);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

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
            sapi: SapiLists {
                voices: vec!["Microsoft Irina Desktop".into()],
                outputs: vec!["Динамики (Realtek)".into(), "Voicemeeter Input".into()],
            },
            notes: vec![],
        }
    }

    fn ctx(name: &str) -> Ctx {
        Ctx { config_path: temp_config(name, SAMPLE), token: "tok123".into(), port: 8765, devices: devices() }
    }

    fn req(method: &str, path: &str, host: &str, body: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            headers: [("host".to_string(), host.to_string())].into_iter().collect(),
            body: body.into(),
        }
    }

    /// Форма, как её присылает браузер: все поля из страницы с заданными изменениями.
    fn full_form(token: &str, changes: &[(&str, &str)]) -> String {
        let mut f: HashMap<String, String> = [
            ("token", token), ("models_dir", "E:\\Prog\\models"), ("audio.device", "Line (2- Steinberg UR28M)"),
            ("audio.channel", "auto"), ("audio.gain_db", "0"), ("audio.buffer_frames", "0"),
            ("vad.threshold", "0.5"), ("vad.min_silence_s", "0.5"), ("vad.min_speech_s", "0.25"),
            ("vad.max_speech_s", "8"), ("vad.pre_roll_s", "0.3"), ("vad.post_roll_s", "0.2"),
            ("voting.min_agree", "2"), ("tts.enabled", "on"), ("tts.voice", "Microsoft Irina Desktop"),
            ("tts.output_device", ""), ("tts.rate", "0"), ("tts.volume", "100"), ("tts.greeting", "Готов."),
            ("tts.answer_window_s", "8"), ("tts.echo_tail_s", "0.4"), ("sql.server", "localhost\\TEW_SQLEXPRESS"),
            ("sql.database", "vcm_state"), ("sql.driver", "ODBC Driver 17 for SQL Server"), ("sql.trusted_connection", "on"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in changes {
            f.insert(k.to_string(), v.to_string());
        }
        f.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect::<Vec<_>>().join("&")
    }

    fn pct(s: &str) -> String {
        s.bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect()
    }

    // --- разбор формы ------------------------------------------------------------

    #[test]
    fn url_decoding_handles_cyrillic_plus_and_garbage() {
        assert_eq!(url_decode("%D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82"), "Привет");
        assert_eq!(url_decode("a+b%20c"), "a b c");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz%4"), "%zz%4");
        let f = parse_form("a=1&b=%D0%B4&c=&d");
        assert_eq!(f["a"], "1");
        assert_eq!(f["b"], "д");
        assert_eq!(f["c"], "");
        assert_eq!(f["d"], "");
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(esc("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }

    #[test]
    fn valid_form_is_read_with_commas_and_checkboxes() {
        let form = parse_form(&full_form("t", &[("audio.gain_db", "12,5"), ("tts.confirm", "on")]));
        let v = read_form(&form).unwrap();
        assert_eq!(v.audio.gain_db, 12.5);
        assert!(v.tts.confirm);
        assert!(v.sql.trusted_connection);
        assert_eq!(v.voting.min_agree, 2);
        // неотмеченный чекбокс в форме отсутствует
        let mut form = parse_form(&full_form("t", &[]));
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
            let err = read_form(&parse_form(&full_form("t", &[(key, bad)]))).unwrap_err();
            assert!(err.contains(label), "{key}: {err}");
        }
    }

    // --- сохранение ------------------------------------------------------------------

    #[test]
    fn save_changes_only_the_edited_values_and_makes_a_backup() {
        let c = ctx("save");
        let form = parse_form(&full_form("tok123", &[("audio.device", "Микрофон (Realtek)"), ("audio.gain_db", "12"), ("tts.output_device", "Динамики (Realtek)")]));
        let view = read_form(&form).unwrap();
        save(&c.config_path, &view).unwrap();
        let after = fs::read_to_string(&c.config_path).unwrap();
        assert!(after.contains("device = \"Микрофон (Realtek)\""));
        assert!(after.contains("gain_db = 12.0"));
        assert!(after.contains("# звук"), "комментарий потерян");
        assert!(after.contains("[gigaam]\nmodel = \"a\\\\b\""), "чужая секция изменилась");
        assert!(after.contains("test_wav = \"x.wav\""));
        // новые секции добавлены, файл читается
        let back: ConfigView = toml::from_str(&after).unwrap();
        assert_eq!(back.tts.output_device, "Динамики (Realtek)");
        assert_eq!(back.audio.device, "Микрофон (Realtek)");
        assert_eq!(back.models_dir, "E:\\Prog\\models");
        let bak = fs::read_to_string(format!("{}.bak", c.config_path.display())).unwrap();
        assert_eq!(bak, SAMPLE);
    }

    // --- страница ---------------------------------------------------------------------

    #[test]
    fn page_lists_devices_and_marks_the_current_one() {
        let (view, _) = load_view(&ctx("page").config_path).unwrap();
        let html = render(&view, &devices(), "tok", None);
        assert!(html.contains("Line (2- Steinberg UR28M) (96000 Гц, 2 кан., f32) — по умолчанию"));
        // в конфиге стоит «UR28M» — выбрано полное имя устройства
        assert!(html.contains("<option value=\"Line (2- Steinberg UR28M)\" selected>"));
        assert!(html.contains("Voicemeeter Input"));
        assert!(html.contains("name=\"token\" value=\"tok\""));
        assert!(html.contains("formaction=\"/test-mic\""));
    }

    #[test]
    fn unknown_current_device_is_kept_and_flagged() {
        let mut view = ConfigView::default();
        view.audio.device = "Steinberg ZZ99".into();
        let html = render(&view, &Devices::default(), "t", None);
        assert!(html.contains("Steinberg ZZ99 — не найдено на этом компьютере"));
    }

    #[test]
    fn user_text_cannot_inject_html() {
        let mut view = ConfigView::default();
        view.tts.greeting = "\"><script>alert(1)</script>".into();
        let html = render(&view, &Devices::default(), "t", Some((false, "<b>x</b>".into())));
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("<b>x</b>"));
    }

    // --- обработчик запросов -----------------------------------------------------------

    #[test]
    fn get_page_ok_and_wrong_host_forbidden() {
        let c = ctx("get");
        let r = handle(&c, &req("GET", "/", "127.0.0.1:8765", ""));
        assert_eq!(r.status, 200);
        assert!(r.body.contains("Настройки слушателя команд"));
        assert_eq!(handle(&c, &req("GET", "/", "evil.example:8765", "")).status, 403);
        assert_eq!(handle(&c, &req("GET", "/", "127.0.0.1:9999", "")).status, 403);
        assert_eq!(handle(&c, &req("GET", "/nope", "localhost:8765", "")).status, 404);
    }

    #[test]
    fn save_requires_the_token() {
        let c = ctx("token");
        let r = handle(&c, &req("POST", "/save", "127.0.0.1:8765", &full_form("wrong", &[("audio.gain_db", "30")])));
        assert_eq!(r.status, 403);
        assert_eq!(fs::read_to_string(&c.config_path).unwrap(), SAMPLE, "файл изменился без токена");
    }

    #[test]
    fn save_through_the_handler_and_bad_input_keeps_the_file() {
        let c = ctx("handler");
        let ok = handle(&c, &req("POST", "/save", "127.0.0.1:8765", &full_form("tok123", &[("audio.gain_db", "9")])));
        assert_eq!(ok.status, 200);
        assert!(ok.body.contains("Сохранено"));
        assert!(fs::read_to_string(&c.config_path).unwrap().contains("gain_db = 9.0"));

        let before = fs::read_to_string(&c.config_path).unwrap();
        let bad = handle(&c, &req("POST", "/save", "127.0.0.1:8765", &full_form("tok123", &[("vad.threshold", "abc")])));
        assert_eq!(bad.status, 200);
        assert!(bad.body.contains("Порог детектора речи"));
        assert_eq!(fs::read_to_string(&c.config_path).unwrap(), before);
    }

    #[test]
    fn level_description_gives_advice() {
        let mk = |rms, peak| capture::MicReport { rms_db: rms, peak_db: peak, seconds: 2.0, glitches: 0 };
        assert!(describe_level(&mk(-80.0, -70.0)).contains("почти нет"));
        assert!(describe_level(&mk(-30.0, 0.0)).contains("перегрузка"));
        assert!(describe_level(&mk(-50.0, -30.0)).contains("тихо"));
        assert!(describe_level(&mk(-28.0, -12.0)).contains("в порядке"));
    }

    // --- настоящий сервер по сети --------------------------------------------------------

    fn http(port: u16, request: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn real_server_get_then_post_save() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = temp_config("net", SAMPLE);
        let c = Arc::new(Ctx { config_path: path.clone(), token: new_token(), port, devices: devices() });
        let token = c.token.clone();
        thread::spawn(move || serve(listener, c));

        let page = http(port, &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"));
        assert!(page.starts_with("HTTP/1.1 200 OK"));
        assert!(page.contains("charset=utf-8"));
        assert!(page.contains(&format!("value=\"{token}\"")), "токен не попал на страницу");

        let body = full_form(&token, &[("audio.gain_db", "6"), ("tts.greeting", "Привет, мир")]);
        let resp = http(
            port,
            &format!(
                "POST /save HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("gain_db = 6.0"));
        assert!(saved.contains("greeting = \"Привет, мир\""));

        // чужой Host отклоняется и по сети
        let evil = http(port, "GET / HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n");
        assert!(evil.starts_with("HTTP/1.1 403"));
    }

    #[test]
    fn tokens_differ_between_runs() {
        assert_ne!(new_token(), new_token());
        assert_eq!(new_token().len(), 32);
    }
}
