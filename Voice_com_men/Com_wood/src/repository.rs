use std::{
    collections::HashMap,
    error::Error,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use configparser::ini::Ini;
use odbc_api::{
    Connection, ConnectionOptions, Cursor, Environment, IntoParameter, Nullable, U16String,
};
use rusqlite::{Connection as SqliteConnection, OpenFlags};

use crate::state::{AppState, CADLink, CADTree, PersistedAppState, RawItem, RawLink};

struct SqlServerConfig {
    server: String,
    database: String,
    driver: String,
    trusted_connection: bool,
}

impl Default for SqlServerConfig {
    fn default() -> Self {
        Self {
            server: r"localhost\TEW_SQLEXPRESS".to_string(),
            database: "vcm_state".to_string(),
            driver: "ODBC Driver 17 for SQL Server".to_string(),
            trusted_connection: true,
        }
    }
}

static ENVIRONMENT: OnceLock<Environment> = OnceLock::new();
static CONNECTION: OnceLock<Mutex<Connection<'static>>> = OnceLock::new();
static HISTORY_CONNECTION: OnceLock<Mutex<Connection<'static>>> = OnceLock::new();
static LAST_SAVED_STATE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static CONFIG: OnceLock<SqlServerConfig> = OnceLock::new();

fn config() -> &'static SqlServerConfig {
    CONFIG.get_or_init(load_config)
}

fn project_root() -> PathBuf {
    let executable = std::env::current_exe().ok();
    executable
        .as_deref()
        .and_then(|path| path.ancestors().find(|dir| dir.join("Cargo.toml").exists()))
        .map(Path::to_path_buf)
        .or_else(|| executable.and_then(|path| path.parent().map(Path::to_path_buf)))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

fn load_config() -> SqlServerConfig {
    let mut settings = SqlServerConfig::default();
    let path = project_root().join("config.ini");
    if !path.exists() {
        return settings;
    }

    let mut ini = Ini::new();
    if let Err(error) = ini.load(&path) {
        eprintln!("[WARN] Не удалось прочитать config.ini: {error}");
        return settings;
    }

    if let Some(value) = ini.get("sqlserver", "server") {
        settings.server = value;
    }
    if let Some(value) = ini.get("sqlserver", "database") {
        settings.database = value;
    }
    if let Some(value) = ini.get("sqlserver", "driver") {
        settings.driver = value;
    }
    match ini.getboolcoerce("sqlserver", "trusted_connection") {
        Ok(Some(value)) => settings.trusted_connection = value,
        Ok(None) => {}
        Err(error) => eprintln!("[WARN] Некорректный trusted_connection в config.ini: {error}"),
    }
    settings
}

fn environment() -> Result<&'static Environment, Box<dyn Error>> {
    if let Some(environment) = ENVIRONMENT.get() {
        return Ok(environment);
    }
    let environment = Environment::new()?;
    let _ = ENVIRONMENT.set(environment);
    ENVIRONMENT
        .get()
        .ok_or_else(|| "ODBC environment initialization failed".into())
}

fn connection_string(config: &SqlServerConfig, database: &str) -> String {
    format!(
        "Driver={{{}}};Server={};Database={};Trusted_Connection={};",
        config.driver,
        config.server,
        database,
        if config.trusted_connection {
            "Yes"
        } else {
            "No"
        }
    )
}

fn connect(database: &str) -> Result<Connection<'static>, Box<dyn Error>> {
    Ok(environment()?.connect_with_connection_string(
        &connection_string(config(), database),
        ConnectionOptions::default(),
    )?)
}

fn execute(connection: &Connection<'_>, sql: &str) -> Result<(), Box<dyn Error>> {
    connection.execute(sql, ())?;
    Ok(())
}

