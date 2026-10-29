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
use std::thread;
use std::time::{Duration, Instant};

use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector, Wave};

use crate::audio_config::{AudioConfig, VadConfig, VotingConfig};
use crate::capture;
use crate::clarify::{self, AnswerResult, Clarify, ClarifyConfig, ClarifyPlan, Vocabulary};
use crate::dialog::{followup_views, judge, match_forms, prompt_for, Dialog, Evaluation, Outcome, PromptPolicy};
use crate::tts::{Speaker, TtsConfig};
use crate::dsp::Resampler;
use crate::engines::{EngineBank, EnginePaths};
use crate::history::History;
use crate::sql_pool::{self, SqlConfig};
use crate::wake::{decide, MatchingConfig, WakeConfig, WakeDecision, WakeState, WakeWords};
use vcm_norm::{speech_form, CommandIndex, Match, SpeechForm};
use crate::voting::Verdict;

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
    speaker: Speaker,
    dialog: Dialog,
    policy: PromptPolicy,
    answer_window: Duration,
    echo_guard: bool,
    speaker_error_shown: bool,
    wake_cfg: WakeConfig,
    wake_words: WakeWords,
    wake_state: WakeState,
    max_extra: usize,
    vocab: Vocabulary,
    clarify: Clarify,
    clarify_cfg: ClarifyConfig,
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

    /// Фраза не нам (нет кодового слова) или только кодовое слово: показываем одной строкой.
    fn print_skipped(&self, seconds: f32, texts: &[crate::engines::EngineText], note: &str) {
        let heard = texts.iter().map(|t| t.text.as_str()).find(|t| !t.is_empty()).unwrap_or("—");
        println!("\n[#{}] {:.1} с  {}: «{}»", self.counter, seconds, note, heard);
    }

    /// Пояснение для строки движка на экране.
    fn describe(&self, m: &Match) -> String {
        match m {
            Match::Exact { cmd_key, number: Some(n) } => {
                format!("→ {} [{}]", self.reference_of(cmd_key), n)
            }
            Match::Exact { cmd_key, number: None } => format!("→ {}", self.reference_of(cmd_key)),
            Match::NeedNumber { cmd_key } => {
                format!("→ {} (число не услышано)", self.reference_of(cmd_key))
            }
            Match::OutOfRange { cmd_key, number, slot } => format!(
                "→ {}: число {} вне диапазона {}–{}",
                self.reference_of(cmd_key),
                number,
                slot.min,
                slot.max
            ),
            Match::Ambiguous(keys) => format!("→ неоднозначно ({} команд)", keys.len()),
            Match::None => "→ такой команды нет".to_string(),
        }
    }

    /// Реакция голосом и состояние диалога.
    fn react(&mut self, outcome: Outcome) {
        let now = Instant::now();
        match &outcome {
            Outcome::AskNumber { cmd_key, slot } | Outcome::OutOfRange { cmd_key, slot } => {
                self.dialog.ask(cmd_key.clone(), *slot, now, self.answer_window)
            }
            // Болтовня рядом не должна сбрасывать вопрос «какое значение?»: он сам истечёт по времени.
            Outcome::Accepted { .. } => self.dialog.clear(),
            Outcome::NotACommand | Outcome::Unclear | Outcome::Silence => {}
        }
        if let Some(text) = prompt_for(&outcome, &self.policy) {
            if self.speaker.is_enabled() {
                println!("  Голос: «{text}»");
                self.say(&text);
            } else {
                println!("  (озвучка выключена) Сказал бы: «{text}»");
            }
        }
    }

    /// Произнести фразу; после речи надо отбросить звук микрофона (эхо колонок).
    fn say(&mut self, text: &str) {
        if let Err(e) = self.speaker.say(text) {
            if !self.speaker_error_shown {
                println!("  (синтезатор: {e})");
                self.speaker_error_shown = true;
            }
        }
        self.echo_guard = true;
    }

    fn take_echo_guard(&mut self) -> bool {
        std::mem::take(&mut self.echo_guard)
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

        let now = Instant::now();
        let has_text: Vec<bool> = texts.iter().map(|t| !t.text.trim().is_empty()).collect();
        let text_refs: Vec<&str> = texts.iter().map(|t| t.text.as_str()).collect();
        let mut forms: Vec<SpeechForm> = texts.iter().map(|t| speech_form(&t.text)).collect();

        // 1. Ответ-число на вопрос «Какое значение?»
        let num_pending = self.dialog.active(now);
        let followup = num_pending.as_ref().and_then(|p| followup_views(&text_refs, p));
        let answering_number = followup.is_some();

        // 2. Ответ на уточняющий вопрос («Где?», «Что?», «Что сделать?»): кодовое слово не нужно
        if !answering_number {
            if let Some(cp) = self.clarify.active(now) {
                forms = forms.iter().map(|f| self.wake_words.strip(f)).collect();
                let result = clarify::answer(&self.index, &self.vocab, &cp, &forms, &has_text, self.min_agree, self.max_extra);
                if matches!(result, AnswerResult::Noise) {
                    self.print_skipped(seconds, &texts, "не ответ на вопрос, жду дальше");
                    return;
                }
                self.print_header(seconds, pre_s, post_s);
                println!("  (ответ на уточняющий вопрос «{}»)", cp.question);
                match result {
                    AnswerResult::Fresh(eval) => {
                        println!("  Ответ оказался самостоятельной командой");
                        self.clarify.clear();
                        self.show_and_finish(&texts, eval);
                    }
                    AnswerResult::Merged(eval) => {
                        println!("  Ответ подставлен в исходную фразу");
                        self.clarify.clear();
                        self.show_and_finish(&texts, eval);
                    }
                    AnswerResult::Failed { merged, ambiguous } => {
                        self.print_engines(&texts, &vec![Match::None; texts.len()]);
                        let to_ask = match clarify::next_step(
                            &self.index,
                            &self.vocab,
                            &self.clarify_cfg,
                            &cp,
                            &merged,
                            ambiguous,
                        ) {
                            ClarifyPlan::Ask { category, question } => {
                                Some((format!("не хватает слов ({category})"), question))
                            }
                            ClarifyPlan::AskDisambiguation { question } => {
                                Some(("подходит несколько команд — уточняю".to_string(), question))
                            }
                            ClarifyPlan::GiveUp => None,
                        };
                        if let Some((why, question)) = to_ask {
                            println!("  {why} — спрашиваю ещё раз");
                            let window = Duration::from_secs_f32(self.clarify_cfg.window_s.max(1.0));
                            self.clarify.ask(merged, question.clone(), cp.round + 1, now, window);
                            self.speak_question(&question);
                        } else {
                            println!("  Уточнение не помогло: такой команды нет");
                            self.clarify.clear();
                            self.wake_state.close();
                            self.react(Outcome::NotACommand);
                        }
                    }
                    AnswerResult::Noise => {}
                }
                return;
            }
        }

        // 3. Кодовое слово (на ответ-число оно не требуется)
        let mut woke = false;
        if !answering_number {
            match decide(&self.wake_words, &self.wake_cfg, self.wake_state.is_awake(now), &forms) {
                WakeDecision::Process { forms: f, woke: w } => {
                    forms = f;
                    woke = w;
                }
                WakeDecision::Ignore => {
                    self.print_skipped(seconds, &texts, "нет кодового слова, пропущено");
                    return;
                }
                WakeDecision::OpenWindow => {
                    self.print_skipped(seconds, &texts, "кодовое слово услышано");
                    let window = Duration::from_secs_f32(self.wake_cfg.window_s.max(1.0));
                    self.wake_state.open(now, window);
                    println!("  Слушаю команду {:.0} с", window.as_secs_f32());
                    if self.speaker.is_enabled() && !self.wake_cfg.ack.trim().is_empty() {
                        let ack = self.wake_cfg.ack.clone();
                        println!("  Голос: «{ack}»");
                        self.say(&ack);
                    }
                    return;
                }
            }
        }

        // 4. Обычный разбор
        let (matches, vocab_hit): (Vec<Match>, Vec<bool>) = match &followup {
            Some(v) => (v.iter().map(|x| x.matched.clone()).collect(), vec![true; texts.len()]),
            None => (
                match_forms(&self.index, &forms, &has_text, self.max_extra, false),
                forms.iter().map(|f| self.index.vocab_hit(f)).collect(),
            ),
        };
        let eval = judge(&self.index, matches, &has_text, &vocab_hit, self.min_agree);

        // Перед «Такой команды нет.» выясняем, чего не хватает, и спрашиваем.
        if !answering_number && matches!(eval.outcome, Outcome::NotACommand) {
            let to_ask = match clarify::plan(&self.index, &self.vocab, &self.clarify_cfg, &forms, 0) {
                ClarifyPlan::Ask { category, question } => {
                    Some((format!("не хватает: {category}"), question))
                }
                ClarifyPlan::AskDisambiguation { question } => {
                    Some(("подходит несколько команд — уточняю".to_string(), question))
                }
                ClarifyPlan::GiveUp => None,
            };
            if let Some((why, question)) = to_ask {
                self.print_header(seconds, pre_s, post_s);
                if woke {
                    println!("  (кодовое слово услышано)");
                }
                self.print_engines(&texts, &eval.matches);
                println!("  Команда не найдена, {why} — спрашиваю");
                self.wake_state.close();
                let window = Duration::from_secs_f32(self.clarify_cfg.window_s.max(1.0));
                self.clarify.ask(forms, question.clone(), 1, now, window);
                self.speak_question(&question);
                return;
            }
        }

        self.print_header(seconds, pre_s, post_s);
        if answering_number {
            println!("  (ответ на вопрос: ждали число)");
        }
        if woke {
            println!("  (кодовое слово услышано)");
        }
        self.show_and_finish(&texts, eval);
    }

    fn print_header(&self, seconds: f32, pre_s: f32, post_s: f32) {
        println!(
            "\n[#{}] фраза {:.1} с (контекст: {:.2} с до, {:.2} с после)",
            self.counter, seconds, pre_s, post_s
        );
    }

    /// Строки движков: текст, время и во что разобран.
    fn print_engines(&self, texts: &[crate::engines::EngineText], matches: &[Match]) {
        for (t, m) in texts.iter().zip(matches) {
            println!(
                "  {:<7} {:>5.2} с  {}  {}",
                t.engine,
                t.elapsed.as_secs_f64(),
                if t.text.is_empty() { "—" } else { t.text.as_str() },
                if t.text.trim().is_empty() { String::new() } else { self.describe(m) }
            );
        }
    }

    fn print_verdict(&self, eval: &Evaluation) {
        match eval.verdict.clone() {
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
                        eval.views.len(),
                        slot.min,
                        slot.max
                    ),
                    (_, number) => println!(
                        "  Голосование: ПРИНЯТО {}/{} — «{}»{}; согласны: {}{}",
                        agreed.len(),
                        eval.views.len(),
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

    /// Показать разбор и голосование, закрыть окно кодового слова и ответить голосом.
    fn show_and_finish(&mut self, texts: &[crate::engines::EngineText], eval: Evaluation) {
        self.print_engines(texts, &eval.matches);
        self.print_verdict(&eval);
        // Окно «слушаю» после кодового слова расходуется на первую настоящую фразу.
        if !matches!(eval.outcome, Outcome::Silence) {
            self.wake_state.close();
        }
        self.react(eval.outcome);
    }

    fn speak_question(&mut self, question: &str) {
        if self.speaker.is_enabled() {
            println!("  Голос: «{question}»");
            self.say(question);
        } else {
            println!("  (озвучка выключена) Спросил бы: «{question}»");
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
    tts: &TtsConfig,
    wake: &WakeConfig,
    matching: &MatchingConfig,
    clarify_cfg: &ClarifyConfig,
    wav: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    println!("Читаю пул команд из SQL Server ({} / {})...", sql.server, sql.database);
    let pool = sql_pool::load_pool(sql)
        .map_err(|e| format!("не удалось прочитать dbo.CommandPool: {e}"))?;
    println!(
        "Команд: {}, вариантов фраз: {}, слов в командах: {}",
        pool.reference.len(),
        pool.index.len(),
        pool.index.vocabulary_len()
    );
    if pool.index.is_empty() {
        println!("ВНИМАНИЕ: пул пуст. Запустите основную программу, чтобы она записала пул, и перезапустите слушатель.");
    }

    let speaker = match Speaker::start(tts) {
        Ok(sp) => {
            if sp.is_enabled() {
                println!("Озвучка: голос «{}»", sp.voice_name());
            } else {
                println!("Озвучка выключена ([tts] enabled = false).");
            }
            sp
        }
        Err(e) => {
            println!("Озвучка недоступна: {e}");
            Speaker::disabled()
        }
    };

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
        speaker,
        dialog: Dialog::new(),
        policy: PromptPolicy {
            confirm: tts.confirm,
            say_not_a_command: tts.say_not_a_command,
            say_unclear: tts.say_unclear,
        },
        answer_window: Duration::from_secs_f32(tts.answer_window_s.max(1.0)),
        echo_guard: false,
        speaker_error_shown: false,
        wake_cfg: wake.clone(),
        wake_words: WakeWords::new(&wake.words),
        wake_state: WakeState::default(),
        max_extra: matching.max_extra_words,
        vocab: Vocabulary::new(&pool.vocab, &clarify_cfg.order),
        clarify: Clarify::new(),
        clarify_cfg: clarify_cfg.clone(),
    };
    if clarify_cfg.enabled {
        if pipeline.vocab.is_empty() {
            println!("ВНИМАНИЕ: словарь категорий пуст (запустите основную программу и задайте деревьям примечания «Локация», «Объект», «Команда») — уточняющие вопросы выключены.");
        } else {
            println!(
                "Категории для уточняющих вопросов: {} (слов: {}).",
                pipeline.vocab.categories().join(", "),
                pipeline.vocab.len()
            );
        }
    }
    if wake.enabled {
        if pipeline.wake_words.is_empty() {
            println!("ВНИМАНИЕ: кодовое слово включено, но список слов пуст — фразы разбираются без него.");
        } else {
            println!("Кодовые слова: {}; ждать команду после слова {:.0} с.", wake.words.join(", "), wake.window_s);
        }
    }
    println!("Допуск на лишние слова во фразе: {}.", matching.max_extra_words);

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
            let capture = capture::start(audio, tx).map_err(|e| {
                format!("{e}\nОткройте окно настроек и выберите устройство: vcm_listener.exe settings")
            })?;
            let i = &capture.info;
            println!(
                "Устройство: {} — {} Гц, {} кан., {}; в распознавание идёт {} Гц моно.",
                i.device_name, i.device_rate, i.channels, i.sample_format, i.target_rate
            );
            let echo_tail = Duration::from_secs_f32(tts.echo_tail_s.max(0.0));
            if pipeline.speaker.is_enabled() && !tts.greeting.trim().is_empty() {
                pipeline.say(&tts.greeting);
                pipeline.take_echo_guard();
                thread::sleep(echo_tail);
                while rx.try_recv().is_ok() {}
            }
            println!("Говорите. Выход — Ctrl+C.");
            let mut meter = Meter::new();
            while let Ok(chunk) = rx.recv() {
                meter.update(&chunk);
                pipeline.feed(&chunk);
                if pipeline.take_echo_guard() {
                    // пока говорил синтезатор, микрофон слышал колонки: отбрасываем накопленное
                    thread::sleep(echo_tail);
                    while rx.try_recv().is_ok() {}
                }
            }
            Ok(())
        }
    }
}
