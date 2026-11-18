//! Syntax highlighting for the text viewer.
//!
//! Zero external dependencies — a single-pass character scanner per line.
//! A [`LineState`] is threaded from line to line for what can run past the
//! end of one: block comments (nested, in Rust), and strings that span
//! lines — Rust strings, Python and TOML triple quotes, JavaScript
//! template literals, shell and YAML quotes, and any string continued with
//! a trailing backslash.
//!
//! The tokenizer is deliberately basic: keywords, strings, comments and
//! numbers, without a grammar. Where it can't tell, it leaves text plain
//! rather than guess: a file with no language ([`Lang::Unknown`]) — prose,
//! logs, release notes — isn't highlighted at all, since an apostrophe, a
//! URL or a `#` in prose is not code. Heredocs and regex literals are not
//! recognised.

// ─── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Plain,
    Keyword,
    Type,
    String,
    Comment,
    Number,
    Macro, // Rust `name!`, shell `$VAR`, batch `%VAR%`, C `#include`
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Shell,
    /// Windows batch (`.bat`, `.cmd`).
    Batch,
    PowerShell,
    Json,
    Toml,
    Yaml,
    /// The C family and its look-alikes: C, C++, Go, Java, C#, Kotlin,
    /// Swift, CSS… `//` and `/* */` comments, `"`/`'` strings, backtick raw
    /// strings, `#directive` lines. No `#` comments.
    CLike,
    /// INI-style config: `[section]`, `key = value`, `;`/`#` comment lines.
    Ini,
    /// No language: shown as plain text.
    Unknown,
}

/// State carried from one line into the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineState {
    #[default]
    Code,
    /// Inside a block comment. `depth` counts nesting, which only Rust has.
    BlockComment { kind: BlockKind, depth: u8 },
    /// Inside a string that ran past the end of the previous line.
    Str(Quote),
}

/// Which block comment: `/* */` or PowerShell's `<# #>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Slash,
    PowerShell,
}

impl BlockKind {
    fn open(self) -> &'static str {
        match self {
            Self::Slash => "/*",
            Self::PowerShell => "<#",
        }
    }
    fn close(self) -> &'static str {
        match self {
            Self::Slash => "*/",
            Self::PowerShell => "#>",
        }
    }
}

/// How a string closes: `count` of `ch` (1, or 3 for triple quotes), then
/// `hashes` `#`s (Rust raw strings), with `escape` escaping the next
/// character (`None` where nothing escapes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    ch: char,
    count: u8,
    hashes: u8,
    escape: Option<char>,
}

impl Quote {
    const fn new(ch: char, escape: Option<char>) -> Self {
        Self {
            ch,
            count: 1,
            hashes: 0,
            escape,
        }
    }
    const fn triple(ch: char, escape: Option<char>) -> Self {
        Self {
            ch,
            count: 3,
            hashes: 0,
            escape,
        }
    }
}

// ─── Language detection ───────────────────────────────────────────────────────

pub fn lang_for_name(name: &str) -> Lang {
    let lower = name.to_ascii_lowercase();
    // Bare filenames first (no extension, or one that isn't the language).
    match lower.as_str() {
        "makefile" | "gnumakefile" | "dockerfile" | "containerfile" | "justfile" | ".bashrc"
        | ".zshrc" | ".profile" | ".bash_profile" | ".bash_aliases" | ".env" => {
            return Lang::Shell;
        }
        ".gitconfig" | ".editorconfig" => return Lang::Ini,
        _ => {}
    }
    // `.env.local`, `Dockerfile.dev`: the name says more than the suffix.
    if lower.starts_with(".env.")
        || lower.starts_with("dockerfile.")
        || lower.starts_with("containerfile.")
    {
        return Lang::Shell;
    }
    let ext = match lower.rsplit_once('.') {
        Some((_, e)) => e,
        None => return Lang::Unknown,
    };
    match ext {
        "rs" => Lang::Rust,
        "py" | "pyw" | "pyi" => Lang::Python,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "ts" | "tsx" | "mts" | "cts" => Lang::TypeScript,
        "sh" | "bash" | "zsh" | "fish" | "env" | "mk" | "dockerfile" => Lang::Shell,
        "bat" | "cmd" => Lang::Batch,
        "ps1" | "psm1" | "psd1" => Lang::PowerShell,
        "json" | "jsonc" => Lang::Json,
        "toml" => Lang::Toml,
        "yaml" | "yml" => Lang::Yaml,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "go" | "java" | "kt" | "kts"
        | "cs" | "swift" | "scala" | "dart" | "proto" | "zig" | "css" | "scss" | "less" => {
            Lang::CLike
        }
        "ini" | "cfg" | "conf" | "properties" => Lang::Ini,
        _ => Lang::Unknown,
    }
}

// ─── Keyword tables ───────────────────────────────────────────────────────────

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "union",
    "unsafe", "use", "where", "while", "yield",
];

const RUST_TYPES: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32",
    "u64", "u128", "usize", "str", "String", "Vec", "Option", "Result", "Box", "Arc", "Rc", "Cell",
    "RefCell", "Mutex", "RwLock", "Self",
];

const PYTHON_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield",
];

const JS_KW: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

const TS_EXTRA_KW: &[&str] = &[
    "abstract",
    "any",
    "as",
    "boolean",
    "declare",
    "enum",
    "implements",
    "interface",
    "is",
    "keyof",
    "module",
    "namespace",
    "never",
    "number",
    "object",
    "override",
    "readonly",
    "string",
    "type",
    "unknown",
];

const SHELL_KW: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac", "in",
    "function", "return", "local", "export", "readonly", "break", "continue", "exit", "echo",
    "source", "alias", "unset", "shift", "trap", "declare",
];

/// Matched ignoring case, as batch is.
const BATCH_KW: &[&str] = &[
    "if",
    "else",
    "for",
    "in",
    "do",
    "goto",
    "call",
    "set",
    "echo",
    "exit",
    "not",
    "exist",
    "defined",
    "errorlevel",
    "setlocal",
    "endlocal",
    "shift",
    "pause",
    "cls",
    "start",
    "title",
    "pushd",
    "popd",
    "equ",
    "neq",
    "lss",
    "leq",
    "gtr",
    "geq",
];

/// Matched ignoring case, as PowerShell is.
const POWERSHELL_KW: &[&str] = &[
    "if", "elseif", "else", "switch", "foreach", "for", "while", "do", "until", "function",
    "filter", "param", "return", "break", "continue", "throw", "try", "catch", "finally", "trap",
    "begin", "process", "end", "in", "class", "enum", "using",
];

const JSON_KW: &[&str] = &["true", "false", "null"];

