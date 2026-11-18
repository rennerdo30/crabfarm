#![warn(unused)]

/// The value util's build script put into its rustc environment.
pub fn built() -> &'static str {
    util::BUILT
}

/// The answer comes from a proc macro, the text from a build script.
pub fn greeting() -> String {
    // Deliberate warning: the end-to-end test checks that its path is the
    // client path of this file.
    let deliberately_unused = 1;
    format!("answer={} generated={}", derive_mac::answer!(), util::GENERATED)
}
