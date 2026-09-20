use rand::SeedableRng;
use rand::Rng;
use rand_chacha::ChaCha8Rng;

/// Fallback seed if remote fetch fails.
pub const FALLBACK_SEED: u64 = 2746317214;

/// Seeds embedded at compile time (fallback if runtime fetch fails).
const SEEDS_EMBEDDED: &str = include_str!("../../seeds.txt");

/// Load seeds: try to fetch from remote URL first, fall back to embedded file,
/// fall back to single fallback seed if all else fails.
pub fn load_seeds() -> Vec<u64> {
    // Try remote fetch using ureq (no PowerShell, no console popup).
    if let Ok(seeds) = fetch_seeds_remote() {
        if seeds.len() >= 10 {
            return seeds;
        }
    }

    // Fall back to embedded seeds.txt
    let embedded = parse_seeds(SEEDS_EMBEDDED);
    if !embedded.is_empty() {
        return embedded;
    }

    // Last resort: single fallback seed
    vec![FALLBACK_SEED]
}

fn fetch_seeds_remote() -> Result<Vec<u64>, ()> {
    let response = ureq::get("https://mazebench.mikeden.site/api/v1/seeds.txt")
        .timeout(std::time::Duration::from_secs(5))
        .call()
        .map_err(|_| ())?;

    let text = response.into_string().map_err(|_| ())?;
    let seeds = parse_seeds(&text);
    if seeds.len() >= 10 {
        Ok(seeds)
    } else {
        Err(())
    }
}

fn parse_seeds(text: &str) -> Vec<u64> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .collect()
}

/// One cell in the maze grid.
/// A SET bit means the wall is OPEN (a passage exists in that direction).
#[derive(Clone, Copy, Default, Debug)]
pub struct Cell(pub u8);

impl Cell {
    pub const NORTH: u8 = 0b0001;
    pub const EAST:  u8 = 0b0010;
    pub const SOUTH: u8 = 0b0100;
    pub const WEST:  u8 = 0b1000;

    #[inline] pub fn open(&mut self, dir: u8)       { self.0 |= dir; }
    #[inline] pub fn is_open(self, dir: u8) -> bool { self.0 & dir != 0 }
}

pub const MAZE_SIZE: usize = 100;

/// A 100×100 perfect maze.
/// grid[row][col] — row 0 = top, col 0 = left.
/// Entry: (0,0). Exit: (99,99).
pub struct Maze {
    pub grid: Box<[[Cell; MAZE_SIZE]; MAZE_SIZE]>,
    pub seed: u64,
}

impl Maze {
    pub fn new(seed: u64) -> Self {
        let grid = generate(seed);
        Maze { grid, seed }
    }
}

/// Iterative DFS maze generation (recursive backtracking without recursion).
fn generate(seed: u64) -> Box<[[Cell; MAZE_SIZE]; MAZE_SIZE]> {
    let mut grid: Box<[[Cell; MAZE_SIZE]; MAZE_SIZE]> =
        Box::new([[Cell::default(); MAZE_SIZE]; MAZE_SIZE]);

    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut visited = [[false; MAZE_SIZE]; MAZE_SIZE];
    visited[0][0] = true;

    let mut stack: Vec<(usize, usize)> = Vec::with_capacity(10_000);
    stack.push((0, 0));

    while let Some(&(row, col)) = stack.last() {
        // Collect unvisited neighbors
        let mut neighbors: Vec<(usize, usize, u8, u8)> = Vec::with_capacity(4);

        if row > 0 && !visited[row - 1][col] {
            neighbors.push((row - 1, col, Cell::NORTH, Cell::SOUTH));
        }
        if col + 1 < MAZE_SIZE && !visited[row][col + 1] {
            neighbors.push((row, col + 1, Cell::EAST, Cell::WEST));
        }
        if row + 1 < MAZE_SIZE && !visited[row + 1][col] {
            neighbors.push((row + 1, col, Cell::SOUTH, Cell::NORTH));
        }
        if col > 0 && !visited[row][col - 1] {
            neighbors.push((row, col - 1, Cell::WEST, Cell::EAST));
        }

        if neighbors.is_empty() {
            stack.pop();
            continue;
        }

        let idx = rng.gen_range(0..neighbors.len());
        let (nr, nc, dir_cur, dir_nbr) = neighbors[idx];

        grid[row][col].open(dir_cur);
        grid[nr][nc].open(dir_nbr);
        visited[nr][nc] = true;
        stack.push((nr, nc));
    }

    grid
}
