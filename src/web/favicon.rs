// Open Estate brand mark ("Approval Stamp": a tilted double-border teal stamp with a house and checkmark on white).
// Single source of truth for the SVG favicon, the page-header logo and the raster fallbacks
// (`assets/brand/*` and `extension/icons/*` are rendered from this exact SVG).

use once_cell::sync::Lazy;

pub const FAVICON_SVG: &str = r###"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <rect width="64" height="64" rx="14" fill="#ffffff"/>
  <g transform="rotate(-8 32 32)">
    <rect x="11" y="11" width="42" height="42" rx="8.5" fill="none" stroke="#0f766e" stroke-width="4.2"/>
    <rect x="16.5" y="16.5" width="31" height="31" rx="4.5" fill="none" stroke="#0f766e" stroke-width="1.8"/>
    <path d="M32 20.5 42.5 29.2V41a2.4 2.4 0 0 1-2.4 2.4H23.9A2.4 2.4 0 0 1 21.5 41V29.2z" fill="#0f766e"/>
    <path d="M27 35.6l3.6 3.6 6.8-7.2" fill="none" stroke="#ffffff" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"/>
  </g>
</svg>"###;

/// Multi-size (16/32/48) ICO served at `/favicon.ico` for clients without SVG favicon support.
pub const FAVICON_ICO: &[u8] = include_bytes!("../../assets/brand/favicon.ico");

/// 180×180 full-bleed PNG for iOS/iPadOS home-screen bookmarks (`/apple-touch-icon.png`).
pub const APPLE_TOUCH_ICON_PNG: &[u8] = include_bytes!("../../assets/brand/apple-touch-icon.png");

/// Self-contained `data:` URI of the brand mark; works offline in saved reports.
pub static LOGO_MARK_DATA_URI: Lazy<String> = Lazy::new(|| {
    let compact = FAVICON_SVG.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("data:image/svg+xml,{}", urlencoding::encode(&compact))
});

pub static FAVICON_HEAD_TAGS: Lazy<String> = Lazy::new(|| {
    format!(
        r#"<link rel="icon" type="image/svg+xml" href="{}"><link rel="alternate icon" href="/favicon.ico"><link rel="apple-touch-icon" href="/apple-touch-icon.png">"#,
        LOGO_MARK_DATA_URI.as_str()
    )
});

/// Decorative header logo `<img>`; the visible "Open Estate" wordmark always sits next to it.
pub fn logo_mark_img(size_px: u32) -> String {
    format!(
        r#"<img class="logo-mark" src="{}" width="{size_px}" height="{size_px}" alt="" aria-hidden="true">"#,
        LOGO_MARK_DATA_URI.as_str()
    )
}
