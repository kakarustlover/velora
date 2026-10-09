fn main() {
    // Compiles ui/app.slint (and everything it imports) into Rust code.
    slint_build::compile("ui/app.slint").expect("failed to compile the Slint UI (see the error above)");
}
