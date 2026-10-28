//! Host skin: operator-applied CSS layered over the embedded UI. The skin is scoped to
//! `body` and stops at dialogs and the alert toast, which pin their own palette and type,
//! so confirmations stay legible whatever a skin does; `/?safe` serves the page without it.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, String>;

pub(super) const MAX_SKIN: usize = 32 * 1024;
pub(super) const LINK: &str = r#"<link rel="stylesheet" href="/skin.css" id="skin">"#;
pub(super) const WORKSPACE_FILE: &str = ".spindle/skin.css";
const KEEP: usize = 12;
const AT_RULES: &[&str] = &["media", "supports", "container"];
// Value functions that fetch, script or read markup; refused anywhere in the text.
const DENIED_TEXT: &[&str] = &[
    "url(",
    "image(",
    "image-set(",
    "cross-fade(",
    "element(",
    "paint(",
    "src(",
    "attr(",
    "expression(",
    "javascript:",
];
/// How a property's value is checked. Only listed properties are allowed: nothing a skin
/// writes can move, hide, overlay or stack a control (no position, offsets, z-index,
/// opacity, transform, display, visibility, pointer events, clipping, masks, filters,
/// generated content or text security), and the box-growing lengths it may set are
/// small, non-negative and free of functions, so a control cannot be inflated over
/// another one to catch its taps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    /// Colours, backgrounds, shadows and custom properties: paint only, any value
    /// (rgb(), var() and gradients included; fetching functions are refused above).
    Paint,
    /// Lengths that grow a box: px <= 64, em/rem <= 4, % <= 100.
    Size,
    /// Upper bounds and radii never grow a box: any non-negative length.
    Limit,
    /// 8-32px or 0.5-2.5rem; never relative to an inherited size, so it cannot compound.
    FontSize,
    /// A unitless factor of 1-3, 8-64px, or `normal`.
    LineHeight,
    /// Width, style and a literal colour (no functions: a var() could carry a width).
    Border,
    /// Identifiers, commas and unitless numbers up to 1000.
    Keyword,
    /// Shadows: offsets, blur and spread within 16px (or 1em) either way, literal
    /// colours (hex, names, rgb()/hsl() of numbers), `inset`. A larger shadow could
    /// black out the screen or draw one control's label over another.
    Shadow,
}
const ALLOWED: &[(&str, Kind)] = &[
    ("color", Kind::Paint),
    ("background", Kind::Paint),
    ("background-color", Kind::Paint),
    ("background-image", Kind::Paint),
    ("background-position", Kind::Paint),
    ("background-size", Kind::Paint),
    ("background-repeat", Kind::Paint),
    ("border-color", Kind::Paint),
    ("border-top-color", Kind::Paint),
    ("border-right-color", Kind::Paint),
    ("border-bottom-color", Kind::Paint),
    ("border-left-color", Kind::Paint),
    ("outline-color", Kind::Paint),
    ("accent-color", Kind::Paint),
    ("caret-color", Kind::Paint),
    ("text-decoration-color", Kind::Paint),
    ("scrollbar-color", Kind::Paint),
    ("box-shadow", Kind::Shadow),
    ("text-shadow", Kind::Shadow),
    ("border", Kind::Border),
    ("border-top", Kind::Border),
    ("border-right", Kind::Border),
    ("border-bottom", Kind::Border),
    ("border-left", Kind::Border),
    ("outline", Kind::Border),
    ("border-width", Kind::Size),
    ("border-top-width", Kind::Size),
    ("border-right-width", Kind::Size),
    ("border-bottom-width", Kind::Size),
    ("border-left-width", Kind::Size),
    ("outline-width", Kind::Size),
    ("outline-offset", Kind::Size),
    ("border-style", Kind::Keyword),
    ("border-top-style", Kind::Keyword),
    ("border-right-style", Kind::Keyword),
    ("border-bottom-style", Kind::Keyword),
    ("border-left-style", Kind::Keyword),
    ("outline-style", Kind::Keyword),
    ("border-radius", Kind::Limit),
    ("border-top-left-radius", Kind::Limit),
    ("border-top-right-radius", Kind::Limit),
    ("border-bottom-left-radius", Kind::Limit),
    ("border-bottom-right-radius", Kind::Limit),
    ("margin", Kind::Size),
    ("margin-top", Kind::Size),
    ("margin-right", Kind::Size),
    ("margin-bottom", Kind::Size),
    ("margin-left", Kind::Size),
    ("padding", Kind::Size),
    ("padding-top", Kind::Size),
    ("padding-right", Kind::Size),
    ("padding-bottom", Kind::Size),
    ("padding-left", Kind::Size),
    ("gap", Kind::Size),
    ("row-gap", Kind::Size),
    ("column-gap", Kind::Size),
    ("width", Kind::Size),
    ("height", Kind::Size),
    ("min-width", Kind::Size),
    ("min-height", Kind::Size),
    ("max-width", Kind::Limit),
    ("max-height", Kind::Limit),
    ("font-family", Kind::Keyword),
    ("font-weight", Kind::Keyword),
    ("font-style", Kind::Keyword),
    ("font-variant", Kind::Keyword),
    ("font-variant-numeric", Kind::Keyword),
    ("font-size", Kind::FontSize),
    ("line-height", Kind::LineHeight),
    ("letter-spacing", Kind::Size),
    ("word-spacing", Kind::Size),
    ("text-indent", Kind::Size),
    ("text-transform", Kind::Keyword),
    ("text-align", Kind::Keyword),
    ("text-decoration", Kind::Border),
    ("text-decoration-line", Kind::Keyword),
    ("text-decoration-style", Kind::Keyword),
    ("text-decoration-thickness", Kind::Size),
    ("text-underline-offset", Kind::Size),
    ("text-overflow", Kind::Keyword),
    ("white-space", Kind::Keyword),
    ("word-break", Kind::Keyword),
    ("overflow-wrap", Kind::Keyword),
    ("hyphens", Kind::Keyword),
    ("tab-size", Kind::Keyword),
    ("justify-content", Kind::Keyword),
    ("list-style-type", Kind::Keyword),
    ("cursor", Kind::Keyword),
];

