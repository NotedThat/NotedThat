use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

pub(super) enum HtmlEnd {
    Marker(&'static str),
    BlankLine,
}

pub(super) struct Fence {
    marker: u8,
    count: usize,
    pub(super) quote_depth: usize,
}

pub(super) fn parse_heading(
    markdown: &str,
    label_char_cap: usize,
) -> Option<(usize, usize, String)> {
    let mut active = None;
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                active = Some((range.start, depth(level), String::new()));
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, _, label)) = active.as_mut() {
                    let remaining = label_char_cap.saturating_sub(label.chars().count());
                    label.extend(text.chars().take(remaining));
                }
            }
            Event::End(TagEnd::Heading(_)) => return active,
            _ => {}
        }
    }
    None
}

pub(super) fn container_content(mut line: &str) -> &str {
    loop {
        let trimmed = markdown_indent(line);
        if let Some(rest) = trimmed.strip_prefix('>') {
            line = rest.strip_prefix(' ').unwrap_or(rest);
            continue;
        }
        let marker_end = trimmed
            .find([' ', '\t'])
            .filter(|end| is_list_marker(&trimmed[..*end]));
        if let Some(end) = marker_end {
            line = trimmed[end..].trim_start_matches([' ', '\t']);
            continue;
        }
        return trimmed;
    }
}

pub(super) fn opens_fence(content: &str, quote_depth: usize) -> Option<Fence> {
    let marker = *content.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let count = content.bytes().take_while(|byte| *byte == marker).count();
    if marker == b'`' && content[count..].contains('`') {
        return None;
    }
    (count >= 3).then_some(Fence {
        marker,
        count,
        quote_depth,
    })
}

pub(super) fn closes_fence(content: &str, fence: &Fence) -> bool {
    let trimmed = content.trim();
    let count = trimmed
        .bytes()
        .take_while(|byte| *byte == fence.marker)
        .count();
    count >= fence.count && trimmed[count..].trim().is_empty()
}

pub(super) fn opens_html(content: &str) -> Option<HtmlEnd> {
    let lower = content.trim_start().to_ascii_lowercase();
    if lower.starts_with("<!--") {
        Some(HtmlEnd::Marker("-->"))
    } else if lower.starts_with("<?") {
        Some(HtmlEnd::Marker("?>"))
    } else if lower.starts_with("<![cdata[") {
        Some(HtmlEnd::Marker("]]>"))
    } else if lower.starts_with("<!") {
        Some(HtmlEnd::Marker(">"))
    } else if lower.starts_with("<script") {
        Some(HtmlEnd::Marker("</script>"))
    } else if lower.starts_with("<pre") {
        Some(HtmlEnd::Marker("</pre>"))
    } else if lower.starts_with("<style") {
        Some(HtmlEnd::Marker("</style>"))
    } else if lower.starts_with("<textarea") {
        Some(HtmlEnd::Marker("</textarea>"))
    } else if is_block_html_tag(&lower) {
        Some(HtmlEnd::BlankLine)
    } else {
        None
    }
}

fn is_block_html_tag(lower: &str) -> bool {
    let tag = lower.strip_prefix('<').map_or("", |value| {
        let value = value.strip_prefix('/').unwrap_or(value);
        value
            .split(|character: char| !character.is_ascii_alphanumeric())
            .next()
            .unwrap_or("")
    });
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "base"
            | "basefont"
            | "blockquote"
            | "body"
            | "caption"
            | "center"
            | "col"
            | "colgroup"
            | "dd"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "frame"
            | "frameset"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hr"
            | "html"
            | "iframe"
            | "legend"
            | "li"
            | "link"
            | "main"
            | "menu"
            | "menuitem"
            | "nav"
            | "noframes"
            | "ol"
            | "optgroup"
            | "option"
            | "p"
            | "param"
            | "search"
            | "section"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "title"
            | "tr"
            | "track"
            | "ul"
    )
}

pub(super) fn html_continues(end: &HtmlEnd, content: &str, tail: &str) -> bool {
    match end {
        HtmlEnd::Marker(marker) => {
            !content.to_ascii_lowercase().contains(marker)
                && !tail.to_ascii_lowercase().contains(marker)
        }
        HtmlEnd::BlankLine => !content.trim().is_empty(),
    }
}

pub(super) fn is_setext_underline(content: &str) -> bool {
    let trimmed = content.trim();
    let Some(marker) = trimmed.bytes().next() else {
        return false;
    };
    matches!(marker, b'=' | b'-') && trimmed.bytes().all(|byte| byte == marker)
}

pub(super) fn parse_giant_atx(content: &str, label_char_cap: usize) -> Option<(usize, String)> {
    let count = content.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&count)
        || !content
            .as_bytes()
            .get(count)
            .is_some_and(u8::is_ascii_whitespace)
    {
        return None;
    }
    let label = content[count..]
        .trim_start()
        .chars()
        .take(label_char_cap)
        .collect();
    Some((depth_from_count(count), label))
}

fn markdown_indent(line: &str) -> &str {
    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
    if spaces <= 3 { &line[spaces..] } else { line }
}

pub(super) fn quote_depth(mut line: &str) -> usize {
    let mut depth = 0;
    loop {
        let trimmed = markdown_indent(line);
        let Some(rest) = trimmed.strip_prefix('>') else {
            return depth;
        };
        depth += 1;
        line = rest.strip_prefix(' ').unwrap_or(rest);
    }
}

fn is_list_marker(value: &str) -> bool {
    matches!(value, "-" | "+" | "*")
        || value.strip_suffix(['.', ')']).is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

const fn depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => 3,
    }
}

const fn depth_from_count(count: usize) -> usize {
    match count {
        1 => 1,
        2 => 2,
        _ => 3,
    }
}
