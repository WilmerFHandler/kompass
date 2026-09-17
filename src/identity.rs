//! Stable, source-derived identity evidence for analyzed units.
//!
//! A report is a snapshot of source, so its unit ids deliberately include the
//! snapshot path and source location.  They are useful for exact matches in
//! an unchanged scope.  Comparisons use the declaration and body fingerprints
//! as the portable evidence when a unit moves, is renamed, or shifts because a
//! sibling was inserted.

use crate::model::{FunctionReport, Language, Position};

/// Version of the unit identity and lexical fingerprint contract.
pub const IDENTITY_CONTRACT: &str = "identity-v1";

/// Version of the evidence fields used to explain a comparison match.
pub const EVIDENCE_VERSION: &str = "evidence-v1";

/// Calculate a deterministic lexical fingerprint for a source range.
///
/// Whitespace and comments are ignored, while token boundaries are retained,
/// so formatting-only edits do not look like body changes and `a b` does not
/// collide with `ab`.  The compact FNV-1a representation keeps reports
/// dependency-free and stable across processes and platforms.
pub fn lexical_fingerprint(source: &str, start: usize, end: usize, language: Language) -> String {
    let start = start.min(source.len());
    let end = end.min(source.len()).max(start);
    let canonical = canonical_tokens(&source[start..end], language);
    format!("{IDENTITY_CONTRACT}:fnv1a64:{:016x}", fnv1a64(&canonical))
}

/// Convert a proc-macro2-style one-based line and zero-based byte column into
/// a source byte offset.  Invalid positions clamp to the nearest valid line.
pub fn rust_offset(source: &str, position: proc_macro2::LineColumn) -> usize {
    line_offset(source, position.line, position.column)
}

/// Fingerprint a Rust span using its source coordinates.
pub fn rust_span_fingerprint(source: &str, span: proc_macro2::Span, language: Language) -> String {
    lexical_fingerprint(
        source,
        rust_offset(source, span.start()),
        rust_offset(source, span.end()),
        language,
    )
}

/// Fingerprint a Python/Ruff byte range.
pub fn range_fingerprint(source: &str, start: usize, end: usize, language: Language) -> String {
    lexical_fingerprint(source, start, end, language)
}

/// Produce a unique id for one report snapshot.  Coordinates are included so
/// two legitimate duplicate declarations receive distinct ids without using a
/// mutable ordinal that could remap every later lambda or closure.
#[allow(clippy::too_many_arguments)]
pub fn snapshot_id(
    path: &str,
    language: Language,
    kind: &str,
    name: &str,
    start: Position,
    end: Position,
    declaration_fingerprint: &str,
    body_fingerprint: &str,
) -> String {
    let seed = format!(
        "{path}\u{1f}{}\u{1f}{kind}\u{1f}{name}\u{1f}{}:{}-{}:{}\u{1f}{declaration_fingerprint}\u{1f}{body_fingerprint}",
        language.serialized(),
        start.line,
        start.column,
        end.line,
        end.column
    );
    format!(
        "{IDENTITY_CONTRACT}:snapshot:{:016x}",
        fnv1a64(seed.as_bytes())
    )
}

/// Fill snapshot ids after a frontend has collected its units.  Frontends own
/// lexical evidence; the analyzer owns the display path that must participate
/// in a report-local id.
pub fn assign_snapshot_ids(path: &str, language: Language, functions: &mut [FunctionReport]) {
    for function in functions {
        function.snapshot_id = snapshot_id(
            path,
            language,
            function.kind.serialized(),
            &function.name,
            function.location.start.clone(),
            function.location.end.clone(),
            &function.declaration_fingerprint,
            &function.body_fingerprint,
        );
    }
}

