// A tiny calculator used by the aic demo.
//
// The methods are spaced apart on purpose. The demo applies three
// deliberately-unrelated edits — a fix, a feat, and a style tweak — that land
// in three separate git hunks, so `aic` can split them into three atomic
// commits. See examples/README.md.

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn subtract(a: i32, b: i32) -> i32 {
    a - a // BUG: should be `a - b`
}

pub fn divide(a: i32, b: i32) -> Option<i32> {
    if b == 0 {
        None
    } else {
        Some(a / b)
    }
}

pub fn modulo(a: i32, b: i32) -> i32 {
    a % b
}

pub fn power(base: i32, exp: u32) -> i32 {
    base.pow(exp)
}

// ---------------------------------------------------------------------------
// Entry point
//
// Everything below this banner is the demo's printout. Keeping it in its own
// region guarantees the "style" edit (reformatting these prints) lands in a
// separate git hunk from the arithmetic edits above, so `aic` can carve it
// into its own atomic commit.
// ---------------------------------------------------------------------------

pub fn main() {
    let sum = add(2, 3);
    println!("2 + 3 = {sum}");

    let diff = subtract(5, 2);
    println!("5 - 2 = {diff}");

    let quotient = divide(10, 2);
    println!("10 / 2 = {quotient:?}");
}