fn ensure_database() -> Result<(), Box<dyn Error>> {
    let database = &config().database;
    if database.is_empty()
        || !database
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        || !database
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
    {
        return Err(format!("Недопустимое имя SQL-базы: {database}").into());
    }
    let master = connect("master")?;
    execute(
        &master,
        &format!("IF DB_ID(N'{database}') IS NULL CREATE DATABASE [{database}];"),
    )?;
    Ok(())
}

fn ensure_schema(connection: &Connection<'_>) -> Result<(), Box<dyn Error>> {
    execute(
        connection,
        r#"
IF OBJECT_ID(N'dbo.AppSettings', N'U') IS NULL
    CREATE TABLE dbo.AppSettings (
        Id TINYINT NOT NULL PRIMARY KEY CHECK (Id = 1),
        SelectedNodeId INT NOT NULL,
        ActiveFocusTreeId INT NOT NULL
    );
IF OBJECT_ID(N'dbo.ApplicationLog', N'U') IS NULL
    CREATE TABLE dbo.ApplicationLog (
        LogId BIGINT IDENTITY(1,1) NOT NULL PRIMARY KEY,
        OccurredAt NVARCHAR(32) NOT NULL
            CONSTRAINT DF_ApplicationLog_OccurredAt DEFAULT (CONVERT(NVARCHAR(19), GETDATE(), 120)),
        Level NVARCHAR(16) NOT NULL,
        Message NVARCHAR(MAX) NOT NULL
    );
IF OBJECT_ID(N'dbo.CadTrees', N'U') IS NULL
    CREATE TABLE dbo.CadTrees (
        TreeId INT NOT NULL PRIMARY KEY,
        CadX REAL NOT NULL,
        CadY REAL NOT NULL,
        Width REAL NOT NULL,
        NodeCount INT NOT NULL
    );
IF OBJECT_ID(N'dbo.CadNodes', N'U') IS NULL
    CREATE TABLE dbo.CadNodes (
        NodeId INT NOT NULL PRIMARY KEY,
        TreeId INT NOT NULL REFERENCES dbo.CadTrees(TreeId),
        NodeOrder INT NOT NULL,
        NodeText NVARCHAR(MAX) NOT NULL,
        ParentNodeId INT NOT NULL,
        IsExpanded BIT NOT NULL,
        IsEditing BIT NOT NULL,
        IsCommand BIT NOT NULL CONSTRAINT DF_CadNodes_IsCommand DEFAULT (0),
        IsCommandManual BIT NOT NULL CONSTRAINT DF_CadNodes_IsCommandManual DEFAULT (0)
    );
IF COL_LENGTH(N'dbo.CadNodes', N'IsCommand') IS NULL
    ALTER TABLE dbo.CadNodes ADD IsCommand BIT NOT NULL
        CONSTRAINT DF_CadNodes_IsCommand DEFAULT (0) WITH VALUES;
IF COL_LENGTH(N'dbo.CadNodes', N'IsCommandManual') IS NULL
    ALTER TABLE dbo.CadNodes ADD IsCommandManual BIT NOT NULL
        CONSTRAINT DF_CadNodes_IsCommandManual DEFAULT (0) WITH VALUES;
IF OBJECT_ID(N'dbo.CommandWords', N'U') IS NULL
    CREATE TABLE dbo.CommandWords (
        Word NVARCHAR(100) NOT NULL PRIMARY KEY,
        Topic NVARCHAR(32) NOT NULL CONSTRAINT DF_CommandWords_Topic DEFAULT (N'general')
    );
IF COL_LENGTH(N'dbo.CommandWords', N'Topic') IS NULL
    ALTER TABLE dbo.CommandWords ADD Topic NVARCHAR(32) NOT NULL
        CONSTRAINT DF_CommandWords_Topic DEFAULT (N'general') WITH VALUES;
IF OBJECT_ID(N'dbo.TreeNotes', N'U') IS NULL
    CREATE TABLE dbo.TreeNotes (
        TreeId INT NOT NULL REFERENCES dbo.CadTrees(TreeId),
        NoteOrder INT NOT NULL,
        NoteText NVARCHAR(MAX) NOT NULL,
        PRIMARY KEY (TreeId, NoteOrder)
    );
IF OBJECT_ID(N'dbo.NodeNotes', N'U') IS NULL
    CREATE TABLE dbo.NodeNotes (
        NodeId INT NOT NULL REFERENCES dbo.CadNodes(NodeId),
        NoteOrder INT NOT NULL,
        NoteText NVARCHAR(MAX) NOT NULL,
        PRIMARY KEY (NodeId, NoteOrder)
    );
IF OBJECT_ID(N'dbo.NodeLinks', N'U') IS NULL
    CREATE TABLE dbo.NodeLinks (
        SourceNodeId INT NOT NULL REFERENCES dbo.CadNodes(NodeId),
        LinkId INT NOT NULL,
        LinkOrder INT NOT NULL,
        TargetNodeId INT NOT NULL REFERENCES dbo.CadNodes(NodeId),
        TargetNodeName NVARCHAR(MAX) NOT NULL,
        PRIMARY KEY (SourceNodeId, LinkId)
    );
IF OBJECT_ID(N'dbo.CadLinks', N'U') IS NULL
    CREATE TABLE dbo.CadLinks (
        LinkId INT NOT NULL PRIMARY KEY,
        Node1Id INT NOT NULL REFERENCES dbo.CadNodes(NodeId),
        Node2Id INT NOT NULL REFERENCES dbo.CadNodes(NodeId),
        X1 REAL NOT NULL,
        Y1 REAL NOT NULL,
        X2 REAL NOT NULL,
        Y2 REAL NOT NULL,
        TreeId1 INT NOT NULL REFERENCES dbo.CadTrees(TreeId),
        TreeId2 INT NOT NULL REFERENCES dbo.CadTrees(TreeId),
        Side1 INT NOT NULL,
        Side2 INT NOT NULL,
        MidX REAL NOT NULL
    );
"#,
    )?;
    Ok(())
}