fn line_offset(source: &str, one_based_line: usize, byte_column: usize) -> usize {
    if source.is_empty() {
        return 0;
    }
    let target_line = one_based_line.max(1);
    let mut line = 1usize;
    let mut start = 0usize;
    for (index, byte) in source.bytes().enumerate() {
        if line == target_line {
            return start.saturating_add(byte_column).min(source.len());
        }
        if byte == b'\n' {
            line = line.saturating_add(1);
            start = index.saturating_add(1);
        }
    }
    if line == target_line {
        start.saturating_add(byte_column).min(source.len())
    } else {
        source.len()
    }
}

fn canonical_tokens(source: &str, language: Language) -> Vec<u8> {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }

        if (language.is_javascript_family() || language == Language::Rust)
            && bytes[index..].starts_with(b"//")
        {
            index = skip_line_comment(bytes, index);
            continue;
        }
        if (language.is_javascript_family() || language == Language::Rust)
            && bytes[index..].starts_with(b"/*")
        {
            index = skip_block_comment(bytes, index);
            continue;
        }
        if language == Language::Python && bytes[index] == b'#' {
            index = skip_line_comment(bytes, index);
            continue;
        }

        let start = index;
        if let Some(end) = raw_literal_end(bytes, index, language) {
            index = end;
        } else if language == Language::JavaScript && bytes[index] == b'`' {
            index = template_literal_end(bytes, index);
        } else if let Some(end) = quoted_literal_end(bytes, index, language) {
            index = end;
        } else if is_identifier_byte(bytes[index]) {
            index += 1;
            while index < bytes.len() && is_identifier_byte(bytes[index]) {
                index += 1;
            }
        } else {
            // Keeping punctuation as one-byte tokens matches the Rust lexical
            // contract and makes the representation language-neutral.
            index += 1;
        }

        if start < index {
            if !output.is_empty() {
                output.push(0x1f);
            }
            output.extend_from_slice(&bytes[start..index]);
        }
    }
    output
}

fn is_identifier_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric() || byte >= 0x80
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 0usize;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"/*") {
            depth = depth.saturating_add(1);
            index += 2;
        } else if bytes[index..].starts_with(b"*/") {
            depth = depth.saturating_sub(1);
            index += 2;
            if depth == 0 {
                break;
            }
        } else {
            index += 1;
        }
    }
    index
}

fn quoted_literal_end(bytes: &[u8], start: usize, language: Language) -> Option<usize> {
    let quote = *bytes.get(start)?;
    let javascript_template = language.is_javascript_family() && quote == b'`';
    if !matches!(quote, b'\'' | b'"') && !javascript_template {
        return None;
    }
    // Rust lifetimes and labels begin with a single quote but are not string
    // or character literals. Treating the quote as punctuation preserves their
    // lexical shape without allowing an absent character terminator to swallow
    // the rest of a function.
    if language == Language::Rust && quote == b'\'' {
        return None;
    }

    let triple = language == Language::Python
        && bytes.get(start..start.saturating_add(3)) == Some(&[quote, quote, quote]);
    let delimiter_len = if triple { 3 } else { 1 };
    let mut index = start.saturating_add(delimiter_len);
    let mut escaped = false;
    while index < bytes.len() {
        if escaped {
            escaped = false;
            index += 1;
        } else if bytes[index] == b'\\' {
            escaped = true;
            index += 1;
        } else if bytes.get(index..index.saturating_add(delimiter_len))
            == Some(&bytes[start..start.saturating_add(delimiter_len)])
        {
            return Some(index.saturating_add(delimiter_len));
        } else {
            index += 1;
        }
    }
    Some(bytes.len())
}

fn template_literal_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start.saturating_add(1);
    let mut escaped = false;
    while index < bytes.len() {
        if escaped {
            escaped = false;
        } else if bytes[index] == b'\\' {
            escaped = true;
        } else if bytes[index] == b'`' {
            return index.saturating_add(1);
        }
        index += 1;
    }
    bytes.len()
}