/// Checks one declaration value against its property's kind.
fn value_ok(kind: Kind, value: &str) -> std::result::Result<(), &'static str> {
    if kind == Kind::Paint {
        return Ok(());
    }
    if kind == Kind::Shadow {
        return shadow_ok(value);
    }
    if value.contains('(') {
        return Err("functions (calc, var, ...) are not allowed here");
    }
    let bytes = value.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let b = bytes[at];
        if b.is_ascii_digit() || (b == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)) {
            if at > 0 && bytes[at - 1] == b'-' {
                return Err("negative values are not allowed");
            }
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'.') {
                at += 1;
            }
            let number: f64 = value[start..at].parse().map_err(|_| "invalid number")?;
            let unit_start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphabetic() || bytes[at] == b'%') {
                at += 1;
            }
            number_ok(kind, number, &value[unit_start..at])?;
        } else if b == b'#' {
            if kind != Kind::Border {
                return Err("colours belong in colour properties");
            }
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_hexdigit() {
                at += 1;
            }
        } else if ident(b) || matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b',' | b'/') {
            // Identifiers run to their end so digits inside them are not numbers.
            if ident(b) && b != b'-' {
                let start = at;
                while at < bytes.len() && ident(bytes[at]) {
                    at += 1;
                }
                // Relative keywords compound through nesting like em does.
                if kind == Kind::FontSize && matches!(&value[start..at], "larger" | "smaller") {
                    return Err("relative font sizes are not allowed");
                }
            } else {
                at += 1;
            }
        } else {
            return Err("unexpected character in value");
        }
    }
    Ok(())
}
fn shadow_ok(value: &str) -> std::result::Result<(), &'static str> {
    // Colour functions of plain numbers are kept as words; nothing else may call.
    let mut plain = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(open) = rest.find('(') {
        let name_start = rest[..open]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .map_or(0, |at| at + 1);
        if !matches!(&rest[name_start..open], "rgb" | "rgba" | "hsl" | "hsla") {
            return Err("only rgb() and hsl() colours may appear in a shadow");
        }
        let close = rest[open..].find(')').ok_or("unbalanced parenthesis")? + open;
        if !rest[open + 1..close]
            .bytes()
            .all(|b| b.is_ascii_digit() || b" .,%/deg".contains(&b))
        {
            return Err("colour functions take plain numbers here");
        }
        plain.push_str(&rest[..name_start]);
        plain.push_str(" colour ");
        rest = &rest[close + 1..];
    }
    plain.push_str(rest);
    for word in plain.split(|c: char| c.is_ascii_whitespace() || c == ',') {
        if word.is_empty() {
            continue;
        }
        if let Some(hex) = word.strip_prefix('#') {
            if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("invalid colour");
            }
            continue;
        }
        let number = word.strip_prefix('-').unwrap_or(word);
        if number.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
            let end = number
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(number.len());
            let amount: f64 = number[..end].parse().map_err(|_| "invalid number")?;
            let fits = match &number[end..] {
                "" => amount == 0.0,
                "px" => amount <= 16.0,
                "em" | "rem" => amount <= 1.0,
                _ => false,
            };
            if !fits {
                return Err("shadows stay within 16px (1em) either way");
            }
        } else if !word.bytes().all(ident) {
            return Err("unexpected character in value");
        }
    }
    Ok(())
}

