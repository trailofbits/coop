//! Shared character-class validator for newtypes that derive an on-disk
//! filename or shell-argv element from user input.
//!
//! Image names, network-interface names, repo segments, and secret-store
//! tool names all accept the same character class — `[a-zA-Z0-9_.-]`. The
//! class lives here so the four newtypes can't drift apart, and so each
//! constructor's rejection path is exercised once by the helper tests
//! rather than re-tested at every call site.

use anyhow::{Result, bail};

/// Predicate for the safe-name character class `[a-zA-Z0-9_.-]`.
pub(crate) fn is_safe_name_char(c: char) -> bool {
    matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.')
}

/// Validate that every char of `name` is in [`is_safe_name_char`].
///
/// `kind` names what is being validated (e.g. `"Image name"`); it is
/// quoted into the error message in front of the offending character so
/// the failure points at both the type and the byte.
pub(crate) fn validate_safe_chars(name: &str, kind: &str) -> Result<()> {
    if let Some(c) = name.chars().find(|c| !is_safe_name_char(*c)) {
        bail!(
            "{kind} contains invalid character {c:?} \
             (allowed: a-z, A-Z, 0-9, '-', '_', '.')"
        );
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn accepts_full_safe_class() {
        // The whole class on one line so a future drift is caught immediately.
        validate_safe_chars(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.",
            "T",
        )
        .unwrap();
    }

    #[test]
    fn rejects_out_of_class_chars() {
        // One example per category of "bad byte" the four newtypes need to
        // reject — whitespace, shell metacharacters, path separators, control
        // bytes, and non-ASCII.
        for s in [
            "with space",
            "tab\there",
            "name\n",
            "name\0",
            "weird!",
            "name;evil",
            "name$x",
            "path/separator",
            "back\\slash",
            "café",
        ] {
            assert!(validate_safe_chars(s, "T").is_err(), "{s:?} should fail");
        }
    }

    #[test]
    fn empty_input_is_accepted() {
        // The character-class check has nothing to reject in the empty
        // string; emptiness is a separate invariant each newtype enforces.
        validate_safe_chars("", "T").unwrap();
    }

    #[test]
    fn error_message_quotes_kind_and_char() {
        let err = validate_safe_chars("a b", "Image name")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Image name"), "missing kind in {err:?}");
        assert!(err.contains("' '"), "missing offending char in {err:?}");
    }
}
