use memmap2::Mmap;
use shared::distance::l2sq_avx2;
use shared::types::Payload;
use shared::{
    cell_lower_bound_sq, neighbor_keys, round4, vectorize_to_i16_and_key, Constants, IndexView,
    TopK, TreeNode, EMPTY_NODE,
};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchEngine {
    Cell,
    Tree,
}

impl SearchEngine {
    pub fn from_env(default: SearchEngine) -> Self {
        match std::env::var("SEARCH_ENGINE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "cell" => SearchEngine::Cell,
            "tree" => SearchEngine::Tree,
            _ => default,
        }
    }
}

pub struct Index {
    mmap: Mmap,
    constants: Constants,
    engine: SearchEngine,
}

unsafe impl Send for Index {}
unsafe impl Sync for Index {}

// Fixed-capacity sorted descending priority queue for branch-and-bound.
// Smallest lower bound is always at the end (index len-1), allowing O(1) popping.
// No heap allocation — stack-allocated arrays.
const PQ_CAP: usize = 32;

pub trait SearchTraceSink {
    #[inline]
    fn enabled(&self) -> bool {
        false
    }

    #[inline]
    fn record_vectorize_us(&mut self, _value: u64) {}

    #[inline]
    fn set_home_cell_count(&mut self, _value: u32) {}

    #[inline]
    fn add_home_scan_us(&mut self, _value: u64, _vectors: u32) {}

    #[inline]
    fn record_neighbor_seed_us(&mut self, _value: u64) {}

    #[inline]
    fn record_branch_bound_us(&mut self, _value: u64) {}

    #[inline]
    fn record_home_tree_us(&mut self, _value: u64) {}

    #[inline]
    fn record_root_filter_us(&mut self, _value: u64) {}

    #[inline]
    fn record_node_best_first_us(&mut self, _value: u64) {}

    #[inline]
    fn record_label_score_us(&mut self, _value: u64) {}

    #[inline]
    fn set_query_key(&mut self, _value: u16) {}

    #[inline]
    fn set_stop_reason(&mut self, _value: &'static str) {}

    #[inline]
    fn record_topk(&mut self, _distances: [i32; 5], _worst_dist_sq: i32) {}

    #[inline]
    fn add_branch_scan(&mut self, _vectors: u32) {}

    #[inline]
    fn add_leaf_scan_us(&mut self, _value: u64, _vectors: u32) {}

    #[inline]
    fn inc_visited_cells(&mut self) {}

    #[inline]
    fn inc_pq_pushes(&mut self) {}

    #[inline]
    fn inc_pq_pops(&mut self) {}

    #[inline]
    fn inc_nodes_visited(&mut self) {}

    #[inline]
    fn inc_leaves_scanned(&mut self) {}

    #[inline]
    fn inc_pruned_nodes(&mut self) {}

    #[inline]
    fn inc_root_candidates(&mut self) {}

    #[inline]
    fn inc_root_cell_pruned(&mut self) {}

    #[inline]
    fn inc_root_bbox_pruned(&mut self) {}

    #[inline]
    fn observe_node_queue_len(&mut self, _value: usize) {}

    #[inline]
    fn inc_node_queue_overflows(&mut self) {}

    #[inline]
    fn inc_dfs_fallbacks(&mut self) {}
}

pub struct NoopTrace;

impl SearchTraceSink for NoopTrace {}

struct Pq {
    entries: [(i32, u16); PQ_CAP],
    len: usize,
}

impl Pq {
    #[inline]
    fn new() -> Self {
        Pq {
            entries: [(i32::MAX, 0); PQ_CAP],
            len: 0,
        }
    }

