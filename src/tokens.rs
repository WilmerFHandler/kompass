use std::str::FromStr;

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};

/// A source position using proc-macro2's one-based line and zero-based UTF-8
/// byte column convention. Keeping the original positions avoids a second
/// lexer and lets function counts be selected from one file token stream.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TokenPosition {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct LexedToken {
    pub start: TokenPosition,
    pub end: TokenPosition,
}

#[derive(Clone, Debug, Default)]
pub struct LexedSource {
    tokens: Vec<LexedToken>,
}

impl LexedSource {
    pub fn total_tokens(&self) -> usize {
        self.tokens.len()
    }

    pub fn tokens_in(&self, start: TokenPosition, end: TokenPosition) -> usize {
        let first = self.lower_bound_start(start);
        let last = self.upper_bound_end(end);
        last.saturating_sub(first)
    }

    fn lower_bound_start(&self, position: TokenPosition) -> usize {
        let mut low = 0;
        let mut high = self.tokens.len();
        while low < high {
            let middle = low + (high - low) / 2;
            if self.tokens[middle].start < position {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn upper_bound_end(&self, position: TokenPosition) -> usize {
        let mut low = 0;
        let mut high = self.tokens.len();
        while low < high {
            let middle = low + (high - low) / 2;
            if self.tokens[middle].end <= position {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }
}

/// Lex Rust source once using proc-macro2's source lexer.
///
/// Every identifier, keyword, literal, punctuation mark, and delimiter is
/// one token. Multi-character punctuation is deliberately counted one
/// punctuation character at a time (`->` is two, `::` is two, and `..=` is
/// three), matching proc-macro2's `Punct` representation. Group delimiters
/// are added explicitly because proc-macro2 exposes them as `Group` metadata.
/// Macro bodies are traversed as written and are never expanded.
pub fn lex(source: &str) -> Result<LexedSource, String> {
    let source_without_doc_comments = mask_doc_comments(source);
    let stream =
        TokenStream::from_str(&source_without_doc_comments).map_err(|error| error.to_string())?;
    let mut tokens = Vec::new();
    flatten(stream, &mut tokens);
    Ok(LexedSource { tokens })
}

/// proc-macro2 intentionally turns Rust doc comments into `#[doc = ...]`
/// attributes. Those attributes are useful to a macro, but for Kompass they
/// are comments and must not inflate the lexical count. Masking only the
/// comment bytes preserves every span position while leaving ordinary comments
/// to the lexer itself.
fn mask_doc_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0;
    let mut raw_hashes = None;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    let mut block_comment_depth = 0;

    while index < bytes.len() {
        if let Some(hashes) = raw_hashes {
            if bytes[index] == b'"' && has_hashes(bytes, index + 1, hashes) {
                index += hashes + 1;
                raw_hashes = None;
                continue;
            }
            index += 1;
            continue;
        }
        if in_string || in_char {
            let current = bytes[index];
            if escaped {
                escaped = false;
            } else if current == b'\\' {
                escaped = true;
            } else if (in_string && current == b'"') || (in_char && current == b'\'') {
                in_string = false;
                in_char = false;
            }
            index += 1;
            continue;
        }

        if block_comment_depth > 0 {
            if bytes[index..].starts_with(b"/*") {
                block_comment_depth += 1;
                index += 2;
                continue;
            }
            if bytes[index..].starts_with(b"*/") {
                block_comment_depth -= 1;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        if let Some((opening_length, hashes)) = raw_string_delimiter(bytes, index) {
            raw_hashes = Some(hashes);
            index += opening_length;
            continue;
        }
        if bytes[index] == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        if bytes[index] == b'\'' && looks_like_char_literal(bytes, index) {
            in_char = true;
            index += 1;
            continue;
        }

        if bytes[index..].starts_with(b"/*") {
            if bytes[index..].starts_with(b"/**") || bytes[index..].starts_with(b"/*!") {
                mask_block_comment(&mut masked, bytes, &mut index);
            } else {
                block_comment_depth = 1;
                index += 2;
            }
            continue;
        }
        if bytes[index..].starts_with(b"///") || bytes[index..].starts_with(b"//!") {
            mask_until_newline(&mut masked, bytes, &mut index);
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        index += 1;
    }

    String::from_utf8(masked).expect("masking comments preserves UTF-8 bytes")
}

pub(crate) fn raw_string_delimiter(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let prefix = if bytes[index] == b'r' {
        index
    } else if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'r') {
        index + 1
    } else {
        return None;
    };
    let mut cursor = prefix + 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some((cursor - index + 1, cursor - prefix - 1))
}

pub(crate) fn looks_like_char_literal(bytes: &[u8], index: usize) -> bool {
    let Some(rest) = bytes
        .get(index + 1..)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
    else {
        return false;
    };
    let mut chars = rest.chars();
    match chars.next() {
        Some('\\') => match chars.next() {
            Some('u') => {
                for character in chars.by_ref() {
                    if character == '}' {
                        break;
                    }
                    if character == '\n' || character == '\r' {
                        return false;
                    }
                }
                chars.next() == Some('\'')
            }
            Some(character) if character != '\n' && character != '\r' => chars.next() == Some('\''),
            _ => false,
        },
        Some(character) if character != '\n' && character != '\r' => chars.next() == Some('\''),
        _ => false,
    }
}

pub(crate) fn has_hashes(bytes: &[u8], start: usize, count: usize) -> bool {
    bytes
        .get(start..start.saturating_add(count))
        .is_some_and(|slice| slice.iter().all(|byte| *byte == b'#'))
}

fn mask_until_newline(masked: &mut [u8], source: &[u8], index: &mut usize) {
    while *index < source.len() && source[*index] != b'\n' {
        if source[*index] != b'\r' {
            masked[*index] = b' ';
        }
        *index += 1;
    }
}

fn mask_block_comment(masked: &mut [u8], source: &[u8], index: &mut usize) {
    let mut depth = 0;
    while *index < source.len() {
        if source[*index..].starts_with(b"/*") {
            mask_comment_bytes(masked, source, index, 2);
            depth += 1;
            continue;
        }
        if source[*index..].starts_with(b"*/") {
            mask_comment_bytes(masked, source, index, 2);
            depth -= 1;
            if depth == 0 {
                break;
            }
            continue;
        }
        if source[*index] != b'\n' && source[*index] != b'\r' {
            masked[*index] = b' ';
        }
        *index += 1;
    }
}

fn mask_comment_bytes(masked: &mut [u8], source: &[u8], index: &mut usize, count: usize) {
    for _ in 0..count {
        if *index >= source.len() {
            break;
        }
        if source[*index] != b'\n' && source[*index] != b'\r' {
            masked[*index] = b' ';
        }
        *index += 1;
    }
}

fn flatten(stream: TokenStream, tokens: &mut Vec<LexedToken>) {
    for token in stream {
        match token {
            TokenTree::Group(group) => {
                if group.delimiter() != Delimiter::None {
                    push_span(group.span_open(), tokens);
                }
                flatten(group.stream(), tokens);
                if group.delimiter() != Delimiter::None {
                    push_span(group.span_close(), tokens);
                }
            }
            TokenTree::Ident(ident) => push_span(ident.span(), tokens),
            TokenTree::Punct(punct) => push_span(punct.span(), tokens),
            TokenTree::Literal(literal) => push_span(literal.span(), tokens),
        }
    }
}

fn push_span(span: Span, tokens: &mut Vec<LexedToken>) {
    let start = span.start();
    let end = span.end();
    tokens.push(LexedToken {
        start: TokenPosition {
            line: start.line,
            column: start.column,
        },
        end: TokenPosition {
            line: end.line,
            column: end.column,
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_whitespace_do_not_change_token_count() {
        let plain = lex("fn f(value: i32) -> i32 { value + 1 }").unwrap();
        let decorated = lex(
            "/// documentation\nfn /* between */ f( value: i32 ) -> i32 {\n  // keep this readable\n  value + 1\n}\n/** trailing docs */",
        )
        .unwrap();

        assert_eq!(plain.total_tokens(), decorated.total_tokens());
    }

    #[test]
    fn punctuation_and_delimiters_are_counted_individually() {
        let source = lex("fn f<'a>(value: &'a str) -> Option<&'a str> { Some(value) }").unwrap();

        // `->`, `&`, `::`-style punctuation, and all delimiters are retained
        // as individual lexical tokens. The exact total protects the model's
        // deliberately frozen convention.
        assert_eq!(source.total_tokens(), 29);
    }

    #[test]
    fn literals_lifetimes_unicode_and_compound_punctuation_have_fixed_counts() {
        assert_eq!(lex(r##"r#"a -> b"#"##).unwrap().total_tokens(), 1);
        assert_eq!(lex("'a").unwrap().total_tokens(), 2);
        assert_eq!(lex("λ").unwrap().total_tokens(), 1);
        assert_eq!(lex("'\\''").unwrap().total_tokens(), 1);
        assert_eq!(lex("-> => ..=").unwrap().total_tokens(), 7);
        assert_eq!(lex("(λ)").unwrap().total_tokens(), 3);
    }

    #[test]
    fn comment_markers_inside_literals_and_comments_are_not_reinterpreted() {
        let source = r##"fn f<'a>(value: &'a str) {
            let raw = r#"/// keep this literal /* intact */"#;
            let character = 'λ';
            /* ordinary comment containing /// and a doc marker spelling */
            let _ = (value, raw, character);
        }"##;
        let without_comments = r##"fn f<'a>(value: &'a str) {
            let raw = r#"/// keep this literal /* intact */"#;
            let character = 'λ';
            let _ = (value, raw, character);
        }"##;

        assert_eq!(
            lex(source).unwrap().total_tokens(),
            lex(without_comments).unwrap().total_tokens()
        );
        assert_eq!(
            lex("/* outer /* inner */ still outer */ fn f() {}")
                .unwrap()
                .total_tokens(),
            lex("fn f() {}").unwrap().total_tokens()
        );
        assert_eq!(
            lex("/** outer /* nested */ still outer */ fn f() {}")
                .unwrap()
                .total_tokens(),
            lex("fn f() {}").unwrap().total_tokens()
        );
    }

    #[test]
    fn macro_bodies_and_literals_are_counted_without_expansion() {
        let source = lex(
            r##"fn f() { println!(r#"a -> b"#); macro_rules! local { ($x:expr) => { $x } } }"##,
        )
        .unwrap();

        assert!(source.total_tokens() > 20);
    }
}
