use rusqlite::{params, Connection};

// Отдельный, не связанный с cad_state.db журнал событий приложения —
// только для чтения человеком постфактум (кто угодно может открыть
// historian.db в любом SQLite-просмотрщике и посмотреть, что происходило
// и когда). cad_state.db хранит ТЕКУЩЕЕ состояние дерева, эта база —
// историю событий с меткой времени, независимо от того, был ли открыт
// терминал при запуске.

const HISTORIAN_DB: &str = "historian.db";

fn open_and_prepare() -> Option<Connection> {
    let conn = Connection::open(HISTORIAN_DB).ok()?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL DEFAULT (datetime('now','localtime')),
            level TEXT NOT NULL,
            message TEXT NOT NULL
        )",
        [],
    )
    .ok()?;
    Some(conn)
}

// level: "INFO" / "WARN". Пишет и в консоль (если она есть — удобно при
// cargo run), и в historian.db (если базы нет — просто не пишет, не паникуя
// и не мешая работе приложения).
pub fn log(level: &str, message: &str) {
    eprintln!("[{level}] {message}");
    if let Some(conn) = open_and_prepare() {
        let _ = conn.execute(
            "INSERT INTO log (level, message) VALUES (?1, ?2)",
            params![level, message],
        );
    }
}