    #[inline]
    fn push(&mut self, lb: i32, key: u16) -> bool {
        if self.len >= PQ_CAP {
            // In descending order, the worst (largest) element is at index 0.
            if lb >= self.entries[0].0 {
                return false;
            }
            // Evict index 0 (worst) by shifting everything left
            for idx in 1..self.len {
                self.entries[idx - 1] = self.entries[idx];
            }
            self.len -= 1;
        }
        // Insert such that it remains sorted descending
        let mut i = self.len;
        while i > 0 && self.entries[i - 1].0 < lb {
            self.entries[i] = self.entries[i - 1];
            i -= 1;
        }
        self.entries[i] = (lb, key);
        self.len += 1;
        true
    }

    #[inline]
    fn pop(&mut self) -> Option<(i32, u16)> {
        if self.len == 0 {
            None
        } else {
            self.len -= 1;
            Some(self.entries[self.len])
        }
    }
}

struct VisitedSet {
    keys: [u16; 128],
    len: usize,
}

impl VisitedSet {
    #[inline]
    fn new() -> Self {
        VisitedSet {
            keys: [0u16; 128],
            len: 0,
        }
    }

    #[inline]
    fn insert(&mut self, key: u16) -> bool {
        if self.len < 128 {
            self.keys[self.len] = key;
            self.len += 1;
            true
        } else {
            false
        }
    }

    #[inline]
    fn contains(&self, key: u16) -> bool {
        for i in 0..self.len {
            if self.keys[i] == key {
                return true;
            }
        }
        false
    }
}

impl Index {
    pub fn new(mmap: Mmap) -> Self {
        let default_engine = {
            let view = IndexView::new(&mmap);
            if view.header().version >= 4 {
                SearchEngine::Tree
            } else {
                SearchEngine::Cell
            }
        };
        Self::with_engine(mmap, SearchEngine::from_env(default_engine))
    }

    pub fn with_engine(mmap: Mmap, engine: SearchEngine) -> Self {
        let constants = Constants::load_embedded();
        eprintln!("Search engine: {:?}", engine);
        Index {
            mmap,
            constants,
            engine,
        }
    }

    fn view(&self) -> IndexView<'_> {
        IndexView::new(&self.mmap)
    }

    pub fn warmup(&self) {
        let view = self.view();
        let n = view.n_vectors();
        // Touch every mmap region used by the hot path before /ready returns.
        let mut sum = 0i64;
        for i in (0..n).step_by(64) {
            let v = view.vector_at(i);
            sum ^= v[0] as i64;
        }
        for i in (0..n).step_by(4096) {
            sum ^= view.label_at(i) as i64;
        }
        for i in (0..n).step_by(1024) {
            sum ^= view.original_id_at(i) as i64;
        }
        for key_idx in 0..view.n_populated_tree_roots() {
            sum ^= view.populated_tree_key_at(key_idx) as i64;
        }
        for node_idx in 0..view.n_tree_nodes() {
            let node = view.tree_node(node_idx);
            sum ^= node.min_original_id as i64;
            sum ^= node.bbox_min[0] as i64;
            sum ^= node.bbox_max[15] as i64;
        }
        std::hint::black_box(sum);
    }

    pub fn search(&self, payload: &Payload<'_>) -> (bool, f64) {
        let mut trace = NoopTrace;
        self.search_inner(payload, &mut trace)
    }

    pub fn search_with_trace<T: SearchTraceSink>(
        &self,
        payload: &Payload<'_>,
        trace: &mut T,
    ) -> (bool, f64) {
        self.search_inner(payload, trace)
    }

