//! Syntax highlighting for the text viewer.
//!
//! Zero external dependencies — single-pass character scanner per line.
//! A [`LineState`] is threaded across lines to handle block comments (`/* */`)
//! and Python triple-quoted strings (`"""`, `'''`).
//!
//! The tokenizer is intentionally "basic": it covers the constructs that make
//! a diff noticeable (keywords, strings, comments, numbers) without attempting
//! full grammar correctness. Edge cases (nested raw strings, heredocs, …) fall
//! back to `Plain` rather than mislabelling them.

// ─── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Plain,
    Keyword,
    Type,
    String,
    Comment,
    Number,
    Macro, // Rust `name!`, shell `$VAR`
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Shell,
    Json,
    Toml,
    Yaml,
    Unknown,
}

/// State carried from one line into the next.
#[derive(Debug, Clone, Copy, Default)]
pub struct LineState {
    /// Inside a `/* ... */` block comment.
    pub in_block_comment: bool,
    /// Inside a Python triple-quoted string; holds the quote char (`"` or `'`).
    pub in_triple_string: Option<char>,
}

// ─── Language detection ───────────────────────────────────────────────────────

pub fn lang_for_name(name: &str) -> Lang {
    let lower = name.to_ascii_lowercase();
    // Bare filenames first (no extension).
    match lower.as_str() {
        "makefile" | "gnumakefile" | "dockerfile" | "justfile"
        | ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".bash_aliases"
        | ".env" => return Lang::Shell,
        _ => {}
    }
    let ext = match lower.rsplit_once('.') {
        Some((_, e)) => e,
        None => return Lang::Unknown,
    };
    match ext {
        "rs" => Lang::Rust,
        "py" | "pyw" | "pyi" => Lang::Python,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "ts" | "tsx" => Lang::TypeScript,
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" => Lang::Shell,
        "json" | "jsonc" => Lang::Json,
        "toml" => Lang::Toml,
        "yaml" | "yml" => Lang::Yaml,
        _ => Lang::Unknown,
    }
}

// ─── Keyword tables ───────────────────────────────────────────────────────────

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn",
    "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "static", "struct", "super", "trait", "true", "type", "union",
    "unsafe", "use", "where", "while", "yield",
];

const RUST_TYPES: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128",
    "isize", "u8", "u16", "u32", "u64", "u128", "usize", "str",
    "String", "Vec", "Option", "Result", "Box", "Arc", "Rc",
    "Cell", "RefCell", "Mutex", "RwLock", "Self",
];

const PYTHON_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "False", "finally", "for",
    "from", "global", "if", "import", "in", "is", "lambda", "None",
    "nonlocal", "not", "or", "pass", "raise", "return", "True", "try",
    "while", "with", "yield",
];

const JS_KW: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue",
    "debugger", "default", "delete", "do", "else", "export", "extends",
    "false", "finally", "for", "from", "function", "if", "import", "in",
    "instanceof", "let", "new", "null", "of", "return", "static", "super",
    "switch", "this", "throw", "true", "try", "typeof", "undefined", "var",
    "void", "while", "with", "yield",
];

const TS_EXTRA_KW: &[&str] = &[
    "abstract", "any", "as", "boolean", "declare", "enum", "implements",
    "interface", "is", "keyof", "module", "namespace", "never", "number",
    "object", "override", "readonly", "string", "type", "unknown",
];

const SHELL_KW: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "do", "done",
    "case", "esac", "in", "function", "return", "local", "export",
    "readonly", "break", "continue", "exit", "echo", "source",
    "alias", "unset", "shift", "trap", "declare",
];

const JSON_KW: &[&str] = &["true", "false", "null"];

// ─── Main tokenizer ───────────────────────────────────────────────────────────

