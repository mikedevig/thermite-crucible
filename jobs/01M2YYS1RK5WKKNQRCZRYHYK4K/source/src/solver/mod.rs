pub mod bfs;
pub mod astar;

#[derive(Clone, Debug)]
pub struct SolveResult {
    /// Number of cells closed/dequeued during solving
    pub cells_explored: u32,
    /// All cells visited (in order) — for rendering the heatmap
    pub explored_cells: Vec<(u8, u8)>,
    /// Number of cells in final path from (0,0) to (99,99), inclusive
    pub path_length: u32,
    /// Solve time in nanoseconds
    pub time_ns: u64,
    /// True if a path was found
    pub solved: bool,
    /// Final path as (row, col) pairs, start → end
    pub path: Vec<(u8, u8)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Algorithm {
    Bfs,
    AStar,
}