    fn search_inner<T: SearchTraceSink>(
        &self,
        payload: &Payload<'_>,
        trace: &mut T,
    ) -> (bool, f64) {
        let view = self.view();
        let vectorize_start = if trace.enabled() {
            Some(Instant::now())
        } else {
            None
        };
        let (qv16, query_key) = vectorize_to_i16_and_key(payload, &self.constants);
        if let Some(start) = vectorize_start {
            trace.record_vectorize_us(start.elapsed().as_micros() as u64);
        }
        trace.set_query_key(query_key);

        let mut topk = TopK::new();

        match self.engine {
            SearchEngine::Cell => search_cell(&view, &qv16, query_key, &mut topk, trace),
            SearchEngine::Tree => search_tree(&view, &qv16, query_key, &mut topk, trace),
        }

        // Count fraud among top-5 directly from the TopK entries.
        let label_start = if trace.enabled() {
            Some(Instant::now())
        } else {
            None
        };
        let fraud_count = topk.fraud_count();

        let fraud_score = round4(fraud_count as f64 / 5.0);
        let approved = fraud_score < 0.6;
        if let Some(start) = label_start {
            trace.record_label_score_us(start.elapsed().as_micros() as u64);
        }
        trace.record_topk(topk.distances(), topk.worst_dist_sq());
        trace.set_stop_reason("exact");
        (approved, fraud_score)
    }
}

fn search_cell<T: SearchTraceSink>(
    view: &IndexView<'_>,
    qv16: &[i16; 16],
    query_key: u16,
    topk: &mut TopK,
    trace: &mut T,
) {
    let mut visited = VisitedSet::new();
    let mut pq = Pq::new();

    if visited.insert(query_key) {
        trace.inc_visited_cells();
    }
    let home_entry = view.cell_entry(query_key);
    trace.set_home_cell_count(home_entry.count);
    if home_entry.count > 0 {
        let scan_start = if trace.enabled() {
            Some(Instant::now())
        } else {
            None
        };
        scan_cell(view, qv16, home_entry.offset, home_entry.count, topk);
        if let Some(start) = scan_start {
            trace.add_home_scan_us(start.elapsed().as_micros() as u64, home_entry.count);
        }
    }

    let seed_start = if trace.enabled() {
        Some(Instant::now())
    } else {
        None
    };
    let worst = topk.worst_dist_sq();
    for nkey in neighbor_keys(query_key) {
        if !visited.contains(nkey) {
            let lb = cell_lower_bound_sq(qv16, query_key, nkey);
            if lb < worst && visited.insert(nkey) {
                trace.inc_visited_cells();
                if pq.push(lb, nkey) {
                    trace.inc_pq_pushes();
                }
            }
        }
    }
    if let Some(start) = seed_start {
        trace.record_neighbor_seed_us(start.elapsed().as_micros() as u64);
    }

    let branch_start = if trace.enabled() {
        Some(Instant::now())
    } else {
        None
    };
    while let Some((lb, nkey)) = pq.pop() {
        trace.inc_pq_pops();
        if !can_bound_improve(topk, lb, 0) {
            break;
        }

        let entry = view.cell_entry(nkey);
        if entry.count > 0 {
            scan_cell(view, qv16, entry.offset, entry.count, topk);
            trace.add_branch_scan(entry.count);
        }

        let worst_next = topk.worst_dist_sq();
        for nnkey in neighbor_keys(nkey) {
            if !visited.contains(nnkey) {
                let nlb = cell_lower_bound_sq(qv16, query_key, nnkey);
                if nlb < worst_next && visited.insert(nnkey) {
                    trace.inc_visited_cells();
                    if pq.push(nlb, nnkey) {
                        trace.inc_pq_pushes();
                    }
                }
            }
        }
    }
    if let Some(start) = branch_start {
        trace.record_branch_bound_us(start.elapsed().as_micros() as u64);
    }
}