/// Shared by the C family; a word that's a keyword in one and a name in
/// another is rare enough to accept.
const CLIKE_KW: &[&str] = &[
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "default",
    "break",
    "continue",
    "return",
    "goto",
    "struct",
    "union",
    "enum",
    "typedef",
    "sizeof",
    "static",
    "const",
    "extern",
    "volatile",
    "inline",
    "void",
    "int",
    "char",
    "short",
    "long",
    "float",
    "double",
    "unsigned",
    "signed",
    "bool",
    "true",
    "false",
    "null",
    "NULL",
    "nullptr",
    "class",
    "public",
    "private",
    "protected",
    "virtual",
    "override",
    "new",
    "delete",
    "this",
    "namespace",
    "using",
    "template",
    "typename",
    "try",
    "catch",
    "throw",
    "final",
    "abstract",
    "interface",
    "extends",
    "implements",
    "import",
    "package",
    "func",
    "var",
    "val",
    "fun",
    "type",
    "map",
    "chan",
    "go",
    "defer",
    "select",
    "range",
    "fallthrough",
    "let",
    "self",
    "super",
    "auto",
];

// ─── Main tokenizer ───────────────────────────────────────────────────────────

/// Tokenize one line, threading `state` in and out.
///
/// Returns a list of `(kind, text)` pairs whose concatenation equals `line`.
pub fn tokenize(lang: Lang, line: &str, state: LineState) -> (Vec<(TokenKind, String)>, LineState) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut spans: Vec<(TokenKind, String)> = Vec::new();

    if lang == Lang::Unknown {
        emit(&mut spans, TokenKind::Plain, &chars);
        return (spans, LineState::Code);
    }

    // ── Carry-over from the line before ──
    let mut pos = 0;
    match state {
        LineState::Code => {}
        LineState::BlockComment { kind, depth } => {
            match scan_block(&chars, 0, kind, depth, lang == Lang::Rust) {
                Ok(end) => {
                    emit(&mut spans, TokenKind::Comment, &chars[..end]);
                    pos = end;
                }
                Err(depth) => {
                    emit(&mut spans, TokenKind::Comment, &chars);
                    return (spans, LineState::BlockComment { kind, depth });
                }
            }
        }
        LineState::Str(q) => match find_close(&chars, 0, q) {
            Close::At(end) => {
                emit(&mut spans, TokenKind::String, &chars[..end]);
                pos = end;
            }
            Close::Open { continued } => {
                emit(&mut spans, TokenKind::String, &chars);
                let next = if carries(lang, q, continued) {
                    LineState::Str(q)
                } else {
                    LineState::Code
                };
                return (spans, next);
            }
        },
    }

    // ── Rules that look at the line as a whole ──
    if pos == 0 {
        match lang {
            Lang::Toml if toml_header(&chars, &mut spans) => return (spans, LineState::Code),
            Lang::Ini => {
                ini_line(&chars, &mut spans);
                return (spans, LineState::Code);
            }
            Lang::Batch if batch_comment_or_label(&chars, &mut spans) => {
                return (spans, LineState::Code);
            }
            Lang::Yaml => pos = yaml_key(&chars, &mut spans),
            Lang::CLike => pos = clike_directive(&chars, &mut spans),
            _ => {}
        }
    }

    // ── Main scan ──
    while pos < n {
        let ch = chars[pos];

        // Block comment.
        if let Some(kind) = block_kind(lang)
            && starts_with_at(&chars, pos, kind.open())
        {
            let from = pos + 2;
            match scan_block(&chars, from, kind, 1, lang == Lang::Rust) {
                Ok(end) => {
                    emit(&mut spans, TokenKind::Comment, &chars[pos..end]);
                    pos = end;
                    continue;
                }
                Err(depth) => {
                    emit(&mut spans, TokenKind::Comment, &chars[pos..]);
                    return (spans, LineState::BlockComment { kind, depth });
                }
            }
        }

        // Line comment.
        if is_line_comment(lang, &chars, pos) {
            emit(&mut spans, TokenKind::Comment, &chars[pos..]);
            return (spans, LineState::Code);
        }

        // String.
        if let Some((q, opener)) = string_at(lang, &chars, pos) {
            match find_close(&chars, pos + opener, q) {
                Close::At(end) => {
                    emit(&mut spans, TokenKind::String, &chars[pos..end]);
                    pos = end;
                    continue;
                }
                Close::Open { continued } => {
                    emit(&mut spans, TokenKind::String, &chars[pos..]);
                    let next = if carries(lang, q, continued) {
                        LineState::Str(q)
                    } else {
                        LineState::Code
                    };
                    return (spans, next);
                }
            }
        }

        // Variables: `$VAR`, `${VAR}`, `%VAR%`…
        if let Some(end) = variable_at(lang, &chars, pos) {
            emit(&mut spans, TokenKind::Macro, &chars[pos..end]);
            pos = end;
            continue;
        }

        // Number, at the start of a word only.
        let at_word_start = pos == 0 || !is_word_char(chars[pos - 1]);
        if at_word_start
            && (ch.is_ascii_digit()
                || (ch == '.' && chars.get(pos + 1).is_some_and(|c| c.is_ascii_digit())))
        {
            let end = scan_number(lang, &chars, pos);
            emit(&mut spans, TokenKind::Number, &chars[pos..end]);
            pos = end;
            continue;
        }

        // Identifier → keyword / type / macro / plain.
        if ch.is_alphabetic() || ch == '_' {
            let id_end = pos
                + chars[pos..]
                    .iter()
                    .take_while(|&&c| is_word_char(c))
                    .count();
            let word: String = chars[pos..id_end].iter().collect();

            // Rust raw string: r"…", r#"…"#, br"…".
            if lang == Lang::Rust
                && (word == "r" || word == "br")
                && let Some((q, opener)) = rust_raw_string(&chars, id_end)
            {
                match find_close(&chars, id_end + opener, q) {
                    Close::At(end) => {
                        emit(&mut spans, TokenKind::String, &chars[pos..end]);
                        pos = end;
                        continue;
                    }
                    Close::Open { .. } => {
                        emit(&mut spans, TokenKind::String, &chars[pos..]);
                        return (spans, LineState::Str(q));
                    }
                }
            }

            // Rust macro call: name!
            if lang == Lang::Rust && chars.get(id_end) == Some(&'!') {
                emit(&mut spans, TokenKind::Macro, &chars[pos..id_end + 1]);
                pos = id_end + 1;
                continue;
            }

            emit(&mut spans, classify_word(lang, &word), &chars[pos..id_end]);
            pos = id_end;
            continue;
        }

        // Everything else: plain (punctuation, operators, whitespace).
        emit(&mut spans, TokenKind::Plain, &chars[pos..pos + 1]);
        pos += 1;
    }

    (spans, LineState::Code)
}

