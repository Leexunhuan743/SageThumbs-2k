//! The per-line lexer: language specs and the single-pass tokenizer.
//!
//! Split out of `highlight.rs` 2026-07-31 (pure move). The keyword tables it reads live
//! next door in `keywords.rs`; the colour resolution and painting stayed in the parent.

use super::keywords::*;

/// Languages we specially lex. `Plain` = no colouring (falls back to today's uncoloured draw).
#[derive(Clone, Copy, PartialEq)]
pub(in crate::preview) enum Lang {
    Rust,
    Py,
    Js,
    Json,
    Yaml,
    Toml,
    C,
    Cs,
    Java,
    Go,
    Ruby,
    Php,
    Lua,
    Kotlin,
    Swift,
    Sh,
    Html,
    Css,
    Xml,
    Sql,
    Plain,
}

#[derive(Clone, Copy, PartialEq)]
pub(in crate::preview) enum Tag {
    Plain,
    Comment,
    Str,
    Num,
    Keyword,
}

/// The canonical fenced-code info string for a language — used when a copied Markdown code
/// block is written back out as ``` fences, so it still highlights wherever it's pasted.
/// `None` for `Plain` (emit a bare fence). Lossy on purpose: the parse maps many tags onto one
/// `Lang` (`powershell`/`bash`/`zsh` all become `Sh`), and `Lang` is all we keep.
pub(in crate::preview) fn lang_tag(l: Lang) -> Option<&'static str> {
    Some(match l {
        Lang::Rust => "rust",
        Lang::Py => "python",
        Lang::Js => "js",
        Lang::Json => "json",
        Lang::Yaml => "yaml",
        Lang::Toml => "toml",
        Lang::C => "c",
        Lang::Cs => "csharp",
        Lang::Java => "java",
        Lang::Go => "go",
        Lang::Ruby => "ruby",
        Lang::Php => "php",
        Lang::Lua => "lua",
        Lang::Kotlin => "kotlin",
        Lang::Swift => "swift",
        Lang::Sh => "sh",
        Lang::Html => "html",
        Lang::Css => "css",
        Lang::Xml => "xml",
        Lang::Sql => "sql",
        Lang::Plain => return None,
    })
}

/// Map a file extension (no dot) to a language.
pub(in crate::preview) fn lang_from_ext(ext: &str) -> Lang {
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Lang::Rust,
        "py" | "pyw" | "pyi" => Lang::Py,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang::Js,
        "json" | "jsonc" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "c" | "h" | "cpp" | "cxx" | "cc" | "hpp" | "hxx" => Lang::C,
        "cs" => Lang::Cs,
        "java" => Lang::Java,
        "go" => Lang::Go,
        "rb" | "rake" | "gemspec" => Lang::Ruby,
        "php" | "phtml" => Lang::Php,
        "lua" => Lang::Lua,
        "kt" | "kts" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "sh" | "bash" | "zsh" | "ps1" | "bat" | "cmd" => Lang::Sh,
        "html" | "htm" | "xhtml" => Lang::Html,
        "css" | "scss" | "less" => Lang::Css,
        // svg is XML — reachable via the caption's "view source" toggle on a rendered SVG.
        "xml" | "svg" => Lang::Xml,
        "sql" => Lang::Sql,
        // ini/cfg files share TOML's shape (# / ; comments, key=value, quoted strings).
        "ini" | "cfg" | "conf" | "properties" | "editorconfig" | "gitconfig" => Lang::Toml,
        _ => Lang::Plain,
    }
}

/// Map a markdown fenced-code info-string tag (e.g. ```` ```rust ````) to a language. Also accepts
/// a bare extension as the tag.
pub(in crate::preview) fn lang_from_fence(tag: &str) -> Lang {
    match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Lang::Rust,
        "python" | "py" => Lang::Py,
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" | "node" => Lang::Js,
        "json" | "jsonc" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "c" | "cpp" | "c++" | "h" | "hpp" => Lang::C,
        "cs" | "csharp" | "c#" => Lang::Cs,
        "java" => Lang::Java,
        "go" | "golang" => Lang::Go,
        "ruby" | "rb" => Lang::Ruby,
        "php" => Lang::Php,
        "lua" => Lang::Lua,
        "kotlin" | "kt" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "sh" | "bash" | "shell" | "zsh" | "ps1" | "powershell" | "bat" | "console" => Lang::Sh,
        "html" | "htm" => Lang::Html,
        "css" | "scss" | "less" => Lang::Css,
        "xml" | "svg" => Lang::Xml,
        "sql" => Lang::Sql,
        other => lang_from_ext(other),
    }
}

/// Per-language lexer spec.
pub(in crate::preview) struct Spec {
    line_comment: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    strings: &'static [u8], // quote chars
    keywords: &'static [&'static str],
}

/// The per-language tuple `spec()` builds from: (line comments, block comment, quotes, keywords).
pub(in crate::preview) type SpecParts = (
    &'static [&'static str],
    Option<(&'static str, &'static str)>,
    &'static [u8],
    &'static [&'static str],
);

