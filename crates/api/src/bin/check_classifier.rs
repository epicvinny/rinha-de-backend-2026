#[path = "../classifier.rs"]
mod classifier;
#[path = "../tree_model.rs"]
mod tree_model;

use serde::Deserialize;
use shared::types::Payload;
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
    expected_approved: bool,
}

fn main() {
    let queries = parse_arg("--queries").unwrap_or_else(|| "test/test-data.json".to_string());
    let data = std::fs::read_to_string(PathBuf::from(queries)).expect("failed to read queries");
    let parsed: TestFile<'_> = serde_json::from_str(&data).expect("failed to parse queries");

    let mut direct = 0usize;
    let mut direct_mismatches = 0usize;
    let mut parse_failures = 0usize;
    let mut printed = 0usize;

    for (idx, entry) in parsed.entries.iter().enumerate() {
        let body = serde_json::to_vec(&entry.request).expect("failed to serialize request");
        match classifier::classify_approved(&body) {
            Some(actual) => {
                direct += 1;
                if actual != entry.expected_approved {
                    direct_mismatches += 1;
                    if printed < 20 {
                        eprintln!(
                            "classifier mismatch at entry {} expected={} actual={} id={}",
                            idx, entry.expected_approved, actual, entry.request.id
                        );
                        printed += 1;
                    }
                }
            }
            None => {
                parse_failures += 1;
            }
        }
    }

    println!(
        "{{\"queries\":{},\"direct\":{},\"parse_failures\":{},\"direct_mismatches\":{}}}",
        parsed.entries.len(),
        direct,
        parse_failures,
        direct_mismatches
    );
    if direct_mismatches != 0 || parse_failures != 0 {
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
