use crate::search::SearchTraceSink;
use serde_json::json;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::time::Instant;

#[derive(Clone, Copy)]
pub struct PerfConfig {
    pub every: u64,
    pub slow_us: u64,
    pub sample: u64,
}

impl PerfConfig {
    pub fn from_env() -> Option<Self> {
        if std::env::var("PERF_TRACE").ok().as_deref() != Some("1") {
            return None;
        }

        Some(Self {
            every: parse_env_u64("PERF_TRACE_EVERY", 10_000).max(1),
            slow_us: parse_env_u64("PERF_TRACE_SLOW_US", 1_000),
            sample: parse_env_u64("PERF_TRACE_SAMPLE", 0),
        })
    }
}

fn parse_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

pub struct PerfCollector {
    config: PerfConfig,
    request_seq: AtomicU64,
    in_flight: AtomicU64,
    max_in_flight: AtomicU64,
    window: Mutex<Vec<RequestPerf>>,
}

impl PerfCollector {
    pub fn new(config: PerfConfig) -> Self {
        Self {
            config,
            request_seq: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            max_in_flight: AtomicU64::new(0),
            window: Mutex::new(Vec::with_capacity(config.every as usize)),
        }
    }

    pub fn begin_request(&self) -> RequestGuard<'_> {
        let request_id = self.request_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let in_flight_at_start = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.update_max_in_flight(in_flight_at_start);

        RequestGuard {
            collector: self,
            request_id,
            in_flight_at_start,
        }
    }

    pub fn max_in_flight_seen(&self) -> u64 {
        self.max_in_flight.load(Ordering::Relaxed)
    }

    pub fn record_request(&self, record: RequestPerf) {
        if record.handler_total_us >= self.config.slow_us {
            eprintln!("{}", record.to_json("perf_slow_request"));
        } else if self.config.sample > 0 && record.request_id % self.config.sample == 0 {
            eprintln!("{}", record.to_json("perf_sample_request"));
        }

        let flush = {
            let mut window = self.window.lock().expect("perf window poisoned");
            window.push(record);
            if window.len() >= self.config.every as usize {
                Some(std::mem::take(&mut *window))
            } else {
                None
            }
        };

        if let Some(records) = flush {
            self.emit_summary(&records);
        }
    }

    fn end_request(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    fn update_max_in_flight(&self, value: u64) {
        let mut current = self.max_in_flight.load(Ordering::Relaxed);
        while value > current {
            match self.max_in_flight.compare_exchange_weak(
                current,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }

    fn emit_summary(&self, records: &[RequestPerf]) {
        let current_in_flight = self.in_flight.load(Ordering::Relaxed);
        let max_in_flight = self.max_in_flight_seen();
        let request_count = self.request_seq.load(Ordering::Relaxed);

        let summary = json!({
            "kind": "perf_summary",
            "requests_in_window": records.len(),
            "request_count": request_count,
            "in_flight": {
                "current": current_in_flight,
                "max_seen": max_in_flight,
            },
            "timings_us": {
                "handler_total": stats(records.iter().map(|r| r.handler_total_us).collect()),
                "json_parse": stats(records.iter().map(|r| r.json_parse_us).collect()),
                "search_total": stats(records.iter().map(|r| r.search_total_us).collect()),
                "response_build": stats(records.iter().map(|r| r.response_build_us).collect()),
                "vectorize": stats(records.iter().map(|r| r.search.vectorize_us).collect()),
                "home_scan": stats(records.iter().map(|r| r.search.home_scan_us).collect()),
                "home_tree": stats(records.iter().map(|r| r.search.home_tree_us).collect()),
                "root_filter": stats(records.iter().map(|r| r.search.root_filter_us).collect()),
                "node_best_first": stats(records.iter().map(|r| r.search.node_best_first_us).collect()),
                "leaf_scan": stats(records.iter().map(|r| r.search.leaf_scan_us).collect()),
                "neighbor_seed": stats(records.iter().map(|r| r.search.neighbor_seed_us).collect()),
                "branch_bound": stats(records.iter().map(|r| r.search.branch_bound_us).collect()),
                "label_score": stats(records.iter().map(|r| r.search.label_score_us).collect()),
            },
            "request_counters": {
                "body_len": stats(records.iter().map(|r| r.body_len as u64).collect()),
                "score_bucket": stats(records.iter().map(|r| r.score_bucket as u64).collect()),
                "in_flight_at_start": stats(records.iter().map(|r| r.in_flight_at_start).collect()),
            },
            "search_counters": {
                "home_cell_count": stats(records.iter().map(|r| r.search.home_cell_count).collect()),
                "scanned_vectors": stats(records.iter().map(|r| r.search.scanned_vectors).collect()),
                "scanned_cells": stats(records.iter().map(|r| r.search.scanned_cells).collect()),
                "visited_cells": stats(records.iter().map(|r| r.search.visited_cells).collect()),
                "pq_pushes": stats(records.iter().map(|r| r.search.pq_pushes).collect()),
                "pq_pops": stats(records.iter().map(|r| r.search.pq_pops).collect()),
                "nodes_visited": stats(records.iter().map(|r| r.search.nodes_visited).collect()),
                "leaves_scanned": stats(records.iter().map(|r| r.search.leaves_scanned).collect()),
                "pruned_nodes": stats(records.iter().map(|r| r.search.pruned_nodes).collect()),
                "root_candidates": stats(records.iter().map(|r| r.search.root_candidates).collect()),
                "root_cell_pruned": stats(records.iter().map(|r| r.search.root_cell_pruned).collect()),
                "root_bbox_pruned": stats(records.iter().map(|r| r.search.root_bbox_pruned).collect()),
                "node_queue_max_len": stats(records.iter().map(|r| r.search.node_queue_max_len).collect()),
                "node_queue_overflows": stats(records.iter().map(|r| r.search.node_queue_overflows).collect()),
                "dfs_fallbacks": stats(records.iter().map(|r| r.search.dfs_fallbacks).collect()),
            },
        });

        eprintln!("{}", summary);
    }
}

pub struct RequestGuard<'a> {
    collector: &'a PerfCollector,
    request_id: u64,
    in_flight_at_start: u64,
}

impl<'a> RequestGuard<'a> {
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    pub fn in_flight_at_start(&self) -> u64 {
        self.in_flight_at_start
    }
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.collector.end_request();
    }
}

