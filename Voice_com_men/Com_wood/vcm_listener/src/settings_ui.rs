#![allow(dead_code)]

// Окно настроек на Slint (разметка — ui/settings.slint). Читает config.toml, показывает
// списки устройств, проверяет значения и сохраняет через settings::save. Проверки
// микрофона и голоса идут в отдельных потоках, чтобы окно не замирало.

use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::capture;
use crate::settings::{
    channel_items, channel_name, describe_level, gather_devices, input_items, load_view, read_form, save,
    Choices, ConfigView, Devices, DEFAULT_LABEL, FIRST_RUSSIAN_LABEL,
};
use crate::tts::{Speaker, TtsConfig};

slint::include_modules!();

/// Какие подписи соответствуют каким значениям в выпадающих списках.
struct UiChoices {
    input: Choices,
    channel: Choices,
    voice: Choices,
    output: Choices,
}

fn model(labels: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(labels.into_iter().map(SharedString::from).collect::<Vec<_>>()))
}

fn s(v: impl ToString) -> SharedString {
    SharedString::from(v.to_string())
}

fn fill(win: &SettingsWindow, v: &ConfigView, d: &Devices) -> UiChoices {
    let (input, sel_input) = Choices::build(Some(DEFAULT_LABEL), input_items(d), &v.audio.device);
    let (channel, sel_channel) = Choices::build(None, channel_items(), channel_name(v.audio.channel));
    let voices = d.sapi.voices.iter().map(|x| (x.clone(), x.clone())).collect();
    let (voice, sel_voice) = Choices::build(Some(FIRST_RUSSIAN_LABEL), voices, &v.tts.voice);
    let outputs = d.sapi.outputs.iter().map(|x| (x.clone(), x.clone())).collect();
    let (output, sel_output) = Choices::build(Some(DEFAULT_LABEL), outputs, &v.tts.output_device);

    win.set_input_devices(model(input.labels()));
    win.set_channels(model(channel.labels()));
    win.set_voices(model(voice.labels()));
    win.set_outputs(model(output.labels()));

    win.set_models_dir(s(&v.models_dir));
    win.set_audio_device(s(sel_input));
    win.set_audio_channel(s(sel_channel));
    win.set_audio_gain_db(s(v.audio.gain_db));
    win.set_audio_buffer_frames(s(v.audio.buffer_frames));
    win.set_vad_threshold(s(v.vad.threshold));
    win.set_vad_min_silence(s(v.vad.min_silence_s));
    win.set_vad_min_speech(s(v.vad.min_speech_s));
    win.set_vad_max_speech(s(v.vad.max_speech_s));
    win.set_vad_pre_roll(s(v.vad.pre_roll_s));
    win.set_vad_post_roll(s(v.vad.post_roll_s));
    win.set_voting_min_agree(s(v.voting.min_agree));
    win.set_tts_enabled(v.tts.enabled);
    win.set_tts_voice(s(sel_voice));
    win.set_tts_output_device(s(sel_output));
    win.set_tts_rate(s(v.tts.rate));
    win.set_tts_volume(s(v.tts.volume));
    win.set_tts_confirm(v.tts.confirm);
    win.set_tts_greeting(s(&v.tts.greeting));
    win.set_tts_answer_window(s(v.tts.answer_window_s));
    win.set_tts_echo_tail(s(v.tts.echo_tail_s));
    win.set_sql_server(s(&v.sql.server));
    win.set_sql_database(s(&v.sql.database));
    win.set_sql_driver(s(&v.sql.driver));
    win.set_sql_trusted(v.sql.trusted_connection);

    UiChoices { input, channel, voice, output }
}