/// Tokenize one line, threading `state` in and out.
///
/// Returns a list of `(kind, text)` pairs whose concatenation equals `line`.
pub fn tokenize(lang: Lang, line: &str, mut state: LineState) -> (Vec<(TokenKind, String)>, LineState) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut spans: Vec<(TokenKind, String)> = Vec::new();
    let mut pos = 0;

    // ── Carry-over: inside a triple-quoted string ──
    if let Some(q) = state.in_triple_string {
        match find_triple(&chars, 0, q) {
            Some(end) => {
                emit(&mut spans, TokenKind::String, &chars[..end + 3]);
                pos = end + 3;
                state.in_triple_string = None;
            }
            None => {
                emit(&mut spans, TokenKind::String, &chars);
                return (spans, state);
            }
        }
    }

    // ── Carry-over: inside a block comment ──
    if state.in_block_comment {
        match find_block_end(&chars, 0) {
            Some(end) => {
                emit(&mut spans, TokenKind::Comment, &chars[..end + 2]);
                pos = end + 2;
                state.in_block_comment = false;
            }
            None => {
                emit(&mut spans, TokenKind::Comment, &chars);
                return (spans, state);
            }
        }
    }

    // ── TOML: section headers colour the whole line ──
    if lang == Lang::Toml {
        let first_nws = chars.iter().position(|&c| !c.is_whitespace()).unwrap_or(n);
        if first_nws < n && chars[first_nws] == '[' {
            if first_nws > 0 {
                emit(&mut spans, TokenKind::Plain, &chars[..first_nws]);
            }
            emit(&mut spans, TokenKind::Type, &chars[first_nws..]);
            return (spans, state);
        }
    }

    // ── Main scan ──
    while pos < n {
        let ch = chars[pos];

        // Block comment: /* ... */
        if has_block_comment(lang) && ch == '/' && chars.get(pos + 1) == Some(&'*') {
            let search = pos + 2;
            match find_block_end(&chars, search) {
                Some(rel) => {
                    emit(&mut spans, TokenKind::Comment, &chars[pos..rel + 2]);
                    pos = rel + 2;
                }
                None => {
                    emit(&mut spans, TokenKind::Comment, &chars[pos..]);
                    state.in_block_comment = true;
                    return (spans, state);
                }
            }
            continue;
        }

        // Line comment: //
        if has_slash_comment(lang) && ch == '/' && chars.get(pos + 1) == Some(&'/') {
            emit(&mut spans, TokenKind::Comment, &chars[pos..]);
            return (spans, state);
        }

        // Line comment: #
        if has_hash_comment(lang) && ch == '#' {
            emit(&mut spans, TokenKind::Comment, &chars[pos..]);
            return (spans, state);
        }

        // Python triple-quoted strings: """ or '''
        if lang == Lang::Python && (ch == '"' || ch == '\'')
            && chars.get(pos + 1) == Some(&ch)
            && chars.get(pos + 2) == Some(&ch)
        {
            let search = pos + 3;
            match find_triple(&chars, search, ch) {
                Some(rel) => {
                    emit(&mut spans, TokenKind::String, &chars[pos..rel + 3]);
                    pos = rel + 3;
                }
                None => {
                    emit(&mut spans, TokenKind::String, &chars[pos..]);
                    state.in_triple_string = Some(ch);
                    return (spans, state);
                }
            }
            continue;
        }

        // Regular string: " or '
        if is_string_start(lang, ch) {
            // Rust lifetime heuristic: 'ident without nearby closing quote.
            if lang == Lang::Rust && ch == '\'' {
                let id_len = chars[pos + 1..]
                    .iter()
                    .take_while(|&&c| c.is_alphanumeric() || c == '_')
                    .count();
                if id_len > 0 {
                    let after = pos + 1 + id_len;
                    let has_close = chars[after..].iter().take(2).any(|&c| c == '\'');
                    if !has_close {
                        // Lifetime: emit as plain and move on.
                        emit(&mut spans, TokenKind::Plain, &chars[pos..after]);
                        pos = after;
                        continue;
                    }
                }
            }
            let (str_end, closed) = scan_string(&chars, pos + 1, ch);
            let end = if closed { str_end + 1 } else { str_end };
            emit(&mut spans, TokenKind::String, &chars[pos..end]);
            pos = end;
            continue;
        }

        // Shell / JS variable: $VAR or ${VAR}
        if ch == '$' && has_dollar_var(lang) {
            if chars.get(pos + 1) == Some(&'{') {
                let close = chars[pos + 2..]
                    .iter()
                    .position(|&c| c == '}')
                    .map(|i| pos + 2 + i + 1)
                    .unwrap_or(n);
                emit(&mut spans, TokenKind::Macro, &chars[pos..close]);
                pos = close;
            } else {
                let id_end = pos + 1
                    + chars[pos + 1..]
                        .iter()
                        .take_while(|&&c| c.is_alphanumeric() || c == '_')
                        .count();
                emit(&mut spans, TokenKind::Macro, &chars[pos..id_end.max(pos + 1)]);
                pos = id_end.max(pos + 1);
            }
            continue;
        }

        // Number: digits, 0x hex, float with optional exponent.
        if ch.is_ascii_digit()
            || (ch == '.'
                && chars.get(pos + 1).map_or(false, |c| c.is_ascii_digit()))
        {
            let end = scan_number(&chars, pos);
            emit(&mut spans, TokenKind::Number, &chars[pos..end]);
            pos = end;
            continue;
        }

        // Identifier → keyword / type / macro / plain.
        if ch.is_alphabetic() || ch == '_' {
            let id_end = pos
                + chars[pos..]
                    .iter()
                    .take_while(|&&c| c.is_alphanumeric() || c == '_')
                    .count();
            let word: String = chars[pos..id_end].iter().collect();

            // Rust macro call: name!
            if lang == Lang::Rust && chars.get(id_end) == Some(&'!') {
                emit(&mut spans, TokenKind::Macro, &chars[pos..id_end + 1]);
                pos = id_end + 1;
                continue;
            }

            // YAML key: word immediately followed by ':'
            if lang == Lang::Yaml && chars.get(id_end) == Some(&':') {
                let after_colon = chars.get(id_end + 1);
                if after_colon.map_or(true, |&c| c == ' ' || c == '\t') {
                    emit(&mut spans, TokenKind::Type, &chars[pos..id_end]);
                    pos = id_end;
                    continue;
                }
            }

            emit(&mut spans, classify_word(lang, &word), &chars[pos..id_end]);
            pos = id_end;
            continue;
        }

        // Everything else: plain (punctuation, operators, whitespace).
        emit(&mut spans, TokenKind::Plain, &chars[pos..pos + 1]);
        pos += 1;
    }

    (spans, state)
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Find the `*/` that closes a block comment, starting the search at `from`.
/// Returns the index of the `*` (so the closing sequence is `[result..result+2]`).
fn find_block_end(chars: &[char], from: usize) -> Option<usize> {
    chars[from..]
        .windows(2)
        .position(|w| w[0] == '*' && w[1] == '/')
        .map(|p| from + p)
}

