//! Checks that the examples in `README.md` stay in sync with the crate documentation.
//!
//! Only the copies in `lib.rs` are compiled as doctests, thus the ones in the README
//! can break without anyone noticing.

const README: &str = include_str!("../README.md");
const LIB: &str = include_str!("../src/lib.rs");

/// Returns the contents of the first fenced code block following `heading`.
fn code_block<'a>(text: &'a str, heading: &str) -> &'a str {
    let from = text.find(heading).unwrap_or_else(|| panic!("heading {heading:?} not found"));
    let fence = text[from..].find("```").expect("no code block found") + from;
    let start = text[fence..].find('\n').expect("unterminated code fence") + fence + 1;
    let end = text[start..].find("```").expect("unterminated code block") + start;
    text[start..end].trim_end()
}

/// Asserts that the example under `section` is identical in both files.
fn assert_in_sync(section: &str) {
    let readme = code_block(README, &format!("### {section}"));
    let lib = code_block(LIB, &format!("//! ## {section}"));

    for (line, (readme, lib)) in readme.lines().zip(lib.lines()).enumerate() {
        assert_eq!(
            readme,
            lib,
            "the {section} example in README.md differs from the one in the crate \
             documentation of lib.rs at line {}.\n  README.md: {readme}\n     lib.rs: {lib}",
            line + 1
        );
    }

    assert_eq!(
        readme.lines().count(),
        lib.lines().count(),
        "the {section} example in README.md has a different number of lines than the \
         one in the crate documentation of lib.rs"
    );
}

#[test]
fn readme_examples_match_crate_docs() {
    assert_in_sync("Channels");
    assert_in_sync("Remote procedure calls");
}