pub fn add_command_word(word: &str, topic: &str) -> Result<bool, Box<dyn Error>> {
    let Some(connection) = CONNECTION.get() else {
        return Err("SQL command dictionary connection is not initialized".into());
    };
    let word_parameter = bind_text(word);
    let topic_parameter = bind_text(topic);
    let connection = connection
        .lock()
        .map_err(|_| "SQL connection lock poisoned")?;
    let mut cursor = connection
        .execute(
            "SET NOCOUNT ON; IF NOT EXISTS (SELECT 1 FROM dbo.CommandWords WHERE Word = ?) BEGIN INSERT INTO dbo.CommandWords (Word, Topic) VALUES (?, ?); SELECT CAST(1 AS INT); END ELSE SELECT CAST(0 AS INT)",
            (&word_parameter, &word_parameter, &topic_parameter),
        )?
        .ok_or("SQL Server did not return the dictionary insert result")?;
    let mut row = cursor
        .next_row()?
        .ok_or("SQL Server returned no dictionary insert result")?;
    Ok(read_i32(&mut row, 1)? != 0)
}

pub fn rename_command_word(old_word: &str, new_word: &str) -> Result<bool, Box<dyn Error>> {
    let Some(connection) = CONNECTION.get() else {
        return Err("SQL command dictionary connection is not initialized".into());
    };
    let old_parameter = bind_text(old_word);
    let new_parameter = bind_text(new_word);
    let connection = connection
        .lock()
        .map_err(|_| "SQL connection lock poisoned")?;
    let mut cursor = connection
        .execute(
            "SET NOCOUNT ON; IF ? <> ? AND EXISTS (SELECT 1 FROM dbo.CommandWords WHERE Word = ?) BEGIN IF EXISTS (SELECT 1 FROM dbo.CommandWords WHERE Word = ?) DELETE FROM dbo.CommandWords WHERE Word = ? ELSE UPDATE dbo.CommandWords SET Word = ? WHERE Word = ?; SELECT CAST(1 AS INT); END ELSE SELECT CAST(0 AS INT)",
            (&old_parameter, &new_parameter, &old_parameter, &new_parameter, &old_parameter, &new_parameter, &old_parameter),
        )?
        .ok_or("SQL Server did not return the dictionary rename result")?;
    let mut row = cursor
        .next_row()?
        .ok_or("SQL Server returned no dictionary rename result")?;
    Ok(read_i32(&mut row, 1)? != 0)
}