// ─── Comments ─────────────────────────────────────────────────────────────────

fn block_kind(lang: Lang) -> Option<BlockKind> {
    match lang {
        Lang::Rust | Lang::JavaScript | Lang::TypeScript | Lang::Json | Lang::CLike => {
            Some(BlockKind::Slash)
        }
        Lang::PowerShell => Some(BlockKind::PowerShell),
        _ => None,
    }
}

/// Scan a block comment from `from` at nesting `depth`, returning the end
/// (past the closer that brings it to zero) or the depth still open at the
/// end of the line. Only Rust nests.
fn scan_block(
    chars: &[char],
    from: usize,
    kind: BlockKind,
    mut depth: u8,
    nests: bool,
) -> Result<usize, u8> {
    let mut i = from;
    while i < chars.len() {
        if starts_with_at(chars, i, kind.close()) {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Ok(i);
            }
        } else if nests && starts_with_at(chars, i, kind.open()) {
            depth = depth.saturating_add(1);
            i += 2;
        } else {
            i += 1;
        }
    }
    Err(depth)
}

/// Whether a line comment starts at `pos`.
fn is_line_comment(lang: Lang, chars: &[char], pos: usize) -> bool {
    match lang {
        Lang::Rust | Lang::JavaScript | Lang::TypeScript | Lang::Json | Lang::CLike => {
            starts_with_at(chars, pos, "//")
        }
        // `#` outside a string is always a comment.
        Lang::Python | Lang::Toml => chars[pos] == '#',
        // Only at the start of a word: `foo#bar` and `http://x/#frag` are
        // words, not comments.
        Lang::Shell | Lang::PowerShell => {
            chars[pos] == '#'
                && (pos == 0
                    || chars[pos - 1].is_whitespace()
                    || matches!(chars[pos - 1], ';' | '|' | '&' | '(' | ')' | '{' | '}'))
        }
        // YAML needs whitespace before it (or the line start).
        Lang::Yaml => chars[pos] == '#' && (pos == 0 || chars[pos - 1].is_whitespace()),
        Lang::Batch | Lang::Ini | Lang::Unknown => false,
    }
}

// ─── Strings ──────────────────────────────────────────────────────────────────

/// The string that opens at `pos`, if one does, and the opener's length.
fn string_at(lang: Lang, chars: &[char], pos: usize) -> Option<(Quote, usize)> {
    const BS: Option<char> = Some('\\');
    let ch = chars[pos];
    let tripled = chars.get(pos + 1) == Some(&ch) && chars.get(pos + 2) == Some(&ch);
    let one = |q: Quote| Some((q, 1));
    match (lang, ch) {
        (Lang::Python, '"' | '\'') if tripled => Some((Quote::triple(ch, BS), 3)),
        (Lang::Python, '"' | '\'') => one(Quote::new(ch, BS)),
        (Lang::Toml, '"') if tripled => Some((Quote::triple('"', BS), 3)),
        (Lang::Toml, '\'') if tripled => Some((Quote::triple('\'', None), 3)),
        (Lang::Toml, '"') => one(Quote::new('"', BS)),
        (Lang::Toml, '\'') => one(Quote::new('\'', None)),
        (Lang::Rust, '"') => one(Quote::new('"', BS)),
        (Lang::Rust, '\'') if !is_lifetime(chars, pos) => one(Quote::new('\'', BS)),
        (Lang::JavaScript | Lang::TypeScript, '"' | '\'' | '`') => one(Quote::new(ch, BS)),
        (Lang::Shell, '"') => one(Quote::new('"', BS)),
        (Lang::Shell, '\'') => one(Quote::new('\'', None)),
        (Lang::PowerShell, '"') => one(Quote::new('"', Some('`'))),
        (Lang::PowerShell, '\'') => one(Quote::new('\'', None)),
        (Lang::Batch, '"') => one(Quote::new('"', None)),
        (Lang::Json, '"') => one(Quote::new('"', BS)),
        // A quote opens a YAML string only where a scalar starts: in
        // `msg: it's fine` the apostrophe is just a letter.
        (Lang::Yaml, '"' | '\'') if yaml_scalar_start(chars, pos) => {
            one(Quote::new(ch, if ch == '"' { BS } else { None }))
        }
        (Lang::CLike, '"' | '\'') => one(Quote::new(ch, BS)),
        (Lang::CLike, '`') => one(Quote::new('`', None)),
        _ => None,
    }
}

/// Rust: `'a` with no quote right after is a lifetime or label, not a
/// char literal.
fn is_lifetime(chars: &[char], pos: usize) -> bool {
    let id_len = chars[pos + 1..]
        .iter()
        .take_while(|&&c| is_word_char(c))
        .count();
    if id_len == 0 {
        return false;
    }
    let after = pos + 1 + id_len;
    !chars[after..].iter().take(2).any(|&c| c == '\'')
}

/// After an `r` or `br` at `at`: `#`s then `"` open a raw string.
fn rust_raw_string(chars: &[char], at: usize) -> Option<(Quote, usize)> {
    let hashes = chars[at..].iter().take_while(|&&c| c == '#').count();
    if chars.get(at + hashes) != Some(&'"') || hashes > u8::MAX as usize {
        return None;
    }
    let q = Quote {
        ch: '"',
        count: 1,
        hashes: hashes as u8,
        escape: None,
    };
    Some((q, hashes + 1))
}

fn yaml_scalar_start(chars: &[char], pos: usize) -> bool {
    chars[..pos]
        .iter()
        .rev()
        .find(|c| !c.is_whitespace())
        .is_none_or(|c| matches!(c, ':' | '-' | '[' | '{' | ','))
}

enum Close {
    /// Closed; the index just past the closing delimiter.
    At(usize),
    /// Still open at the end of the line; `continued` if the line ends in
    /// an escape character, which continues the string onto the next.
    Open { continued: bool },
}

/// Find where a string closes, scanning from `from` (just past its opener).
fn find_close(chars: &[char], from: usize, q: Quote) -> Close {
    let (count, hashes) = (q.count as usize, q.hashes as usize);
    let mut i = from;
    while i < chars.len() {
        if Some(chars[i]) == q.escape {
            if i + 1 >= chars.len() {
                return Close::Open { continued: true };
            }
            i += 2;
            continue;
        }
        // `''` inside a single-quoted string with no escape character is
        // a quote, as YAML and PowerShell write one; in shell it's two
        // adjacent strings, which looks the same.
        if q.ch == '\''
            && q.escape.is_none()
            && count == 1
            && chars.get(i + 1) == Some(&'\'')
            && chars[i] == '\''
        {
            i += 2;
            continue;
        }
        if chars[i] == q.ch
            && (1..count).all(|k| chars.get(i + k) == Some(&q.ch))
            && (0..hashes).all(|k| chars.get(i + count + k) == Some(&'#'))
        {
            return Close::At(i + count + hashes);
        }
        i += 1;
    }
    Close::Open { continued: false }
}

