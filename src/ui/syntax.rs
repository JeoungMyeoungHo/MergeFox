use egui::{
    text::{LayoutJob, TextFormat},
    Color32, FontId, Visuals,
};

const CODE_FONT_SIZE: f32 = 12.5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    JsLike,
    Python,
    Go,
    Shell,
    Data,
    Generic,
}

pub fn highlighted_code_job(line: &str, path: Option<&str>, visuals: &Visuals) -> LayoutJob {
    let mut job = LayoutJob::default();
    let line = line.replace('\t', "    ");
    let lang = detect_language(path);
    let palette = Palette::from_visuals(visuals);
    let mut idx = 0;

    while idx < line.len() {
        let rest = &line[idx..];

        if uses_hash_comments(lang) && rest.starts_with('#') {
            append(&mut job, rest, palette.comment);
            break;
        }
        if uses_slash_comments(lang) && rest.starts_with("//") {
            append(&mut job, rest, palette.comment);
            break;
        }
        if uses_block_comments(lang) && rest.starts_with("/*") {
            let end = block_comment_end(rest);
            append(&mut job, &rest[..end], palette.comment);
            idx += end;
            continue;
        }

        let ch = rest.chars().next().unwrap_or_default();
        if is_string_delimiter(lang, ch) {
            let end = string_end(rest, ch);
            append(&mut job, &rest[..end], palette.string);
            idx += end;
            continue;
        }
        if ch.is_ascii_digit() {
            let end = number_end(rest);
            append(&mut job, &rest[..end], palette.number);
            idx += end;
            continue;
        }
        if is_ident_start(ch) {
            let end = ident_end(rest);
            let token = &rest[..end];
            let color = if is_keyword(lang, token) {
                palette.keyword
            } else if is_literal(token) {
                palette.literal
            } else if token.chars().next().is_some_and(|c| c.is_uppercase()) {
                palette.type_name
            } else {
                palette.text
            };
            append(&mut job, token, color);
            idx += end;
            continue;
        }

        let color = if is_operator(ch) {
            palette.operator
        } else {
            palette.text
        };
        append(&mut job, &rest[..ch.len_utf8()], color);
        idx += ch.len_utf8();
    }

    job
}

fn append(job: &mut LayoutJob, text: &str, color: Color32) {
    job.append(
        text,
        0.0,
        TextFormat::simple(FontId::monospace(CODE_FONT_SIZE), color),
    );
}

fn detect_language(path: Option<&str>) -> Language {
    let ext = path
        .and_then(|path| path.rsplit('.').next())
        .map(|ext| ext.to_ascii_lowercase());
    match ext.as_deref() {
        Some("rs") => Language::Rust,
        Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "json" | "css" | "scss" | "java") => {
            Language::JsLike
        }
        Some("py" | "pyw") => Language::Python,
        Some("go") => Language::Go,
        Some("sh" | "bash" | "zsh" | "fish" | "env") => Language::Shell,
        Some("toml" | "yaml" | "yml" | "ini" | "conf" | "md") => Language::Data,
        _ => Language::Generic,
    }
}

fn uses_hash_comments(lang: Language) -> bool {
    matches!(lang, Language::Python | Language::Shell | Language::Data)
}

fn uses_slash_comments(lang: Language) -> bool {
    matches!(lang, Language::Rust | Language::JsLike | Language::Go)
}

fn uses_block_comments(lang: Language) -> bool {
    matches!(lang, Language::Rust | Language::JsLike | Language::Go)
}

