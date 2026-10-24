#![allow(dead_code)]

// Точечное изменение значений в config.toml: комментарии, порядок и незнакомые ключи
// остаются как были. Нужен окну настроек, чтобы не переписывать файл целиком.

/// Строка TOML (в двойных кавычках, с экранированием).
pub fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Число с плавающей точкой: всегда с точкой (иначе TOML прочтёт его как целое).
pub fn toml_float(f: f64) -> String {
    let s = format!("{f}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

fn is_header(t: &str) -> Option<String> {
    let t = t.split('#').next().unwrap_or("").trim();
    if t.starts_with('[') && !t.starts_with("[[") && t.ends_with(']') {
        Some(t[1..t.len() - 1].trim().to_string())
    } else {
        None
    }
}

fn is_key_line(t: &str, key: &str) -> bool {
    if t.starts_with('#') || !t.starts_with(key) {
        return false;
    }
    t[key.len()..].trim_start().starts_with('=')
}

/// Задаёт `key = value` в секции `section` (пустая строка — верхний уровень).
/// `value` — уже готовое значение TOML (см. `toml_string`, `toml_float`).
pub fn set_value(text: &str, section: &str, key: &str, value: &str) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let mut current = String::new();
    let mut section_found = section.is_empty();
    let mut last_in_section: Option<usize> = None;
    let mut key_line: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if let Some(name) = is_header(t) {
            current = name;
            if current == section {
                section_found = true;
                last_in_section = Some(i);
            }
            continue;
        }
        if current == section {
            if !t.is_empty() {
                last_in_section = Some(i);
            }
            if key_line.is_none() && is_key_line(t, key) {
                key_line = Some(i);
            }
        }
    }

    let new_line = |indent: &str| format!("{indent}{key} = {value}");
    if let Some(i) = key_line {
        let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
        lines[i] = new_line(&indent);
    } else if section_found {
        let at = last_in_section.map(|i| i + 1).unwrap_or(0);
        lines.insert(at, new_line(""));
    } else {
        if lines.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
            lines.push(String::new());
        }
        lines.push(format!("[{section}]"));
        lines.push(new_line(""));
    }

    let mut out = lines.join(nl);
    if trailing || !text.is_empty() && key_line.is_none() {
        out.push_str(nl);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_escaped() {
        assert_eq!(toml_string(r"E:\Prog\x"), r#""E:\\Prog\\x""#);
        assert_eq!(toml_string("a\"b"), r#""a\"b""#);
        assert_eq!(toml_string("Готов."), "\"Готов.\"");
        assert_eq!(toml_string("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn floats_always_have_a_point() {
        assert_eq!(toml_float(8.0), "8.0");
        assert_eq!(toml_float(0.5), "0.5");
        assert_eq!(toml_float(-3.0), "-3.0");
    }

    const SAMPLE: &str = "models_dir = \"E:\\\\m\"\n\n[audio]\n# устройство\ndevice = \"UR28M\"   # старое\nchannel = \"auto\"\n\n[vad]\nthreshold = 0.5\n";

    #[test]
    fn replaces_existing_value_and_keeps_the_rest() {
        let out = set_value(SAMPLE, "audio", "device", "\"Микрофон\"");
        assert!(out.contains("device = \"Микрофон\""));
        assert!(out.contains("# устройство"));
        assert!(out.contains("channel = \"auto\""));
        assert!(out.contains("models_dir = \"E:\\\\m\""));
        assert!(!out.contains("UR28M"));
    }

    #[test]
    fn adds_missing_key_to_an_existing_section() {
        let out = set_value(SAMPLE, "audio", "gain_db", "6.0");
        let audio = out.split("[vad]").next().unwrap();
        assert!(audio.contains("gain_db = 6.0"));
        assert!(out.contains("threshold = 0.5"));
    }

    #[test]
    fn adds_a_new_section_at_the_end() {
        let out = set_value(SAMPLE, "tts", "voice", "\"Irina\"");
        assert!(out.trim_end().ends_with("[tts]\nvoice = \"Irina\""));
        // повторный вызов меняет, а не дублирует
        let again = set_value(&out, "tts", "voice", "\"Pavel\"");
        assert_eq!(again.matches("voice").count(), 1);
        assert!(again.contains("voice = \"Pavel\""));
    }

    #[test]
    fn top_level_key_is_edited_or_added_before_sections() {
        let out = set_value(SAMPLE, "", "models_dir", "\"D:\\\\n\"");
        assert!(out.starts_with("models_dir = \"D:\\\\n\""));
        let no_top = "[audio]\ndevice = \"x\"\n";
        let out = set_value(no_top, "", "models_dir", "\"D:\\\\n\"");
        assert!(out.starts_with("models_dir = \"D:\\\\n\"\n[audio]"));
    }

    #[test]
    fn same_key_in_another_section_is_untouched_and_prefixes_are_not_confused() {
        let text = "[a]\ngain = 1\ngain_db = 2\n[b]\ngain_db = 3\n";
        let out = set_value(text, "a", "gain_db", "9");
        assert!(out.contains("[a]\ngain = 1\ngain_db = 9\n[b]\ngain_db = 3"));
    }

    #[test]
    fn crlf_is_preserved() {
        let text = "[audio]\r\ndevice = \"x\"\r\n";
        let out = set_value(text, "audio", "device", "\"y\"");
        assert_eq!(out, "[audio]\r\ndevice = \"y\"\r\n");
        let out = set_value(text, "audio", "channel", "\"left\"");
        assert!(out.contains("\r\nchannel = \"left\""));
        assert!(!out.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn section_header_with_comment_and_indentation() {
        let text = "[audio] # звук\n  device = \"x\"\n";
        let out = set_value(text, "audio", "device", "\"y\"");
        assert_eq!(out, "[audio] # звук\n  device = \"y\"\n");
    }

    #[test]
    fn result_still_parses_as_toml() {
        let mut t = SAMPLE.to_string();
        for (sec, k, v) in [
            ("audio", "gain_db", "12.0".to_string()),
            ("tts", "greeting", toml_string("Готов \"к\" работе.")),
            ("sql", "server", toml_string(r"localhost\TEW_SQLEXPRESS")),
        ] {
            t = set_value(&t, sec, k, &v);
        }
        let v: toml::Value = toml::from_str(&t).unwrap();
        assert_eq!(v["audio"]["gain_db"].as_float(), Some(12.0));
        assert_eq!(v["tts"]["greeting"].as_str(), Some("Готов \"к\" работе."));
        assert_eq!(v["sql"]["server"].as_str(), Some(r"localhost\TEW_SQLEXPRESS"));
    }
}
