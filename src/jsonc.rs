//! JSONC (`//`, `/* */`, trailing commas) to plain JSON.
//!
//! `devcontainer.json` is hand-authored and crosses a trust boundary, so
//! the scanner must tolerate any byte sequence without panicking. The
//! conversion is intentionally permissive: it strips comments and trailing
//! commas and leaves everything else for `serde_json` to validate, so
//! malformed input surfaces as a parse error rather than a crash. A
//! standing fuzz target (`fuzz/fuzz_targets/jsonc_to_json.rs`) pins the
//! never-panics property.

/// Convert JSONC (`//`, `/* */`, trailing commas) to plain JSON.
///
/// Implements a single-pass byte scanner aware of string literals (with
/// `\"` and `\\` escapes). Trailing commas are detected by buffering each
/// `,` and dropping it if the next non-whitespace, non-comment token is
/// `]` or `}`. Whitespace bytes are preserved so error offsets in
/// downstream `serde_json` diagnostics roughly track the source.
#[must_use]
pub fn jsonc_to_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut pending_comma = false;
    let mut in_string = false;

    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if pending_comma {
                out.push(',');
                pending_comma = false;
            }
            out.push(c as char);
            if c == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                if pending_comma {
                    out.push(',');
                    pending_comma = false;
                }
                in_string = true;
                out.push('"');
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    i += 2;
                }
            }
            b',' => {
                pending_comma = true;
                i += 1;
            }
            b']' | b'}' => {
                pending_comma = false;
                out.push(c as char);
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                out.push(b as char);
                i += 1;
            }
            _ => {
                if pending_comma {
                    out.push(',');
                    pending_comma = false;
                }
                out.push(c as char);
                i += 1;
            }
        }
    }
    if pending_comma {
        // Trailing comma at EOF without a closer; let serde report it.
        out.push(',');
    }
    out
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests use unwrap for brevity")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn jsonc_strips_line_comments() {
        let src = r#"{
            // a comment
            "name": "demo" // trailing
        }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["name"], "demo");
    }

    #[test]
    fn jsonc_strips_block_comments() {
        let src = r#"{ /* block */ "k": /* inline */ 1 }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["k"], 1);
    }

    #[test]
    fn jsonc_strips_trailing_commas() {
        let src = r#"{ "a": 1, "b": [1, 2, 3,], }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"][2], 3);
    }

    #[test]
    fn jsonc_keeps_commas_inside_strings() {
        let src = r#"{ "k": "a, b, c", }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["k"], "a, b, c");
    }

    #[test]
    fn jsonc_keeps_slashes_inside_strings() {
        let src = r#"{ "k": "https://example.com/a", "j": "/* not a comment */" }"#;
        let json = jsonc_to_json(src);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["k"], "https://example.com/a");
        assert_eq!(v["j"], "/* not a comment */");
    }

    proptest! {
        /// The scanner must never panic on arbitrary input — `devcontainer.json`
        /// is hand-edited and untrusted. This mirrors the standing fuzz target
        /// inside the unit-test suite so regressions surface without a fuzz run.
        #[test]
        fn jsonc_to_json_never_panics(input in ".*") {
            let _ = jsonc_to_json(&input);
        }

        /// Input with no comments and no commas is passed through verbatim:
        /// the scanner only ever removes comment spans and trailing commas, so
        /// text containing neither must be returned unchanged.
        #[test]
        fn jsonc_to_json_is_identity_without_comments_or_commas(
            input in "[a-zA-Z0-9 \t\n{}:_]*",
        ) {
            prop_assert_eq!(jsonc_to_json(&input), input);
        }

        /// Quoted string contents survive intact: a JSON document whose only
        /// string value is comma- and slash-bearing text round-trips through
        /// the scanner and back out of `serde_json` unchanged. The character
        /// class stays within printable, unescaped JSON string content (no
        /// quote, backslash, or control characters).
        #[test]
        fn jsonc_to_json_preserves_string_values(value in "[a-zA-Z0-9 ,./:_-]*") {
            let src = format!(r#"{{ "k": "{value}" }}"#);
            let json = jsonc_to_json(&src);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(v["k"].as_str().unwrap(), value.as_str());
        }
    }
}