fn search_tree<T: SearchTraceSink>(
    view: &IndexView<'_>,
    qv16: &[i16; 16],
    query_key: u16,
    topk: &mut TopK,
    trace: &mut T,
) {
    let branch_start = if trace.enabled() {
        Some(Instant::now())
    } else {
        None
    };

    let home = view.tree_root_entry(query_key);
    trace.set_home_cell_count(home.count);
    if home.root != EMPTY_NODE {
        let home_start = if trace.enabled() {
            Some(Instant::now())
        } else {
            None
        };
        traverse_tree(view, qv16, home.root, topk, trace);
        if let Some(start) = home_start {
            trace.record_home_tree_us(start.elapsed().as_micros() as u64);
        }
    }

    let mut candidates = [RootCandidate {
        key: u64::MAX,
        root_idx: EMPTY_NODE,
        lower_bound: i32::MAX,
    }; 8192];
    let mut candidate_len = 0usize;

    let root_filter_start = if trace.enabled() {
        Some(Instant::now())
    } else {
        None
    };
    for idx in 0..view.n_populated_tree_roots() {
        let key = view.populated_tree_key_at(idx);
        if key == query_key {
            continue;
        }

        let root_entry = view.tree_root_entry(key);
        if root_entry.root == EMPTY_NODE {
            continue;
        }
        let root = view.tree_node(root_entry.root);
        let cell_lb = cell_lower_bound_sq(qv16, query_key, key);
        if !can_bound_improve(topk, cell_lb, root.min_original_id) {
            trace.inc_pruned_nodes();
            trace.inc_root_cell_pruned();
            continue;
        }

        let lb = bbox_lower_bound_sq(qv16, root);
        if can_bound_improve(topk, lb, root.min_original_id) {
            candidates[candidate_len] = RootCandidate {
                key: TopK::make_key(lb, root.min_original_id),
                root_idx: root_entry.root,
                lower_bound: lb,
            };
            candidate_len += 1;
            trace.inc_root_candidates();
        } else {
            trace.inc_pruned_nodes();
            trace.inc_root_bbox_pruned();
        }
    }
    if let Some(start) = root_filter_start {
        trace.record_root_filter_us(start.elapsed().as_micros() as u64);
    }

    let node_best_first_start = if trace.enabled() {
        Some(Instant::now())
    } else {
        None
    };
    traverse_candidates_best_first(view, qv16, &candidates[..candidate_len], topk, trace);
    if let Some(start) = node_best_first_start {
        trace.record_node_best_first_us(start.elapsed().as_micros() as u64);
    }

    if let Some(start) = branch_start {
        trace.record_branch_bound_us(start.elapsed().as_micros() as u64);
    }
}

#[derive(Clone, Copy)]
struct StackEntry {
    node_idx: u32,
    lower_bound: i32,
}

#[derive(Clone, Copy)]
struct RootCandidate {
    key: u64,
    root_idx: u32,
    lower_bound: i32,
}

#[derive(Clone, Copy)]
struct NodeCandidate {
    key: u64,
    node_idx: u32,
    lower_bound: i32,
}

const NODE_QUEUE_CAP: usize = 4096;

struct NodeQueue {
    entries: [NodeCandidate; NODE_QUEUE_CAP],
    len: usize,
}

impl NodeQueue {
    #[inline]
    fn new() -> Self {
        Self {
            entries: [NodeCandidate {
                key: u64::MAX,
                node_idx: EMPTY_NODE,
                lower_bound: i32::MAX,
            }; NODE_QUEUE_CAP],
            len: 0,
        }
    }