fn is_string_delimiter(lang: Language, ch: char) -> bool {
    matches!(ch, '"' | '\'') || (lang == Language::JsLike && ch == '`')
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn ident_end(text: &str) -> usize {
    let mut end = 0;
    for ch in text.chars() {
        if is_ident_continue(ch) {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    end.max(1)
}

fn number_end(text: &str) -> usize {
    let mut end = 0;
    for ch in text.chars() {
        if ch.is_ascii_hexdigit() || matches!(ch, '_' | '.' | 'x' | 'o' | 'b') {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    end.max(1)
}

fn string_end(text: &str, quote: char) -> usize {
    let mut escaped = false;
    let mut end = 0;
    for ch in text.chars() {
        end += ch.len_utf8();
        if end == quote.len_utf8() {
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != '\'' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return end;
        }
    }
    text.len()
}

fn block_comment_end(text: &str) -> usize {
    text.find("*/").map(|idx| idx + 2).unwrap_or(text.len())
}

fn is_operator(ch: char) -> bool {
    matches!(
        ch,
        '{' | '}'
            | '('
            | ')'
            | '['
            | ']'
            | '<'
            | '>'
            | '='
            | '+'
            | '-'
            | '*'
            | '/'
            | ':'
            | ';'
            | ','
            | '.'
            | '|'
            | '&'
            | '!'
            | '?'
    )
}

fn is_literal(token: &str) -> bool {
    matches!(
        token,
        "true" | "false" | "null" | "None" | "Some" | "Ok" | "Err" | "self" | "Self"
    )
}

fn is_keyword(lang: Language, token: &str) -> bool {
    match lang {
        Language::Rust => matches!(
            token,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "else"
                | "enum"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "static"
                | "struct"
                | "trait"
                | "type"
                | "use"
                | "where"
                | "while"
        ),
        Language::JsLike => matches!(
            token,
            "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "else"
                | "export"
                | "extends"
                | "finally"
                | "for"
                | "function"
                | "if"
                | "import"
                | "interface"
                | "let"
                | "new"
                | "return"
                | "switch"
                | "throw"
                | "try"
                | "type"
                | "var"
                | "while"
        ),
        Language::Python => matches!(
            token,
            "and"
                | "as"
                | "async"
                | "await"
                | "class"
                | "def"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        Language::Go => matches!(
            token,
            "break"
                | "case"
                | "chan"
                | "const"
                | "continue"
                | "defer"
                | "else"
                | "fallthrough"
                | "for"
                | "func"
                | "go"
                | "if"
                | "import"
                | "interface"
                | "map"
                | "package"
                | "range"
                | "return"
                | "select"
                | "struct"
                | "switch"
                | "type"
                | "var"
        ),
        Language::Shell => matches!(
            token,
            "case"
                | "do"
                | "done"
                | "elif"
                | "else"
                | "esac"
                | "fi"
                | "for"
                | "function"
                | "if"
                | "in"
                | "return"
                | "then"
                | "while"
        ),
        Language::Data => matches!(token, "true" | "false" | "null"),
        Language::Generic => false,
    }
}

struct Palette {
    text: Color32,
    keyword: Color32,
    string: Color32,
    number: Color32,
    comment: Color32,
    literal: Color32,
    type_name: Color32,
    operator: Color32,
}

impl Palette {
    fn from_visuals(visuals: &Visuals) -> Self {
        let text = visuals
            .override_text_color
            .unwrap_or(visuals.widgets.noninteractive.fg_stroke.color);
        if visuals.dark_mode {
            Self {
                text,
                keyword: Color32::from_rgb(248, 142, 92),
                string: Color32::from_rgb(177, 219, 126),
                number: Color32::from_rgb(110, 189, 255),
                comment: Color32::from_rgb(128, 140, 160),
                literal: Color32::from_rgb(206, 167, 255),
                type_name: Color32::from_rgb(122, 214, 210),
                operator: Color32::from_rgb(206, 210, 220),
            }
        } else {
            Self {
                text,
                keyword: Color32::from_rgb(182, 72, 16),
                string: Color32::from_rgb(54, 121, 35),
                number: Color32::from_rgb(34, 102, 168),
                comment: Color32::from_rgb(120, 128, 140),
                literal: Color32::from_rgb(129, 78, 194),
                type_name: Color32::from_rgb(36, 132, 140),
                operator: Color32::from_rgb(78, 84, 92),
            }
        }
    }
}
