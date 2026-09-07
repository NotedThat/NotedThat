use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

pub(super) struct Fence {
    marker: u8,
    count: usize,
    pub(super) quote_depth: usize,
    list_indent: Option<usize>,
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

pub(super) fn opens_fence(
    content: &str,
    quote_depth: usize,
    list_indent: Option<usize>,
) -> Option<Fence> {
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
        list_indent,
    })
}

pub(super) fn fence_continues(fence: &Fence, line: &str) -> bool {
    fence
        .list_indent
        .is_none_or(|indent| list_continuation_indent(line).is_some_and(|actual| actual >= indent))
}

pub(super) fn list_fence_indent(mut line: &str) -> Option<usize> {
    loop {
        let trimmed = markdown_indent(line);
        if let Some(rest) = trimmed.strip_prefix('>') {
            line = rest.strip_prefix(' ').unwrap_or(rest);
            continue;
        }
        let marker_end = trimmed
            .find([' ', '\t'])
            .filter(|end| is_list_marker(&trimmed[..*end]));
        return marker_end.map(|end| {
            let whitespace = trimmed[end..]
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            end + whitespace
        });
    }
}

pub(super) fn closes_fence(content: &str, fence: &Fence) -> bool {
    let trimmed = content.trim();
    let count = trimmed
        .bytes()
        .take_while(|byte| *byte == fence.marker)
        .count();
    count >= fence.count && trimmed[count..].trim().is_empty()
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

fn list_continuation_indent(mut line: &str) -> Option<usize> {
    loop {
        let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
        let trimmed = if spaces <= 3 { &line[spaces..] } else { line };
        if let Some(rest) = trimmed.strip_prefix('>') {
            line = rest.strip_prefix(' ').unwrap_or(rest);
            continue;
        }
        return Some(line.bytes().take_while(|byte| *byte == b' ').count());
    }
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
