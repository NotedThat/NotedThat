//! HTML for the browse pages.
//!
//! There is no templating crate in the workspace and this surface does not
//! justify introducing one: three page shapes and a handful of dynamic values,
//! against a proc macro and a template directory.
//!
//! The safety property this module exists to hold is that **no raw object key
//! reaches the output**. [`RowView`] carries only pre-escaped, pre-sanitised,
//! pre-encoded strings, so a template cannot reach around the escaping — a
//! discipline, not a hope.

use super::format::ABSENT;
use std::fmt::Write as _;

/// Escape text for HTML, safe in both text and double-quoted attribute position.
///
/// `'` is escaped too, so the helper stays correct if any markup here ever moves
/// to single-quoted attributes.
pub(super) fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Replace characters that cannot be rendered honestly with `U+FFFD`.
///
/// Escaping stops a key from becoming markup; it does nothing about a key that
/// lies about what it is. Control characters break the layout, and a
/// right-to-left override turns `report<U+202E>fdp.exe` into something that
/// reads as `reportexe.pdf` on the page.
///
/// Applied to the **displayed name only**. The link is built from the true key,
/// so a sanitised name still points at the object it names.
pub(super) fn sanitise_display(input: &str) -> String {
    input
        .chars()
        .map(|character| {
            if is_dishonest(character) {
                '\u{FFFD}'
            } else {
                character
            }
        })
        .collect()
}

/// Whether a character would make the rendered name misrepresent the key.
fn is_dishonest(character: char) -> bool {
    matches!(
        character,
        // C0 controls and DEL, and the C1 range: these break the layout.
        '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}'
        // Bidirectional marks, embeddings, overrides and isolates: these
        // reorder the name so it reads as something it is not.
        | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        // Zero-width and invisible separators: these hide a difference.
        | '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}'
    )
}

/// Text safe to place in the page: sanitised, then escaped, in that order.
pub(super) fn display_text(input: &str) -> String {
    escape_html(&sanitise_display(input))
}

/// What a row is, which decides how it is styled and ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowKind {
    /// The link to the parent directory.
    Parent,
    /// A synthesised subdirectory.
    Folder,
    /// A stored object.
    Object,
    /// A stored object the caller may see listed but may not read.
    RestrictedObject,
}

/// One rendered row, with every field already made safe.
///
/// The raw key is deliberately absent: there is nothing here to forget to
/// escape.
#[derive(Debug, Clone)]
pub(super) struct RowView {
    /// Display name, sanitised and escaped, with a trailing `/` for folders.
    pub(super) label: String,
    /// Percent-encoded, escaped href. `None` renders the label as plain text.
    pub(super) href: Option<String>,
    /// Human-readable size, or [`ABSENT`].
    pub(super) size: String,
    /// `YYYY-MM-DD`, or [`ABSENT`].
    pub(super) modified: String,
    /// The full RFC 7231 timestamp for the tooltip, if there is one.
    pub(super) modified_title: Option<String>,
    pub(super) kind: RowKind,
}

/// One breadcrumb segment.
pub(super) struct Crumb {
    /// Escaped display text.
    pub(super) label: String,
    /// Escaped href.
    pub(super) href: String,
}

/// Everything a directory or index page needs to render.
pub(super) struct PageView {
    /// Browser title, escaped.
    pub(super) title: String,
    pub(super) crumbs: Vec<Crumb>,
    pub(super) rows: Vec<RowView>,
    /// Summary line, e.g. `1 folder, 2 objects`.
    pub(super) summary: String,
    /// Shown under the table when a listing stopped early.
    pub(super) notice: Option<String>,
    /// Shown when some rows are listed but not readable.
    pub(super) footnote: Option<String>,
}

/// Render a directory or index page.
pub(super) fn page(view: &PageView) -> String {
    let mut html = String::with_capacity(2048 + view.rows.len() * 160);
    push_head(&mut html, &view.title);

    let _ = write!(&mut html, "<main><h1>");
    for (index, crumb) in view.crumbs.iter().enumerate() {
        if index > 0 {
            let _ = write!(&mut html, " / ");
        }
        let _ = write!(&mut html, "<a href=\"{}\">{}</a>", crumb.href, crumb.label);
    }
    let _ = write!(&mut html, " /</h1>");

    html.push_str(
        "<table><thead><tr><th class=\"name\">Name</th>\
         <th class=\"size\">Size</th><th class=\"modified\">Modified</th></tr></thead><tbody>",
    );
    for row in &view.rows {
        push_row(&mut html, row);
    }
    html.push_str("</tbody></table>");

    if let Some(notice) = &view.notice {
        let _ = write!(&mut html, "<p class=\"notice\">{notice}</p>");
    }
    let _ = write!(&mut html, "<p class=\"count\">{}</p>", view.summary);
    if let Some(footnote) = &view.footnote {
        let _ = write!(&mut html, "<p class=\"count\">{footnote}</p>");
    }

    html.push_str("</main></body></html>");
    html
}

/// Render an error page: a heading, a sentence, a way back, and a request id.
pub(super) fn error_page(heading: &str, detail: &str, request_id: &str) -> String {
    let mut html = String::with_capacity(1600);
    push_head(&mut html, heading);
    let _ = write!(
        &mut html,
        "<main><h1>{}</h1><p>{}</p><p><a href=\"/browse/\">Back to the index</a></p>\
         <p class=\"meta\">Request {}</p></main></body></html>",
        escape_html(heading),
        escape_html(detail),
        escape_html(request_id),
    );
    html
}

