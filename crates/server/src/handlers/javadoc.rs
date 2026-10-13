//! A Javadoc comment as Markdown, for a hover.
//!
//! The common shapes and no more: the description; inline `{@code}`,
//! `{@link}`, `{@linkplain}`, `{@literal}` and `{@value}`; the block tags a
//! reader looks for — parameters, return, throws, since, deprecated, see; and
//! the HTML the JDK's own comments are written in, as Markdown where it has
//! a counterpart and stripped where it has none.
//!
//! ponytail: a tag or an element outside that list passes through as text or
//! is dropped, and `{@inheritDoc}` is dropped rather than filled from the
//! overridden method. The upgrade is a renderer that walks the supertype for
//! it, which hover would need a member's overridden site to do.

/// The comment `/** … */`, rendered.
pub(crate) fn render(comment: &str) -> String {
    let body = comment
        .trim()
        .trim_start_matches("/**")
        .trim_end_matches("*/");
    let lines: Vec<&str> = body
        .lines()
        .map(|line| {
            let line = line.trim_start();
            line.strip_prefix('*')
                .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
                .unwrap_or(line)
        })
        .collect();
    // Description first, then each block tag with its continuation lines.
    let mut description = String::new();
    let mut tags: Vec<(String, String)> = Vec::new();
    // Inside a `<pre>` or an inline tag still open, a line starting with
    // `@` is an example's `@Override`, not a block tag.
    let mut in_pre = false;
    let mut open_braces = 0i32;
    for line in lines {
        let trimmed = line.trim_start();
        let literal = in_pre || open_braces > 0;
        let lower = line.to_ascii_lowercase();
        in_pre = (in_pre || lower.contains("<pre")) && !lower.contains("</pre");
        open_braces = (open_braces + line.matches("{@").count() as i32
            - line.matches('}').count() as i32)
            .max(0);
        if !literal
            && let Some(tag) = trimmed.strip_prefix('@')
            && tag.starts_with(|c: char| c.is_ascii_alphabetic())
        {
            let (name, rest) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
            tags.push((name.to_owned(), rest.trim().to_owned()));
        } else if let Some((_, text)) = tags.last_mut() {
            text.push(' ');
            text.push_str(trimmed);
        } else {
            description.push_str(line);
            description.push('\n');
        }
    }
    let mut out = html(&inline(description.trim()));
    let section = |out: &mut String, heading: &str, items: Vec<String>| {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("\n\n**{heading}**"));
        if let [only] = items.as_slice()
            && !only.starts_with('`')
        {
            out.push_str(&format!(" {only}"));
            return;
        }
        for item in items {
            out.push_str(&format!("\n- {item}"));
        }
    };
    let named = |name: &str| -> Vec<String> {
        tags.iter()
            .filter(|(tag, _)| tag == name)
            .map(|(_, text)| {
                let (first, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
                let rest = html(&inline(rest.trim()));
                if rest.is_empty() {
                    format!("`{first}`")
                } else {
                    format!("`{first}` — {rest}")
                }
            })
            .collect()
    };
    let plain = |name: &str| -> Vec<String> {
        tags.iter()
            .filter(|(tag, _)| tag == name)
            .map(|(_, text)| html(&inline(text)))
            .filter(|text| !text.is_empty())
            .collect()
    };
    section(&mut out, "Parameters", named("param"));
    section(&mut out, "Returns", plain("return"));
    let mut throws = named("throws");
    throws.extend(named("exception"));
    section(&mut out, "Throws", throws);
    section(&mut out, "Since", plain("since"));
    section(&mut out, "Deprecated", plain("deprecated"));
    section(&mut out, "See", plain("see"));
    out.trim().to_owned()
}

