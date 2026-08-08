// Tiny throwaway program for scripts/demo.sh: shows aic splitting one file's
// mixed, unrelated edits into block-level atomic commits.
//
// Three concerns live in three functions, spaced far enough apart that git
// emits one hunk per concern.

/// Program entry: parse args and print a greeting.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    // BUG: indexing args[1] panics when no name is given.
    let name = &args[1];
    println!("{}", greet(name));
}

/// A decorative separator used when printing headings.
fn divider() -> String {
    let bar = "=".repeat(20);
    bar
}

/// Build a friendly greeting for a name.
fn greet(name: &str) -> String {
    format!("Hello, {}", name)
}

/// Wrap a heading with a divider on each side.
fn banner(heading: &str) -> String {
    let bar = divider();
    let body = heading.to_string();
    format!("{}\n{}\n{}", bar, body, bar)
}

/// Emit a one-line access record for a caller.
fn log_access(who: &str) {
    let message = who.to_string();
    let stamped = format!("[access] {}", message);
    println!("{}", stamped);
}
