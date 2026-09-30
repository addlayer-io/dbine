//! Decrypt a DBine backup file and print its content (JSON), without the
//! app: to check what it holds, or to recover data by hand.
//!
//!   DBINE_BACKUP_PASSPHRASE='…' cargo run -p dbine-sync --example open_backup -- dbine-backup.json
//!
//! Without the variable, the passphrase is read from stdin (one line).

use dbine_sync::crypto;
use std::io::BufRead;

fn main() {
    let path = std::env::args().nth(1).expect("uso: open_backup <archivo>");
    let file = std::fs::read(&path).expect("no se pudo leer el archivo");
    let header = crypto::peek(&file).unwrap_or_else(|e| panic!("{e}"));
    eprintln!("backup de {} ({}), DBine {}", header.device, header.updated_at, header.app_version);
    let pass = std::env::var("DBINE_BACKUP_PASSPHRASE").unwrap_or_else(|_| {
        eprint!("frase clave: ");
        let mut l = String::new();
        std::io::stdin().lock().read_line(&mut l).expect("stdin");
        l.trim_end_matches(['\r', '\n']).to_string()
    });
    let key = crypto::derive(&pass, &header.kdf).unwrap_or_else(|e| panic!("{e}"));
    let plain = crypto::open(&key, &file).unwrap_or_else(|e| panic!("{e}"));
    let v: serde_json::Value = serde_json::from_slice(&plain).expect("contenido inválido");
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
}
