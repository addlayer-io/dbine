#![no_main]
//! Connections pasted by the user, one per line
//! (`dbine_core::conn_import::read_text`): engine URLs, JDBC URLs, SQL
//! Server connection strings and SQLite / DuckDB file paths. Pure: it
//! reads no file and no keychain.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let found = dbine_core::conn_import::read_text(&text);
    for c in &found.candidates {
        let _ = c.has_secret();
    }
});