pub fn remove_command_word(word: &str) -> Result<bool, Box<dyn Error>> {
    let Some(connection) = CONNECTION.get() else {
        return Err("SQL command dictionary connection is not initialized".into());
    };
    let word_parameter = bind_text(word);
    let connection = connection
        .lock()
        .map_err(|_| "SQL connection lock poisoned")?;
    let mut cursor = connection
        .execute(
            "SET NOCOUNT ON; IF EXISTS (SELECT 1 FROM dbo.CommandWords WHERE Word = ?) BEGIN DELETE FROM dbo.CommandWords WHERE Word = ?; SELECT CAST(1 AS INT); END ELSE SELECT CAST(0 AS INT)",
            (&word_parameter, &word_parameter),
        )?
        .ok_or("SQL Server did not return the dictionary delete result")?;
    let mut row = cursor
        .next_row()?
        .ok_or("SQL Server returned no dictionary delete result")?;
    Ok(read_i32(&mut row, 1)? != 0)
}

fn load_command_dictionary(connection: &Connection<'_>) -> Result<Vec<String>, Box<dyn Error>> {
    let mut words = Vec::new();
    if let Some(mut cursor) = connection.execute("SELECT Word FROM dbo.CommandWords", ())? {
        while let Some(mut row) = cursor.next_row()? {
            words.push(read_string(&mut row, 1)?);
        }
    }
    Ok(words)
}

fn read_i32(row: &mut odbc_api::CursorRow<'_>, column: u16) -> Result<i32, Box<dyn Error>> {
    let mut value = Nullable::<i32>::null();
    row.get_data(column, &mut value)?;
    value
        .into_opt()
        .ok_or_else(|| format!("Unexpected NULL in column {column}").into())
}

fn read_f32(row: &mut odbc_api::CursorRow<'_>, column: u16) -> Result<f32, Box<dyn Error>> {
    let mut value = Nullable::<f32>::null();
    row.get_data(column, &mut value)?;
    value
        .into_opt()
        .ok_or_else(|| format!("Unexpected NULL in column {column}").into())
}

fn read_bool(row: &mut odbc_api::CursorRow<'_>, column: u16) -> Result<bool, Box<dyn Error>> {
    Ok(read_i32(row, column)? != 0)
}

fn read_string(row: &mut odbc_api::CursorRow<'_>, column: u16) -> Result<String, Box<dyn Error>> {
    let mut value = Vec::new();
    row.get_wide_text(column, &mut value)?;
    Ok(String::from_utf16_lossy(&value))
}

