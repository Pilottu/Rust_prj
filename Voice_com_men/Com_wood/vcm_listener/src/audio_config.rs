#![allow(dead_code)]

use serde::Deserialize;

/// Как выбрать канал из многоканального входа.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChannelMode {
    /// Взять тот канал, где сигнал громче (если микрофон подключён к одному гнезду).
    Auto,
    Left,
    Right,
    /// Среднее обоих каналов.
    Mix,
}

/// Секция [audio] файла config.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioConfig {
    /// Часть имени устройства записи, регистр не важен. Пусто — устройство по умолчанию.
    pub device: String,
    pub channel: ChannelMode,
    /// Усиление в дБ после выбора канала.
    pub gain_db: f32,
    /// Частота для VAD и распознавателей.
    pub target_sample_rate: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: String::new(),
            channel: ChannelMode::Auto,
            gain_db: 0.0,
            target_sample_rate: 16_000,
        }
    }
}

/// Секция [vad] файла config.toml: детектор речи Silero.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct VadConfig {
    /// Порог уверенности «это речь», 0..1.
    pub threshold: f32,
    /// Сколько секунд тишины считать концом фразы.
    pub min_silence_s: f32,
    /// Фразы короче этого (секунд) отбрасываются как щелчки.
    pub min_speech_s: f32,
    /// Фразы длиннее этого (секунд) режутся принудительно.
    pub max_speech_s: f32,
    /// Сколько секунд реального звука взять ПЕРЕД фразой (детектор начинает с опозданием).
    pub pre_roll_s: f32,
    /// Сколько секунд реального звука взять ПОСЛЕ фразы.
    pub post_roll_s: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            min_silence_s: 0.5,
            min_speech_s: 0.25,
            max_speech_s: 8.0,
            pre_roll_s: 0.3,
            post_roll_s: 0.2,
        }
    }
}

/// Секция [voting] файла config.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct VotingConfig {
    /// Сколько движков из трёх должны дать одинаковый разбор.
    pub min_agree: usize,
}

impl Default for VotingConfig {
    fn default() -> Self {
        Self { min_agree: 2 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        audio: AudioConfig,
        #[serde(default)]
        voting: VotingConfig,
        #[serde(default)]
        vad: VadConfig,
    }

    #[test]
    fn parses_audio_and_voting_sections() {
        let text = r#"
            models_dir = "E:\\x"

            [audio]
            device = "UR28M"
            channel = "auto"
            gain_db = 6.0
            target_sample_rate = 16000

            [voting]
            min_agree = 2

            [vad]
            threshold = 0.6
            min_silence_s = 0.7
        "#;
        let root: Root = toml::from_str(text).unwrap();
        assert_eq!(root.audio.device, "UR28M");
        assert_eq!(root.audio.channel, ChannelMode::Auto);
        assert_eq!(root.audio.gain_db, 6.0);
        assert_eq!(root.voting.min_agree, 2);
        assert_eq!(root.vad.threshold, 0.6);
        assert_eq!(root.vad.min_silence_s, 0.7);
        assert_eq!(root.vad.max_speech_s, 8.0); // не указано — значение по умолчанию
    }

    #[test]
    fn missing_sections_use_defaults() {
        let root: Root = toml::from_str("models_dir = \"E:\\\\x\"").unwrap();
        assert_eq!(root.audio, AudioConfig::default());
        assert_eq!(root.voting.min_agree, 2);
        assert_eq!(root.vad, VadConfig::default());
    }

    #[test]
    fn unknown_channel_value_is_an_error() {
        let r: Result<Root, _> = toml::from_str("[audio]\nchannel = \"center\"");
        assert!(r.is_err());
    }
}
