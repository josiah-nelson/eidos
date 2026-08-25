//! Rendering a stored path back into one the filesystem accepts.
//!
//! The catalog stores a source root and, per object, the chain of names below
//! it. Turning that back into a path needs a separator, and the separator is a
//! property of *the source*, not of the machine doing the rendering: a v1
//! central catalog holds paths from every host in the fleet, and even
//! standalone, a rendered path is what the content pipeline opens. Deriving it
//! from the build target renders `\` on macOS, where every such path then
//! fails to open.

/// Separator for paths under `root`, chosen from the root's own shape: a drive
/// letter (`G:`), a UNC share (`\\server\share`), or an existing backslash
/// means Windows; anything else is a POSIX path.
pub fn separator(root: &str) -> char {
    // A POSIX absolute path decides first: a backslash is a legal character in
    // a Unix file name, so `/Users/x/odd\name` must not read as Windows.
    if root.starts_with('/') {
        return '/';
    }
    let bytes = root.as_bytes();
    let drive_letter = bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_alphabetic();
    if drive_letter || root.starts_with("\\\\") || root.contains('\\') {
        '\\'
    } else {
        '/'
    }
}

/// The root as it should be displayed: a bare drive letter keeps its trailing
/// separator, because `G:` and `G:\` mean different things to Windows.
pub fn root_display(root: &str) -> String {
    if root.len() == 2 && root.ends_with(':') {
        format!("{root}\\")
    } else {
        root.to_string()
    }
}

/// Append `name` to `base` with the separator `root` implies.
pub fn join(root: &str, base: &str, name: &str) -> String {
    let separator = separator(root);
    let trimmed = base.trim_end_matches(separator);
    // A POSIX root is a single separator, and trimming leaves nothing.
    if trimmed.is_empty() && separator == '/' {
        return format!("/{name}");
    }
    format!("{trimmed}{separator}{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_decides_the_separator() {
        assert_eq!(separator("G:\\Corpus"), '\\');
        assert_eq!(separator("G:"), '\\');
        assert_eq!(separator("\\\\fileserver\\share"), '\\');
        assert_eq!(separator("/Volumes/Corpus"), '/');
        assert_eq!(separator("/"), '/');
        assert_eq!(separator("relative/path"), '/');
        // A backslash is a legal character in a Unix file name.
        assert_eq!(separator("/Volumes/odd\\name"), '/');
    }

    #[test]
    fn names_join_the_way_their_root_spells_paths() {
        assert_eq!(
            join("G:\\Corpus", "G:\\Corpus\\a", "b.txt"),
            "G:\\Corpus\\a\\b.txt"
        );
        assert_eq!(join("G:", "G:\\", "a"), "G:\\a");
        assert_eq!(
            join("/Volumes/C", "/Volumes/C/a", "b.txt"),
            "/Volumes/C/a/b.txt"
        );
        assert_eq!(join("/", "/", "etc"), "/etc");
        assert_eq!(
            join("\\\\fileserver\\share", "\\\\fileserver\\share", "a"),
            "\\\\fileserver\\share\\a"
        );
    }

    #[test]
    fn a_bare_drive_root_keeps_its_separator() {
        assert_eq!(root_display("G:"), "G:\\");
        assert_eq!(root_display("G:\\Corpus"), "G:\\Corpus");
        assert_eq!(root_display("/Volumes/Corpus"), "/Volumes/Corpus");
    }
}
