use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::Instant;
use crate::maze::{Maze, Cell, MAZE_SIZE};
use super::SolveResult;

#[inline]
fn heuristic(row: u8, col: u8) -> u32 {
    (99u32.wrapping_sub(row as u32)) + (99u32.wrapping_sub(col as u32))
}

pub fn solve(maze: &Maze, collect_cells: bool) -> SolveResult {
    let start = Instant::now();

    let mut g_score = [[u32::MAX; MAZE_SIZE]; MAZE_SIZE];
    g_score[0][0] = 0;

    let mut parent = [[(255u8, 255u8); MAZE_SIZE]; MAZE_SIZE];
    let mut closed  = [[false; MAZE_SIZE]; MAZE_SIZE];

    // min-heap via Reverse: (Reverse(f), row, col)
    let mut open: BinaryHeap<(Reverse<u32>, u8, u8)> = BinaryHeap::with_capacity(10_000);
    open.push((Reverse(heuristic(0, 0)), 0u8, 0u8));

    let mut cells_explored: u32 = 0;
    // Only allocate when the caller needs the cell list (snapshot path).
    let mut explored_cells: Vec<(u8, u8)> = if collect_cells {
        Vec::with_capacity(MAZE_SIZE * MAZE_SIZE)
    } else {
        Vec::new()
    };
    let mut found = false;

    'outer: while let Some((_, row, col)) = open.pop() {
        if closed[row as usize][col as usize] {
            continue; // stale entry
        }
        closed[row as usize][col as usize] = true;
        cells_explored += 1;
        if collect_cells { explored_cells.push((row, col)); }

        if row == 99 && col == 99 {
            found = true;
            break 'outer;
        }

        let cell = maze.grid[row as usize][col as usize];
        let tent_g = g_score[row as usize][col as usize] + 1;

        // NORTH
        if cell.is_open(Cell::NORTH) && row > 0 {
            let (nr, nc) = (row - 1, col);
            if !closed[nr as usize][nc as usize] && tent_g < g_score[nr as usize][nc as usize] {
                g_score[nr as usize][nc as usize] = tent_g;
                parent[nr as usize][nc as usize] = (row, col);
                open.push((Reverse(tent_g + heuristic(nr, nc)), nr, nc));
            }
        }
        // EAST
        if cell.is_open(Cell::EAST) && col < 99 {
            let (nr, nc) = (row, col + 1);
            if !closed[nr as usize][nc as usize] && tent_g < g_score[nr as usize][nc as usize] {
                g_score[nr as usize][nc as usize] = tent_g;
                parent[nr as usize][nc as usize] = (row, col);
                open.push((Reverse(tent_g + heuristic(nr, nc)), nr, nc));
            }
        }
        // SOUTH
        if cell.is_open(Cell::SOUTH) && row < 99 {
            let (nr, nc) = (row + 1, col);
            if !closed[nr as usize][nc as usize] && tent_g < g_score[nr as usize][nc as usize] {
                g_score[nr as usize][nc as usize] = tent_g;
                parent[nr as usize][nc as usize] = (row, col);
                open.push((Reverse(tent_g + heuristic(nr, nc)), nr, nc));
            }
        }
        // WEST
        if cell.is_open(Cell::WEST) && col > 0 {
            let (nr, nc) = (row, col - 1);
            if !closed[nr as usize][nc as usize] && tent_g < g_score[nr as usize][nc as usize] {
                g_score[nr as usize][nc as usize] = tent_g;
                parent[nr as usize][nc as usize] = (row, col);
                open.push((Reverse(tent_g + heuristic(nr, nc)), nr, nc));
            }
        }
    }

    // Reconstruct. Always count path_length (needed for correctness check).
    // Only materialise the path Vec when collect_cells is set.
    let (path_length, path) = if found {
        let mut path: Vec<(u8, u8)> = if collect_cells {
            Vec::with_capacity(256)
        } else {
            Vec::new()
        };
        let mut cur = (99u8, 99u8);
        let mut len = 0u32;
        loop {
            len += 1;
            if collect_cells { path.push(cur); }
            let p = parent[cur.0 as usize][cur.1 as usize];
            if p == (255, 255) { break; }
            cur = p;
        }
        if collect_cells { path.reverse(); }
        (len, path)
    } else {
        (0, Vec::new())
    };

    let time_ns = start.elapsed().as_nanos() as u64;

    SolveResult {
        cells_explored,
        explored_cells,
        path_length,
        time_ns,
        solved: found,
        path,
    }
}