/// Whether a string still open at the end of a line goes on into the next.
/// Where the language says a string can't span lines, it's an error in the
/// file, and stopping at the line end keeps the rest of it readable.
fn carries(lang: Lang, q: Quote, continued: bool) -> bool {
    continued
        || match lang {
            Lang::Rust => q.ch == '"',
            Lang::Python | Lang::Toml => q.count == 3,
            Lang::JavaScript | Lang::TypeScript | Lang::CLike => q.ch == '`',
            Lang::Shell | Lang::PowerShell | Lang::Yaml => true,
            _ => false,
        }
}

// ─── Variables ────────────────────────────────────────────────────────────────

/// The end of a variable reference starting at `pos`, if one does.
fn variable_at(lang: Lang, chars: &[char], pos: usize) -> Option<usize> {
    let ch = chars[pos];
    let next = chars.get(pos + 1).copied();
    let word_end = |from: usize| {
        from + chars[from..]
            .iter()
            .take_while(|&&c| is_word_char(c))
            .count()
    };
    match (lang, ch) {
        (Lang::Shell | Lang::PowerShell | Lang::JavaScript | Lang::TypeScript, '$') => {
            if next == Some('{') {
                let close = chars[pos + 2..]
                    .iter()
                    .position(|&c| c == '}')
                    .map_or(chars.len(), |i| pos + 2 + i + 1);
                return Some(close);
            }
            // Shell's special parameters: `$#`, `$?`, `$$`, `$1`…
            if lang == Lang::Shell && next.is_some_and(|c| "#?$!@*-0123456789".contains(c)) {
                return Some(pos + 2);
            }
            Some(word_end(pos + 1).max(pos + 1))
        }
        (Lang::Batch, '%') => {
            // `%%i` (a for-loop variable), `%1`, `%~dp0`, `%NAME%`.
            match next {
                Some('%') if chars.get(pos + 2).is_some_and(|c| c.is_alphabetic()) => Some(pos + 3),
                Some(c) if c.is_ascii_digit() || c == '*' => Some(pos + 2),
                Some('~') => {
                    let mods = chars[pos + 2..]
                        .iter()
                        .take_while(|c| c.is_ascii_alphabetic())
                        .count();
                    let end = pos + 2 + mods;
                    chars
                        .get(end)
                        .is_some_and(|c| c.is_ascii_digit())
                        .then_some(end + 1)
                }
                _ => delimited_name(chars, pos, '%'),
            }
        }
        // Delayed expansion: `!NAME!`.
        (Lang::Batch, '!') => delimited_name(chars, pos, '!'),
        _ => None,
    }
}

/// `%NAME%` / `!NAME!`: the closing `delim` with a non-empty name and no
/// whitespace between.
fn delimited_name(chars: &[char], pos: usize, delim: char) -> Option<usize> {
    let len = chars[pos + 1..].iter().position(|&c| c == delim)?;
    let name = &chars[pos + 1..pos + 1 + len];
    (!name.is_empty() && !name.iter().any(|c| c.is_whitespace())).then_some(pos + len + 2)
}

// ─── Numbers and words ────────────────────────────────────────────────────────

/// Scan past a numeric literal, returning the end position.
fn scan_number(lang: Lang, chars: &[char], from: usize) -> usize {
    let mut i = from;
    let digit_at = |i: usize| chars.get(i).is_some_and(|c| c.is_ascii_digit());
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
    if chars.get(i) == Some(&'.') && digit_at(i + 1) {
        i += 1;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
            i += 1;
        }
    }
    // Exponent: e5, e+3, E-10 — only with digits after it, or `1each`
    // would lose its `e`.
    if matches!(chars.get(i), Some(&'e') | Some(&'E')) {
        let sign = usize::from(matches!(chars.get(i + 1), Some(&'+') | Some(&'-')));
        if digit_at(i + 1 + sign) {
            i += 1 + sign;
            while digit_at(i) {
                i += 1;
            }
        }
    }
    // Rust numeric suffix: u8, i32, f64, usize, …
    if lang == Lang::Rust && matches!(chars.get(i), Some(&'u') | Some(&'i') | Some(&'f')) {
        i += chars[i..]
            .iter()
            .take_while(|c| c.is_alphanumeric())
            .count();
    }
    i
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn classify_word(lang: Lang, word: &str) -> TokenKind {
    let keyword = |table: &[&str]| {
        if table.contains(&word) {
            TokenKind::Keyword
        } else {
            TokenKind::Plain
        }
    };
    let keyword_any_case = |table: &[&str]| {
        if table.iter().any(|k| k.eq_ignore_ascii_case(word)) {
            TokenKind::Keyword
        } else {
            TokenKind::Plain
        }
    };
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
        Lang::Python => keyword(PYTHON_KW),
        Lang::JavaScript => keyword(JS_KW),
        Lang::TypeScript => {
            if TS_EXTRA_KW.contains(&word) {
                TokenKind::Keyword
            } else {
                keyword(JS_KW)
            }
        }
        Lang::Shell => keyword(SHELL_KW),
        Lang::Batch => keyword_any_case(BATCH_KW),
        Lang::PowerShell => keyword_any_case(POWERSHELL_KW),
        Lang::Json => keyword(JSON_KW),
        Lang::CLike => keyword(CLIKE_KW),
        Lang::Toml | Lang::Yaml | Lang::Ini | Lang::Unknown => TokenKind::Plain,
    }
}

// ─── Whole-line rules ─────────────────────────────────────────────────────────

/// `[table]` / `[[array.of.tables]]`, optionally followed by a comment.
/// An array element on its own line (`  [1, 2],`, `  [3]`) is not a
/// header: it has a comma inside or after it, or it's indented and holds
/// no key-like text.
fn toml_header(chars: &[char], spans: &mut Vec<(TokenKind, String)>) -> bool {
    let n = chars.len();
    let start = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(n);
    if chars.get(start) != Some(&'[') {
        return false;
    }
    let close = if chars.get(start + 1) == Some(&'[') {
        "]]"
    } else {
        "]"
    };
    let inner_from = start + close.len();
    let Some(rel) = (inner_from..n).find(|&i| starts_with_at(chars, i, close)) else {
        return false;
    };
    let inner = &chars[inner_from..rel];
    let end = rel + close.len();
    let rest_start = (end..n).find(|&i| !chars[i].is_whitespace()).unwrap_or(n);
    // Indented, it must also look like a key: `  [3]` is the last element
    // of an array more often than a table named 3.
    let key_like = start == 0
        || inner
            .iter()
            .any(|c| c.is_alphabetic() || *c == '"' || *c == '\'');
    if inner.iter().all(|c| c.is_whitespace())
        || inner.contains(&',')
        || (rest_start < n && chars[rest_start] != '#')
        || !key_like
    {
        return false;
    }
    emit(spans, TokenKind::Plain, &chars[..start]);
    emit(spans, TokenKind::Type, &chars[start..end]);
    emit(spans, TokenKind::Plain, &chars[end..rest_start]);
    emit(spans, TokenKind::Comment, &chars[rest_start..]);
    true
}