/// Find the closing triple quote `qqq` starting at `from`.
/// Returns the index of the first `q` in the closing triple.
fn find_triple(chars: &[char], from: usize, q: char) -> Option<usize> {
    chars[from..]
        .windows(3)
        .position(|w| w[0] == q && w[1] == q && w[2] == q)
        .map(|p| from + p)
}

/// Scan a string body starting just after the opening quote.
/// Returns `(pos_of_closing_quote, closed)`.
fn scan_string(chars: &[char], from: usize, quote: char) -> (usize, bool) {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == quote {
            return (i, true);
        }
        i += 1;
    }
    (i, false)
}

/// Scan past a numeric literal, returning the end position.
fn scan_number(chars: &[char], from: usize) -> usize {
    let mut i = from;
    // Hex: 0x…
    if chars.get(i) == Some(&'0') && matches!(chars.get(i + 1), Some(&'x') | Some(&'X')) {
        i += 2;
        while i < chars.len() && (chars[i].is_ascii_hexdigit() || chars[i] == '_') {
            i += 1;
        }
        return i;
    }
    // Integer and optional decimal part.
    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
        i += 1;
    }
    if i < chars.len()
        && chars[i] == '.'
        && chars.get(i + 1).map_or(false, |c| c.is_ascii_digit())
    {
        i += 1;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
            i += 1;
        }
    }
    // Exponent: e+3, E-10.
    if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
        i += 1;
        if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
            i += 1;
        }
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
    }
    // Rust numeric suffix: u8, i32, f64, usize, …
    if i < chars.len() && matches!(chars[i], 'u' | 'i' | 'f') {
        let end = i + chars[i..].iter().take_while(|&&c| c.is_alphanumeric()).count();
        i = end;
    }
    i
}

