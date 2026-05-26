use std::fs::File;
use std::io::{self, Write};

pub const MAGIC: u32 = 0x524E4833; // "RNH3"
pub const VERSION: u32 = 4;
pub const N_DIM_TOTAL: u32 = 14;
pub const N_DIM_PADDED: u32 = 16;
pub const N_CELLS: usize = 8192;
pub const TREE_LEAF_SIZE: usize = 192;
pub const EMPTY_NODE: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IndexHeader {
    pub magic: u32,
    pub version: u32,
    pub n_vectors: u32,
    pub n_cells_populated: u32,
    pub n_dim_total: u32,
    pub n_dim_padded: u32,
    pub n_cell_dims: u32,
    pub q_scale: u32,
    pub reserved: [u32; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CellEntry {
    pub offset: u32, // index into vector slab (units of records)
    pub count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TreeRootEntry {
    pub root: u32,
    pub count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TreeNode {
    pub bbox_min: [i16; 16],
    pub bbox_max: [i16; 16],
    pub left: u32,
    pub right: u32,
    pub offset: u32,
    pub count: u32,
    pub min_original_id: u32,
    pub flags: u32,
}

impl Default for TreeNode {
    fn default() -> Self {
        Self {
            bbox_min: [0; 16],
            bbox_max: [0; 16],
            left: EMPTY_NODE,
            right: EMPTY_NODE,
            offset: 0,
            count: 0,
            min_original_id: u32::MAX,
            flags: 0,
        }
    }
}

impl TreeNode {
    pub const LEAF_FLAG: u32 = 1;

    #[inline]
    pub fn is_leaf(&self) -> bool {
        (self.flags & Self::LEAF_FLAG) != 0
    }
}

pub fn write_index(
    path: &std::path::Path,
    vectors: &[[i16; 16]],
    labels: &[u8],
    original_ids: &[u32],
    cell_dir: &[CellEntry; 8192],
    tree_roots: &[TreeRootEntry; 8192],
    populated_tree_keys: &[u32],
    tree_nodes: &[TreeNode],
    n_cells_populated: u32,
    leaf_size: u32,
) -> io::Result<()> {
    let mut f = File::create(path)?;
    assert_eq!(vectors.len(), labels.len());
    assert_eq!(vectors.len(), original_ids.len());

    let header = IndexHeader {
        magic: MAGIC,
        version: VERSION,
        n_vectors: vectors.len() as u32,
        n_cells_populated,
        n_dim_total: N_DIM_TOTAL,
        n_dim_padded: N_DIM_PADDED,
        n_cell_dims: 13,
        q_scale: 10000,
        reserved: [
            tree_nodes.len() as u32,
            populated_tree_keys.len() as u32,
            leaf_size,
            N_CELLS as u32,
            0,
            0,
            0,
            0,
        ],
    };

    // Write header
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            &header as *const IndexHeader as *const u8,
            std::mem::size_of::<IndexHeader>(),
        )
    };
    f.write_all(header_bytes)?;

    // Write cell directory (8192 entries x 8 bytes = 65536 bytes)
    let dir_bytes = unsafe {
        std::slice::from_raw_parts(
            cell_dir.as_ptr() as *const u8,
            8192 * std::mem::size_of::<CellEntry>(),
        )
    };
    f.write_all(dir_bytes)?;

    // Write vector slab
    for v in vectors {
        let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, 32) };
        f.write_all(bytes)?;
    }

    // Write label slab
    f.write_all(labels)?;

    // Write original reference IDs
    let id_bytes = unsafe {
        std::slice::from_raw_parts(
            original_ids.as_ptr() as *const u8,
            original_ids.len() * std::mem::size_of::<u32>(),
        )
    };
    f.write_all(id_bytes)?;

    // Write tree root directory
    let root_bytes = unsafe {
        std::slice::from_raw_parts(
            tree_roots.as_ptr() as *const u8,
            8192 * std::mem::size_of::<TreeRootEntry>(),
        )
    };
    f.write_all(root_bytes)?;

    // Write populated root keys
    let key_bytes = unsafe {
        std::slice::from_raw_parts(
            populated_tree_keys.as_ptr() as *const u8,
            populated_tree_keys.len() * std::mem::size_of::<u32>(),
        )
    };
    f.write_all(key_bytes)?;

    // Write tree nodes
    let node_bytes = unsafe {
        std::slice::from_raw_parts(
            tree_nodes.as_ptr() as *const u8,
            std::mem::size_of_val(tree_nodes),
        )
    };
    f.write_all(node_bytes)?;

    Ok(())
}

