//! The Muster Kit as Muster serves it (docs/AGENT_PANELS.md "The kit"). Built-in apps carry the
//! kit's CSS with a `/*MK-FONTS*/` marker where the fonts go; a custom panel opened with
//! `kit: true` gets kit.css and kit.js in front of the agent's HTML. Both are added when the
//! panel's HTML is served, never stored, so each font exists once in the binary and none of it
//! counts toward the agent's 1 MiB. Nothing here loads from the network: the fonts are data:
//! URIs, which the panel CSP allows as `font-src data:` and nothing else.

use std::sync::LazyLock;

use base64::Engine;

pub const KIT_CSS: &str = include_str!("../web/panels/kit.css");
pub const KIT_JS: &str = include_str!("../web/panels/kit.js");
/// Inter and JetBrains Mono, Latin subsets, SIL OFL 1.1 (web/panels/fonts/).
const INTER: &[u8] = include_bytes!("../web/panels/fonts/inter-latin.woff2");
const MONO: &[u8] = include_bytes!("../web/panels/fonts/jetbrains-mono-latin.woff2");
pub const FONT_MARKER: &str = "/*MK-FONTS*/";

/// The two @font-face rules (the same text as web/dev/build_panels.ts `fontFaces`).
pub fn font_faces() -> &'static str {
    static FACES: LazyLock<String> = LazyLock::new(|| {
        let uri = |b: &[u8]| format!("data:font/woff2;base64,{}", base64::engine::general_purpose::STANDARD.encode(b));
        format!("@font-face{{font-family:\"MK Inter\";font-weight:100 900;font-style:normal;font-display:block;src:url({}) format(\"woff2\")}}\
            @font-face{{font-family:\"MK Mono\";font-weight:100 800;font-style:normal;font-display:block;src:url({}) format(\"woff2\")}}",
            uri(INTER), uri(MONO))
    });
    &FACES
}

/// kit.css (with its fonts) and kit.js, ready to go in front of an agent's HTML.
pub fn kit_head() -> &'static str {
    static HEAD: LazyLock<String> = LazyLock::new(|| {
        // The build escapes "</script"; do it again so a hand edit can never end the element early.
        let js = KIT_JS.replace("</script", "<\\/script").replace("</SCRIPT", "<\\/SCRIPT");
        format!("<style>{}</style><script>{js}</script>", KIT_CSS.replacen(FONT_MARKER, font_faces(), 1))
    });
    &HEAD
}

/// A panel's HTML as the sandbox receives it: a built-in gets its fonts; a custom panel opened
/// with `kit` gets the kit in front of the agent's own HTML; any other custom HTML is untouched.
pub fn serve(app: &str, kit: bool, html: String) -> String {
    if app != "custom" { return html.replacen(FONT_MARKER, font_faces(), 1) }
    if !kit { return html }
    let mut out = String::with_capacity(kit_head().len() + html.len());
    out.push_str(kit_head());
    out.push_str(&html);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fonts_and_kit_go_in_only_where_they_belong() {
        let builtin = crate::panel_apps::html("chart").unwrap().to_string();
        assert_eq!(builtin.matches(FONT_MARKER).count(), 1, "a built-in holds the font marker once");
        let served = serve("chart", false, builtin.clone());
        assert!(!served.contains(FONT_MARKER) && served.contains("@font-face{font-family:\"MK Inter\"") && served.contains("data:font/woff2;base64,d09GMg"));
        // A custom panel without the kit is exactly the agent's HTML, even if it holds the marker.
        let agent = "<main>/*MK-FONTS*/ hello</main>".to_string();
        assert_eq!(serve("custom", false, agent.clone()), agent);
        let kit = serve("custom", true, agent.clone());
        assert!(kit.starts_with("<style>") && kit.ends_with(&agent));
        assert!(kit.contains("<script>") && kit.matches("</script>").count() == 1, "kit.js cannot end its element early");
        assert!(!kit_head().contains("fetch(") && !kit_head().contains("XMLHttpRequest") && !kit_head().contains("<iframe"),
            "the kit adds no network or nested frames");
        assert!(kit_head().contains("@font-face") && !kit_head().contains(FONT_MARKER));
    }
}
