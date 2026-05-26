#[allow(dead_code)]
#[path = "../search.rs"]
mod search;

use memmap2::MmapOptions;
use search::{Index, SearchEngine, SearchTraceSink};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Deserialize)]
struct QueryEntry<'a> {
    #[serde(borrow)]
    request: shared::types::Payload<'a>,
    expected_approved: bool,
    expected_fraud_score: f64,
}

#[derive(Deserialize)]
struct TestData<'a> {
    #[serde(borrow)]
    entries: Vec<QueryEntry<'a>>,
}

#[derive(Default)]
struct BenchTrace {
    scanned_vectors: u64,
    scanned_cells: u64,
    nodes_visited: u64,
    leaves_scanned: u64,
    pruned_nodes: u64,
}

impl SearchTraceSink for BenchTrace {
    fn set_home_cell_count(&mut self, _value: u32) {}

    fn add_home_scan_us(&mut self, _value: u64, vectors: u32) {
        self.scanned_vectors += vectors as u64;
        self.scanned_cells += 1;
    }

    fn add_branch_scan(&mut self, vectors: u32) {
        self.scanned_vectors += vectors as u64;
        self.scanned_cells += 1;
    }

    fn inc_nodes_visited(&mut self) {
        self.nodes_visited += 1;
    }

    fn inc_leaves_scanned(&mut self) {
        self.leaves_scanned += 1;
    }

    fn inc_pruned_nodes(&mut self) {
        self.pruned_nodes += 1;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut index_path = PathBuf::from("index.bin");
    let mut queries_path = PathBuf::from("test/test-data.json");
    let mut engine = SearchEngine::Tree;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--index" => {
                index_path = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--queries" => {
                queries_path = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--engine" => {
                engine = match args[i + 1].as_str() {
                    "cell" => SearchEngine::Cell,
                    "tree" => SearchEngine::Tree,
                    other => panic!("unknown engine: {}", other),
                };
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    let file = File::open(&index_path).expect("failed to open index");
    let mmap = unsafe { MmapOptions::new().map(&file).expect("failed to mmap index") };
    let index = Index::with_engine(mmap, engine);
    index.warmup();

    let query_data = std::fs::read_to_string(&queries_path).expect("failed to read queries");
    let test_data: TestData<'_> =
        serde_json::from_str(&query_data).expect("failed to parse queries");

    let mut search_us = Vec::with_capacity(test_data.entries.len());
    let mut scanned_vectors = Vec::with_capacity(test_data.entries.len());
    let mut scanned_cells = Vec::with_capacity(test_data.entries.len());
    let mut nodes_visited = Vec::with_capacity(test_data.entries.len());
    let mut leaves_scanned = Vec::with_capacity(test_data.entries.len());
    let mut pruned_nodes = Vec::with_capacity(test_data.entries.len());
    let mut mismatches = 0u64;

    for entry in &test_data.entries {
        let mut trace = BenchTrace::default();
        let start = Instant::now();
        let (approved, fraud_score) = index.search_with_trace(&entry.request, &mut trace);
        let elapsed = start.elapsed().as_micros() as u64;

        if approved != entry.expected_approved
            || (fraud_score - entry.expected_fraud_score).abs() > 1e-9
        {
            mismatches += 1;
        }

        search_us.push(elapsed);
        scanned_vectors.push(trace.scanned_vectors);
        scanned_cells.push(trace.scanned_cells);
        nodes_visited.push(trace.nodes_visited);
        leaves_scanned.push(trace.leaves_scanned);
        pruned_nodes.push(trace.pruned_nodes);
    }

    let result = json!({
        "engine": format!("{:?}", engine),
        "queries": test_data.entries.len(),
        "mismatches": mismatches,
        "search_us": stats(search_us),
        "scanned_vectors": stats(scanned_vectors),
        "scanned_cells": stats(scanned_cells),
        "nodes_visited": stats(nodes_visited),
        "leaves_scanned": stats(leaves_scanned),
        "pruned_nodes": stats(pruned_nodes),
    });

    println!("{}", serde_json::to_string_pretty(&result).unwrap());

    if mismatches != 0 {
        std::process::exit(1);
    }
}

fn stats(mut values: Vec<u64>) -> serde_json::Value {
    values.sort_unstable();
    let len = values.len();
    let sum: u128 = values.iter().map(|&v| v as u128).sum();

    json!({
        "avg": (sum as f64) / (len as f64),
        "p50": percentile(&values, 50),
        "p90": percentile(&values, 90),
        "p99": percentile(&values, 99),
        "max": values[len - 1],
    })
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    let idx = ((values.len() - 1) * percentile).div_ceil(100);
    values[idx]
}
