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