fn push_row(html: &mut String, row: &RowView) {
    let class = match row.kind {
        RowKind::Parent => " class=\"up\"",
        RowKind::RestrictedObject => " class=\"restricted\"",
        RowKind::Folder | RowKind::Object => "",
    };
    let _ = write!(html, "<tr{class}><td class=\"name\">");
    match &row.href {
        Some(href) => {
            let _ = write!(html, "<a href=\"{href}\">{}</a>", row.label);
        }
        None => html.push_str(&row.label),
    }
    let _ = write!(html, "</td><td class=\"size\">{}</td>", row.size);

    match &row.modified_title {
        Some(title) if row.modified != ABSENT => {
            let _ = write!(
                html,
                "<td class=\"modified\"><time datetime=\"{}\" title=\"{title}\">{}</time></td>",
                row.modified, row.modified
            );
        }
        _ => {
            let _ = write!(html, "<td class=\"modified\">{}</td>", row.modified);
        }
    }
    html.push_str("</tr>");
}

fn push_head(html: &mut String, title: &str) {
    let _ = write!(
        html,
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex, nofollow\">\
         <title>{} — NotedThat</title><style>{BROWSE_STYLE}</style></head><body>",
        escape_html(title),
    );
}

/// The whole stylesheet, inline.
///
/// A `/browse/style.css` route would add a route, a cache policy and a second
/// artefact to keep in step, for about 1.4 KiB a response.
const BROWSE_STYLE: &str = "\
:root{color-scheme:light dark;--bg:#fcfcfc;--fg:#1a1a1a;--muted:#6b6b6b;--rule:#e3e3e3;--link:#0b5fbe;--row:#f3f3f3}\
@media(prefers-color-scheme:dark){:root{--bg:#16181a;--fg:#e6e6e6;--muted:#9aa0a6;--rule:#2c2f33;--link:#79b8ff;--row:#1e2124}}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.55 system-ui,-apple-system,\"Segoe UI\",Roboto,\"Helvetica Neue\",Arial,sans-serif}\
main{max-width:68rem;margin:0 auto;padding:2rem 1.25rem 4rem}\
h1{margin:0 0 1.5rem;font-size:1rem;font-weight:600;letter-spacing:.01em;word-break:break-all}\
a{color:var(--link);text-decoration:none}\
a:hover,a:focus{text-decoration:underline}\
table{width:100%;border-collapse:collapse}\
th,td{padding:.35rem .75rem .35rem 0;text-align:left;vertical-align:baseline;border-bottom:1px solid var(--rule);white-space:nowrap}\
th{font-size:.78rem;font-weight:600;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);border-bottom-width:2px}\
td.name{width:100%;white-space:normal;word-break:break-all}\
td.name,td.size,td.modified,th.size,th.modified,code{font-family:ui-monospace,SFMono-Regular,\"SF Mono\",Menlo,Consolas,\"Liberation Mono\",monospace}\
th.size,td.size{text-align:right}\
td.size,td.modified{font-variant-numeric:tabular-nums}\
td.modified{color:var(--muted)}\
tbody tr:hover td{background:var(--row)}\
tr.up td.name a{color:var(--muted)}\
tr.restricted td.name{color:var(--muted)}\
p.count,p.notice,p.meta{margin:1.25rem 0 0;font-size:.85rem;color:var(--muted)}\
p.count+p.count{margin-top:.25rem}\
p.notice{padding:.6rem .8rem;border-left:3px solid var(--rule);background:var(--row)}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_html_metacharacter_is_escaped() {
        assert_eq!(
            escape_html(r#"a & b < c > d " e ' f"#),
            "a &amp; b &lt; c &gt; d &quot; e &#39; f"
        );
    }

    #[test]
    fn a_script_tag_in_an_object_name_cannot_become_markup() {
        // Given / When
        let escaped = escape_html("<script>alert(1)</script>.md");

        // Then
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(!escaped.contains('>'), "{escaped}");
    }

    #[test]
    fn control_and_bidirectional_characters_are_replaced_before_display() {
        // Given — a name crafted to display as `reportexe.pdf`.
        let spoof = "report\u{202E}fdp.exe";

        // When
        let display = sanitise_display(spoof);

        // Then
        assert_eq!(display, "report\u{FFFD}fdp.exe");
        assert_eq!(sanitise_display("a\nb\tc"), "a\u{FFFD}b\u{FFFD}c");
        assert_eq!(sanitise_display("a\u{200B}b"), "a\u{FFFD}b");
        assert_eq!(sanitise_display("ordinary-name.md"), "ordinary-name.md");
    }

    #[test]
    fn a_hostile_name_survives_both_passes_with_nothing_active_left() {
        // Given
        let hostile = "\u{202E}\"><script>x</script>\n.md";

        // When
        let rendered = display_text(hostile);

        // Then
        assert!(!rendered.contains('<'));
        assert!(!rendered.contains('"'));
        assert!(!rendered.contains('\u{202E}'));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn an_error_page_carries_the_request_id_so_a_denial_is_traceable() {
        // Given / When — the page says nothing about why, which is what makes
        // the correlation id load-bearing rather than decorative.
        let html = error_page("Not found", "There is no page here.", "req-123");

        // Then
        assert!(html.contains("req-123"), "{html}");
        assert!(html.contains("<a href=\"/browse/\">"), "{html}");
    }
}
