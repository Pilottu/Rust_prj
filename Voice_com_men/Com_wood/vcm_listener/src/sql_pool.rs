#![allow(dead_code)]

// Загрузка пула команд из SQL Server (таблица dbo.CommandPool, её пишет основная
// программа) и построение индекса для сопоставления речи.

use std::collections::HashMap;
use std::error::Error;

use odbc_api::{ConnectionOptions, Cursor, Environment, Nullable};
use serde::Deserialize;
use vcm_norm::{CommandIndex, IndexEntry, Slot};

/// Секция [sql] файла config.toml. По умолчанию те же значения, что у основной программы.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct SqlConfig {
    pub server: String,
    pub database: String,
    pub driver: String,
    pub trusted_connection: bool,
}

impl Default for SqlConfig {
    fn default() -> Self {
        Self {
            server: r"localhost\TEW_SQLEXPRESS".to_string(),
            database: "vcm_state".to_string(),
            driver: "ODBC Driver 17 for SQL Server".to_string(),
            trusted_connection: true,
        }
    }
}

impl SqlConfig {
    pub fn connection_string(&self) -> String {
        format!(
            "Driver={{{}}};Server={};Database={};Trusted_Connection={};",
            self.driver,
            self.server,
            self.database,
            if self.trusted_connection { "Yes" } else { "No" }
        )
    }
}

pub struct LoadedPool {
    pub index: CommandIndex,
    /// CmdKey -> референтная фраза (строка с Rang = 0): её видит пользователь при привязке к тегу.
    pub reference: HashMap<String, String>,
    /// Слова команд по категориям (stem, категория) из dbo.CommandVocab; пусто, если таблицы ещё нет.
    pub vocab: Vec<(String, String)>,
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

pub fn load_pool(config: &SqlConfig) -> Result<LoadedPool, Box<dyn Error>> {
    let environment = Environment::new()?;
    let connection = environment
        .connect_with_connection_string(&config.connection_string(), ConnectionOptions::default())?;

    let mut index = CommandIndex::new();
    let mut reference = HashMap::new();
    let sql = "SELECT CmdKey, Vozmozhnie, Rang, MatchKey, SlotMin, SlotMax \
               FROM dbo.CommandPool WHERE MatchKey <> N''";
    if let Some(mut cursor) = connection.execute(sql, (), None)? {
        while let Some(mut row) = cursor.next_row()? {
            let cmd_key = read_string(&mut row, 1)?;
            let phrase = read_string(&mut row, 2)?;
            let rang = read_opt_i32(&mut row, 3)?.unwrap_or(0);
            let match_key = read_string(&mut row, 4)?;
            let slot_min = read_opt_i32(&mut row, 5)?;
            let slot_max = read_opt_i32(&mut row, 6)?;
            let slot = match (slot_min, slot_max) {
                (Some(a), Some(b)) if a >= 0 && b >= 0 => Some(Slot { min: a as u32, max: b as u32 }),
                _ => None,
            };
            if rang == 0 {
                reference.insert(cmd_key.clone(), phrase);
            }
            index.insert(IndexEntry { cmd_key, match_key, slot });
        }
    }

    // Словарь категорий нужен только для уточняющих вопросов: нет таблицы — просто не уточняем.
    let mut vocab: Vec<(String, String)> = Vec::new();
    if let Ok(Some(mut cursor)) = connection.execute("SELECT Stem, Category FROM dbo.CommandVocab", (), None) {
        while let Ok(Some(mut row)) = cursor.next_row() {
            let (Ok(stem), Ok(category)) = (read_string(&mut row, 1), read_string(&mut row, 2)) else {
                continue;
            };
            vocab.push((stem, category));
        }
    }
    Ok(LoadedPool { index, reference, vocab })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_main_program() {
        let c = SqlConfig::default();
        assert_eq!(
            c.connection_string(),
            r"Driver={ODBC Driver 17 for SQL Server};Server=localhost\TEW_SQLEXPRESS;Database=vcm_state;Trusted_Connection=Yes;"
        );
    }

    #[test]
    fn sql_auth_flag() {
        let c = SqlConfig { trusted_connection: false, ..SqlConfig::default() };
        assert!(c.connection_string().ends_with("Trusted_Connection=No;"));
    }
}
