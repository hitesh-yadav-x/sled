//! POSIX shell quoting shared by the resume command and the installed hook command.

/// Leaves `s` alone when every character is in `[A-Za-z0-9_./~-]`, else wraps it in single
/// quotes, writing each `'` as `'\''`.
pub fn shell_quote(s: &str) -> String {
    quote_unless(s, |c| plain_path_char(c) || c == '~')
}

/// Like [`shell_quote`] but also quotes `~`: leaves `s` alone only when every character is in
/// `[A-Za-z0-9_./-]`.
pub fn shell_quote_path(s: &str) -> String {
    quote_unless(s, plain_path_char)
}

fn plain_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')
}

fn quote_unless(s: &str, plain: impl Fn(char) -> bool) -> String {
    if !s.is_empty() && s.chars().all(plain) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Splits the first shell word off `s` (after leading whitespace), undoing single quotes,
/// double quotes (taken literally, as older installs wrote them) and backslash escapes.
/// Returns the word and the text after it, or None when a quote is left open.
pub fn split_first_word(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let mut word = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            c if c.is_whitespace() => return Some((word, &s[i..])),
            '\'' | '"' => loop {
                match chars.next() {
                    Some((_, q)) if q == c => break,
                    Some((_, q)) => word.push(q),
                    None => return None,
                }
            },
            '\\' => word.push(chars.next()?.1),
            c => word.push(c),
        }
    }
    Some((word, ""))
}