fn classify_word(lang: Lang, word: &str) -> TokenKind {
    match lang {
        Lang::Rust => {
            if RUST_KW.contains(&word) {
                TokenKind::Keyword
            } else if RUST_TYPES.contains(&word) {
                TokenKind::Type
            } else {
                TokenKind::Plain
            }
        }
        Lang::Python => {
            if PYTHON_KW.contains(&word) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            }
        }
        Lang::JavaScript => {
            if JS_KW.contains(&word) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            }
        }
        Lang::TypeScript => {
            if JS_KW.contains(&word) || TS_EXTRA_KW.contains(&word) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            }
        }
        Lang::Shell => {
            if SHELL_KW.contains(&word) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            }
        }
        Lang::Json => {
            if JSON_KW.contains(&word) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            }
        }
        Lang::Toml | Lang::Yaml | Lang::Unknown => TokenKind::Plain,
    }
}

/// `true` if `lang` uses `/* */` block comments.
fn has_block_comment(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Rust | Lang::JavaScript | Lang::TypeScript | Lang::Json | Lang::Unknown
    )
}

/// `true` if `lang` uses `//` line comments.
fn has_slash_comment(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Rust | Lang::JavaScript | Lang::TypeScript | Lang::Json | Lang::Unknown
    )
}

/// `true` if `lang` uses `#` line comments.
fn has_hash_comment(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Python | Lang::Shell | Lang::Toml | Lang::Yaml | Lang::Unknown
    )
}

/// `true` if `ch` opens a string literal in `lang`.
fn is_string_start(lang: Lang, ch: char) -> bool {
    match lang {
        // YAML bare scalars are not strings; only explicitly quoted ones are.
        Lang::Yaml => ch == '"',
        // Shell single quotes are strong-quoting; include them.
        _ => ch == '"' || ch == '\'',
    }
}

/// `true` if `lang` treats `$` as a variable sigil.
fn has_dollar_var(lang: Lang) -> bool {
    matches!(lang, Lang::Shell | Lang::JavaScript | Lang::TypeScript)
}

