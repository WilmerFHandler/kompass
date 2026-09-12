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
        self.tokens.partition_point(|token| token.start < position)
    }

    fn upper_bound_end(&self, position: TokenPosition) -> usize {
        self.tokens.partition_point(|token| token.end <= position)
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
    DocCommentMasker::new(source).run()
}

#[derive(Clone, Copy, Debug)]
enum MaskState {
    Code,
    Quoted { quote: u8, escaped: bool },
    RawString { hashes: usize },
    BlockComment { depth: usize, mask: bool },
}

struct DocCommentMasker<'a> {
    source: &'a [u8],
    masked: Vec<u8>,
    index: usize,
    state: MaskState,
}

impl<'a> DocCommentMasker<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source: source.as_bytes(),
            masked: source.as_bytes().to_vec(),
            index: 0,
            state: MaskState::Code,
        }
    }

    fn run(mut self) -> String {
        while self.index < self.source.len() {
            self.advance();
        }
        String::from_utf8(self.masked).expect("masking comments preserves UTF-8 bytes")
    }

    fn advance(&mut self) {
        match self.state {
            MaskState::Code => self.advance_code(),
            MaskState::Quoted { quote, escaped } => self.advance_quoted(quote, escaped),
            MaskState::RawString { hashes } => self.advance_raw_string(hashes),
            MaskState::BlockComment { depth, mask } => self.advance_block_comment(depth, mask),
        }
    }

    fn advance_code(&mut self) {
        let bytes = self.source;
        if let Some((opening_length, hashes)) = raw_string_delimiter(bytes, self.index) {
            self.state = MaskState::RawString { hashes };
            self.index += opening_length;
        } else if bytes[self.index] == b'"' {
            self.state = MaskState::Quoted {
                quote: b'"',
                escaped: false,
            };
            self.index += 1;
        } else if bytes[self.index] == b'\'' && looks_like_char_literal(bytes, self.index) {
            self.state = MaskState::Quoted {
                quote: b'\'',
                escaped: false,
            };
            self.index += 1;
        } else if bytes[self.index..].starts_with(b"/**") || bytes[self.index..].starts_with(b"/*!")
        {
            self.state = MaskState::BlockComment {
                depth: 1,
                mask: true,
            };
            mask_comment_bytes(&mut self.masked, bytes, &mut self.index, 2);
        } else if bytes[self.index..].starts_with(b"/*") {
            self.state = MaskState::BlockComment {
                depth: 1,
                mask: false,
            };
            self.index += 2;
        } else if bytes[self.index..].starts_with(b"///") || bytes[self.index..].starts_with(b"//!")
        {
            mask_until_newline(&mut self.masked, bytes, &mut self.index);
        } else if bytes[self.index..].starts_with(b"//") {
            self.skip_line_comment();
        } else {
            self.index += 1;
        }
    }

    fn advance_quoted(&mut self, quote: u8, escaped: bool) {
        let current = self.source[self.index];
        self.index += 1;
        self.state = if escaped {
            MaskState::Quoted {
                quote,
                escaped: false,
            }
        } else if current == b'\\' {
            MaskState::Quoted {
                quote,
                escaped: true,
            }
        } else if current == quote {
            MaskState::Code
        } else {
            MaskState::Quoted {
                quote,
                escaped: false,
            }
        };
    }

    fn advance_raw_string(&mut self, hashes: usize) {
        if self.source[self.index] == b'"' && has_hashes(self.source, self.index + 1, hashes) {
            self.index += hashes + 1;
            self.state = MaskState::Code;
        } else {
            self.index += 1;
        }
    }

    fn advance_block_comment(&mut self, depth: usize, mask: bool) {
        let bytes = self.source;
        if bytes[self.index..].starts_with(b"/*") {
            if mask {
                mask_comment_bytes(&mut self.masked, bytes, &mut self.index, 2);
            } else {
                self.index += 2;
            }
            self.state = MaskState::BlockComment {
                depth: depth + 1,
                mask,
            };
        } else if bytes[self.index..].starts_with(b"*/") {
            if mask {
                mask_comment_bytes(&mut self.masked, bytes, &mut self.index, 2);
            } else {
                self.index += 2;
            }
            self.state = if depth == 1 {
                MaskState::Code
            } else {
                MaskState::BlockComment {
                    depth: depth - 1,
                    mask,
                }
            };
        } else {
            if mask && bytes[self.index] != b'\n' && bytes[self.index] != b'\r' {
                self.masked[self.index] = b' ';
            }
            self.index += 1;
        }
    }

    fn skip_line_comment(&mut self) {
        while self.index < self.source.len() && self.source[self.index] != b'\n' {
            self.index += 1;
        }
    }
}