/// An INI line: a `;`/`#` comment, a `[section]`, or `key = value` with
/// the key coloured. Values are left plain: INI has no types to tell.
fn ini_line(chars: &[char], spans: &mut Vec<(TokenKind, String)>) {
    let n = chars.len();
    let start = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(n);
    emit(spans, TokenKind::Plain, &chars[..start]);
    match chars.get(start) {
        Some(';' | '#') => emit(spans, TokenKind::Comment, &chars[start..]),
        Some('[') => {
            let end = chars[start..]
                .iter()
                .position(|&c| c == ']')
                .map_or(n, |i| start + i + 1);
            emit(spans, TokenKind::Type, &chars[start..end]);
            emit(spans, TokenKind::Plain, &chars[end..]);
        }
        Some(_) => {
            let key_end = chars[start..]
                .iter()
                .position(|&c| c == '=' || c == ':')
                .map_or(start, |i| start + i);
            emit(spans, TokenKind::Type, &chars[start..key_end]);
            emit(spans, TokenKind::Plain, &chars[key_end..]);
        }
        None => {}
    }
}

/// A batch comment (`rem …`, `@rem …`, `:: …`) or `:label` line.
fn batch_comment_or_label(chars: &[char], spans: &mut Vec<(TokenKind, String)>) -> bool {
    let n = chars.len();
    let start = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(n);
    let rest: String = chars[start..]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    let after_rem = |p: &str| {
        rest.strip_prefix(p)
            .is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace))
    };
    let kind = if after_rem("rem") || after_rem("@rem") || rest.starts_with("::") {
        TokenKind::Comment
    } else if rest.starts_with(':') {
        TokenKind::Type
    } else {
        return false;
    };
    emit(spans, TokenKind::Plain, &chars[..start]);
    emit(spans, kind, &chars[start..]);
    true
}

/// Colour a YAML mapping key at the start of the line (after indentation
/// and any `- ` sequence markers), returning where the main scan goes on.
/// A key is whatever comes before the first `: ` (or a `:` ending the
/// line), so `my-key`, `app.kubernetes.io/name` and quoted keys all count.
fn yaml_key(chars: &[char], spans: &mut Vec<(TokenKind, String)>) -> usize {
    let n = chars.len();
    let mut i = 0;
    loop {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if chars.get(i) == Some(&'-') && chars.get(i + 1).is_none_or(|c| c.is_whitespace()) {
            i += 1;
        } else {
            break;
        }
    }
    let colon_ends_key = |at: usize| {
        chars.get(at) == Some(&':') && chars.get(at + 1).is_none_or(|c| c.is_whitespace())
    };
    let key_end = match chars.get(i) {
        None => return 0,
        Some(&q @ ('"' | '\'')) => {
            let escape = (q == '"').then_some('\\');
            match find_close(chars, i + 1, Quote::new(q, escape)) {
                Close::At(end) if colon_ends_key(end) => end,
                _ => return 0,
            }
        }
        // Flow collections, anchors, aliases, tags, block scalars,
        // directives, comments: not a plain key.
        Some('[' | '{' | '&' | '*' | '!' | '|' | '>' | '%' | '@' | '`' | '#') => return 0,
        Some(_) => match (i..n).find(|&j| colon_ends_key(j)) {
            // ` #` before the colon makes it part of a comment.
            Some(end) if !(i..end).any(|j| chars[j] == '#' && chars[j - 1].is_whitespace()) => end,
            _ => return 0,
        },
    };
    emit(spans, TokenKind::Plain, &chars[..i]);
    emit(spans, TokenKind::Type, &chars[i..key_end]);
    key_end
}