/// Append a span, merging with the previous one if its kind matches.
fn emit(spans: &mut Vec<(TokenKind, String)>, kind: TokenKind, chars: &[char]) {
    if chars.is_empty() {
        return;
    }
    let text: String = chars.iter().collect();
    if let Some(last) = spans.last_mut() {
        if last.0 == kind {
            last.1.push_str(&text);
            return;
        }
    }
    spans.push((kind, text));
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lang: Lang, line: &str) -> Vec<TokenKind> {
        tokenize(lang, line, LineState::default()).0.into_iter().map(|(k, _)| k).collect()
    }

    #[allow(dead_code)]
    fn texts(lang: Lang, line: &str) -> Vec<String> {
        tokenize(lang, line, LineState::default()).0.into_iter().map(|(_, t)| t).collect()
    }

    fn roundtrip(lang: Lang, line: &str) {
        let (spans, _) = tokenize(lang, line, LineState::default());
        let rejoined: String = spans.into_iter().map(|(_, t)| t).collect();
        assert_eq!(rejoined, line, "spans must reconstruct the original line");
    }

    // ── Roundtrip invariant ───────────────────────────────────────────────────

    #[test]
    fn roundtrip_rust() {
        roundtrip(Lang::Rust, r#"    let x: Vec<u8> = vec![1, 2, 3]; // comment"#);
    }

    #[test]
    fn roundtrip_python() {
        roundtrip(Lang::Python, r#"def foo(x: int) -> str:  # hint"#);
    }

    #[test]
    fn roundtrip_empty() {
        roundtrip(Lang::Rust, "");
    }

    #[test]
    fn roundtrip_whitespace_only() {
        roundtrip(Lang::Rust, "   ");
    }

    // ── Language detection ────────────────────────────────────────────────────

    #[test]
    fn lang_for_rs() {
        assert_eq!(lang_for_name("main.rs"), Lang::Rust);
    }

    #[test]
    fn lang_for_py() {
        assert_eq!(lang_for_name("script.py"), Lang::Python);
    }

    #[test]
    fn lang_for_json() {
        assert_eq!(lang_for_name("data.json"), Lang::Json);
    }

    #[test]
    fn lang_for_dockerfile() {
        assert_eq!(lang_for_name("Dockerfile"), Lang::Shell);
    }

    #[test]
    fn lang_for_unknown() {
        assert_eq!(lang_for_name("binary.bin"), Lang::Unknown);
    }

    // ── Comments ─────────────────────────────────────────────────────────────

    #[test]
    fn rust_line_comment() {
        let ks = kinds(Lang::Rust, "// this is a comment");
        assert!(ks.iter().all(|&k| k == TokenKind::Comment));
    }

    #[test]
    fn rust_inline_comment() {
        let (spans, _) = tokenize(Lang::Rust, "let x = 1; // ok", LineState::default());
        let comment = spans.iter().find(|(k, _)| *k == TokenKind::Comment);
        assert!(comment.is_some());
        assert!(comment.unwrap().1.contains("// ok"));
    }

    #[test]
    fn rust_block_comment_single_line() {
        let ks = kinds(Lang::Rust, "/* block */");
        assert!(ks.iter().all(|&k| k == TokenKind::Comment));
    }

    #[test]
    fn rust_block_comment_spanning_lines() {
        let state0 = LineState::default();
        let (_, s1) = tokenize(Lang::Rust, "/* start", state0);
        assert!(s1.in_block_comment);
        let (spans2, s2) = tokenize(Lang::Rust, "middle line", s1);
        assert!(s2.in_block_comment);
        assert!(spans2.iter().all(|(k, _)| *k == TokenKind::Comment));
        let (_, s3) = tokenize(Lang::Rust, "end */", s2);
        assert!(!s3.in_block_comment);
    }

    #[test]
    fn python_hash_comment() {
        let ks = kinds(Lang::Python, "# whole line");
        assert!(ks.iter().all(|&k| k == TokenKind::Comment));
    }

    // ── Keywords ─────────────────────────────────────────────────────────────

    #[test]
    fn rust_keywords_identified() {
        let (spans, _) = tokenize(Lang::Rust, "fn main() {}", LineState::default());
        let kw: Vec<&str> = spans
            .iter()
            .filter(|(k, _)| *k == TokenKind::Keyword)
            .map(|(_, t)| t.as_str())
            .collect();
        assert!(kw.contains(&"fn"));
    }

    #[test]
    fn rust_type_identified() {
        let (spans, _) = tokenize(Lang::Rust, "let v: Vec<u8>;", LineState::default());
        let types: Vec<&str> = spans
            .iter()
            .filter(|(k, _)| *k == TokenKind::Type)
            .map(|(_, t)| t.as_str())
            .collect();
        assert!(types.contains(&"Vec"));
        assert!(types.contains(&"u8"));
    }

    #[test]
    fn python_keyword_def() {
        let ks = kinds(Lang::Python, "def foo():");
        assert_eq!(ks[0], TokenKind::Keyword);
    }

    // ── Strings ──────────────────────────────────────────────────────────────

    #[test]
    fn rust_double_quoted_string() {
        let (spans, _) = tokenize(Lang::Rust, r#"let s = "hello";"#, LineState::default());
        let strs: Vec<&str> = spans
            .iter()
            .filter(|(k, _)| *k == TokenKind::String)
            .map(|(_, t)| t.as_str())
            .collect();
        assert!(strs.iter().any(|s| s.contains("hello")));
    }

    #[test]
    fn rust_string_with_escaped_quote() {
        let (spans, _) = tokenize(Lang::Rust, r#"let s = "say \"hi\"";"#, LineState::default());
        let str_text: String = spans
            .iter()
            .filter(|(k, _)| *k == TokenKind::String)
            .map(|(_, t)| t.clone())
            .collect();
        assert!(str_text.contains("say"));
    }

    #[test]
    fn python_triple_string_single_line() {
        let (spans, state) = tokenize(Lang::Python, r#""""hello""""#, LineState::default());
        assert!(!state.in_triple_string.is_some());
        assert!(spans.iter().any(|(k, _)| *k == TokenKind::String));
    }

    #[test]
    fn python_triple_string_spanning_lines() {
        let (_, s1) = tokenize(Lang::Python, r#"x = """start"#, LineState::default());
        assert!(s1.in_triple_string.is_some());
        let (spans2, s2) = tokenize(Lang::Python, "middle", s1);
        assert!(s2.in_triple_string.is_some());
        assert!(spans2.iter().all(|(k, _)| *k == TokenKind::String));
        let (_, s3) = tokenize(Lang::Python, r#"end""""#, s2);
        assert!(s3.in_triple_string.is_none());
    }

    // ── Numbers ──────────────────────────────────────────────────────────────

    #[test]
    fn integer_number() {
        let (spans, _) = tokenize(Lang::Rust, "let x = 42;", LineState::default());
        assert!(spans.iter().any(|(k, t)| *k == TokenKind::Number && t == "42"));
    }

    #[test]
    fn hex_number() {
        let (spans, _) = tokenize(Lang::Rust, "0xFF", LineState::default());
        assert!(spans.iter().any(|(k, _)| *k == TokenKind::Number));
    }

    #[test]
    fn float_number() {
        let (spans, _) = tokenize(Lang::Python, "x = 3.14", LineState::default());
        assert!(spans.iter().any(|(k, t)| *k == TokenKind::Number && t.contains('.')));
    }

    // ── Rust macros ──────────────────────────────────────────────────────────

    #[test]
    fn rust_macro_call() {
        let (spans, _) = tokenize(Lang::Rust, "println!(\"hi\");", LineState::default());
        assert!(spans.iter().any(|(k, t)| *k == TokenKind::Macro && t == "println!"));
    }

    // ── Shell variables ───────────────────────────────────────────────────────

    #[test]
    fn shell_dollar_var() {
        let (spans, _) = tokenize(Lang::Shell, "echo $HOME", LineState::default());
        assert!(spans.iter().any(|(k, t)| *k == TokenKind::Macro && t == "$HOME"));
    }

    #[test]
    fn shell_brace_var() {
        let (spans, _) = tokenize(Lang::Shell, "echo ${MY_VAR}", LineState::default());
        assert!(spans.iter().any(|(k, t)| *k == TokenKind::Macro && t == "${MY_VAR}"));
    }

    // ── TOML ─────────────────────────────────────────────────────────────────

    #[test]
    fn toml_section_header() {
        let (spans, _) = tokenize(Lang::Toml, "[dependencies]", LineState::default());
        assert!(spans.iter().all(|(k, _)| *k == TokenKind::Type));
    }

    // ── YAML ─────────────────────────────────────────────────────────────────

    #[test]
    fn yaml_key() {
        let (spans, _) = tokenize(Lang::Yaml, "name: blink", LineState::default());
        let types: Vec<&str> = spans
            .iter()
            .filter(|(k, _)| *k == TokenKind::Type)
            .map(|(_, t)| t.as_str())
            .collect();
        assert!(types.contains(&"name"));
    }
}
