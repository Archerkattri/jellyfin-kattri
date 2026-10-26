use std::fmt;

/// Why a template is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateError {
    /// A `{` opens a placeholder that is never closed.
    Unclosed,
    /// A `}` appears without a second `}` directly after it.
    StrayBrace,
    /// A placeholder name is empty or contains characters outside `[A-Za-z0-9_]`.
    BadPlaceholder,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unclosed => "unclosed placeholder",
            Self::StrayBrace => "stray `}` (write `}}` for a literal brace)",
            Self::BadPlaceholder => "placeholder names use only letters, digits and `_`",
        })
    }
}

/// Output of [`render`]. `missing_args` lists placeholder names with no supplied value; those
/// placeholders stay in the text as `{name}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub missing_args: Vec<String>,
}

enum Piece<'a> {
    Text(&'a str),
    Placeholder(&'a str),
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn scan<'a>(template: &'a str, mut emit: impl FnMut(Piece<'a>)) -> Result<(), TemplateError> {
    let mut rest = template;
    loop {
        let Some(at) = rest.find(['{', '}']) else {
            if !rest.is_empty() {
                emit(Piece::Text(rest));
            }
            return Ok(());
        };
        if at > 0 {
            emit(Piece::Text(&rest[..at]));
        }
        let after = &rest[at + 1..];
        if rest[at..].starts_with('{') {
            if let Some(tail) = after.strip_prefix('{') {
                emit(Piece::Text("{"));
                rest = tail;
                continue;
            }
            let Some(end) = after.find('}') else {
                return Err(TemplateError::Unclosed);
            };
            let name = &after[..end];
            if !is_valid_name(name) {
                return Err(TemplateError::BadPlaceholder);
            }
            emit(Piece::Placeholder(name));
            rest = &after[end + 1..];
        } else {
            let Some(tail) = after.strip_prefix('}') else {
                return Err(TemplateError::StrayBrace);
            };
            emit(Piece::Text("}"));
            rest = tail;
        }
    }
}

/// Checks that `template` is well formed.
pub fn validate(template: &str) -> Result<(), TemplateError> {
    scan(template, |_| {})
}

/// Substitutes `{name}` placeholders from `args` (first match wins). `{{` and `}}` render as
/// literal braces. Values are inserted as-is and are not scanned for placeholders.
pub fn render(template: &str, args: &[(&str, &str)]) -> Result<Rendered, TemplateError> {
    let mut out = Rendered::default();
    scan(template, |piece| match piece {
        Piece::Text(text) => out.text.push_str(text),
        Piece::Placeholder(name) => match args.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => out.text.push_str(value),
            None => {
                out.text.push('{');
                out.text.push_str(name);
                out.text.push('}');
                out.missing_args.push(name.to_owned());
            }
        },
    })?;
    Ok(out)
}