/// Значения из окна в том виде, как их ждёт settings::read_form.
fn form_of(w: &SettingsWindow, c: &UiChoices) -> HashMap<String, String> {
    let mut f: HashMap<String, String> = HashMap::new();
    let mut put = |k: &str, v: String| {
        f.insert(k.to_string(), v);
    };
    put("models_dir", w.get_models_dir().to_string());
    put("audio.device", c.input.value_of(&w.get_audio_device()));
    put("audio.channel", c.channel.value_of(&w.get_audio_channel()));
    put("audio.gain_db", w.get_audio_gain_db().to_string());
    put("audio.buffer_frames", w.get_audio_buffer_frames().to_string());
    put("vad.threshold", w.get_vad_threshold().to_string());
    put("vad.min_silence_s", w.get_vad_min_silence().to_string());
    put("vad.min_speech_s", w.get_vad_min_speech().to_string());
    put("vad.max_speech_s", w.get_vad_max_speech().to_string());
    put("vad.pre_roll_s", w.get_vad_pre_roll().to_string());
    put("vad.post_roll_s", w.get_vad_post_roll().to_string());
    put("voting.min_agree", w.get_voting_min_agree().to_string());
    put("tts.voice", c.voice.value_of(&w.get_tts_voice()));
    put("tts.output_device", c.output.value_of(&w.get_tts_output_device()));
    put("tts.rate", w.get_tts_rate().to_string());
    put("tts.volume", w.get_tts_volume().to_string());
    put("tts.greeting", w.get_tts_greeting().to_string());
    put("tts.answer_window_s", w.get_tts_answer_window().to_string());
    put("tts.echo_tail_s", w.get_tts_echo_tail().to_string());
    put("sql.server", w.get_sql_server().to_string());
    put("sql.database", w.get_sql_database().to_string());
    put("sql.driver", w.get_sql_driver().to_string());
    if w.get_tts_enabled() {
        put("tts.enabled", "on".to_string());
    }
    if w.get_tts_confirm() {
        put("tts.confirm", "on".to_string());
    }
    if w.get_sql_trusted() {
        put("sql.trusted_connection", "on".to_string());
    }
    f
}

fn report(w: &SettingsWindow, ok: bool, text: String) {
    w.set_status(s(text));
    w.set_status_ok(ok);
}

pub fn run(config_path: &Path) -> Result<(), Box<dyn Error>> {
    let (view, _) = load_view(config_path)?;
    println!("Собираю список устройств (1–2 секунды)...");
    let devices = gather_devices();

    let win = SettingsWindow::new()?;
    let choices = Rc::new(fill(&win, &view, &devices));
    if !devices.notes.is_empty() {
        report(&win, false, devices.notes.join(" "));
    }
    let path = config_path.to_path_buf();

    // Сохранить
    {
        let weak = win.as_weak();
        let choices = Rc::clone(&choices);
        let path = path.clone();
        win.on_save(move || {
            let Some(w) = weak.upgrade() else { return };
            match read_form(&form_of(&w, &choices)) {
                Err(e) => report(&w, false, e),
                Ok(v) => match save(&path, &v) {
                    Ok(()) => report(&w, true, "Сохранено. Перезапустите «listen», чтобы применить.".to_string()),
                    Err(e) => report(&w, false, e),
                },
            }
        });
    }

    // Проверить микрофон (2 секунды, в отдельном потоке)
    {
        let weak = win.as_weak();
        let choices = Rc::clone(&choices);
        win.on_test_mic(move || {
            let Some(w) = weak.upgrade() else { return };
            let v = match read_form(&form_of(&w, &choices)) {
                Ok(v) => v,
                Err(e) => return report(&w, false, e),
            };
            w.set_busy(true);
            report(&w, true, "Говорите в микрофон 2 секунды…".to_string());
            let audio = v.audio.clone();
            let weak = w.as_weak();
            std::thread::spawn(move || {
                let (ok, text) = match capture::measure(&audio, 2.0) {
                    Ok(r) => (r.rms_db > -60.0, describe_level(&r)),
                    Err(e) => (false, format!("Микрофон: {e}")),
                };
                let _ = weak.upgrade_in_event_loop(move |w| {
                    report(&w, ok, text);
                    w.set_busy(false);
                });
            });
        });
    }

    // Проверить голос
    {
        let weak = win.as_weak();
        let choices = Rc::clone(&choices);
        win.on_test_voice(move || {
            let Some(w) = weak.upgrade() else { return };
            let v = match read_form(&form_of(&w, &choices)) {
                Ok(v) => v,
                Err(e) => return report(&w, false, e),
            };
            w.set_busy(true);
            report(&w, true, "Запускаю синтезатор (до нескольких секунд)…".to_string());
            let tts = TtsConfig { enabled: true, ..v.tts.clone() };
            let weak = w.as_weak();
            std::thread::spawn(move || {
                let (ok, text) = match Speaker::start(&tts).and_then(|mut sp| sp.say("Проверка голоса. Вы меня слышите?")) {
                    Ok(()) => (true, "Голос проверен. Если вы ничего не услышали, выберите другое устройство воспроизведения.".to_string()),
                    Err(e) => (false, format!("Голос: {e}")),
                };
                let _ = weak.upgrade_in_event_loop(move |w| {
                    report(&w, ok, text);
                    w.set_busy(false);
                });
            });
        });
    }

    // Закрыть
    {
        let weak = win.as_weak();
        win.on_close_window(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }

    win.run()?;
    Ok(())
}