fn read_state(connection: &Connection<'_>) -> Result<Option<PersistedAppState>, Box<dyn Error>> {
    let mut trees = Vec::<CADTree>::new();
    let mut tree_indexes = HashMap::new();
    if let Some(mut cursor) = connection.execute(
        "SELECT TreeId, CadX, CadY, Width, NodeCount FROM dbo.CadTrees ORDER BY TreeId",
        (),
    )? {
        while let Some(mut row) = cursor.next_row()? {
            let id = read_i32(&mut row, 1)?;
            tree_indexes.insert(id, trees.len());
            trees.push(CADTree {
                id,
                cad_x: read_f32(&mut row, 2)?,
                cad_y: read_f32(&mut row, 3)?,
                width: read_f32(&mut row, 4)?,
                count: read_i32(&mut row, 5)?,
                items: Vec::new(),
                notes: Vec::new(),
            });
        }
    }
    if trees.is_empty() {
        return Ok(None);
    }

    let mut node_indexes = HashMap::new();
    if let Some(mut cursor) = connection.execute(
        "SELECT NodeId, TreeId, NodeText, ParentNodeId, IsExpanded, IsEditing, IsCommand, IsCommandManual FROM dbo.CadNodes ORDER BY TreeId, NodeOrder",
        (),
    )? {
        while let Some(mut row) = cursor.next_row()? {
            let node_id = read_i32(&mut row, 1)?;
            let tree_id = read_i32(&mut row, 2)?;
            let tree_index = *tree_indexes
                .get(&tree_id)
                .ok_or_else(|| format!("Node {node_id} references missing tree {tree_id}"))?;
            let item_index = trees[tree_index].items.len();
            trees[tree_index].items.push(RawItem {
                id: node_id,
                text: read_string(&mut row, 3)?,
                parent_id: read_i32(&mut row, 4)?,
                is_expanded: read_bool(&mut row, 5)?,
                is_editing: read_bool(&mut row, 6)?,
                is_command: read_bool(&mut row, 7)?,
                is_command_manual: read_bool(&mut row, 8)?,
                notes: Vec::new(),
                links: Vec::new(),
            });
            node_indexes.insert(node_id, (tree_index, item_index));
        }
    }

    if let Some(mut cursor) = connection.execute(
        "SELECT TreeId, NoteText FROM dbo.TreeNotes ORDER BY TreeId, NoteOrder",
        (),
    )? {
        while let Some(mut row) = cursor.next_row()? {
            let tree_id = read_i32(&mut row, 1)?;
            if let Some(tree_index) = tree_indexes.get(&tree_id) {
                trees[*tree_index].notes.push(read_string(&mut row, 2)?);
            }
        }
    }

    if let Some(mut cursor) = connection.execute(
        "SELECT NodeId, NoteText FROM dbo.NodeNotes ORDER BY NodeId, NoteOrder",
        (),
    )? {
        while let Some(mut row) = cursor.next_row()? {
            let node_id = read_i32(&mut row, 1)?;
            if let Some((tree_index, item_index)) = node_indexes.get(&node_id) {
                trees[*tree_index].items[*item_index]
                    .notes
                    .push(read_string(&mut row, 2)?);
            }
        }
    }

    if let Some(mut cursor) = connection.execute(
        "SELECT SourceNodeId, LinkId, TargetNodeId, TargetNodeName FROM dbo.NodeLinks ORDER BY SourceNodeId, LinkOrder",
        (),
    )? {
        while let Some(mut row) = cursor.next_row()? {
            let source_id = read_i32(&mut row, 1)?;
            if let Some((tree_index, item_index)) = node_indexes.get(&source_id) {
                trees[*tree_index].items[*item_index].links.push(RawLink {
                    id: read_i32(&mut row, 2)?,
                    target_node_id: read_i32(&mut row, 3)?,
                    target_node_name: read_string(&mut row, 4)?,
                });
            }
        }
    }

    let mut cad_links = Vec::new();
    if let Some(mut cursor) = connection.execute(
        "SELECT LinkId, Node1Id, Node2Id, X1, Y1, X2, Y2, TreeId1, TreeId2, Side1, Side2, MidX FROM dbo.CadLinks ORDER BY LinkId",
        (),
    )? {
        while let Some(mut row) = cursor.next_row()? {
            cad_links.push(CADLink {
                id: read_i32(&mut row, 1)?,
                id_obj1: read_i32(&mut row, 2)?,
                id_obj2: read_i32(&mut row, 3)?,
                x1: read_f32(&mut row, 4)?,
                y1: read_f32(&mut row, 5)?,
                x2: read_f32(&mut row, 6)?,
                y2: read_f32(&mut row, 7)?,
                tree_id_1: read_i32(&mut row, 8)?,
                tree_id_2: read_i32(&mut row, 9)?,
                side1: read_i32(&mut row, 10)?,
                side2: read_i32(&mut row, 11)?,
                mid_x: read_f32(&mut row, 12)?,
            });
        }
    }

    let mut selected_node_id = 1;
    let mut active_focus_tree_id = trees[0].id;
    if let Some(mut cursor) = connection.execute(
        "SELECT SelectedNodeId, ActiveFocusTreeId FROM dbo.AppSettings WHERE Id = 1",
        (),
    )? {
        if let Some(mut row) = cursor.next_row()? {
            selected_node_id = read_i32(&mut row, 1)?;
            active_focus_tree_id = read_i32(&mut row, 2)?;
        }
    }

    Ok(Some(PersistedAppState {
        trees,
        cad_links,
        selected_node_id,
        active_focus_tree_id,
    }))
}

