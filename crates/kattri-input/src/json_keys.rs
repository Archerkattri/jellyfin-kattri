//! Finds object keys that repeat inside one JSON object.
//!
//! `serde_json::Value` keeps only the last of two equal keys and says nothing about it.
//! The keymap loaders use this scanner to report that case as a warning. The input must
//! already be valid JSON (comments removed); the scanner does not validate syntax.

use std::collections::HashSet;

/// One open `{` or `[` while scanning.
enum Frame {
    Object {
        seen: HashSet<String>,
        /// True right after `{` or `,`, when the next string is a member name.
        expect_key: bool,
    },
    Array,
}

/// Keys that repeat within the same object, in document order. Every repeat is listed,
/// so a key written three times appears twice. The same key in two different objects is
/// not a duplicate. Keys are compared after JSON unescaping.
pub(crate) fn duplicate_object_keys(json: &str) -> Vec<String> {
    let bytes = json.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut duplicates = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        match bytes[pos] {
            b'{' => {
                stack.push(Frame::Object {
                    seen: HashSet::new(),
                    expect_key: true,
                });
                pos += 1;
            }
            b'[' => {
                stack.push(Frame::Array);
                pos += 1;
            }
            b'}' | b']' => {
                stack.pop();
                pos += 1;
            }
            b',' => {
                if let Some(Frame::Object { expect_key, .. }) = stack.last_mut() {
                    *expect_key = true;
                }
                pos += 1;
            }
            b':' => {
                if let Some(Frame::Object { expect_key, .. }) = stack.last_mut() {
                    *expect_key = false;
                }
                pos += 1;
            }
            b'"' => {
                let end = string_end(bytes, pos);
                if let Some(Frame::Object { seen, expect_key }) = stack.last_mut()
                    && *expect_key
                {
                    let literal = &json[pos..end];
                    let key = serde_json::from_str::<String>(literal)
                        .unwrap_or_else(|_| literal.to_owned());
                    if !seen.insert(key.clone()) {
                        duplicates.push(key);
                    }
                    *expect_key = false;
                }
                pos = end;
            }
            _ => pos += 1,
        }
    }
    duplicates
}

/// Index just past the closing quote of the string that starts at `start`.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut pos = start + 1;
    while pos < bytes.len() {
        match bytes[pos] {
            b'\\' => pos += 2,
            b'"' => return pos + 1,
            _ => pos += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::duplicate_object_keys;

    #[test]
    fn reports_a_repeated_key_in_one_object() {
        assert_eq!(
            duplicate_object_keys(r#"{"a":1,"b":2,"a":3}"#),
            vec!["a".to_owned()]
        );
    }

    #[test]
    fn lists_each_extra_occurrence() {
        assert_eq!(
            duplicate_object_keys(r#"{"a":1,"a":2,"a":3}"#),
            vec!["a".to_owned(), "a".to_owned()]
        );
    }

    #[test]
    fn same_key_in_different_objects_is_not_a_duplicate() {
        assert!(duplicate_object_keys(r#"{"x":{"k":1},"y":{"k":2}}"#).is_empty());
        assert!(duplicate_object_keys(r#"[{"k":1},{"k":2}]"#).is_empty());
    }

    #[test]
    fn finds_duplicates_in_nested_objects_and_arrays() {
        let text = r#"{"bindings":{"back":["key:Esc"],"back":["key:B"]}, "list":[{"n":1,"n":2}]}"#;
        assert_eq!(
            duplicate_object_keys(text),
            vec!["back".to_owned(), "n".to_owned()]
        );
    }

    #[test]
    fn compares_keys_after_unescaping() {
        assert_eq!(
            duplicate_object_keys(r#"{"ab":1,"ab":2}"#),
            vec!["ab".to_owned()]
        );
    }

    #[test]
    fn string_values_with_braces_and_quotes_do_not_confuse_the_scan() {
        let text = r#"{"a":"} { \" ,","b":"x:y","c":1}"#;
        assert!(duplicate_object_keys(text).is_empty());
        let text = r#"{"a":"}\"","a":"{"}"#;
        assert_eq!(duplicate_object_keys(text), vec!["a".to_owned()]);
    }

    #[test]
    fn non_ascii_text_is_handled() {
        let text = r#"{"ключ":"значение","ключ":"ü"}"#;
        assert_eq!(duplicate_object_keys(text), vec!["ключ".to_owned()]);
    }
}
