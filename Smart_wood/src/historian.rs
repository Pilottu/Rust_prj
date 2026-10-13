// SQL Server is the durable history store. Console output remains useful for cargo run.
pub fn log(level: &str, message: &str) {
    eprintln!("[{level}] {message}");
    if let Err(error) = crate::repository::write_history_event(level, message) {
        eprintln!("[WARN] Не удалось записать событие в SQL Server: {error}");
    }
}