fn number_ok(kind: Kind, number: f64, unit: &str) -> std::result::Result<(), &'static str> {
    let fits = match (kind, unit) {
        (Kind::FontSize, "px") => (8.0..=32.0).contains(&number),
        (Kind::FontSize, "rem") => (0.5..=2.5).contains(&number),
        (Kind::LineHeight, "") => (1.0..=3.0).contains(&number),
        (Kind::LineHeight, "px") => (8.0..=64.0).contains(&number),
        (Kind::FontSize | Kind::LineHeight, _) => false,
        (_, "") if number == 0.0 => true,
        (Kind::Size | Kind::Border, "px") => number <= 64.0,
        (Kind::Size | Kind::Border, "em" | "rem") => number <= 4.0,
        (Kind::Size | Kind::Border, "%") => number <= 100.0,
        (Kind::Limit, "px" | "em" | "rem" | "%" | "ch" | "vw" | "vh" | "dvh" | "svh" | "lvh") => {
            true
        }
        (Kind::Keyword, "") => number <= 1000.0,
        _ => false,
    };
    if fits {
        Ok(())
    } else {
        Err("value is out of the range a skin may use")
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}
fn digest(css: &str) -> String {
    Sha256::digest(css.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn version_id(id: &str) -> bool {
    id.len() == 37
        && id.as_bytes()[20] == b'-'
        && id[..20].bytes().all(|b| b.is_ascii_digit())
        && id[21..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn sanitize(text: &str) -> Result<String> {
    if text.len() > MAX_SKIN {
        return Err(format!("skin exceeds {} KiB", MAX_SKIN / 1024));
    }
    if !text
        .bytes()
        .all(|b| matches!(b, b'\n' | b'\t' | b'\r') || (0x20..0x7f).contains(&b))
    {
        return Err("skin must be plain ASCII CSS".into());
    }
    let mut css = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        css.push_str(&rest[..start]);
        let end = rest[start + 2..]
            .find("*/")
            .ok_or("unterminated comment in skin")?;
        css.push(' ');
        rest = &rest[start + 2 + end + 2..];
    }
    css.push_str(rest);
    let css = css.trim().to_string();
    let lower = css.to_ascii_lowercase();
    // `!` can only be `!important` (however spaced), which would beat the host's rules.
    if let Some(c) = lower
        .chars()
        .find(|c| matches!(c, '"' | '\'' | '\\' | '<' | '!'))
    {
        return Err(format!("skin may not contain {c}"));
    }
    // Quotes, escapes and comments are gone, so every brace is structural; balance keeps
    // the text inside the host's @layer/@scope wrapper.
    let mut depth = 0usize;
    for b in lower.bytes() {
        match b {
            b'{' => {
                depth += 1;
                if depth > 8 {
                    return Err("skin nests too deeply".into());
                }
            }
            b'}' => depth = depth.checked_sub(1).ok_or("unbalanced braces in skin")?,
            _ => {}
        }
    }
    if depth != 0 {
        return Err("unbalanced braces in skin".into());
    }
    for (at, _) in lower.match_indices('@') {
        let name: String = lower[at + 1..]
            .bytes()
            .take_while(|b| ident(*b))
            .map(char::from)
            .collect();
        if !AT_RULES.contains(&name.as_str()) {
            return Err(format!("@{name} is not allowed in a skin"));
        }
    }
    for word in DENIED_TEXT {
        if lower.contains(word) {
            return Err(format!("{word} is not allowed in a skin"));
        }
    }
    // With quotes, escapes and comments gone, text ending at `{` is a selector or at-rule
    // prelude and text ending at `;` or `}` is a declaration.
    let mut start = 0;
    for (at, b) in lower.bytes().enumerate() {
        if !matches!(b, b'{' | b';' | b'}') {
            continue;
        }
        let segment = lower[start..at].trim();
        start = at + 1;
        if b == b'{' || segment.is_empty() {
            continue;
        }
        let (property, value) = segment
            .split_once(':')
            .ok_or_else(|| format!("unexpected text in skin: {segment}"))?;
        let property = property.trim();
        if property.is_empty() || !property.bytes().all(ident) {
            return Err(format!("invalid property in skin: {property}"));
        }
        let kind = if property.starts_with("--") {
            Kind::Paint
        } else {
            ALLOWED
                .iter()
                .find(|(name, _)| *name == property)
                .map(|(_, kind)| *kind)
                .ok_or_else(|| format!("the {property} property is not allowed in a skin"))?
        };
        value_ok(kind, value.trim())
            .map_err(|why| format!("{property}: {why} in a skin ({})", value.trim()))?;
    }
    if !lower[start..].trim().is_empty() {
        return Err("skin has text outside a rule".into());
    }
    Ok(css)
}

pub(super) fn wrap(css: &str) -> String {
    if css.is_empty() {
        return "@layer base, skin;\n".into();
    }
    format!(
        "@layer base, skin;\n@layer skin {{\n@scope (body) to (dialog, #toast) {{\n{css}\n}}\n}}\n"
    )
}

pub(super) struct Skins {
    dir: PathBuf,
    // Serializes mutations: concurrent requests would otherwise share temp files.
    lock: std::sync::Mutex<()>,
}

impl Skins {
    pub(super) fn open(root: &Path) -> Result<Self> {
        let dir = root.join("skin");
        super::private_dir(&dir)?;
        super::private_dir(&dir.join("versions"))?;
        Ok(Self {
            dir,
            lock: std::sync::Mutex::new(()),
        })
    }
    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(name)).ok()
    }
    fn write(&self, name: &str, text: &str) -> Result<()> {
        spindle_store::durable::write(&self.dir.join(name), text.as_bytes()).map_err(err)
    }
    fn versions(&self) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(self.dir.join("versions"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter_map(|name| name.strip_suffix(".css").map(str::to_string))
            .filter(|id| version_id(id))
            .collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids
    }
    /// What `/skin.css` serves. A file that no longer passes the sanitizer serves nothing.
    pub(super) fn served(&self, staged: bool) -> String {
        let name = if staged { "staged.css" } else { "active.css" };
        wrap(
            &self
                .read(name)
                .and_then(|css| sanitize(&css).ok())
                .unwrap_or_default(),
        )
    }
    pub(super) fn state(&self) -> Value {
        let active = self.read("active.css").filter(|css| !css.is_empty());
        let staged = self.read("staged.css");
        json!({
            "active": active.as_deref().map(|css| json!({"digest":digest(css),"bytes":css.len()})),
            "staged": staged.as_deref().map(|css| json!({"digest":digest(css),"bytes":css.len(),"css":css})),
            "versions": self.versions().iter().map(|id| json!({"id":id,"digest":&id[21..],"at":id[..20].parse::<u64>().unwrap_or(0)})).collect::<Vec<_>>(),
            "max_bytes": MAX_SKIN,
            "workspace_file": WORKSPACE_FILE,
        })
    }
    pub(super) fn stage(&self, css: &str) -> Result<Value> {
        let css = sanitize(css)?;
        if css.is_empty() {
            return Err("the skin is empty; use Turn off instead".into());
        }
        let _guard = self.guard();
        self.write("staged.css", &css)?;
        Ok(self.state())
    }
    pub(super) fn apply(&self, expected: &str) -> Result<Value> {
        let _guard = self.guard();
        let css = self.read("staged.css").ok_or("nothing is staged")?;
        if digest(&css) != expected {
            return Err("the staged skin changed; reopen settings and review it again".into());
        }
        let css = sanitize(&css)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(err)?
            .as_millis();
        self.write(&format!("versions/{now:020}-{}.css", digest(&css)), &css)?;
        self.write("active.css", &css)?;
        let _ = std::fs::remove_file(self.dir.join("staged.css"));
        for id in self.versions().iter().skip(KEEP) {
            let _ = std::fs::remove_file(self.dir.join("versions").join(format!("{id}.css")));
        }
        Ok(self.state())
    }
    pub(super) fn discard(&self) -> Result<Value> {
        let _guard = self.guard();
        match std::fs::remove_file(self.dir.join("staged.css")) {
            Ok(()) => Ok(self.state()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(self.state()),
            Err(e) => Err(err(e)),
        }
    }
    pub(super) fn restore(&self, id: &str) -> Result<Value> {
        if !version_id(id) {
            return Err("unknown skin version".into());
        }
        let _guard = self.guard();
        let css = self
            .read(&format!("versions/{id}.css"))
            .ok_or("unknown skin version")?;
        self.write("active.css", &sanitize(&css)?)?;
        Ok(self.state())
    }
    pub(super) fn off(&self) -> Result<Value> {
        let _guard = self.guard();
        self.write("active.css", "")?;
        Ok(self.state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_keeps_palette_and_layout_but_refuses_escapes_fetches_and_relabels() {
        let css = "/* paper */ body { --accent: #224466; --page: #fffaf0 }\n#bar button { min-height: 48px; justify-content: center; width: fit-content }\n@media (max-width: 400px) { header { padding: 4px } }\n#filter, .zoom-in:hover, .content-box { border-color: var(--accent) }";
        let clean = sanitize(css).unwrap();
        assert!(!clean.contains("paper"));
        assert!(clean.contains("fit-content"));
        assert!(
            clean.contains("#filter"),
            "selectors named like denied properties are fine"
        );
        for bad in [
            "body { background: url(x) }",
            "@import x;",
            "@layer base { body { color: red } }",
            "@scope (dialog) { button { color: red } }",
            "@font-face { font-family: x }",
            "body { color: red !important }",
            "body { color: red ! important }",
            "body { color: red !/**/important }",
            "#bar button { box-shadow: 0 0 0 100vmax #000 }",
            "button { text-shadow: 0 -60px 0 currentColor }",
            "button { box-shadow: 0 0 0 1px var(--x) }",
            "button { box-shadow: 0 0 2px calc(100vw) red }",
            "button::after { content: x }",
            "button::after { content : x }",
            "body { font-family: \"x\" }",
            "body { color: r\\65 d }",
            "} dialog { color: red } body {",
            "body { color: red",
            "body /* } */ { color: red",
            "body { filter: invert(1) }",
            "body { zoom: 4 }",
            "body { color: red } \u{00e9}",
            "body { -moz-binding: x }",
            "body { -webkit-filter: blur(1px) }",
            "body { -webkit-mask-image: linear-gradient(red, blue) }",
            "body { color: red } stray",
            "body { color red }",
            "body { col@r: red }",
            "body { unknown-property: 1 }",
        ] {
            assert!(sanitize(bad).is_err(), "{bad}");
        }
        let rich = "body { border: 1px solid #ccc; box-shadow: 0 1px 2px rgba(0, 0, 0, .2), inset 0 0 0 1px #ddd; text-shadow: 0 -1px 0 #fff; font-family: menlo, monospace; font-weight: 600; line-height: 1.6; font-size: 15px; max-width: 900px; border-radius: 4px; letter-spacing: .02em; background: linear-gradient(#fff, #eee) }";
        assert!(sanitize(rich).is_ok(), "{:?}", sanitize(rich));
        assert!(sanitize(&"a{}".repeat(MAX_SKIN)).is_err());
    }

    /// Review findings: an inherited text-security scrambles every confirmation, and a
    /// transparent full-screen control catches taps meant for others. Neither can be
    /// written, nor can a control be grown, offset or hidden by any other route.
    #[test]
    fn skins_cannot_scramble_text_or_hijack_taps() {
        for attack in [
            "body{-webkit-text-security:disc}",
            "body { text-security: disc }",
            "#continue{position:fixed;inset:0;opacity:0;z-index:99}",
            "#continue { position: fixed }",
            "#continue { inset: 0 }",
            "#continue { top: 0 }",
            "#continue { opacity: 0 }",
            "#continue { z-index: 99 }",
            "#continue { transform: scale(40) }",
            "#continue { translate: 0 -400px }",
            "#stop { pointer-events: none }",
            "#stop { visibility: hidden }",
            "#stop { display: none }",
            "#stop { clip-path: inset(50%) }",
            "#stop { clip: rect(0 0 0 0) }",
            "#bar { align-items: flex-end }",
            "#bar { flex-direction: column-reverse }",
            "#bar { overflow: visible }",
            "#continue { height: 900px }",
            "#continue { min-height: 90vh }",
            "#continue { padding-top: 70px }",
            "#continue { margin-top: -50px }",
            "#continue { height: var(--tall) }",
            "#continue { padding: calc(10px * 90) }",
            "#continue { border-top: 900px solid transparent }",
            "#continue { border: 1px solid var(--line) }",
            "body { font-size: 0 }",
            "body { font-size: 2em }",
            "body { font-size: larger }",
            "body { line-height: 0 }",
            "h1 { letter-spacing: -0.5em }",
            "body { writing-mode: vertical-rl }",
            "body { direction: rtl }",
        ] {
            assert!(sanitize(attack).is_err(), "{attack}");
        }
        assert!(sanitize("#bar button { min-height: 44px; padding: 0 12px }").is_ok());
    }

    #[test]
    fn wrapped_skin_stays_in_its_layer_and_scope() {
        let served = wrap("body { --accent: red }");
        assert!(served.starts_with(
            "@layer base, skin;\n@layer skin {\n@scope (body) to (dialog, #toast) {\n"
        ));
        assert_eq!(wrap(""), "@layer base, skin;\n");
    }

    #[test]
    fn stage_apply_version_restore_and_off() {
        let root = std::env::temp_dir().join(format!("spindle-skin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let skins = Skins::open(&root).unwrap();
        assert_eq!(skins.served(false), wrap(""));
        assert!(skins.apply("x").is_err(), "nothing staged");
        assert!(skins.stage("body { background: url(x) }").is_err());
        let first = skins.stage("body { --accent: #123456 }").unwrap();
        let staged = first["staged"]["digest"].as_str().unwrap().to_string();
        assert!(skins.served(true).contains("#123456"));
        assert_eq!(
            skins.served(false),
            wrap(""),
            "staging never changes the live skin"
        );
        assert!(
            skins.apply("0000000000000000").is_err(),
            "stale review refused"
        );
        let applied = skins.apply(&staged).unwrap();
        assert!(applied["staged"].is_null());
        assert_eq!(applied["active"]["digest"], staged.as_str());
        assert!(skins.served(false).contains("#123456"));
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = skins.stage("body { --accent: #abcdef }").unwrap();
        skins
            .apply(second["staged"]["digest"].as_str().unwrap())
            .unwrap();
        let versions = skins.state()["versions"].as_array().unwrap().clone();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[1]["digest"], staged.as_str());
        skins.restore(versions[1]["id"].as_str().unwrap()).unwrap();
        assert!(skins.served(false).contains("#123456"));
        assert!(skins.restore("../../pairing").is_err());
        skins.off().unwrap();
        assert_eq!(skins.served(false), wrap(""));
        assert!(skins.state()["active"].is_null());
        assert_eq!(skins.state()["versions"].as_array().unwrap().len(), 2);
        std::fs::write(root.join("skin/active.css"), "body { background: url(x) }").unwrap();
        assert_eq!(
            skins.served(false),
            wrap(""),
            "tampered file serves nothing"
        );
        skins.stage("body { color: red }").unwrap();
        skins.discard().unwrap();
        assert!(skins.state()["staged"].is_null());
        std::fs::remove_dir_all(root).unwrap();
    }
}
