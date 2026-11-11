#![allow(dead_code)]

// Запись принятой команды в dbo.CommandResults: по CmdKey находим привязку
// (dbo.CommandBindings), пишем значение нужного типа и увеличиваем Seq.
// OPC-сервер (KEPServerEX, ODBC) опрашивает эту таблицу.

use std::error::Error;
use std::time::Duration;

use odbc_api::{ConnectionOptions, Cursor, Environment, IntoParameter, Nullable};
use serde::Deserialize;

use crate::sql_pool::SqlConfig;

/// Секция [opc] файла config.toml.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct OpcConfig {
    /// Писать принятые команды в dbo.CommandResults.
    pub enabled: bool,
    /// Через сколько секунд вернуть бит из 1 в 0. 0 — не сбрасывать (событие видно по Seq).
    pub bit_reset_s: f32,
    /// Команды, начинающиеся с этих слов, пишут в бит 0 («Выключить Освещение …»), остальные — 1.
    pub off_words: Vec<String>,
}

impl Default for OpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bit_reset_s: 2.0,
            off_words: vec!["выключить".to_string(), "отключить".to_string(), "погасить".to_string()],
        }
    }
}

pub struct Written {
    /// "bit", "num" или "text".
    pub value_type: String,
    /// Для бита: записана единица (true) или ноль (false).
    pub bit_on: bool,
    pub seq: i64,
}

/// Команда «выключающая»: её первое слово совпадает с одним из `off_words` (по основе слова).
pub fn is_off_command(phrase: &str, off_words: &[String]) -> bool {
    let first = phrase.split_whitespace().next().unwrap_or("");
    let key = vcm_norm::command_form(first).key;
    !key.is_empty() && off_words.iter().any(|word| vcm_norm::command_form(word).key == key)
}

/// Строка для вставки в SQL как N'...' (кавычки удваиваются).
fn sql_text(value: &str) -> String {
    let short: String = value.chars().take(256).collect();
    format!("N'{}'", short.replace('\'', "''"))
}

fn read_string(row: &mut odbc_api::CursorRow<'_>, column: u16) -> Result<String, Box<dyn Error>> {
    let mut value = Vec::new();
    row.get_wide_text(column, &mut value)?;
    Ok(String::from_utf16_lossy(&value))
}

fn read_opt_i32(row: &mut odbc_api::CursorRow<'_>, column: u16) -> Result<Option<i32>, Box<dyn Error>> {
    let mut value = Nullable::<i32>::null();
    row.get_data(column, &mut value)?;
    Ok(value.into_opt())
}

fn read_opt_i64(row: &mut odbc_api::CursorRow<'_>, column: u16) -> Result<Option<i64>, Box<dyn Error>> {
    let mut value = Nullable::<i64>::null();
    row.get_data(column, &mut value)?;
    Ok(value.into_opt())
}

/// Записывает результат команды. `Ok(None)` — привязки нет или команда не «в работе».
pub fn write_result(
    sql: &SqlConfig,
    cmd_key: &str,
    number: Option<u32>,
    phrase: &str,
    bit_on: bool,
) -> Result<Option<Written>, Box<dyn Error>> {
    let environment = Environment::new()?;
    let connection =
        environment.connect_with_connection_string(&sql.connection_string(), ConnectionOptions::default())?;
    // CmdKey — только цифры и дефисы, поэтому обычная строка безопасна.
    let key = cmd_key.into_parameter();

    // Тип и признак «в работе»; строка должна быть и в Bindings, и в Results.
    let mut value_type = String::new();
    let mut active = false;
    let mut found = false;
    if let Some(mut cursor) = connection.execute(
        "SELECT b.ValueType, CAST(b.Active AS INT) FROM dbo.CommandBindings AS b \
         JOIN dbo.CommandResults AS r ON r.CmdKey = b.CmdKey WHERE b.CmdKey = ?",
        (&key,),
        None,
    )? {
        if let Some(mut row) = cursor.next_row()? {
            value_type = read_string(&mut row, 1)?.trim().to_lowercase();
            active = read_opt_i32(&mut row, 2)?.unwrap_or(1) != 0;
            found = true;
        }
    }
    if !found || !active {
        return Ok(None);
    }

    match value_type.as_str() {
        "num" | "number" => {
            let n = number.ok_or("у команды тип «число», но число не услышано")?;
            let value = n as f64;
            connection.execute(
                "UPDATE dbo.CommandResults SET ValueNum = ?, Seq = Seq + 1, UpdatedAt = SYSUTCDATETIME() WHERE CmdKey = ?",
                (&value, &key),
                None,
            )?;
            value_type = "num".to_string();
        }
        "text" | "string" => {
            // Задел под будущие скрипты для ПК: пока пишем число или фразу команды.
            let text = sql_text(&number.map(|n| n.to_string()).unwrap_or_else(|| phrase.to_string()));
            connection.execute(
                &format!(
                    "UPDATE dbo.CommandResults SET ValueText = {text}, Seq = Seq + 1, UpdatedAt = SYSUTCDATETIME() WHERE CmdKey = ?"
                ),
                (&key,),
                None,
            )?;
            value_type = "text".to_string();
        }
        _ => {
            connection.execute(
                &format!(
                    "UPDATE dbo.CommandResults SET ValueBit = {}, Seq = Seq + 1, UpdatedAt = SYSUTCDATETIME() WHERE CmdKey = ?",
                    u8::from(bit_on)
                ),
                (&key,),
                None,
            )?;
            value_type = "bit".to_string();
        }
    }

    let mut seq = 0;
    if let Some(mut cursor) = connection.execute(
        "SELECT CAST(Seq AS BIGINT) FROM dbo.CommandResults WHERE CmdKey = ?",
        (&key,),
        None,
    )? {
        if let Some(mut row) = cursor.next_row()? {
            seq = read_opt_i64(&mut row, 1)?.unwrap_or(0);
        }
    }
    Ok(Some(Written { value_type, bit_on, seq }))
}

/// Через `delay` возвращает бит в 0, если за это время не пришла новая команда (Seq тот же).
pub fn schedule_bit_reset(sql: SqlConfig, cmd_key: String, seq: i64, delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let result = (|| -> Result<(), Box<dyn Error>> {
            let environment = Environment::new()?;
            let connection = environment
                .connect_with_connection_string(&sql.connection_string(), ConnectionOptions::default())?;
            let key = cmd_key.as_str().into_parameter();
            connection.execute(
                "UPDATE dbo.CommandResults SET ValueBit = 0, UpdatedAt = SYSUTCDATETIME() WHERE CmdKey = ? AND Seq = ?",
                (&key, &seq),
                None,
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("OPC: не удалось сбросить бит команды {cmd_key}: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pulse_two_seconds() {
        let c = OpcConfig::default();
        assert!(c.enabled);
        assert_eq!(c.bit_reset_s, 2.0);
    }

    #[test]
    fn off_commands_are_recognised_by_first_word() {
        let words = OpcConfig::default().off_words;
        assert!(is_off_command("Выключить Освещение зал Второй этаж", &words));
        assert!(!is_off_command("Включить Освещение зал Второй этаж", &words));
        assert!(!is_off_command("", &words));
    }

    #[test]
    fn sql_text_escapes_quotes() {
        assert_eq!(sql_text("it's"), "N'it''s'");
    }
}