fn raw_literal_end(bytes: &[u8], start: usize, language: Language) -> Option<usize> {
    if language != Language::Rust {
        return None;
    }
    let prefix = if bytes.get(start) == Some(&b'r') {
        start
    } else if bytes.get(start) == Some(&b'b') && bytes.get(start + 1) == Some(&b'r') {
        start + 1
    } else if bytes.get(start) == Some(&b'c') && bytes.get(start + 1) == Some(&b'r') {
        // Rust's raw C string literal uses the `cr` prefix. Keep it in the
        // same opaque token as `r` and `br`, otherwise a quote in its body
        // can expose comment markers and truncate the fingerprint.
        start + 1
    } else {
        return None;
    };
    let mut cursor = prefix + 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    let hashes = cursor.saturating_sub(prefix + 1);
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"'
            && bytes
                .get(cursor + 1..cursor + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return Some(cursor + 1 + hashes);
        }
        cursor += 1;
    }
    Some(bytes.len())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_and_comments_do_not_change_fingerprint() {
        assert_eq!(
            lexical_fingerprint("if x { y() }", 0, 12, Language::Rust),
            lexical_fingerprint("if  x{/*comment*/y()}", 0, 21, Language::Rust)
        );
    }

    #[test]
    fn token_boundaries_are_preserved() {
        assert_ne!(
            lexical_fingerprint("ab", 0, 2, Language::Python),
            lexical_fingerprint("a b", 0, 3, Language::Python)
        );
    }

    #[test]
    fn literal_comment_markers_remain_literal() {
        let raw = r####"fn run() { let value = r###"contains " // and /* */"###; value }"####;
        let changed = r####"fn run() {
            let value = r###"contains " // and /* changed */"###;
            value
        }"####;
        assert_ne!(
            lexical_fingerprint(raw, 0, raw.len(), Language::Rust),
            lexical_fingerprint(changed, 0, changed.len(), Language::Rust)
        );
        let first = "let x = 'a; // still code";
        let second = "let x='a;// still code";
        assert_eq!(
            lexical_fingerprint(first, 0, first.len(), Language::Rust),
            lexical_fingerprint(second, 0, second.len(), Language::Rust)
        );
    }

    #[test]
    fn raw_literals_do_not_expose_inner_quotes_as_comments() {
        let before = "let value = r###\"contains \" // one /* marker */\"###; value";
        let after = "let value=r###\"contains \" // two /* changed */\"###; value";
        assert_ne!(
            lexical_fingerprint(before, 0, before.len(), Language::Rust),
            lexical_fingerprint(after, 0, after.len(), Language::Rust)
        );
    }

    #[test]
    fn python_prefixed_and_triple_literals_protect_hashes() {
        let before = "value = f\"\"\"# inside // text\"\"\"";
        let after = "value=f\"\"\"# inside // text\"\"\"";
        assert_eq!(
            lexical_fingerprint(before, 0, before.len(), Language::Python),
            lexical_fingerprint(after, 0, after.len(), Language::Python)
        );
    }

    #[test]
    fn javascript_comments_and_templates_are_lexically_opaque() {
        let before = "const render = () => `// literal ${value}`; /* comment */ render();";
        let after = "const render=()=>`// literal ${value}`; /* changed */ render();";
        assert_eq!(
            lexical_fingerprint(before, 0, before.len(), Language::JavaScript),
            lexical_fingerprint(after, 0, after.len(), Language::JavaScript)
        );
        assert_eq!(
            lexical_fingerprint(before, 0, before.len(), Language::TypeScript),
            lexical_fingerprint(after, 0, after.len(), Language::TypeScript)
        );
    }

    #[test]
    fn raw_c_literals_protect_quotes_and_comment_markers() {
        let before = r####"let value = cr###"contains " // marker /* one */"###; value"####;
        let after = r####"let value=cr###"contains " // changed /* two */"###; value"####;
        assert_ne!(
            lexical_fingerprint(before, 0, before.len(), Language::Rust),
            lexical_fingerprint(after, 0, after.len(), Language::Rust)
        );
    }
}
