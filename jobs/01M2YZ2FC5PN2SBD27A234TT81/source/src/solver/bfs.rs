use std::collections::VecDeque;
use std::time::Instant;
use crate::maze::{Maze, Cell, MAZE_SIZE};
use super::SolveResult;

pub fn solve(maze: &Maze, collect_cells: bool) -> SolveResult {
    let start = Instant::now();

    let mut visited = [[false; MAZE_SIZE]; MAZE_SIZE];
    // parent[r][c] = cell we came from. (255,255) = no parent / unvisited.
    let mut parent = [[(255u8, 255u8); MAZE_SIZE]; MAZE_SIZE];

    let mut queue = VecDeque::<(u8, u8)>::with_capacity(10_000);
    queue.push_back((0, 0));
    visited[0][0] = true;

    let mut cells_explored: u32 = 0;
    // Only allocate the explored-cells vec when the caller will use it (snapshot).
    let mut explored_cells: Vec<(u8, u8)> = if collect_cells {
        Vec::with_capacity(MAZE_SIZE * MAZE_SIZE)
    } else {
        Vec::new()
    };
    let mut found = false;

    'outer: while let Some((row, col)) = queue.pop_front() {
        cells_explored += 1;
        if collect_cells { explored_cells.push((row, col)); }

        if row == 99 && col == 99 {
            found = true;
            break 'outer;
        }

        let cell = maze.grid[row as usize][col as usize];

        // NORTH
        if cell.is_open(Cell::NORTH) && row > 0 {
            let (nr, nc) = (row - 1, col);
            if !visited[nr as usize][nc as usize] {
                visited[nr as usize][nc as usize] = true;
                parent[nr as usize][nc as usize] = (row, col);
                queue.push_back((nr, nc));
            }
        }
        // EAST
        if cell.is_open(Cell::EAST) && col < 99 {
            let (nr, nc) = (row, col + 1);
            if !visited[nr as usize][nc as usize] {
                visited[nr as usize][nc as usize] = true;
                parent[nr as usize][nc as usize] = (row, col);
                queue.push_back((nr, nc));
            }
        }
        // SOUTH
        if cell.is_open(Cell::SOUTH) && row < 99 {
            let (nr, nc) = (row + 1, col);
            if !visited[nr as usize][nc as usize] {
                visited[nr as usize][nc as usize] = true;
                parent[nr as usize][nc as usize] = (row, col);
                queue.push_back((nr, nc));
            }
        }
        // WEST
        if cell.is_open(Cell::WEST) && col > 0 {
            let (nr, nc) = (row, col - 1);
            if !visited[nr as usize][nc as usize] {
                visited[nr as usize][nc as usize] = true;
                parent[nr as usize][nc as usize] = (row, col);
                queue.push_back((nr, nc));
            }
        }
    }

    // Reconstruct path length (always needed for correctness check).
    // Only build the path Vec when collect_cells is set.
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