#[derive(Clone, Copy, Default)]
pub struct SearchTrace {
    pub vectorize_us: u64,
    pub home_scan_us: u64,
    pub home_tree_us: u64,
    pub root_filter_us: u64,
    pub node_best_first_us: u64,
    pub leaf_scan_us: u64,
    pub neighbor_seed_us: u64,
    pub branch_bound_us: u64,
    pub label_score_us: u64,
    pub home_cell_count: u64,
    pub scanned_vectors: u64,
    pub scanned_cells: u64,
    pub visited_cells: u64,
    pub pq_pushes: u64,
    pub pq_pops: u64,
    pub query_key: u64,
    pub nodes_visited: u64,
    pub leaves_scanned: u64,
    pub pruned_nodes: u64,
    pub root_candidates: u64,
    pub root_cell_pruned: u64,
    pub root_bbox_pruned: u64,
    pub node_queue_max_len: u64,
    pub node_queue_overflows: u64,
    pub dfs_fallbacks: u64,
    pub worst_topk_dist: i64,
    pub topk_distances: [i64; 5],
    pub stop_reason: &'static str,
}

impl SearchTraceSink for SearchTrace {
    #[inline]
    fn enabled(&self) -> bool {
        true
    }

    #[inline]
    fn record_vectorize_us(&mut self, value: u64) {
        self.vectorize_us += value;
    }

    #[inline]
    fn set_home_cell_count(&mut self, value: u32) {
        self.home_cell_count = value as u64;
    }

    #[inline]
    fn add_home_scan_us(&mut self, value: u64, vectors: u32) {
        self.home_scan_us += value;
        self.scanned_vectors += vectors as u64;
        self.scanned_cells += 1;
    }

    #[inline]
    fn record_neighbor_seed_us(&mut self, value: u64) {
        self.neighbor_seed_us += value;
    }

    #[inline]
    fn record_branch_bound_us(&mut self, value: u64) {
        self.branch_bound_us += value;
    }

    #[inline]
    fn record_home_tree_us(&mut self, value: u64) {
        self.home_tree_us += value;
    }

    #[inline]
    fn record_root_filter_us(&mut self, value: u64) {
        self.root_filter_us += value;
    }

    #[inline]
    fn record_node_best_first_us(&mut self, value: u64) {
        self.node_best_first_us += value;
    }

    #[inline]
    fn record_label_score_us(&mut self, value: u64) {
        self.label_score_us += value;
    }

    #[inline]
    fn set_query_key(&mut self, value: u16) {
        self.query_key = value as u64;
    }

    #[inline]
    fn set_stop_reason(&mut self, value: &'static str) {
        self.stop_reason = value;
    }

    #[inline]
    fn record_topk(&mut self, distances: [i32; 5], worst_dist_sq: i32) {
        self.worst_topk_dist = worst_dist_sq as i64;
        for (idx, distance) in distances.iter().enumerate() {
            self.topk_distances[idx] = *distance as i64;
        }
    }

    #[inline]
    fn add_branch_scan(&mut self, vectors: u32) {
        self.scanned_vectors += vectors as u64;
        self.scanned_cells += 1;
    }

    #[inline]
    fn add_leaf_scan_us(&mut self, value: u64, _vectors: u32) {
        self.leaf_scan_us += value;
    }