fn bind_text(value: &str) -> <U16String as IntoParameter>::Parameter {
    U16String::from_str(value).into_parameter()
}

fn save_state(
    connection: &Connection<'_>,
    state: &PersistedAppState,
) -> Result<(), Box<dyn Error>> {
    connection.set_autocommit(false)?;
    let result = (|| -> Result<(), Box<dyn Error>> {
        execute(
            connection,
            "DELETE FROM dbo.NodeLinks; DELETE FROM dbo.CadLinks; DELETE FROM dbo.NodeNotes; DELETE FROM dbo.TreeNotes; DELETE FROM dbo.CadNodes; DELETE FROM dbo.CadTrees;",
        )?;

        for tree in &state.trees {
            connection.execute(
                "INSERT INTO dbo.CadTrees (TreeId, CadX, CadY, Width, NodeCount) VALUES (?, ?, ?, ?, ?)",
                (&tree.id, &tree.cad_x, &tree.cad_y, &tree.width, &tree.count),
            )?;
        }

        for tree in &state.trees {
            for (order, item) in tree.items.iter().enumerate() {
                let text = bind_text(&item.text);
                let is_expanded = i32::from(item.is_expanded);
                let is_editing = i32::from(item.is_editing);
                let is_command_manual = i32::from(item.is_command_manual);
                connection.execute(
                    "INSERT INTO dbo.CadNodes (NodeId, TreeId, NodeOrder, NodeText, ParentNodeId, IsExpanded, IsEditing, IsCommand, IsCommandManual) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (&item.id, &tree.id, &(order as i32), &text, &item.parent_id, &is_expanded, &is_editing, &i32::from(item.is_command), &is_command_manual),
                )?;
            }
        }

        for tree in &state.trees {
            for (order, note) in tree.notes.iter().enumerate() {
                let note = bind_text(note);
                connection.execute(
                    "INSERT INTO dbo.TreeNotes (TreeId, NoteOrder, NoteText) VALUES (?, ?, ?)",
                    (&tree.id, &(order as i32), &note),
                )?;
            }
            for item in &tree.items {
                for (order, note) in item.notes.iter().enumerate() {
                    let note = bind_text(note);
                    connection.execute(
                        "INSERT INTO dbo.NodeNotes (NodeId, NoteOrder, NoteText) VALUES (?, ?, ?)",
                        (&item.id, &(order as i32), &note),
                    )?;
                }
                for (order, link) in item.links.iter().enumerate() {
                    let target_name = bind_text(&link.target_node_name);
                    connection.execute(
                        "INSERT INTO dbo.NodeLinks (SourceNodeId, LinkId, LinkOrder, TargetNodeId, TargetNodeName) VALUES (?, ?, ?, ?, ?)",
                        (&item.id, &link.id, &(order as i32), &link.target_node_id, &target_name),
                    )?;
                }
            }
        }

        for link in &state.cad_links {
            connection.execute(
                "INSERT INTO dbo.CadLinks (LinkId, Node1Id, Node2Id, X1, Y1, X2, Y2, TreeId1, TreeId2, Side1, Side2, MidX) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (&link.id, &link.id_obj1, &link.id_obj2, &link.x1, &link.y1, &link.x2, &link.y2, &link.tree_id_1, &link.tree_id_2, &link.side1, &link.side2, &link.mid_x),
            )?;
        }

        connection.execute(
            "IF EXISTS (SELECT 1 FROM dbo.AppSettings WHERE Id = 1) UPDATE dbo.AppSettings SET SelectedNodeId = ?, ActiveFocusTreeId = ? WHERE Id = 1 ELSE INSERT INTO dbo.AppSettings (Id, SelectedNodeId, ActiveFocusTreeId) VALUES (1, ?, ?)",
            (&state.selected_node_id, &state.active_focus_tree_id, &state.selected_node_id, &state.active_focus_tree_id),
        )?;
        Ok(())
    })();

    match result {
        Ok(()) => connection.commit()?,
        Err(error) => {
            let _ = connection.rollback();
            let _ = connection.set_autocommit(true);
            return Err(error);
        }
    }
    connection.set_autocommit(true)?;
    Ok(())
}

