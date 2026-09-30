//! Notification body markup. Capability `body-markup` is advertised as a minimal subset:
//! the tags the spec lists (`b i u a img`) are accepted and rendered as plain text
//! (links keep their text, images their `alt`), every other tag is dropped, and the five
//! XML entities plus numeric references are decoded. Nothing is ever interpreted as
//! rich text, so a hostile body can only ever show text.

/// Longest entity we look for after `&` (`&#x10ffff;` is 10 chars).
const MAX_ENTITY: usize = 10;

/// Strips markup from `input` and returns plain text with `\n` line breaks.
pub fn strip_markup(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(['<', '&']) {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        if rest.starts_with('<') {
            match tag_at(rest) {
                Some((len, text)) => {
                    out.push_str(&text);
                    rest = &rest[len..];
                }
                None => {
                    out.push('<');
                    rest = &rest[1..];
                }
            }
        } else {
            match entity_at(rest) {
                Some((len, c)) => {
                    out.push(c);
                    rest = &rest[len..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        }
    }
    out.push_str(rest);
    clean(&out)
}

/// A tag starting at `s[0] == '<'`: the byte length to skip and the text it stands for.
/// `None` when it is not a tag (a lone `<` in prose).
fn tag_at(s: &str) -> Option<(usize, String)> {
    let end = s.find('>')?;
    let inner = &s[1..end];
    if inner.contains('<') {
        return None;
    }
    let first = inner.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '/') {
        return None;
    }
    let body = inner.trim_start_matches('/');
    let closing = inner.starts_with('/');
    let name_end = body
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    let text = match name.as_str() {
        "br" => "\n".to_string(),
        "img" if !closing => attr(body, "alt").map(|a| decode(&a)).unwrap_or_default(),
        _ => String::new(),
    };
    Some((end + 1, text))
}

/// Value of `name="..."` (or single quoted) inside a tag body.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        let boundary_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after = tag[at + name.len()..].trim_start();
        if boundary_ok && let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            let quote = v.chars().next()?;
            if quote == '"' || quote == '\'' {
                let inner = &v[1..];
                let close = inner.find(quote)?;
                return Some(inner[..close].to_string());
            }
        }
        from = at + name.len();
    }
    None
}

fn decode(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        match entity_at(rest) {
            Some((len, c)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// An entity starting at `s[0] == '&'`: byte length and the character.
fn entity_at(s: &str) -> Option<(usize, char)> {
    let semi = s
        .char_indices()
        .take(MAX_ENTITY + 1)
        .find(|&(_, c)| c == ';')?
        .0;
    let name = &s[1..semi];
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            char::from_u32(code)?
        }
    };
    Some((semi + 1, c))
}

/// Control characters other than newline become spaces; trailing blank space goes.
fn clean(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\r' => '\n',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    mapped.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(strip_markup("hello world"), "hello world");
        assert_eq!(strip_markup(""), "");
        assert_eq!(strip_markup("a < b and c > d"), "a < b and c > d");
    }

    #[test]
    fn tags_are_dropped_text_kept() {
        assert_eq!(strip_markup("<b>bold</b> and <i>it</i>"), "bold and it");
        assert_eq!(
            strip_markup("see <a href=\"http://x.y/?a=1&b=2\">the site</a>"),
            "see the site"
        );
        assert_eq!(strip_markup("<u>under</u>"), "under");
        assert_eq!(strip_markup("<script>evil()</script>"), "evil()");
    }

    #[test]
    fn line_breaks() {
        assert_eq!(strip_markup("a<br>b<br/>c<BR />d"), "a\nb\nc\nd");
    }

    #[test]
    fn images_show_alt() {
        assert_eq!(
            strip_markup("x <img src=\"a.png\" alt=\"logo\"/> y"),
            "x logo y"
        );
        assert_eq!(strip_markup("<img src='a.png'>"), "");
        assert_eq!(strip_markup("<img alt='a &amp; b'>"), "a & b");
    }

    #[test]
    fn entities_decode_once() {
        assert_eq!(
            strip_markup("&lt;b&gt; &amp; &quot;q&quot; &apos;"),
            "<b> & \"q\" '"
        );
        assert_eq!(strip_markup("&amp;lt;"), "&lt;");
        assert_eq!(strip_markup("&#65;&#x42;&#X43;"), "ABC");
        assert_eq!(strip_markup("fish & chips"), "fish & chips");
        assert_eq!(
            strip_markup("&bogus; &#xzz; &#1114112;"),
            "&bogus; &#xzz; &#1114112;"
        );
    }

    #[test]
    fn unterminated_constructs_stay_literal() {
        assert_eq!(strip_markup("a <b unterminated"), "a <b unterminated");
        assert_eq!(strip_markup("x &amp"), "x &amp");
        assert_eq!(strip_markup("&"), "&");
        assert_eq!(strip_markup("tail&"), "tail&");
    }

    #[test]
    fn control_characters_are_neutralised() {
        assert_eq!(strip_markup("a\tb\u{7}c"), "a b c");
        assert_eq!(strip_markup("text   \n\n"), "text");
    }

    #[test]
    fn non_ascii_is_safe() {
        assert_eq!(strip_markup("héllo <b>wörld</b> ✓"), "héllo wörld ✓");
        assert_eq!(strip_markup("&é;x"), "&é;x");
        assert_eq!(strip_markup("&amp;héllo"), "&héllo");
    }
}