/// `{@code x}` and its siblings.
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("{@") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        // Braces nest inside `{@code}`: `{@code Map<K, {V}>}` is rare but
        // legal, so the close is the matching brace, not the first.
        let mut depth = 1;
        let close = after.char_indices().find_map(|(at, c)| {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(at)
        });
        let Some(close) = close else {
            out.push_str(&rest[open..]);
            return out;
        };
        let tag = &after[..close];
        let (name, body) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        // Escaped, so `html` reads `a < b` as text and not as a tag; it
        // decodes them again after it strips the tags that are real.
        let body = escaped(body.trim());
        match name {
            "code" => out.push_str(&format!("`{body}`")),
            "inheritDoc" => {}
            "link" | "linkplain" | "value" => {
                // `{@link Foo#bar(int, int) label}`: the label when there is
                // one — after the first space outside the parentheses.
                let mut depth = 0;
                let split = body.char_indices().find(|&(_, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    depth == 0 && c.is_whitespace()
                });
                let (target, label) = match split {
                    Some((at, _)) => (&body[..at], &body[at..]),
                    None => (body.as_str(), ""),
                };
                let shown = if label.trim().is_empty() {
                    target.trim_start_matches('#').replace('#', ".")
                } else {
                    label.trim().to_owned()
                };
                if !shown.is_empty() {
                    out.push_str(&format!("`{shown}`"));
                }
            }
            // `{@literal}`, `{@systemProperty}`, `{@summary}`, `{@index}` and
            // any other: their text, without the tag.
            _ => out.push_str(&body),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// `&`, `<` and `>` as the entities `html` decodes.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The HTML the JDK writes its comments in, as Markdown.
fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close].trim().to_ascii_lowercase();
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .find(|it| !it.is_empty());
        let closing = tag.starts_with('/');
        match (name.unwrap_or_default(), closing) {
            ("p", false) => out.push_str("\n\n"),
            ("br", _) => out.push('\n'),
            ("li", false) => out.push_str("\n- "),
            ("code" | "tt", _) => out.push('`'),
            ("b" | "strong", _) => out.push_str("**"),
            ("i" | "em", _) => out.push('*'),
            ("pre", false) => out.push_str("\n```\n"),
            ("pre", true) => out.push_str("\n```\n"),
            _ => {}
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    let decoded = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#64;", "@")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    // Collapse what the tags left: runs of blank lines to one.
    let mut collapsed = String::with_capacity(decoded.len());
    let mut blank = 0;
    for line in decoded.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank == 1 {
                collapsed.push('\n');
            }
            continue;
        }
        blank = 0;
        collapsed.push_str(line.trim_end());
        collapsed.push('\n');
    }
    collapsed.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comment_renders_its_description_and_its_block_tags() {
        let comment = "/**\n     * Returns a {@code String} that is a substring of this one.\n     * <p>\n     * See {@link #length()} and {@link java.util.List the lists}.\n     *\n     * @param beginIndex the beginning index, inclusive.\n     * @param endIndex   the ending index,\n     *                   exclusive.\n     * @return the specified substring.\n     * @throws IndexOutOfBoundsException if {@code beginIndex} is negative\n     * @since 1.0\n     */";
        assert_eq!(
            render(comment),
            "Returns a `String` that is a substring of this one.\n\nSee `length()` and `the lists`.\n\n\
             **Parameters**\n- `beginIndex` — the beginning index, inclusive.\n\
             - `endIndex` — the ending index, exclusive.\n\n\
             **Returns** the specified substring.\n\n\
             **Throws**\n- `IndexOutOfBoundsException` — if `beginIndex` is negative\n\n\
             **Since** 1.0"
        );
    }

    #[test]
    fn html_becomes_markdown_and_entities_their_characters() {
        assert_eq!(
            render("/** A <b>bold</b> <code>x &lt; y</code><ul><li>one<li>two</ul> &amp; done */"),
            "A **bold** `x < y`\n- one\n- two & done"
        );
    }

    /// What a code body says is what the hover says: `<` is not a tag.
    #[test]
    fn code_and_links_are_rendered_as_written() {
        assert_eq!(
            render("/** Returns {@code true} if {@code a < b}, {@code List<String>} otherwise. */"),
            "Returns `true` if `a < b`, `List<String>` otherwise."
        );
        assert_eq!(
            render(
                "/** See {@link #substring(int, int)} and {@link String#format(String, Object...) format}. */"
            ),
            "See `substring(int, int)` and `format`."
        );
        assert_eq!(
            render("/** The {@systemProperty java.home} property{@inheritDoc}. */"),
            "The java.home property."
        );
    }

    /// An example's `@Override` inside `<pre>` is code, not a block tag.
    #[test]
    fn an_annotation_in_an_example_is_not_a_block_tag() {
        let comment = "/**\n * Use it so:\n * <pre>\n * @Override\n * void run() {}\n * </pre>\n * Then done.\n * @param x the x\n */";
        assert_eq!(
            render(comment),
            "Use it so:\n\n```\n\n@Override\nvoid run() {}\n\n```\n\nThen done.\n\n**Parameters**\n- `x` — the x"
        );
    }

    #[test]
    fn an_unclosed_inline_tag_is_kept_as_written() {
        assert_eq!(render("/** half {@code open */"), "half {@code open");
    }
}

#[cfg(test)]
mod placement {
    use crate::handlers::doc_at;

    /// The comment must be a doc comment and come before the name it is
    /// read for; the name must be the member's, or the field a Lombok
    /// accessor is generated from.
    #[test]
    fn a_doc_is_read_only_before_the_name_it_belongs_to() {
        let text = "/** A. */ int a; /** B. */ int b;";
        let span = |needle: &str| {
            let at = text.find(needle).expect("in the text") as u32;
            (at, at + needle.len() as u32)
        };
        let b = |text: &str| {
            let at = text.find("int b").expect("b") as u32 + 4;
            (at, at + 1)
        };
        let a = (
            4 + text.find("int a").expect("a") as u32,
            5 + text.find("int a").expect("a") as u32,
        );
        assert_eq!(doc_at(text, "a", a, span("/** A. */")), Some("/** A. */"));
        assert_eq!(
            doc_at(text, "a", a, span("/** B. */")),
            None,
            "a comment after the name"
        );
        assert_eq!(
            doc_at(text, "b", b(text), span("int a;")),
            None,
            "not a comment"
        );
        assert_eq!(
            doc_at(text, "b", a, span("/** A. */")),
            None,
            "another name there"
        );
        assert_eq!(
            doc_at(text, "getA", a, span("/** A. */")),
            Some("/** A. */"),
            "an accessor"
        );
        // Another declaration between the comment and the name.
        let gap = "/** Z. */ int z; int a;";
        let at = gap.find("int a").expect("a") as u32 + 4;
        assert_eq!(
            doc_at(gap, "a", (at, at + 1), (0, 9)),
            None,
            "the comment is z's"
        );
        // `/**/` is no doc comment.
        let empty = "/**/ int a;";
        assert_eq!(doc_at(empty, "a", (9, 10), (0, 4)), None);
    }
}
