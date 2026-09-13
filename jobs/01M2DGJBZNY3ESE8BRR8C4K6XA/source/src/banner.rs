use std::io::{self, Write};

const WIDTH: usize = 38;
const RULE: &str = "======================================";

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";

/// Prints the boxed "Initial Setup" title, e.g.:
/// ```text
/// ======================================
///           Exliatycl Initial Setup
/// ======================================
/// ```
pub fn title(product: &str) {
    let heading = format!("{product} Initial Setup");
    println!("{CYAN}{RULE}{RESET}");
    println!("{CYAN}{BOLD}{:^width$}{RESET}", heading, width = WIDTH);
    println!("{CYAN}{RULE}{RESET}");
}

/// Prints a step label (e.g. "Checking IP..........") with no trailing newline,
/// so `step_done`/`step_failed` can finish the line once the step resolves.
pub fn step(label: &str) {
    print!("{label}");
    let _ = io::stdout().flush();
}

pub fn step_done() {
    println!(" {GREEN}\u{2713}{RESET}");
}

pub fn step_failed() {
    println!(" {RED}\u{2717}{RESET}");
}

pub fn token(setup_code: &str) {
    println!(
        "!! Your setup token: {YELLOW}{BOLD}{}{RESET}",
        setup_code.to_uppercase()
    );
}

pub fn linked() {
    println!("{GREEN}{BOLD}\u{2713} Your server has been linked successfully.{RESET}");
}