pub struct IndexView<'a> {
    data: &'a [u8],
}

impl<'a> IndexView<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        IndexView { data }
    }

    pub fn header(&self) -> &IndexHeader {
        unsafe { &*(self.data.as_ptr() as *const IndexHeader) }
    }

    fn cell_dir_offset() -> usize {
        std::mem::size_of::<IndexHeader>()
    }

    fn vector_slab_offset() -> usize {
        Self::cell_dir_offset() + 8192 * 8
    }

    fn label_slab_offset(&self) -> usize {
        Self::vector_slab_offset() + self.header().n_vectors as usize * 32
    }

    fn original_id_slab_offset(&self) -> usize {
        self.label_slab_offset() + self.header().n_vectors as usize
    }

    fn tree_root_dir_offset(&self) -> usize {
        self.original_id_slab_offset()
            + self.header().n_vectors as usize * std::mem::size_of::<u32>()
    }

    fn populated_tree_keys_offset(&self) -> usize {
        self.tree_root_dir_offset() + N_CELLS * std::mem::size_of::<TreeRootEntry>()
    }

    fn tree_nodes_offset(&self) -> usize {
        self.populated_tree_keys_offset()
            + self.n_populated_tree_roots() as usize * std::mem::size_of::<u32>()
    }

    pub fn cell_entry(&self, key: u16) -> CellEntry {
        let offset = Self::cell_dir_offset() + (key as usize) * 8;
        unsafe { *(self.data.as_ptr().add(offset) as *const CellEntry) }
    }

    pub fn vector_at(&self, idx: u32) -> &[i16; 16] {
        let offset = Self::vector_slab_offset() + idx as usize * 32;
        unsafe { &*(self.data.as_ptr().add(offset) as *const [i16; 16]) }
    }

    pub fn vector_base_ptr(&self) -> *const [i16; 16] {
        unsafe { self.data.as_ptr().add(Self::vector_slab_offset()) as *const [i16; 16] }
    }

    pub fn label_base_ptr(&self) -> *const u8 {
        unsafe { self.data.as_ptr().add(self.label_slab_offset()) }
    }

    pub fn original_id_base_ptr(&self) -> *const u32 {
        unsafe { self.data.as_ptr().add(self.original_id_slab_offset()) as *const u32 }
    }

    pub fn label_at(&self, idx: u32) -> u8 {
        let offset = self.label_slab_offset() + idx as usize;
        self.data[offset]
    }

    pub fn original_id_at(&self, idx: u32) -> u32 {
        unsafe { *self.original_id_base_ptr().add(idx as usize) }
    }

    pub fn n_vectors(&self) -> u32 {
        self.header().n_vectors
    }

    pub fn n_tree_nodes(&self) -> u32 {
        self.header().reserved[0]
    }

    pub fn n_populated_tree_roots(&self) -> u32 {
        self.header().reserved[1]
    }

    pub fn tree_leaf_size(&self) -> u32 {
        self.header().reserved[2]
    }

    pub fn tree_root_entry(&self, key: u16) -> TreeRootEntry {
        let offset =
            self.tree_root_dir_offset() + (key as usize) * std::mem::size_of::<TreeRootEntry>();
        unsafe { *(self.data.as_ptr().add(offset) as *const TreeRootEntry) }
    }

    pub fn populated_tree_key_at(&self, idx: u32) -> u16 {
        let offset = self.populated_tree_keys_offset() + idx as usize * std::mem::size_of::<u32>();
        unsafe { *(self.data.as_ptr().add(offset) as *const u32) as u16 }
    }

    pub fn tree_node(&self, idx: u32) -> &TreeNode {
        let offset = self.tree_nodes_offset() + idx as usize * std::mem::size_of::<TreeNode>();
        unsafe { &*(self.data.as_ptr().add(offset) as *const TreeNode) }
    }
}