pub(crate) fn raw_string_delimiter(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let prefix = match bytes.get(index) {
        Some(b'r') => index,
        Some(b'b') if bytes.get(index + 1) == Some(&b'r') => index + 1,
        _ => return None,
    };
    let mut cursor = prefix + 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some((cursor - index + 1, cursor - prefix - 1))
}

/// Recognize the literal forms needed to distinguish a character from a
/// lifetime or label. Plain characters inspect at most one UTF-8 scalar, and
/// Unicode escapes inspect only the bytes through their closing brace.
pub(crate) fn looks_like_char_literal(bytes: &[u8], index: usize) -> bool {
    if bytes.get(index) != Some(&b'\'') {
        return false;
    }

    match bytes.get(index + 1) {
        Some(b'\\') => escaped_char_literal(bytes, index + 2),
        Some(b'\n' | b'\r') | None => false,
        Some(&first_byte) => plain_char_literal(bytes, index + 1, first_byte),
    }
}

fn escaped_char_literal(bytes: &[u8], escape: usize) -> bool {
    match bytes.get(escape) {
        Some(b'n' | b'r' | b't' | b'\\' | b'\'' | b'"' | b'0') => {
            bytes.get(escape + 1) == Some(&b'\'')
        }
        Some(b'x') => {
            bytes
                .get(escape + 1)
                .is_some_and(|byte| byte.is_ascii_hexdigit())
                && bytes
                    .get(escape + 2)
                    .is_some_and(|byte| byte.is_ascii_hexdigit())
                && bytes.get(escape + 3) == Some(&b'\'')
        }
        Some(b'u') => unicode_char_escape(bytes, escape + 1),
        _ => false,
    }
}

fn unicode_char_escape(bytes: &[u8], brace: usize) -> bool {
    if bytes.get(brace) != Some(&b'{') {
        return false;
    }

    let mut cursor = brace + 1;
    let mut has_digit = false;
    while let Some(&byte) = bytes.get(cursor) {
        if byte == b'}' {
            return has_digit && bytes.get(cursor + 1) == Some(&b'\'');
        }
        if byte == b'_' {
            cursor += 1;
            continue;
        }
        if !byte.is_ascii_hexdigit() {
            return false;
        }
        has_digit = true;
        cursor += 1;
    }
    false
}

fn plain_char_literal(bytes: &[u8], start: usize, first_byte: u8) -> bool {
    let Some(width) = utf8_char_width(first_byte) else {
        return false;
    };
    let Some(end) = start.checked_add(width) else {
        return false;
    };
    let Some(candidate) = bytes.get(start..end) else {
        return false;
    };
    let Some(character) = std::str::from_utf8(candidate)
        .ok()
        .and_then(|text| text.chars().next())
    else {
        return false;
    };
    character != '\n' && character != '\r' && bytes.get(end) == Some(&b'\'')
}

fn utf8_char_width(first_byte: u8) -> Option<usize> {
    match first_byte {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
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
        // as individual lexical tokens. The exact total protects the score's
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

    #[test]
    fn char_literals_are_bounded_and_distinguish_lifetimes_and_labels() {
        for source in [
            "'λ'",
            "'\\u{1_F980}'",
            "'\\u{41_}'",
            "'\\x41'",
            "'\\''",
            "'\\n'",
        ] {
            assert!(
                looks_like_char_literal(source.as_bytes(), 0),
                "expected {source:?} to be a char literal"
            );
        }

        for source in [
            "'a",
            "'label:",
            "'\\u{}'",
            "'\\u{not-hex}'",
            "'\\u{1-2}'",
            "'\\x4'",
            "'\\q'",
        ] {
            assert!(
                !looks_like_char_literal(source.as_bytes(), 0),
                "expected {source:?} not to be a char literal"
            );
        }

        let mut malformed = vec![b'\'', b'\\', b'u', b'{'];
        malformed.extend(std::iter::repeat_n(b'0', 1024));
        assert!(!looks_like_char_literal(&malformed, 0));
        assert!(!looks_like_char_literal(b"", 0));
        assert!(!looks_like_char_literal(b"x'a'", 0));
    }

    #[test]
    fn raw_strings_and_nested_doc_comments_keep_comment_markers_opaque() {
        let source = r####"/// ignored documentation
fn render() {
    let raw = br###"/// this stays in the literal /* and here */"###;
    /** outer doc /* nested doc */ still outer */
    let value = 1;
}
"####;
        let without_docs = r####"fn render() {
    let raw = br###"/// this stays in the literal /* and here */"###;
    let value = 1;
}
"####;

        assert_eq!(
            lex(source).unwrap().total_tokens(),
            lex(without_docs).unwrap().total_tokens()
        );
    }
}