pub(in crate::preview) fn spec(lang: Lang) -> Spec {
    let (lc, bl, st, kw): SpecParts = match lang {
        Lang::Rust => (&["//"], Some(("/*", "*/")), b"\"", RUST_KW),
        Lang::Py => (&["#"], None, b"\"'", PY_KW),
        Lang::Js => (&["//"], Some(("/*", "*/")), b"\"'`", JS_KW),
        Lang::Json => (&[], None, b"\"", &["true", "false", "null"]),
        Lang::Yaml => (&["#"], None, b"\"'", YAML_KW),
        Lang::Toml => (&["#"], None, b"\"'", &["true", "false"]),
        Lang::C => (&["//"], Some(("/*", "*/")), b"\"'", C_KW),
        Lang::Cs => (&["//"], Some(("/*", "*/")), b"\"'", CS_KW),
        Lang::Java => (&["//"], Some(("/*", "*/")), b"\"", JAVA_KW),
        Lang::Go => (&["//"], Some(("/*", "*/")), b"\"`", GO_KW),
        Lang::Ruby => (&["#"], None, b"\"'", RUBY_KW),
        Lang::Php => (&["//", "#"], Some(("/*", "*/")), b"\"'", PHP_KW),
        Lang::Lua => (&["--"], None, b"\"'", LUA_KW),
        Lang::Kotlin => (&["//"], Some(("/*", "*/")), b"\"", KOTLIN_KW),
        Lang::Swift => (&["//"], Some(("/*", "*/")), b"\"", SWIFT_KW),
        Lang::Sh => (&["#"], None, b"\"'", SH_KW),
        Lang::Html | Lang::Xml => (&[], Some(("<!--", "-->")), b"\"'", &[]),
        Lang::Css => (&[], Some(("/*", "*/")), b"\"'", &[]),
        Lang::Sql => (&["--"], Some(("/*", "*/")), b"'", SQL_KW),
        Lang::Plain => (&[], None, b"", &[]),
    };
    Spec {
        line_comment: lc,
        block: bl,
        strings: st,
        keywords: kw,
    }
}

/// UTF-8 byte length of the char starting with lead byte `b`.
pub(in crate::preview) fn utf8_len(b: u8) -> usize {
    if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        1
    }
}

pub(in crate::preview) fn find_from(hay: &str, from: usize, needle: &str) -> Option<usize> {
    hay.get(from..)
        .and_then(|s| s.find(needle))
        .map(|p| p + from)
}

/// Tokenize one line into `(tag, slice)` runs. `in_block` carries block-comment state across
/// lines. Non-keyword identifiers + punctuation stay in `Plain` runs (few runs per line).
pub(in crate::preview) fn tokenize<'a>(
    line: &'a str,
    sp: &Spec,
    in_block: &mut bool,
) -> Vec<(Tag, &'a str)> {
    let b = line.as_bytes();
    let n = b.len();
    let mut out: Vec<(Tag, &'a str)> = Vec::new();
    let mut i = 0usize;
    let mut seg = 0usize; // start of the pending Plain segment

    macro_rules! flush {
        ($upto:expr) => {
            if $upto > seg {
                out.push((Tag::Plain, &line[seg..$upto]));
            }
        };
    }

    while i < n {
        // carried-over block comment
        if *in_block {
            if let Some((_, close)) = sp.block {
                if let Some(pos) = find_from(line, i, close) {
                    let end = pos + close.len();
                    out.push((Tag::Comment, &line[i..end]));
                    i = end;
                    seg = i;
                    *in_block = false;
                    continue;
                }
            }
            out.push((Tag::Comment, &line[i..]));
            seg = n;
            break;
        }
        // line comment -> rest of line
        if sp.line_comment.iter().any(|c| line[i..].starts_with(*c)) {
            flush!(i);
            out.push((Tag::Comment, &line[i..]));
            seg = n;
            break;
        }
        // block comment open
        if let Some((open, close)) = sp.block {
            if line[i..].starts_with(open) {
                flush!(i);
                if let Some(pos) = find_from(line, i + open.len(), close) {
                    let end = pos + close.len();
                    out.push((Tag::Comment, &line[i..end]));
                    i = end;
                    seg = i;
                } else {
                    out.push((Tag::Comment, &line[i..]));
                    i = n;
                    seg = n;
                    *in_block = true;
                }
                continue;
            }
        }
        let ch = b[i];
        // string literal
        if sp.strings.contains(&ch) {
            flush!(i);
            let start = i;
            i += 1;
            while i < n {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == ch {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i.min(n);
            // A string immediately followed by `:` is an object KEY / property — colour it like a
            // keyword (matches QuickLook's blue property names) instead of an orange string value.
            let is_key = line
                .get(end..)
                .is_some_and(|r| r.trim_start().starts_with(':'));
            out.push((
                if is_key { Tag::Keyword } else { Tag::Str },
                &line[start..end],
            ));
            i = end;
            seg = i;
            continue;
        }
        // number literal
        if ch.is_ascii_digit() {
            flush!(i);
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            out.push((Tag::Num, &line[start..i]));
            seg = i;
            continue;
        }
        // identifier -> keyword lookup (non-keywords stay in the plain segment)
        if ch.is_ascii_alphabetic() || ch == b'_' {
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if sp.keywords.contains(&word) {
                flush!(start);
                out.push((Tag::Keyword, word));
                seg = i;
            }
            continue;
        }
        // plain char (advance by full UTF-8 char so slices never split a codepoint)
        i += if ch < 0x80 { 1 } else { utf8_len(ch) };
    }
    flush!(n);
    out
}