/// A C preprocessor line (`#include`, `#define`): the directive is a
/// macro, and the rest is scanned as usual. Returns where the scan goes on.
fn clike_directive(chars: &[char], spans: &mut Vec<(TokenKind, String)>) -> usize {
    let n = chars.len();
    let start = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(n);
    if chars.get(start) != Some(&'#') {
        return 0;
    }
    let mut end = start + 1;
    while end < n && chars[end].is_whitespace() {
        end += 1;
    }
    let word = chars[end..]
        .iter()
        .take_while(|c| c.is_alphabetic())
        .count();
    if word == 0 {
        return 0;
    }
    emit(spans, TokenKind::Plain, &chars[..start]);
    emit(spans, TokenKind::Macro, &chars[start..end + word]);
    end + word
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn starts_with_at(chars: &[char], pos: usize, pat: &str) -> bool {
    (pos..)
        .zip(pat.chars())
        .all(|(i, p)| chars.get(i) == Some(&p))
}

/// Append a span, merging with the previous one if its kind matches.
fn emit(spans: &mut Vec<(TokenKind, String)>, kind: TokenKind, chars: &[char]) {
    if chars.is_empty() {
        return;
    }
    let text: String = chars.iter().collect();
    if let Some(last) = spans.last_mut()
        && last.0 == kind
    {
        last.1.push_str(&text);
        return;
    }
    spans.push((kind, text));
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lang: Lang, line: &str) -> Vec<TokenKind> {
        tokenize(lang, line, LineState::default())
            .0
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    #[allow(dead_code)]
    fn texts(lang: Lang, line: &str) -> Vec<String> {
        tokenize(lang, line, LineState::default())
            .0
            .into_iter()
            .map(|(_, t)| t)
            .collect()
    }

    fn roundtrip(lang: Lang, line: &str) {
        let (spans, _) = tokenize(lang, line, LineState::default());
        let rejoined: String = spans.into_iter().map(|(_, t)| t).collect();
        assert_eq!(rejoined, line, "spans must reconstruct the original line");
    }

    // ── Roundtrip invariant ───────────────────────────────────────────────────

    #[test]
    fn roundtrip_rust() {
        roundtrip(
            Lang::Rust,
            r#"    let x: Vec<u8> = vec![1, 2, 3]; // comment"#,
        );
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
        assert!(matches!(s1, LineState::BlockComment { .. }));
        let (spans2, s2) = tokenize(Lang::Rust, "middle line", s1);
        assert!(matches!(s2, LineState::BlockComment { .. }));
        assert!(spans2.iter().all(|(k, _)| *k == TokenKind::Comment));
        let (_, s3) = tokenize(Lang::Rust, "end */", s2);
        assert_eq!(s3, LineState::Code);
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
        assert_eq!(state, LineState::Code);
        assert!(spans.iter().any(|(k, _)| *k == TokenKind::String));
    }

    #[test]
    fn python_triple_string_spanning_lines() {
        let (_, s1) = tokenize(Lang::Python, r#"x = """start"#, LineState::default());
        assert!(matches!(s1, LineState::Str(_)));
        let (spans2, s2) = tokenize(Lang::Python, "middle", s1);
        assert!(matches!(s2, LineState::Str(_)));
        assert!(spans2.iter().all(|(k, _)| *k == TokenKind::String));
        let (_, s3) = tokenize(Lang::Python, r#"end""""#, s2);
        assert_eq!(s3, LineState::Code);
    }

    // ── Numbers ──────────────────────────────────────────────────────────────

    #[test]
    fn integer_number() {
        let (spans, _) = tokenize(Lang::Rust, "let x = 42;", LineState::default());
        assert!(
            spans
                .iter()
                .any(|(k, t)| *k == TokenKind::Number && t == "42")
        );
    }

    #[test]
    fn hex_number() {
        let (spans, _) = tokenize(Lang::Rust, "0xFF", LineState::default());
        assert!(spans.iter().any(|(k, _)| *k == TokenKind::Number));
    }

    #[test]
    fn float_number() {
        let (spans, _) = tokenize(Lang::Python, "x = 3.14", LineState::default());
        assert!(
            spans
                .iter()
                .any(|(k, t)| *k == TokenKind::Number && t.contains('.'))
        );
    }

    // ── Rust macros ──────────────────────────────────────────────────────────

    #[test]
    fn rust_macro_call() {
        let (spans, _) = tokenize(Lang::Rust, "println!(\"hi\");", LineState::default());
        assert!(
            spans
                .iter()
                .any(|(k, t)| *k == TokenKind::Macro && t == "println!")
        );
    }

    // ── Shell variables ───────────────────────────────────────────────────────

    #[test]
    fn shell_dollar_var() {
        let (spans, _) = tokenize(Lang::Shell, "echo $HOME", LineState::default());
        assert!(
            spans
                .iter()
                .any(|(k, t)| *k == TokenKind::Macro && t == "$HOME")
        );
    }

    #[test]
    fn shell_brace_var() {
        let (spans, _) = tokenize(Lang::Shell, "echo ${MY_VAR}", LineState::default());
        assert!(
            spans
                .iter()
                .any(|(k, t)| *k == TokenKind::Macro && t == "${MY_VAR}")
        );
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

    // ── Pathological / edge inputs ───────────────────────────────────────────
    //
    // Regression coverage for inputs that previously panicked or silently
    // mis-tokenised. Where current behaviour is "good enough but not perfect"
    // (raw strings, byte-prefixed literals), tests document it explicitly so
    // any future cleanup is intentional.

    #[test]
    fn rust_string_trailing_backslash_does_not_panic() {
        // Before the fix, scan_string would advance i past chars.len() on a
        // trailing `\`, and the caller's slice would panic. The roundtrip
        // invariant exercises both the scanner and the slice.
        let line = r#"let s = "abc\"#;
        roundtrip(Lang::Rust, line);
    }

    #[test]
    fn rust_char_trailing_backslash_does_not_panic() {
        // Same hazard via the single-quote string path (Rust char literal).
        let line = r"let c = '\";
        roundtrip(Lang::Rust, line);
    }

    #[test]
    fn rust_only_backslash_inside_quotes_does_not_panic() {
        // Two-char string body: just one backslash. Unterminated.
        let line = "\"\\";
        roundtrip(Lang::Rust, line);
    }

    #[test]
    fn rust_escaped_quote_then_eol_does_not_panic() {
        // `\"` consumes both chars, leaving no closing quote.
        let line = r#""abc\""#;
        roundtrip(Lang::Rust, line);
    }

    #[test]
    fn rust_byte_char_with_hex_escape() {
        // b'\xff' — the byte-prefix `b` tokenises as Plain, then the char
        // literal is a String span. The backslash-x-f-f escape is consumed
        // by scan_string's escape rule.
        let line = r"let n = b'\xff';";
        roundtrip(Lang::Rust, line);
        let (spans, _) = tokenize(Lang::Rust, line, LineState::default());
        let str_span = spans
            .iter()
            .find(|(k, _)| *k == TokenKind::String)
            .expect("byte char must tokenise as String");
        assert_eq!(str_span.1, r"'\xff'");
    }

    #[test]
    fn rust_byte_string_b_prefix() {
        // b"hello" — `b` is currently tokenised as a separate Plain span
        // rather than being absorbed into the String. Documented behaviour.
        let line = r#"let bs = b"hello";"#;
        roundtrip(Lang::Rust, line);
        let (spans, _) = tokenize(Lang::Rust, line, LineState::default());
        let str_span = spans
            .iter()
            .find(|(k, _)| *k == TokenKind::String)
            .expect("byte string body must tokenise as String");
        assert_eq!(str_span.1, r#""hello""#);
    }

    #[test]
    fn rust_raw_strings_are_one_string_prefix_included() {
        for (line, raw) in [
            (r##"let r = r"hello";"##, r##"r"hello""##),
            (
                r####"let raw = r#"con"tains"#;"####,
                r####"r#"con"tains"#"####,
            ),
            (r####"let b = br##"a"#b"##;"####, r####"br##"a"#b"##"####),
        ] {
            roundtrip(Lang::Rust, line);
            let (spans, _) = tokenize(Lang::Rust, line, LineState::default());
            let strings: Vec<&str> = spans
                .iter()
                .filter(|(k, _)| *k == TokenKind::String)
                .map(|(_, t)| t.as_str())
                .collect();
            assert_eq!(strings, [raw], "{line}");
        }
    }

    /// A raw string has no escapes: `\` before the closing quote doesn't
    /// keep it open.
    #[test]
    fn rust_raw_string_ignores_backslashes() {
        let (spans, state) = tokenize(Lang::Rust, r#"let p = r"C:\"; x"#, LineState::default());
        assert_eq!(state, LineState::Code);
        assert!(
            spans
                .iter()
                .any(|(k, t)| *k == TokenKind::String && t == r#"r"C:\""#)
        );
    }

    #[test]
    fn empty_string_literal_does_not_panic() {
        let line = r#"let s = "";"#;
        roundtrip(Lang::Rust, line);
        let (spans, _) = tokenize(Lang::Rust, line, LineState::default());
        assert!(
            spans
                .iter()
                .any(|(k, t)| *k == TokenKind::String && t == "\"\"")
        );
    }

    #[test]
    fn rust_unterminated_string_does_not_panic() {
        // Open-quote-no-close: scan_string returns the whole tail as String.
        let line = r#"println!("oops"#;
        roundtrip(Lang::Rust, line);
    }
    // ── Audit fixes ───────────────────────────────────────────────────────────

    /// Tokenize `lines` in order, threading the state.
    fn run(lang: Lang, lines: &[&str]) -> Vec<Vec<(TokenKind, String)>> {
        let mut state = LineState::default();
        lines
            .iter()
            .map(|l| {
                let (spans, next) = tokenize(lang, l, state);
                let rejoined: String = spans.iter().map(|(_, t)| t.as_str()).collect();
                assert_eq!(&rejoined, l, "spans must reconstruct the line");
                state = next;
                spans
            })
            .collect()
    }

    /// The text of every span of `kind`.
    fn of(spans: &[(TokenKind, String)], kind: TokenKind) -> Vec<&str> {
        spans
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, t)| t.as_str())
            .collect()
    }

    fn all_plain(spans: &[(TokenKind, String)]) -> bool {
        spans.iter().all(|(k, _)| *k == TokenKind::Plain)
    }

    /// Prose isn't code: an apostrophe, a URL, a `#` or a `/*` in a text
    /// file coloured as strings and comments, the last to the end of the
    /// file.
    #[test]
    fn a_file_with_no_language_is_plain_text() {
        let lines = run(
            Lang::Unknown,
            &[
                "Don't panic, it's fine.",
                "see https://example.com/x",
                "rm /tmp/*.log",
                "still plain?",
                "Issue #12 fixed in 3.14",
            ],
        );
        assert!(lines.iter().all(|l| all_plain(l)), "{lines:?}");
        for name in [
            "notes.txt",
            "README.md",
            "app.log",
            "release.nfo",
            "README",
            "x.html",
        ] {
            assert_eq!(lang_for_name(name), Lang::Unknown, "{name}");
        }
    }

    // Strings across lines.

    #[test]
    fn a_rust_string_spanning_lines_stays_a_string() {
        let lines = run(
            Lang::Rust,
            &["let s = \"first", "second line\";", "let x = 1;"],
        );
        assert_eq!(of(&lines[1], TokenKind::String), ["second line\""]);
        assert_eq!(of(&lines[2], TokenKind::Keyword), ["let"]);
        assert!(of(&lines[2], TokenKind::String).is_empty());
    }

    #[test]
    fn shell_quotes_span_lines() {
        let lines = run(Lang::Shell, &["msg=\"line one", "line two\"", "ls"]);
        assert_eq!(of(&lines[1], TokenKind::String), ["line two\""]);
        assert!(all_plain(&lines[2]));
    }

    #[test]
    fn a_trailing_backslash_continues_a_string() {
        let lines = run(Lang::Python, &["s = \"abc\\", "def\"", "x"]);
        assert_eq!(of(&lines[1], TokenKind::String), ["def\""]);
        assert!(all_plain(&lines[2]));
    }

    /// Where a string can't span lines, one left open is an error, and the
    /// next line is read as code again rather than as more string.
    #[test]
    fn an_unclosed_single_line_string_stops_at_the_line_end() {
        let lines = run(Lang::JavaScript, &["x = \"oops", "let y = 1;"]);
        assert_eq!(of(&lines[1], TokenKind::Keyword), ["let"]);
    }

    #[test]
    fn javascript_template_literals_are_strings_and_span_lines() {
        let lines = run(
            Lang::JavaScript,
            &["const t = `it's ${x}", "and more`;", "y;"],
        );
        assert_eq!(of(&lines[0], TokenKind::String), ["`it's ${x}"]);
        assert_eq!(of(&lines[1], TokenKind::String), ["and more`"]);
        assert!(all_plain(&lines[2]));
    }

    #[test]
    fn rust_block_comments_nest() {
        let lines = run(
            Lang::Rust,
            &[
                "/* a /* b */ still */ let x;",
                "/* one /* two",
                "*/ in */ let y;",
            ],
        );
        assert_eq!(of(&lines[0], TokenKind::Keyword), ["let"]);
        assert_eq!(of(&lines[2], TokenKind::Comment), ["*/ in */"]);
        assert_eq!(of(&lines[2], TokenKind::Keyword), ["let"]);
        // Elsewhere they don't: the first `*/` ends it.
        let js = run(Lang::JavaScript, &["/* a /* b */ let x;"]);
        assert_eq!(of(&js[0], TokenKind::Keyword), ["let"]);
    }

    // Shell.

    #[test]
    fn a_shell_hash_inside_a_word_is_not_a_comment() {
        for line in ["echo foo#bar", "url=http://x/#frag"] {
            let lines = run(Lang::Shell, &[line]);
            assert!(of(&lines[0], TokenKind::Comment).is_empty(), "{line}");
        }
        let lines = run(Lang::Shell, &["ls # list;x"]);
        assert_eq!(of(&lines[0], TokenKind::Comment), ["# list;x"]);
    }

    #[test]
    fn shell_special_parameters_are_variables() {
        let lines = run(Lang::Shell, &["echo $# $? $$ $1 args"]);
        assert_eq!(of(&lines[0], TokenKind::Macro), ["$#", "$?", "$$", "$1"]);
        assert!(of(&lines[0], TokenKind::Comment).is_empty());
    }

    // TOML.

    #[test]
    fn toml_multiline_strings() {
        let lines = run(
            Lang::Toml,
            &[
                "s = \"\"\"",
                "multi \"line",
                "\"\"\"",
                "l = '''",
                "raw \\",
                "'''",
                "x = 1",
            ],
        );
        assert_eq!(of(&lines[1], TokenKind::String), ["multi \"line"]);
        assert_eq!(of(&lines[4], TokenKind::String), ["raw \\"]);
        assert_eq!(of(&lines[6], TokenKind::Number), ["1"]);
    }

    #[test]
    fn toml_headers_and_array_elements() {
        let lines = run(
            Lang::Toml,
            &[
                "x = [",
                "  [1, 2],",
                "  [3]",
                "  [4],",
                "  [5, 6]",
                "  [\"a\"],",
                "  [\"a\", \"b\"]",
                "]",
                "[a] # note",
                "[[b.c]]",
                "  [indented.table]",
            ],
        );
        // Each array element trips a different rule: comma after, a bare
        // number indented, comma inside — the last two with strings, which
        // look like keys.
        for l in &lines[1..7] {
            assert!(of(l, TokenKind::Type).is_empty(), "{l:?}");
        }
        assert_eq!(of(&lines[10], TokenKind::Type), ["[indented.table]"]);
        assert_eq!(of(&lines[8], TokenKind::Type), ["[a]"]);
        assert_eq!(of(&lines[8], TokenKind::Comment), ["# note"]);
        assert_eq!(of(&lines[9], TokenKind::Type), ["[[b.c]]"]);
    }

    // YAML.

    #[test]
    fn yaml_keys_with_punctuation_or_quotes() {
        for (line, key) in [
            ("my-key: value", "my-key"),
            ("app.kubernetes.io/name: web", "app.kubernetes.io/name"),
            ("  - name: x", "name"),
            ("\"quoted key\": 1", "\"quoted key\""),
            ("'single': 1", "'single'"),
            ("empty:", "empty"),
            ("url: http://x", "url"),
        ] {
            let lines = run(Lang::Yaml, &[line]);
            assert_eq!(of(&lines[0], TokenKind::Type), [key], "{line}");
        }
        for line in ["- just an item", "# key: commented", "a #b: c"] {
            let lines = run(Lang::Yaml, &[line]);
            assert!(
                of(&lines[0], TokenKind::Type).is_empty(),
                "{line}: {:?}",
                lines[0]
            );
        }
    }

    #[test]
    fn yaml_quotes_and_comments() {
        let lines = run(
            Lang::Yaml,
            &["name: 'it''s'", "msg: it's fine", "color: a#b", "x: 1 # c"],
        );
        assert_eq!(of(&lines[0], TokenKind::String), ["'it''s'"]);
        assert!(
            of(&lines[1], TokenKind::String).is_empty(),
            "{:?}",
            lines[1]
        );
        assert!(
            of(&lines[2], TokenKind::Comment).is_empty(),
            "{:?}",
            lines[2]
        );
        assert_eq!(of(&lines[3], TokenKind::Comment), ["# c"]);
    }

    // Numbers.

    #[test]
    fn numbers_dont_swallow_words() {
        let lines = run(Lang::Python, &["x = 1each + 3files + 2e10 + 1.5e-3"]);
        assert_eq!(
            of(&lines[0], TokenKind::Number),
            ["1", "3", "2e10", "1.5e-3"]
        );
        // The type suffix is Rust's alone.
        let rust = run(Lang::Rust, &["let n = 1u8 + 2.0f64;"]);
        assert_eq!(of(&rust[0], TokenKind::Number), ["1u8", "2.0f64"]);
        let c = run(Lang::CLike, &["x = 1if"]);
        assert_eq!(of(&c[0], TokenKind::Number), ["1"]);
    }

    // File names.

    #[test]
    fn languages_for_names() {
        for (name, lang) in [
            ("run.bat", Lang::Batch),
            ("RUN.CMD", Lang::Batch),
            ("setup.ps1", Lang::PowerShell),
            ("mod.psm1", Lang::PowerShell),
            (".env", Lang::Shell),
            ("prod.env", Lang::Shell),
            (".env.local", Lang::Shell),
            ("Dockerfile.dev", Lang::Shell),
            ("Containerfile", Lang::Shell),
            ("api.dockerfile", Lang::Shell),
            ("main.c", Lang::CLike),
            ("main.go", Lang::CLike),
            ("Main.java", Lang::CLike),
            ("style.css", Lang::CLike),
            ("app.ini", Lang::Ini),
            ("nginx.conf", Lang::Ini),
            (".gitconfig", Lang::Ini),
            ("x.mts", Lang::TypeScript),
        ] {
            assert_eq!(lang_for_name(name), lang, "{name}");
        }
    }

    // Batch.

    #[test]
    fn batch_comments_labels_and_variables() {
        let lines = run(
            Lang::Batch,
            &[
                "rem a comment",
                "  @REM another",
                ":: and another",
                ":loop",
                "ECHO don't %NAME% %1 %%i %~dp0 !X!",
                "remark is not a comment",
            ],
        );
        for l in &lines[..3] {
            assert_eq!(of(l, TokenKind::Comment).len(), 1, "{l:?}");
        }
        assert_eq!(of(&lines[3], TokenKind::Type), [":loop"]);
        assert_eq!(of(&lines[4], TokenKind::Keyword), ["ECHO"]);
        assert_eq!(
            of(&lines[4], TokenKind::Macro),
            ["%NAME%", "%1", "%%i", "%~dp0", "!X!"]
        );
        assert!(
            of(&lines[4], TokenKind::String).is_empty(),
            "an apostrophe isn't a quote"
        );
        assert!(of(&lines[5], TokenKind::Comment).is_empty());
    }

    // PowerShell.

    #[test]
    fn powershell_comments_strings_and_variables() {
        let lines = run(
            Lang::PowerShell,
            &[
                "<# block",
                "still #>  $x = \"a`\"b\" # note",
                "Write-Host a#b",
            ],
        );
        assert!(lines[0].iter().all(|(k, _)| *k == TokenKind::Comment));
        assert_eq!(of(&lines[1], TokenKind::Comment), ["still #>", "# note"]);
        assert_eq!(of(&lines[1], TokenKind::String), ["\"a`\"b\""]);
        assert_eq!(of(&lines[1], TokenKind::Macro), ["$x"]);
        assert!(of(&lines[2], TokenKind::Comment).is_empty());
    }

    // C family.

    #[test]
    fn c_like_directives_comments_and_raw_strings() {
        let lines = run(
            Lang::CLike,
            &[
                "#include <stdio.h> // io",
                "int x = 0; /* c */",
                "s := `raw",
                "line`",
                "a # b",
            ],
        );
        assert_eq!(of(&lines[0], TokenKind::Macro), ["#include"]);
        assert_eq!(of(&lines[0], TokenKind::Comment), ["// io"]);
        assert_eq!(of(&lines[1], TokenKind::Keyword), ["int"]);
        assert_eq!(of(&lines[3], TokenKind::String), ["line`"]);
        assert!(
            of(&lines[4], TokenKind::Comment).is_empty(),
            "# is not a C comment"
        );
    }

    // INI.

    #[test]
    fn ini_lines() {
        let lines = run(
            Lang::Ini,
            &[
                "; comment",
                "# also",
                "[section]",
                "key = it's # not a comment",
            ],
        );
        assert_eq!(of(&lines[0], TokenKind::Comment), ["; comment"]);
        assert_eq!(of(&lines[1], TokenKind::Comment), ["# also"]);
        assert_eq!(of(&lines[2], TokenKind::Type), ["[section]"]);
        assert_eq!(of(&lines[3], TokenKind::Type), ["key "]);
        assert!(of(&lines[3], TokenKind::Comment).is_empty());
    }
}