fn load_legacy_state(path: &Path) -> Result<Option<PersistedAppState>, Box<dyn Error>> {
    if !path.exists() {
        return Ok(None);
    }
    let connection = SqliteConnection::open(path)?;
    let payload = connection
        .query_row("SELECT payload FROM app_state WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .ok();
    payload
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

fn migrate_legacy_history(
    connection: &Connection<'_>,
    path: &Path,
) -> Result<usize, Box<dyn Error>> {
    if !path.exists() {
        return Ok(0);
    }
    let history_count = {
        let mut count_cursor = connection
            .execute("SELECT COUNT(*) FROM dbo.ApplicationLog", ())?
            .ok_or("SQL Server did not return the history row count")?;
        let mut count_row = count_cursor
            .next_row()?
            .ok_or("SQL Server returned no history count row")?;
        read_i32(&mut count_row, 1)?
    };
    if history_count > 0 {
        return Ok(0);
    }

    let legacy = SqliteConnection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = legacy.prepare("SELECT ts, level, message FROM log ORDER BY id")?;
    let mut rows = statement.query([])?;

    connection.set_autocommit(false)?;
    let imported = (|| -> Result<usize, Box<dyn Error>> {
        let mut imported = 0;
        while let Some(row) = rows.next()? {
            let occurred_at: String = row.get(0)?;
            let level: String = row.get(1)?;
            let message: String = row.get(2)?;
            let occurred_at = bind_text(&occurred_at);
            let level = bind_text(&level);
            let message = bind_text(&message);
            connection.execute(
                "INSERT INTO dbo.ApplicationLog (OccurredAt, Level, Message) VALUES (?, ?, ?)",
                (&occurred_at, &level, &message),
            )?;
            imported += 1;
        }
        Ok(imported)
    })();

    match imported {
        Ok(count) => {
            connection.commit()?;
            connection.set_autocommit(true)?;
            Ok(count)
        }
        Err(error) => {
            let _ = connection.rollback();
            let _ = connection.set_autocommit(true);
            Err(error)
        }
    }
}

fn history_connection() -> Result<&'static Mutex<Connection<'static>>, Box<dyn Error>> {
    if let Some(connection) = HISTORY_CONNECTION.get() {
        return Ok(connection);
    }
    let connection = Mutex::new(connect(&config().database)?);
    let _ = HISTORY_CONNECTION.set(connection);
    HISTORY_CONNECTION
        .get()
        .ok_or_else(|| "SQL history connection initialization failed".into())
}

pub(crate) fn write_history_event(level: &str, message: &str) -> Result<(), Box<dyn Error>> {
    let connection = history_connection()?;
    let level = bind_text(level);
    let message = bind_text(message);
    let connection = connection
        .lock()
        .map_err(|_| "SQL history connection lock poisoned")?;
    connection.execute(
        "INSERT INTO dbo.ApplicationLog (Level, Message) VALUES (?, ?)",
        (&level, &message),
    )?;
    Ok(())
}

fn to_persisted_state(state: &AppState) -> PersistedAppState {
    PersistedAppState {
        trees: state.trees.clone(),
        cad_links: state.cad_links.clone(),
        selected_node_id: state.selected_node_id,
        active_focus_tree_id: state.active_focus_tree_id,
    }
}

pub fn load_initial_state() -> AppState {
    let loaded = (|| -> Result<Option<PersistedAppState>, Box<dyn Error>> {
        ensure_database()?;
        let connection = connect(&config().database)?;
        ensure_schema(&connection)?;
        crate::command_classifier::initialize_dictionary(load_command_dictionary(&connection)?);

        if let Err(error) =
            migrate_legacy_history(&connection, &project_root().join("historian.db"))
        {
            eprintln!("[ERROR] Не удалось перенести historian.db в SQL Server: {error}");
        }

        if let Some(state) = read_state(&connection)? {
            return Ok(Some(state));
        }

        let legacy_state_path = project_root().join(format!("{}.db", config().database));
        if let Some(state) = load_legacy_state(&legacy_state_path)? {
            save_state(&connection, &state)?;
            crate::historian::log(
                "INFO",
                &format!(
                    "Состояние перенесено из {} в SQL Server Express",
                    legacy_state_path.display()
                ),
            );
            return Ok(Some(state));
        }

        Ok(None)
    })();

    match loaded {
        Ok(Some(state)) => {
            if let Ok(snapshot) = serde_json::to_string(&state) {
                let _ = LAST_SAVED_STATE.set(Mutex::new(Some(snapshot)));
            }
            match connect(&config().database) {
                Ok(connection) => {
                    let _ = CONNECTION.set(Mutex::new(connection));
                }
                Err(error) => crate::historian::log(
                    "WARN",
                    &format!("Не удалось открыть постоянное подключение SQL Server: {error}"),
                ),
            }
            crate::historian::log(
                "INFO",
                &format!(
                    "Загружено из SQL Server: деревьев={}, связей={}",
                    state.trees.len(),
                    state.cad_links.len()
                ),
            );
            state.into_app_state()
        }
        Ok(None) => {
            if let Ok(connection) = connect(&config().database) {
                let _ = CONNECTION.set(Mutex::new(connection));
            }
            crate::historian::log("INFO", "В SQL Server пока нет CAD-состояния");
            AppState::default()
        }
        Err(error) => {
            crate::historian::log(
                "ERROR",
                &format!("Не удалось загрузить состояние из SQL Server: {error}"),
            );
            AppState::default()
        }
    }
}

pub fn save_persisted_state(state: &AppState) {
    let Some(connection) = CONNECTION.get() else {
        return;
    };
    let persisted = to_persisted_state(state);
    let Ok(snapshot) = serde_json::to_string(&persisted) else {
        return;
    };
    let cache = LAST_SAVED_STATE.get_or_init(|| Mutex::new(None));
    let Ok(mut last_saved) = cache.lock() else {
        return;
    };
    if last_saved.as_deref() == Some(&snapshot) {
        return;
    }

    match connection.lock() {
        Ok(connection) => match save_state(&connection, &persisted) {
            Ok(()) => *last_saved = Some(snapshot),
            Err(error) => crate::historian::log(
                "ERROR",
                &format!("Не удалось записать CAD-состояние в SQL Server: {error}"),
            ),
        },
        Err(_) => crate::historian::log("ERROR", "Не удалось получить SQL-соединение"),
    }
}
