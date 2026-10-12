//! A fake language server scripted by `FAKE_LSP_DIR`, else `<root>/.fake-lsp`: see `scripted` and the flags.

use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use herdr_reviewr::lsp::rpc::{read_message, write_message};
use serde_json::{Value, json};

fn main() {
    let mut dir = std::env::var_os("FAKE_LSP_DIR").map(PathBuf::from);
    if dir.as_ref().is_some_and(|d| d.join("deaf").exists()) {
        std::thread::sleep(std::time::Duration::MAX);
    }
    let mut input = BufReader::new(std::io::stdin());
    let mut out = std::io::stdout();
    let mut docs: HashMap<String, String> = HashMap::new();
    let mut pid_written = false;
    while let Ok(Some(frame)) = read_message(&mut input) {
        // Without the env, the workspace root names the script: tests run side by side.
        let dir = dir.get_or_insert_with(|| {
            let root = frame["params"]["rootUri"].as_str().unwrap_or_default();
            let root = url::Url::parse(root).ok().and_then(|u| u.to_file_path().ok()).unwrap();
            root.join(".fake-lsp")
        });
        let _ = fs::create_dir_all(&*dir);
        if !std::mem::replace(&mut pid_written, true) {
            fs::write(dir.join("pid"), std::process::id().to_string()).unwrap();
        }
        let dir = dir.clone();
        let utf8 = dir.join("utf8").exists();
        let mut log = fs::OpenOptions::new().create(true).append(true).open(dir.join("lsp.log"));
        if let Ok(log) = log.as_mut() {
            let _ = writeln!(log, "{frame}");
        }
        let params = &frame["params"];
        let uri = params["textDocument"]["uri"].as_str().unwrap_or_default().to_string();
        let result = match frame["method"].as_str().unwrap_or_default() {
            "initialize" => json!({"capabilities": {
                "definitionProvider": true,
                "referencesProvider": true,
                "positionEncoding": if utf8 { "utf-8" } else { "utf-16" },
            }}),
            "textDocument/didOpen" => {
                let text = params["textDocument"]["text"].as_str().unwrap_or_default();
                docs.insert(uri, text.to_string());
                continue;
            }
            "textDocument/didChange" => {
                let text = params["contentChanges"][0]["text"].as_str().unwrap_or_default();
                docs.insert(uri, text.to_string());
                continue;
            }
            "textDocument/didClose" => {
                docs.remove(&uri);
                continue;
            }
            "exit" => return,
            "textDocument/definition" => {
                scripted(&dir, "definition.json", &docs, &uri, params, utf8)
            }
            "textDocument/references" => {
                scripted(&dir, "references.json", &docs, &uri, params, utf8)
            }
            _ => Value::Null,
        };
        let Some(id) = frame.get("id") else { continue };
        if frame["method"] == "initialize" && dir.join("slow_init").exists() {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if dir.join("silent").exists() && frame["method"] != "initialize" {
            continue;
        }
        if dir.join("chatty").exists() {
            let ask = json!({"jsonrpc": "2.0", "id": "ask", "method": "workspace/configuration",
                "params": {"items": [{}]}});
            let note = json!({"jsonrpc": "2.0", "method": "window/logMessage",
                "params": {"type": 3, "message": "busy"}});
            write_message(&mut out, &ask).unwrap();
            write_message(&mut out, &note).unwrap();
        }
        write_message(&mut out, &json!({"jsonrpc": "2.0", "id": id, "result": result})).unwrap();
    }
}

/// The answer in `file` for the word at the asked position of the text it was sent, never the disk.
fn scripted(
    dir: &Path,
    file: &str,
    docs: &HashMap<String, String>,
    uri: &str,
    params: &Value,
    utf8: bool,
) -> Value {
    let script: Value = fs::read_to_string(dir.join(file))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let line = params["position"]["line"].as_u64().unwrap_or(0) as usize;
    let unit = params["position"]["character"].as_u64().unwrap_or(0) as usize;
    let text = docs.get(uri).and_then(|t| t.lines().nth(line)).unwrap_or_default();
    // Code units of the advertised encoding to a char index.
    let mut units = 0;
    let at = text.chars().take_while(|c| {
        units += if utf8 { c.len_utf8() } else { c.len_utf16() };
        units <= unit
    });
    let at = at.count();
    let chars: Vec<char> = text.chars().collect();
    let word = |c: &char| c.is_alphanumeric() || *c == '_';
    let start = chars[..at.min(chars.len())].iter().rev().take_while(|c| word(c)).count();
    let start = at.min(chars.len()) - start;
    let end = start + chars[start..].iter().take_while(|c| word(c)).count();
    let name: String = chars[start..end].iter().collect();
    script.get(&name).cloned().unwrap_or(Value::Null)
}
