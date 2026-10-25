use std::{env, ffi::OsString};

use serde_json::{Value, json};

fn os_string_json(value: OsString) -> Value {
    match value.into_string() {
        Ok(value) => Value::String(value),
        // Serde's native representation preserves non-Unicode bytes or UTF-16 code units.
        Err(value) => json!(value),
    }
}

fn main() {
    let argv = env::args_os().skip(1).map(os_string_json).collect::<Vec<_>>();
    // Entries preserve non-Unicode environment names, which cannot be JSON object keys.
    let env = env::vars_os()
        .map(|(name, value)| json!({ "name": os_string_json(name), "value": os_string_json(value) }))
        .collect::<Vec<_>>();
    println!("{}", json!({ "argv": argv, "env": env }));
}
