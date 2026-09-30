//! Try a provider from the command line (development):
//!
//!   cargo run -p dbine-ai --features embedded --example ask -- embedded qwen2.5-coder-3b "pregunta"
//!   cargo run -p dbine-ai --example ask -- claude_code haiku "pregunta"
//!   cargo run -p dbine-ai --example ask -- detect
//!
//! The built-in model is downloaded first if missing (DBINE_MODELS_DIR, or
//! the app's models folder).

use dbine_ai::{embedded, Cancel, ChatMessage, ChatRequest, Delta, Endpoints, ProviderKind};
use std::io::Write;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut ep = Endpoints::default();
    ep.models_dir = std::env::var("DBINE_MODELS_DIR").map(Into::into).unwrap_or_else(|_| {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/com.addlayer.dbine/models")
    });
    if args.first().map(String::as_str) == Some("detect") {
        println!("{}", serde_json::to_string_pretty(&dbine_ai::detect(&ep).await).unwrap());
        return;
    }
    let kind: ProviderKind = serde_json::from_value(serde_json::Value::String(args[0].clone())).expect("proveedor");
    let model = args.get(1).cloned();
    let question = args.get(2).cloned().unwrap_or_else(|| "Listá los clientes con más de 5 pedidos en 2025.".into());
    if kind == ProviderKind::Embedded {
        let m = embedded::catalog_model(model.as_deref().unwrap_or_default()).expect("modelo del catálogo");
        let t = std::time::Instant::now();
        embedded::download(&ep.models_dir, m, &|d, total| eprint!("\rdescargando {:.1}%", d as f64 * 100.0 / total as f64), &Cancel::new())
            .await
            .expect("descarga");
        eprintln!("\nmodelo listo en {:.1}s", t.elapsed().as_secs_f64());
    }
    let req = ChatRequest {
        kind,
        model,
        system: "Sos un asistente de SQL Server. Esquema:\nclientes(id int PK, nombre nvarchar(120), ciudad nvarchar(80))\n\
                 pedidos(id int PK, cliente_id int FK->clientes.id, fecha date, total decimal(14,2))\n\
                 Respondé con un bloque ```sql y una línea de explicación."
            .into(),
        messages: vec![ChatMessage { role: "user".into(), content: question }],
    };
    let t = std::time::Instant::now();
    let first = std::sync::Mutex::new(None::<f64>);
    let out = dbine_ai::chat(
        &req,
        &ep,
        &|d| {
            if let Delta::Text(t2) = d {
                first.lock().unwrap().get_or_insert(t.elapsed().as_secs_f64());
                print!("{t2}");
                let _ = std::io::stdout().flush();
            }
        },
        &Cancel::new(),
    )
    .await;
    println!();
    match out {
        Ok(text) => eprintln!("--- {} caracteres · primer texto a los {:.2}s · total {:.2}s", text.len(), first.lock().unwrap().unwrap_or(0.0), t.elapsed().as_secs_f64()),
        Err(e) => eprintln!("ERROR: {e}"),
    }
    embedded::shutdown();
}