    #[inline]
    fn inc_visited_cells(&mut self) {
        self.visited_cells += 1;
    }

    #[inline]
    fn inc_pq_pushes(&mut self) {
        self.pq_pushes += 1;
    }

    #[inline]
    fn inc_pq_pops(&mut self) {
        self.pq_pops += 1;
    }

    #[inline]
    fn inc_nodes_visited(&mut self) {
        self.nodes_visited += 1;
    }

    #[inline]
    fn inc_leaves_scanned(&mut self) {
        self.leaves_scanned += 1;
    }

    #[inline]
    fn inc_pruned_nodes(&mut self) {
        self.pruned_nodes += 1;
    }

    #[inline]
    fn inc_root_candidates(&mut self) {
        self.root_candidates += 1;
    }

    #[inline]
    fn inc_root_cell_pruned(&mut self) {
        self.root_cell_pruned += 1;
    }

    #[inline]
    fn inc_root_bbox_pruned(&mut self) {
        self.root_bbox_pruned += 1;
    }

    #[inline]
    fn observe_node_queue_len(&mut self, value: usize) {
        self.node_queue_max_len = self.node_queue_max_len.max(value as u64);
    }

    #[inline]
    fn inc_node_queue_overflows(&mut self) {
        self.node_queue_overflows += 1;
    }

    #[inline]
    fn inc_dfs_fallbacks(&mut self) {
        self.dfs_fallbacks += 1;
    }
}

#[derive(Clone, Copy)]
pub struct RequestPerf {
    pub request_id: u64,
    pub in_flight_at_start: u64,
    pub max_in_flight_seen: u64,
    pub body_len: usize,
    pub score_bucket: u8,
    pub handler_total_us: u64,
    pub json_parse_us: u64,
    pub search_total_us: u64,
    pub response_build_us: u64,
    pub search: SearchTrace,
}

impl RequestPerf {
    fn to_json(&self, kind: &str) -> serde_json::Value {
        json!({
            "kind": kind,
            "request_id": self.request_id,
            "query_key": self.search.query_key,
            "stop_reason": self.search.stop_reason,
            "body_len": self.body_len,
            "score_bucket": self.score_bucket,
            "in_flight_at_start": self.in_flight_at_start,
            "max_in_flight_seen": self.max_in_flight_seen,
            "timings_us": {
                "handler_total": self.handler_total_us,
                "json_parse": self.json_parse_us,
                "search_total": self.search_total_us,
                "response_build": self.response_build_us,
                "vectorize": self.search.vectorize_us,
                "home_scan": self.search.home_scan_us,
                "home_tree": self.search.home_tree_us,
                "root_filter": self.search.root_filter_us,
                "node_best_first": self.search.node_best_first_us,
                "leaf_scan": self.search.leaf_scan_us,
                "neighbor_seed": self.search.neighbor_seed_us,
                "branch_bound": self.search.branch_bound_us,
                "label_score": self.search.label_score_us,
            },
            "search_counters": {
                "home_cell_count": self.search.home_cell_count,
                "scanned_vectors": self.search.scanned_vectors,
                "scanned_cells": self.search.scanned_cells,
                "visited_cells": self.search.visited_cells,
                "pq_pushes": self.search.pq_pushes,
                "pq_pops": self.search.pq_pops,
                "nodes_visited": self.search.nodes_visited,
                "leaves_scanned": self.search.leaves_scanned,
                "pruned_nodes": self.search.pruned_nodes,
                "root_candidates": self.search.root_candidates,
                "root_cell_pruned": self.search.root_cell_pruned,
                "root_bbox_pruned": self.search.root_bbox_pruned,
                "node_queue_max_len": self.search.node_queue_max_len,
                "node_queue_overflows": self.search.node_queue_overflows,
                "dfs_fallbacks": self.search.dfs_fallbacks,
            },
            "topk": {
                "worst_dist_sq": self.search.worst_topk_dist,
                "distances": self.search.topk_distances,
            },
        })
    }
}

#[inline]
pub fn elapsed_us(start: Instant) -> u64 {
    start.elapsed().as_micros() as u64
}

fn stats(mut values: Vec<u64>) -> serde_json::Value {
    if values.is_empty() {
        return json!({
            "avg": 0.0,
            "p50": 0,
            "p90": 0,
            "p99": 0,
            "max": 0,
        });
    }

    values.sort_unstable();
    let sum: u128 = values.iter().map(|&v| v as u128).sum();
    let len = values.len();

    json!({
        "avg": (sum as f64) / (len as f64),
        "p50": percentile(&values, 50),
        "p90": percentile(&values, 90),
        "p99": percentile(&values, 99),
        "max": values[len - 1],
    })
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    let len = values.len();
    if len == 0 {
        return 0;
    }

    let idx = ((len - 1) * percentile).div_ceil(100);
    values[idx]
}