    #[inline]
    fn push(&mut self, candidate: NodeCandidate) -> bool {
        if self.len == self.entries.len() {
            return false;
        }

        let mut idx = self.len;
        self.len += 1;
        while idx > 0 {
            let parent = (idx - 1) / 2;
            if self.entries[parent].key <= candidate.key {
                break;
            }
            self.entries[idx] = self.entries[parent];
            idx = parent;
        }
        self.entries[idx] = candidate;
        true
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn pop(&mut self) -> Option<NodeCandidate> {
        if self.len == 0 {
            return None;
        }

        let result = self.entries[0];
        self.len -= 1;
        if self.len == 0 {
            return Some(result);
        }

        let last = self.entries[self.len];
        let mut idx = 0usize;
        loop {
            let left = idx * 2 + 1;
            if left >= self.len {
                break;
            }
            let right = left + 1;
            let child = if right < self.len && self.entries[right].key < self.entries[left].key {
                right
            } else {
                left
            };

            if self.entries[child].key >= last.key {
                break;
            }
            self.entries[idx] = self.entries[child];
            idx = child;
        }
        self.entries[idx] = last;
        Some(result)
    }
}

fn traverse_candidates_best_first<T: SearchTraceSink>(
    view: &IndexView<'_>,
    qv16: &[i16; 16],
    candidates: &[RootCandidate],
    topk: &mut TopK,
    trace: &mut T,
) {
    let mut queue = NodeQueue::new();
    let mut overflow = false;

    for candidate in candidates {
        if candidate.key < topk.worst_key() {
            if !queue.push(NodeCandidate {
                key: candidate.key,
                node_idx: candidate.root_idx,
                lower_bound: candidate.lower_bound,
            }) {
                overflow = true;
                trace.inc_node_queue_overflows();
                break;
            }
            trace.observe_node_queue_len(queue.len());
        } else {
            trace.inc_pruned_nodes();
        }
    }

    while !overflow {
        let Some(candidate) = queue.pop() else {
            break;
        };

        if candidate.key >= topk.worst_key() {
            trace.inc_pruned_nodes();
            break;
        }

        let node = view.tree_node(candidate.node_idx);
        if !can_bound_improve(topk, candidate.lower_bound, node.min_original_id) {
            trace.inc_pruned_nodes();
            continue;
        }

        trace.inc_nodes_visited();
        if node.is_leaf() {
            trace.inc_leaves_scanned();
            let leaf_start = if trace.enabled() {
                Some(Instant::now())
            } else {
                None
            };
            scan_cell(view, qv16, node.offset, node.count, topk);
            if let Some(start) = leaf_start {
                trace.add_leaf_scan_us(start.elapsed().as_micros() as u64, node.count);
            }
            trace.add_branch_scan(node.count);
            continue;
        }

        let left = view.tree_node(node.left);
        let left_lb = bbox_lower_bound_sq(qv16, left);
        if can_bound_improve(topk, left_lb, left.min_original_id) {
            overflow = !queue.push(NodeCandidate {
                key: TopK::make_key(left_lb, left.min_original_id),
                node_idx: node.left,
                lower_bound: left_lb,
            });
            if overflow {
                trace.inc_node_queue_overflows();
                break;
            }
            trace.observe_node_queue_len(queue.len());
        } else {
            trace.inc_pruned_nodes();
        }

        let right = view.tree_node(node.right);
        let right_lb = bbox_lower_bound_sq(qv16, right);
        if can_bound_improve(topk, right_lb, right.min_original_id) {
            overflow = !queue.push(NodeCandidate {
                key: TopK::make_key(right_lb, right.min_original_id),
                node_idx: node.right,
                lower_bound: right_lb,
            });
            if overflow {
                trace.inc_node_queue_overflows();
            } else {
                trace.observe_node_queue_len(queue.len());
            }
        } else {
            trace.inc_pruned_nodes();
        }
    }

    if overflow {
        trace.inc_dfs_fallbacks();
        for candidate in candidates {
            if candidate.key < topk.worst_key() {
                traverse_tree_with_lb(
                    view,
                    qv16,
                    candidate.root_idx,
                    candidate.lower_bound,
                    topk,
                    trace,
                );
            }
        }
    }
}

fn traverse_tree<T: SearchTraceSink>(
    view: &IndexView<'_>,
    qv16: &[i16; 16],
    root_idx: u32,
    topk: &mut TopK,
    trace: &mut T,
) {
    let root = view.tree_node(root_idx);
    let root_lb = bbox_lower_bound_sq(qv16, root);
    traverse_tree_with_lb(view, qv16, root_idx, root_lb, topk, trace);
}

fn traverse_tree_with_lb<T: SearchTraceSink>(
    view: &IndexView<'_>,
    qv16: &[i16; 16],
    root_idx: u32,
    root_lb: i32,
    topk: &mut TopK,
    trace: &mut T,
) {
    let mut stack = [StackEntry {
        node_idx: EMPTY_NODE,
        lower_bound: 0,
    }; 256];
    let mut len = 0usize;

    push_stack(&mut stack, &mut len, root_idx, root_lb);

    while len > 0 {
        len -= 1;
        let current = stack[len];
        let node = view.tree_node(current.node_idx);

        if !can_bound_improve(topk, current.lower_bound, node.min_original_id) {
            trace.inc_pruned_nodes();
            continue;
        }

        trace.inc_nodes_visited();
        if node.is_leaf() {
            trace.inc_leaves_scanned();
            let leaf_start = if trace.enabled() {
                Some(Instant::now())
            } else {
                None
            };
            scan_cell(view, qv16, node.offset, node.count, topk);
            if let Some(start) = leaf_start {
                trace.add_leaf_scan_us(start.elapsed().as_micros() as u64, node.count);
            }
            trace.add_branch_scan(node.count);
            continue;
        }

        let left = view.tree_node(node.left);
        let right = view.tree_node(node.right);
        let left_lb = bbox_lower_bound_sq(qv16, left);
        let right_lb = bbox_lower_bound_sq(qv16, right);

        let (near_idx, near_lb, near_min_id, far_idx, far_lb, far_min_id) = if left_lb <= right_lb {
            (
                node.left,
                left_lb,
                left.min_original_id,
                node.right,
                right_lb,
                right.min_original_id,
            )
        } else {
            (
                node.right,
                right_lb,
                right.min_original_id,
                node.left,
                left_lb,
                left.min_original_id,
            )
        };

        if can_bound_improve(topk, far_lb, far_min_id) {
            push_stack(&mut stack, &mut len, far_idx, far_lb);
        } else {
            trace.inc_pruned_nodes();
        }
        if can_bound_improve(topk, near_lb, near_min_id) {
            push_stack(&mut stack, &mut len, near_idx, near_lb);
        } else {
            trace.inc_pruned_nodes();
        }
    }
}

#[inline]
fn push_stack(stack: &mut [StackEntry; 256], len: &mut usize, node_idx: u32, lower_bound: i32) {
    assert!(*len < stack.len(), "tree traversal stack overflow");
    stack[*len] = StackEntry {
        node_idx,
        lower_bound,
    };
    *len += 1;
}

#[inline]
fn can_bound_improve(topk: &TopK, lower_bound: i32, min_original_id: u32) -> bool {
    !topk.is_full() || TopK::make_key(lower_bound, min_original_id) < topk.worst_key()
}

#[inline]
fn bbox_lower_bound_sq(qv: &[i16; 16], node: &TreeNode) -> i32 {
    let mut sum = 0i32;
    for dim in 0..16 {
        let q = qv[dim] as i32;
        let min = node.bbox_min[dim] as i32;
        let max = node.bbox_max[dim] as i32;
        let d = if q < min {
            min - q
        } else if q > max {
            q - max
        } else {
            0
        };
        sum += d * d;
    }
    sum
}

#[inline]
fn scan_cell(view: &IndexView<'_>, qv: &[i16; 16], offset: u32, count: u32, topk: &mut TopK) {
    let mut i = 0;
    let base_ptr = view.vector_base_ptr();
    let label_ptr = view.label_base_ptr();
    let original_id_ptr = view.original_id_base_ptr();

    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    {
        unsafe {
            use std::arch::x86_64::*;
            let q = _mm256_loadu_si256(qv.as_ptr() as *const __m256i);
            let mut worst_key = topk.worst_key();
            let mut worst_dist = topk.worst_dist_sq();
            let mut topk_full = topk.is_full();

            macro_rules! maybe_insert {
                ($dist:expr, $idx:expr) => {{
                    if !topk_full || $dist <= worst_dist {
                        let oid = *original_id_ptr.add($idx as usize);
                        let key = TopK::make_key($dist, oid);
                        if key < worst_key {
                            topk.insert_labeled($dist, oid, *label_ptr.add($idx as usize));
                            worst_key = topk.worst_key();
                            worst_dist = topk.worst_dist_sq();
                            topk_full = topk.is_full();
                        }
                    }
                }};
            }

            // Compute L2^2 distance of four vectors in parallel (4-way unrolled)
            while i + 3 < count {
                let idx1 = offset + i;
                let idx2 = offset + i + 1;
                let idx3 = offset + i + 2;
                let idx4 = offset + i + 3;

                // Prefetch 8 vectors ahead to hide memory latency during parallel calculations
                if i + 8 < count {
                    _mm_prefetch(base_ptr.add((idx1 + 8) as usize) as *const i8, _MM_HINT_T0);
                    _mm_prefetch(base_ptr.add((idx2 + 8) as usize) as *const i8, _MM_HINT_T0);
                    _mm_prefetch(base_ptr.add((idx3 + 8) as usize) as *const i8, _MM_HINT_T0);
                    _mm_prefetch(base_ptr.add((idx4 + 8) as usize) as *const i8, _MM_HINT_T0);
                }

                // Aligned 32-byte loads
                let r1 = _mm256_load_si256(base_ptr.add(idx1 as usize) as *const __m256i);
                let r2 = _mm256_load_si256(base_ptr.add(idx2 as usize) as *const __m256i);
                let r3 = _mm256_load_si256(base_ptr.add(idx3 as usize) as *const __m256i);
                let r4 = _mm256_load_si256(base_ptr.add(idx4 as usize) as *const __m256i);

                let diff1 = _mm256_sub_epi16(q, r1);
                let diff2 = _mm256_sub_epi16(q, r2);
                let diff3 = _mm256_sub_epi16(q, r3);
                let diff4 = _mm256_sub_epi16(q, r4);

                let sq_sum1 = _mm256_madd_epi16(diff1, diff1);
                let sq_sum2 = _mm256_madd_epi16(diff2, diff2);
                let sq_sum3 = _mm256_madd_epi16(diff3, diff3);
                let sq_sum4 = _mm256_madd_epi16(diff4, diff4);

                // Reduce first vector's squared sum
                let low1 = _mm256_castsi256_si128(sq_sum1);
                let high1 = _mm256_extracti128_si256(sq_sum1, 1);
                let sum128_1 = _mm_add_epi32(low1, high1);
                let s1_1 = _mm_shuffle_epi32(sum128_1, 0x4E);
                let sum2_1 = _mm_add_epi32(sum128_1, s1_1);
                let s2_1 = _mm_shuffle_epi32(sum2_1, 0xB1);
                let sum3_1 = _mm_add_epi32(sum2_1, s2_1);
                let d1 = _mm_cvtsi128_si32(sum3_1);

                // Reduce second vector's squared sum
                let low2 = _mm256_castsi256_si128(sq_sum2);
                let high2 = _mm256_extracti128_si256(sq_sum2, 1);
                let sum128_2 = _mm_add_epi32(low2, high2);
                let s1_2 = _mm_shuffle_epi32(sum128_2, 0x4E);
                let sum2_2 = _mm_add_epi32(sum128_2, s1_2);
                let s2_2 = _mm_shuffle_epi32(sum2_2, 0xB1);
                let sum3_2 = _mm_add_epi32(sum2_2, s2_2);
                let d2 = _mm_cvtsi128_si32(sum3_2);

                // Reduce third vector's squared sum
                let low3 = _mm256_castsi256_si128(sq_sum3);
                let high3 = _mm256_extracti128_si256(sq_sum3, 1);
                let sum128_3 = _mm_add_epi32(low3, high3);
                let s1_3 = _mm_shuffle_epi32(sum128_3, 0x4E);
                let sum2_3 = _mm_add_epi32(sum128_3, s1_3);
                let s2_3 = _mm_shuffle_epi32(sum2_3, 0xB1);
                let sum3_3 = _mm_add_epi32(sum2_3, s2_3);
                let d3 = _mm_cvtsi128_si32(sum3_3);

                // Reduce fourth vector's squared sum
                let low4 = _mm256_castsi256_si128(sq_sum4);
                let high4 = _mm256_extracti128_si256(sq_sum4, 1);
                let sum128_4 = _mm_add_epi32(low4, high4);
                let s1_4 = _mm_shuffle_epi32(sum128_4, 0x4E);
                let sum2_4 = _mm_add_epi32(sum128_4, s1_4);
                let s2_4 = _mm_shuffle_epi32(sum2_4, 0xB1);
                let sum3_4 = _mm_add_epi32(sum2_4, s2_4);
                let d4 = _mm_cvtsi128_si32(sum3_4);

                maybe_insert!(d1, idx1);
                maybe_insert!(d2, idx2);
                maybe_insert!(d3, idx3);
                maybe_insert!(d4, idx4);

                i += 4;
            }

            // Compute L2^2 distance of two vectors in parallel (2-way tail loop)
            while i + 1 < count {
                let idx1 = offset + i;
                let idx2 = offset + i + 1;

                if i + 4 < count {
                    _mm_prefetch(base_ptr.add((idx1 + 4) as usize) as *const i8, _MM_HINT_T0);
                    _mm_prefetch(base_ptr.add((idx2 + 4) as usize) as *const i8, _MM_HINT_T0);
                }

                let r1 = _mm256_load_si256(base_ptr.add(idx1 as usize) as *const __m256i);
                let r2 = _mm256_load_si256(base_ptr.add(idx2 as usize) as *const __m256i);

                let diff1 = _mm256_sub_epi16(q, r1);
                let diff2 = _mm256_sub_epi16(q, r2);

                let sq_sum1 = _mm256_madd_epi16(diff1, diff1);
                let sq_sum2 = _mm256_madd_epi16(diff2, diff2);

                let low1 = _mm256_castsi256_si128(sq_sum1);
                let high1 = _mm256_extracti128_si256(sq_sum1, 1);
                let sum128_1 = _mm_add_epi32(low1, high1);
                let s1_1 = _mm_shuffle_epi32(sum128_1, 0x4E);
                let sum2_1 = _mm_add_epi32(sum128_1, s1_1);
                let s2_1 = _mm_shuffle_epi32(sum2_1, 0xB1);
                let sum3_1 = _mm_add_epi32(sum2_1, s2_1);
                let d1 = _mm_cvtsi128_si32(sum3_1);

                let low2 = _mm256_castsi256_si128(sq_sum2);
                let high2 = _mm256_extracti128_si256(sq_sum2, 1);
                let sum128_2 = _mm_add_epi32(low2, high2);
                let s1_2 = _mm_shuffle_epi32(sum128_2, 0x4E);
                let sum2_2 = _mm_add_epi32(sum128_2, s1_2);
                let s2_2 = _mm_shuffle_epi32(sum2_2, 0xB1);
                let sum3_2 = _mm_add_epi32(sum2_2, s2_2);
                let d2 = _mm_cvtsi128_si32(sum3_2);

                maybe_insert!(d1, idx1);
                maybe_insert!(d2, idx2);

                i += 2;
            }
        }
    }

    // Fallback or tail loop for remaining/scalar elements
    let mut worst_key = topk.worst_key();
    let mut worst_dist = topk.worst_dist_sq();
    let mut topk_full = topk.is_full();
    while i < count {
        let idx = offset + i;
        let rv = unsafe { &*base_ptr.add(idx as usize) };
        let d = l2sq_avx2(qv, rv);
        if !topk_full || d <= worst_dist {
            let original_id = unsafe { *original_id_ptr.add(idx as usize) };
            let key = TopK::make_key(d, original_id);
            if key < worst_key {
                let label = unsafe { *label_ptr.add(idx as usize) };
                topk.insert_labeled(d, original_id, label);
                worst_key = topk.worst_key();
                worst_dist = topk.worst_dist_sq();
                topk_full = topk.is_full();
            }
        }
        i += 1;
    }
}
