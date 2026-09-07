pub(super) enum HtmlEnd {
    Marker(HtmlMarker),
    BlankLine,
}

#[derive(Clone, Copy)]
pub(super) enum HtmlMarker {
    Comment,
    ProcessingInstruction,
    Cdata,
    Declaration,
    Script,
    Pre,
    Style,
    Textarea,
}

impl HtmlMarker {
    const fn index(self) -> usize {
        match self {
            Self::Comment => 0,
            Self::ProcessingInstruction => 1,
            Self::Cdata => 2,
            Self::Declaration => 3,
            Self::Script => 4,
            Self::Pre => 5,
            Self::Style => 6,
            Self::Textarea => 7,
        }
    }
}

pub(super) const HTML_MARKERS: [&str; 8] = [
    "-->",
    "?>",
    "]]>",
    ">",
    "</script>",
    "</pre>",
    "</style>",
    "</textarea>",
];

pub(super) fn opens_html(content: &str) -> Option<HtmlEnd> {
    let lower = content.to_ascii_lowercase();
    if lower.starts_with("<!--") {
        Some(HtmlEnd::Marker(HtmlMarker::Comment))
    } else if lower.starts_with("<?") {
        Some(HtmlEnd::Marker(HtmlMarker::ProcessingInstruction))
    } else if lower.starts_with("<![cdata[") {
        Some(HtmlEnd::Marker(HtmlMarker::Cdata))
    } else if lower.starts_with("<!") {
        Some(HtmlEnd::Marker(HtmlMarker::Declaration))
    } else if starts_html_tag(&lower, "<script") {
        Some(HtmlEnd::Marker(HtmlMarker::Script))
    } else if starts_html_tag(&lower, "<pre") {
        Some(HtmlEnd::Marker(HtmlMarker::Pre))
    } else if starts_html_tag(&lower, "<style") {
        Some(HtmlEnd::Marker(HtmlMarker::Style))
    } else if starts_html_tag(&lower, "<textarea") {
        Some(HtmlEnd::Marker(HtmlMarker::Textarea))
    } else if is_block_html_tag(&lower) {
        Some(HtmlEnd::BlankLine)
    } else {
        None
    }
}

fn starts_html_tag(lower: &str, tag: &str) -> bool {
    lower.strip_prefix(tag).is_some_and(|suffix| {
        suffix.is_empty()
            || suffix
                .bytes()
                .next()
                .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>'))
    })
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

pub(super) fn html_continues(
    end: &HtmlEnd,
    content: &str,
    markers_seen: [bool; HTML_MARKERS.len()],
) -> bool {
    match end {
        HtmlEnd::Marker(marker) => !markers_seen[marker.index()],
        HtmlEnd::BlankLine => !content.trim().is_empty(),
    }
}
