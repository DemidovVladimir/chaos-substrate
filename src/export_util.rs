//! Shared HTML/JSON escaping helpers used by graph and feature-context HTML
//! exporters.

/// Escape a JSON string so that it is safe to embed inside a `<script>` tag.
///
/// Replaces `&`, `<`, and `>` with their Unicode escape sequences (`&`,
/// `<`, `>`). This prevents the browser's HTML parser from
/// prematurely terminating the script block when the JSON contains `</script`
/// or other HTML-significant characters.
pub(crate) fn escape_script_json(json: &str) -> String {
    json.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

/// Escape text for embedding in HTML body content or double-quoted attributes.
///
/// Replacement order matters: `&` is escaped first so the entities introduced
/// by the later replacements are not themselves re-escaped. Pages that emit
/// single-quoted attribute values additionally escape `'` (see `user_story`).
pub(crate) fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
