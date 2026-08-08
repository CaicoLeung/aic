// A tiny throwaway program used by `scripts/demo.sh` to show aic splitting a
// single file's mixed edits into block-level atomic commits.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = &args[1];
    println!("Hello, {}", name);
}
