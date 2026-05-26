use serde::Deserialize;
use shared::types::Payload;
use shared::{parse_payload_to_i16_and_key, vectorize_to_i16_and_key, Constants};
use std::path::PathBuf;

#[derive(Deserialize)]
struct TestFile<'a> {
    #[serde(borrow)]
    entries: Vec<Entry<'a>>,
}

#[derive(Deserialize)]
struct Entry<'a> {
    #[serde(borrow)]
    request: Payload<'a>,
}

fn main() {
    let queries = parse_arg("--queries").unwrap_or_else(|| "test/test-data.json".to_string());
    let data = std::fs::read_to_string(PathBuf::from(queries)).expect("failed to read queries");
    let parsed: TestFile<'_> = serde_json::from_str(&data).expect("failed to parse queries");
    let constants = Constants::load_embedded();

    let mut mismatches = 0usize;
    for (idx, entry) in parsed.entries.iter().enumerate() {
        let body = serde_json::to_vec(&entry.request).expect("failed to serialize request");
        let expected = vectorize_to_i16_and_key(&entry.request, &constants);
        let Some(actual) = parse_payload_to_i16_and_key(&body, &constants) else {
            eprintln!("fast parser failed at entry {}", idx);
            mismatches += 1;
            continue;
        };
        if actual != expected {
            eprintln!(
                "fast parser mismatch at entry {} expected={:?} actual={:?}",
                idx, expected, actual
            );
            mismatches += 1;
        }
    }

    println!(
        "{{\"queries\":{},\"mismatches\":{}}}",
        parsed.entries.len(),
        mismatches
    );
    if mismatches != 0 {
        std::process::exit(1);
    }
}

fn parse_arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
    }
    None
}
