//! Text shared between the front-ends.
//!
//! Small enough to look unnecessary, and it earns its place: WinDrop counts
//! things in the window, in `windrop list`, in `windrop doctor` and in the
//! installer output, and every one of those said `1 application(s)` before this
//! existed. A bracketed plural is the sort of detail that makes an application
//! feel unfinished, and it is one function to fix.

/// `1 application`, `4 applications`.
///
/// Only the regular English plural is handled, which is every noun WinDrop
/// counts: application, profile, environment, component, line.
pub fn count(noun: &str, n: usize) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// `1 file` / `3 files`, for a count that is already a string.
///
/// Used where a count arrives formatted rather than as a number — from a
/// `usize` in another type, or from a caller that also wants the digits grouped.
pub fn plural_after(noun: &str, count: &str) -> String {
    if count == "1" {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_singular_and_everything_else_is_not() {
        assert_eq!(count("application", 0), "0 applications");
        assert_eq!(count("application", 1), "1 application");
        assert_eq!(count("application", 2), "2 applications");
        assert_eq!(count("profile", 16), "16 profiles");
    }

    #[test]
    fn a_formatted_count_is_pluralised_the_same_way() {
        assert_eq!(plural_after("line", "1"), "1 line");
        assert_eq!(plural_after("line", "80"), "80 lines");
    }
}
