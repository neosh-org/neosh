//! Getting a picture out of the machine and into a message.
//!
//! Three ways in, because there are three ways people actually have an image to hand and only one
//! of them is a keystroke:
//!
//! * **the clipboard** ([`from_clipboard`]), which is where a screenshot is. This is the one that
//!   needs a key, because a terminal's own paste can only ever hand over *text* — bracketed paste
//!   is a text protocol, and no amount of pressing `⌘V` will make a PNG arrive down stdin. Every
//!   agent that does this shells out to the platform's clipboard tool for exactly that reason, and
//!   so does this.
//! * **a path that was pasted** ([`from_path`]), which is what dragging a file onto a terminal
//!   window does: the terminal pastes its path. Typing one has the same effect, which is the point
//!   — it is not a separate gesture to learn.
//! * **a plugin**, through the same [`from_path`], because the API is the product.
//!
//! The clipboard is the one with two halves. A program that copies a picture chooses what to put
//! on the clipboard, and copying one off a web page very often puts no bytes there at all — a URL,
//! or the `<img>` tag that named it, and the picture itself is still on somebody's server. So
//! [`from_clipboard`] answers with a [`Clipped`]: bytes it has already kept, or a reference for
//! [`from_url`] to go and fetch off the loop.
//!
//! # What is done to the bytes
//!
//! Read, sniffed, shrunk if it is enormous, and written once into the workspace's own directory.
//!
//! Sniffed rather than trusted: the media type has to be right or the provider rejects the turn,
//! and a `.png` that is really a JPEG is a thing that exists. The bytes say what they are in their
//! first few, and that is what is believed.
//!
//! Shrunk because a screenshot of a 4K display is eight megapixels and every provider we target
//! scales it down on arrival anyway — so the only thing full resolution buys is a slower request
//! and, on a metered connection, a worse one. [`MAX_EDGE`] is the long edge past which nothing is
//! gained. An image already inside it is written through *untouched*, which matters: re-encoding a
//! PNG that did not need it is a way to make a screenshot of text blurrier for nothing.

use std::path::{Path, PathBuf};

/// The long edge past which no provider we target keeps the extra pixels.
const MAX_EDGE: u32 = 1568;

/// The most that will be pulled off a server for one picture. Not a picture budget — [`MAX_BYTES`]
/// is that, and it is applied after shrinking — but a stop on a link that turns out to point at a
/// disc image.
const MAX_FETCH: u64 = 25_000_000;

/// How long somebody else's server gets. Long enough for a slow one and short enough that a key
/// press has visibly finished.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// What a request will carry. Well under every provider's per-image ceiling once base64 has added
/// its third, and far past anything a screenshot produces — this is the guard against somebody
/// pasting a RAW file, not a budget.
const MAX_BYTES: usize = 3_500_000;

/// An image the composer is holding, or a message is carrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Where the bytes are, in the workspace's directory.
    pub path: PathBuf,
    /// What they are, read off the bytes.
    pub media_type: String,
    /// For the chip. What is worth knowing about an attachment you cannot see is how big it is.
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

impl Attachment {
    /// What the chip says: `png 1920×1080 · 412 KB`.
    pub fn label(&self) -> String {
        let kind = self.media_type.strip_prefix("image/").unwrap_or(&self.media_type);
        format!("{kind} {}\u{d7}{} \u{b7} {}", self.width, self.height, human(self.bytes))
    }

    pub fn block(&self) -> neosh_proto::ContentBlock {
        neosh_proto::ContentBlock::Image {
            path: self.path.display().to_string(),
            media_type: self.media_type.clone(),
        }
    }
}

fn human(bytes: usize) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b / (1 << 10)),
        b => format!("{b} B"),
    }
}

/// What the bytes are, from the bytes.
///
/// Only the four types every provider we target accepts. Anything else — a TIFF, a PDF, a text
/// file somebody dragged in — is not an image as far as this is concerned, and saying so here is
/// what stops it becoming a turn that fails on the wire with somebody else's error message.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.starts_with(PNG) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Take an image from a file and put a copy where the workspace keeps them.
///
/// A copy rather than a reference, and this is not paranoia: the path came from a paste, which
/// means it is very often `/tmp/Screenshot from 2026-08-21.png` or a file the next screenshot
/// overwrites. A conversation is reopened days later and replayed to a driver that reads the path
/// then — so what it reads has to still be the picture that was sent, and the only way to promise
/// that is to own the bytes.
pub fn from_path(store: &Path, path: &Path) -> Result<Attachment, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    keep(store, bytes)
}

/// What the clipboard turned out to be holding.
///
/// Two answers rather than one, because they cost different things. Bytes are here already and
/// become an attachment in the same breath. A *reference* — which is what a browser leaves when
/// you copy a picture off a page: a URL, or the `<img>` tag naming one — is a network round trip,
/// and the loop that draws the screen is not where a network round trip goes. So this says which
/// of the two it found and the caller decides where the waiting happens.
#[derive(Debug)]
pub enum Clipped {
    /// Bytes, already sniffed, shrunk and written down. Nothing left to do.
    Image(Attachment),
    /// A picture on somebody else's server. [`from_url`] is what finishes it.
    Remote(String),
}

/// Take whatever image the system clipboard is holding — or the one it is pointing at.
///
/// Nothing here knows how to talk to a clipboard, and that is on purpose. Every desktop already
/// ships a program whose whole job this is, all of them are present by default on the systems they
/// belong to, and shelling out to them is how the terminal agents that do this do it. A library
/// instead would mean linking X11 and Wayland into a program that spends its life on a pipe, and
/// would still be wrong on the machine where the workspace is headless.
///
/// Two things are looked for, in this order, because a clipboard holds a picture in two quite
/// different ways and only the first of them is bytes:
///
/// * **the picture itself**, under whatever type the program that copied it chose to offer — which
///   is why the offer is *read* rather than guessed. Asking for `image/png` and nothing else is
///   how a JPEG copied out of an image viewer, a WebP off a page and a BMP out of an X11
///   application all arrive as "nothing on the clipboard that is an image".
/// * **a picture named somewhere else**: a `file://` URI from a file manager, an `<img src>` in
///   the `text/html` a browser leaves beside the bytes, a `data:` URI, or a plain URL that ends in
///   `.png`. Copying an image on a web page is *usually* both — and on plenty of pages, and every
///   time somebody takes the address rather than the picture, it is only ever the second.
pub fn from_clipboard(store: &Path) -> Result<Clipped, String> {
    let mut trouble = Trouble::default();
    // Asked once and passed along: it is a subprocess, and the reference path wants the answer to
    // the same question the picture path just asked.
    let offer = asks_first().then(|| offer(&mut trouble)).flatten();
    if let Some(found) = clipboard_picture(offer.as_ref(), &mut trouble) {
        return match found {
            Picture::Bytes(bytes) => keep(store, bytes).map(Clipped::Image),
            Picture::File(path) => from_path(store, &path).map(Clipped::Image),
        };
    }
    for named in clipboard_references(offer.as_ref(), &mut trouble) {
        if let Some(resolved) = resolve(store, &named) {
            return resolved;
        }
    }
    Err(trouble.into_message())
}

/// What went wrong on the way, for a sentence at the end.
///
/// Three outcomes and they are three different problems: nothing installed to look with, something
/// that looked and could not see, and a clipboard that simply has no picture on it. Only the middle
/// one is ours to explain, and it is the one nobody can guess — a workspace outlives the terminal
/// that started it, so a `wl-paste` here can perfectly well be one with no compositor to ask.
#[derive(Default)]
struct Trouble {
    /// Whether any program that could have answered was there to ask.
    asked: bool,
    /// What one of them said when it could not look at all.
    said: Vec<String>,
    /// Whether one of them was *refused* rather than merely unable to answer, which is the one
    /// case with something a person can go and do about it.
    denied: bool,
}

impl Trouble {
    fn into_message(self) -> String {
        if !self.asked {
            return format!(
                "no clipboard tool to read an image with \u{2014} install one of {}",
                reader_names().join(", ")
            );
        }
        if !self.said.is_empty() {
            let mut message = format!("could not read the clipboard: {}", self.said.join("; "));
            // Only on the platform where the refusal is a grant somebody can give. Elsewhere the
            // program's own complaint is the whole of what is known, and inventing a fix for it
            // would be advice pointing at a settings pane that does not exist.
            if self.denied && cfg!(target_os = "macos") {
                message.push_str(&format!(
                    " \u{2014} macOS refused {}; allow it when the prompt appears, or under \
                     System Settings \u{203a} Privacy & Security",
                    crate::access::terminal()
                ));
            }
            return message;
        }
        "nothing on the clipboard that is an image".to_string()
    }
}

/// A picture that has been found but not yet kept.
enum Picture {
    Bytes(Vec<u8>),
    File(PathBuf),
}

/// Whether this desktop's clipboard can be *asked what it holds* before being asked for it.
///
/// Wayland and X11 can, and it is the whole difference between this working and not. macOS and
/// Windows cannot cheaply, so there the programs are tried in turn and the first that answers wins.
fn asks_first() -> bool {
    !cfg!(target_os = "macos") && !cfg!(target_os = "windows")
}

/// What a Wayland or X11 clipboard says it is holding, and the program that answered.
struct Offer {
    reader: &'static str,
    /// Exactly as offered, case and parameters and all: `image/x-MS-bmp` is a real target atom and
    /// a request for `image/x-ms-bmp` gets nothing.
    types: Vec<String>,
}

impl Offer {
    /// The offered type matching `mime`, or nothing.
    fn holds(&self, mime: &str) -> Option<&str> {
        self.types
            .iter()
            .find(|t| t.split(';').next().unwrap_or(t).trim().eq_ignore_ascii_case(mime))
            .map(String::as_str)
    }

    /// The best picture type on offer. Best rather than first: a program that offers three has
    /// ranked nothing, and PNG before JPEG before the rest is our ranking to make.
    fn picture(&self) -> Option<&str> {
        DECODABLE.iter().find_map(|mime| self.holds(mime))
    }

    fn read(&self, mime: &str) -> Option<Vec<u8>> {
        let args: Vec<&str> = match self.reader {
            "wl-paste" => vec!["--no-newline", "--type", mime],
            _ => vec!["-selection", "clipboard", "-t", mime, "-o"],
        };
        let out = std::process::Command::new(self.reader)
            .args(&args)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
    }
}

/// The picture types worth asking for, best first.
///
/// Every one of them is a type the decoder here can actually read — offering to take a thing we
/// cannot open would turn "no picture" into "a picture that failed", which is worse. `image/svg+xml`
/// is the notable absence: it is a document, and the `<img src>` beside it on the clipboard is
/// usually the real picture.
const DECODABLE: &[&str] = &[
    "image/png",
    "image/webp",
    "image/jpeg",
    "image/jpg",
    "image/gif",
    "image/bmp",
    "image/x-bmp",
    "image/x-ms-bmp",
];

/// Ask the clipboard what it has, with the first program that can answer.
fn offer(trouble: &mut Trouble) -> Option<Offer> {
    // Wayland before X11 because a Wayland session usually has `xclip` too, through XWayland,
    // looking at a clipboard that is not the one you copied into.
    let asking: [(&'static str, &[&str]); 2] = [
        ("wl-paste", &["--list-types"]),
        ("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]),
    ];
    for (reader, args) in asking {
        if which(reader).is_none() {
            continue;
        }
        trouble.asked = true;
        let Ok(out) = std::process::Command::new(reader).args(args).output() else { continue };
        if !out.status.success() {
            // The one place stderr is repeated. Everything after this point fails quietly and
            // ordinarily — a type that is not on offer is not an error — but a clipboard that
            // cannot be reached at all fails *here*, and the reason is the answer.
            let said = String::from_utf8_lossy(&out.stderr);
            if let Some(line) = said.lines().map(str::trim).find(|l| !l.is_empty())
                && !just_empty(line)
            {
                trouble.said.push(format!("{reader}: {line}"));
            }
            continue;
        }
        let types = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        return Some(Offer { reader, types });
    }
    None
}

/// Whether what a clipboard program complained about is only that there is nothing on it.
///
/// An empty clipboard is not a problem with the clipboard: `wl-paste` reports it by failing, and
/// repeating that back turns the ordinary answer — you pressed the key and there was no picture —
/// into a sentence that reads like something is broken.
fn just_empty(said: &str) -> bool {
    let said = said.to_ascii_lowercase();
    said.contains("nothing is copied") || said.contains("not available")
}

/// The line of a clipboard program's complaint that means *you were not allowed*, or nothing.
///
/// Almost everything these programs print on the way to failing is ordinary and must stay
/// invisible: `osascript` asked for a file URL on a pasteboard holding text fails with a coercion
/// error (`-1700`) every single time the clipboard has no file on it, and repeating that to
/// somebody who pressed `^V` over a screenshot would be noise generated by the common case.
///
/// A refusal is the exception, because it is the one outcome with something to do about it and the
/// one nobody can guess: the pasteboard is readable, the picture is on it, and the answer is a
/// grant that belongs to the terminal. `-1743` is the AppleScript code for it.
fn denied_line(said: &str) -> Option<String> {
    let is_refusal = |l: &str| {
        let l = l.to_ascii_lowercase();
        l.contains("not authorized")
            || l.contains("not permitted")
            || l.contains("not allowed")
            || l.contains("permission denied")
            || l.contains("-1743")
    };
    said.lines().map(str::trim).find(|l| !l.is_empty() && is_refusal(l)).map(str::to_string)
}

/// The picture on the clipboard, as bytes or as the file it turned out to name.
fn clipboard_picture(offer: Option<&Offer>, trouble: &mut Trouble) -> Option<Picture> {
    if let Some(offer) = offer {
        let mime = offer.picture()?;
        return offer.read(mime).map(Picture::Bytes);
    }
    if asks_first() {
        // There was nothing to ask with; `trouble` already knows.
        return None;
    }
    for (program, args) in clipboard_readers() {
        if which(program).is_none() {
            continue;
        }
        trouble.asked = true;
        // stderr is kept rather than dropped, and then almost all of it is thrown away again by
        // `denied_line`. The one thing worth rescuing is a refusal: macOS will not let a terminal
        // it has no grant for read another application's pasteboard, and discarding that turned it
        // into "nothing on the clipboard that is an image" — the one sentence guaranteed to send
        // somebody back to copy the picture again.
        let out = std::process::Command::new(program).args(args).output();
        let Ok(out) = out else { continue };
        if !out.status.success() || out.stdout.is_empty() {
            if let Some(line) = denied_line(&String::from_utf8_lossy(&out.stderr)) {
                trouble.said.push(format!("{program}: {line}"));
                trouble.denied = true;
            }
            continue;
        }
        // Some of these print a path rather than the picture — `osascript` asked for a file URL,
        // and again after it has written the pasteboard's PNG somewhere; `powershell` likewise.
        // One line that names a file that exists is a path; anything else is the bytes.
        if let Some(p) = as_path_line(&out.stdout) {
            return Some(Picture::File(p));
        }
        if sniff(&out.stdout).is_some() {
            return Some(Picture::Bytes(out.stdout));
        }
    }
    None
}

/// Everything on the clipboard that names a picture, in the order we trust it.
///
/// A list rather than one answer: a browser puts several things on at once, and the first of them
/// to look like a picture is not always the one that is. Each is tried in turn.
fn clipboard_references(offer: Option<&Offer>, trouble: &mut Trouble) -> Vec<String> {
    // Not lossless and not meant to be — this is text about a picture, and a byte that is not
    // UTF-8 in it is a byte in something that was never going to be a URL.
    let mut flavours: Vec<(&'static str, String)> = Vec::new();
    match offer {
        Some(offer) => {
            for kind in ["text/uri-list", "text/html", "text/plain"] {
                if let Some(mime) = offer.holds(kind)
                    && let Some(bytes) = offer.read(mime)
                {
                    flavours.push((kind, String::from_utf8_lossy(&bytes).into_owned()));
                }
            }
        }
        None => {
            for (program, args, kind) in text_readers() {
                if which(program).is_none() {
                    continue;
                }
                trouble.asked = true;
                let out = std::process::Command::new(program)
                    .args(args)
                    .stderr(std::process::Stdio::null())
                    .output();
                let Ok(out) = out else { continue };
                if !out.status.success() || out.stdout.is_empty() {
                    continue;
                }
                flavours.push((kind, String::from_utf8_lossy(&out.stdout).into_owned()));
            }
        }
    }
    flavours.iter().flat_map(|(kind, text)| named_in(kind, text)).collect()
}

/// What one flavour of clipboard text says about where a picture is.
fn named_in(kind: &str, text: &str) -> Vec<String> {
    match kind {
        // A file manager, or a drag that got as far as the clipboard. Comments are part of the
        // format and are not paths.
        "text/uri-list" => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect(),
        "text/html" => img_srcs(text),
        _ => plain_named(text).into_iter().collect(),
    }
}

/// A line of plain text read as "the picture is over there", or nothing.
///
/// Strict, and deliberately stricter than the HTML case: an `<img>` tag has already said the thing
/// it points at is a picture, whereas a line of text is usually a line of text. So a URL here has
/// to *end* in a picture's extension before a key press will go and fetch it — a clipboard holding
/// an ordinary link is not a reason to make a request to it.
fn plain_named(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.lines().count() != 1 {
        return None;
    }
    if text.starts_with("data:") {
        return Some(text.to_string());
    }
    // A path, copied as text. `pasted_path` is the same four gates the paste path uses.
    if pasted_path(text).is_some() {
        return Some(text.to_string());
    }
    let url = url::Url::parse(text).ok()?;
    match url.scheme() {
        "file" => Some(text.to_string()),
        "http" | "https" => named_like_a_picture(&url).then(|| text.to_string()),
        _ => None,
    }
}

/// Whether a URL's own path ends in something only a picture is called.
fn named_like_a_picture(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"].iter().any(|ext| path.ends_with(ext))
}

/// Every `src` in the `<img>` tags of a fragment of clipboard HTML, in the order they appear.
///
/// A parser would be a dependency and a worse fit: this is not a document, it is the one tag a
/// browser wrote out when you pressed copy, and what is wanted from it is one attribute. Relative
/// srcs are dropped rather than guessed at — the fragment carries no base to resolve them against,
/// and a wrong URL is a request to somebody who did not ask for one.
fn img_srcs(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find("<img") {
        let start = from + at;
        let end = lower[start..].find('>').map(|e| start + e).unwrap_or(lower.len());
        if let Some(src) = attribute(&html[start..end], "src")
            && (src.starts_with("http://")
                || src.starts_with("https://")
                || src.starts_with("data:"))
        {
            found.push(src);
        }
        from = end.max(start + 4);
    }
    found
}

/// One attribute out of one tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(name) {
        let at = from + at;
        from = at + name.len();
        // `srcset` and `data-src` are not `src`: the name has to start and end where it looks
        // like it does.
        let before_is_space = tag[..at].chars().last().is_none_or(char::is_whitespace);
        let rest = tag[from..].trim_start();
        if !before_is_space || !rest.starts_with('=') {
            continue;
        }
        let rest = rest[1..].trim_start();
        let (quote, rest) = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => (Some(q), &rest[1..]),
            _ => (None, rest),
        };
        let end = match quote {
            Some(q) => rest.find(q)?,
            None => rest.find(char::is_whitespace).unwrap_or(rest.len()),
        };
        return Some(unentity(&rest[..end]));
    }
    None
}

/// The five entities that turn up in an attribute somebody meant as a URL.
fn unentity(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// A thing the clipboard named, turned into an attachment or a reason to go and get one.
///
/// `None` is "that was not a picture" — the next candidate gets its turn, and running out of them
/// is what the sentence at the end of [`from_clipboard`] is for.
fn resolve(store: &Path, named: &str) -> Option<Result<Clipped, String>> {
    if let Some(rest) = named.strip_prefix("data:") {
        return Some(data_url(store, rest));
    }
    if let Some(path) = pasted_path(named) {
        return Some(from_path(store, &path).map(Clipped::Image));
    }
    let url = url::Url::parse(named).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| Ok(Clipped::Remote(String::from(url))))
}

/// `data:image/png;base64,iVBOR…` — a picture that is *in* the clipboard rather than named by it.
///
/// Small images on a page are often only this, so a `data:` URI is the difference between copying
/// an icon working and not. No network is involved and the bytes still have to earn their type
/// from [`sniff`]: the `image/png` in the URL is what somebody wrote, not what they encoded.
fn data_url(store: &Path, rest: &str) -> Result<Clipped, String> {
    let (meta, payload) = rest.split_once(',').ok_or("that data: URL has no data in it")?;
    if !meta.split(';').any(|p| p.eq_ignore_ascii_case("base64")) {
        return Err("that data: URL is not base64, so it is not a picture".to_string());
    }
    // Whitespace is how a long one survives being wrapped, and is not part of the payload.
    let packed: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, packed)
        .map_err(|e| format!("could not read that data: URL: {e}"))?;
    keep(store, bytes).map(Clipped::Image)
}

/// Fetch a picture that is on somebody else's server.
///
/// The other half of [`Clipped::Remote`], and it is deliberately not called by [`from_clipboard`]:
/// this waits on a machine we have never met, for as long as that machine feels like taking, and
/// the host loop is the single writer for the editor.
///
/// What comes back is trusted for its length and nothing else — the bytes are sniffed like every
/// other picture here, because a URL ending in `.png` is a filename and filenames lie.
pub async fn from_url(store: &Path, url: &str) -> Result<Attachment, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("{}: not a thing to fetch", parsed.scheme()));
    }
    let host = parsed.host_str().unwrap_or(url).to_string();
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("neosh/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("could not fetch that: {e}"))?;
    let response =
        client.get(parsed).send().await.map_err(|e| format!("could not reach {host}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        // Said as the server's own answer, because that is what it is. A 403 here is almost always
        // a site declining to serve a picture to anything that is not its page, which is a thing it
        // is entitled to do and nothing on this side can put right.
        return Err(format!("{host} said {status}"));
    }
    // The header when there is one, and the running total when there is not: a server is not
    // obliged to say how big a thing is, and "download it all and then check" is what a cap is for.
    if response.content_length().is_some_and(|len| len > MAX_FETCH) {
        return Err(format!(
            "{host} is offering more than {} of picture",
            human(MAX_FETCH as usize)
        ));
    }
    let mut response = response;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) =
        response.chunk().await.map_err(|e| format!("{host} stopped sending: {e}"))?
    {
        if bytes.len() + chunk.len() > MAX_FETCH as usize {
            return Err(format!("that is more than {} of picture", human(MAX_FETCH as usize)));
        }
        bytes.extend_from_slice(&chunk);
    }
    keep(store, bytes)
}

/// The programs that can hand over a clipboard image where it cannot be asked what it holds.
///
/// Linux is not here: [`offer`] is what answers there, and asking for `image/png` and nothing else
/// is the bug this list used to have.
fn clipboard_readers() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "macos") {
        return vec![
            ("pngpaste", vec!["-"]),
            // Always present, so these are the floor rather than an extra to install. The first
            // asks for a *file* URL: an image copied in Finder is a file reference, and this is
            // the only one of the three that can see it. The second is the one that matters for a
            // picture copied off a page — the pasteboard has the bytes, AppleScript can only hand
            // over text, so they go through a file.
            ("osascript", vec!["-e", MAC_FILE]),
            ("osascript", vec!["-e", MAC_PNG]),
        ];
    }
    if cfg!(target_os = "windows") {
        return vec![("powershell", vec!["-NoProfile", "-Command", WINDOWS_DUMP])];
    }
    Vec::new()
}

/// The programs that can hand over clipboard *text*, where it cannot be asked what it holds.
fn text_readers() -> Vec<(&'static str, Vec<&'static str>, &'static str)> {
    if cfg!(target_os = "macos") {
        return vec![("pbpaste", Vec::new(), "text/plain")];
    }
    if cfg!(target_os = "windows") {
        return vec![(
            "powershell",
            vec!["-NoProfile", "-Command", "Get-Clipboard -Raw"],
            "text/plain",
        )];
    }
    Vec::new()
}

/// Everything that could have looked, for the sentence that says nothing could.
fn reader_names() -> Vec<&'static str> {
    if asks_first() {
        return vec!["wl-paste", "xclip"];
    }
    clipboard_readers().iter().map(|(p, _)| *p).collect()
}

/// The path of whatever file is on the pasteboard — a copy made in Finder.
const MAC_FILE: &str = "get POSIX path of (the clipboard as \u{ab}class furl\u{bb})";

/// The pasteboard's picture, written to a file and its path printed. Nothing there exits quietly,
/// which is how the caller tells the two apart.
const MAC_PNG: &str = "set p to (POSIX path of (path to temporary items)) & \"neosh-clipboard.png\"\n\
try\n\
  set d to (the clipboard as \u{ab}class PNGf\u{bb})\n\
on error\n\
  return \"\"\n\
end try\n\
set f to open for access (POSIX file p) with write permission\n\
set eof f to 0\n\
write d to f\n\
close access f\n\
return p";

/// Save the clipboard image to a temporary PNG and print where it went. Nothing on the clipboard
/// exits non-zero, which is how the caller tells the two apart.
const WINDOWS_DUMP: &str = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
$img = Get-Clipboard -Format Image; if ($img -eq $null) { exit 1 }; \
$p = [System.IO.Path]::ChangeExtension([System.IO.Path]::GetTempFileName(), 'png'); \
$img.Save($p, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output $p";

/// A program's output read as "this is where the file is", or nothing.
fn as_path_line(out: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(out).ok()?.trim();
    if text.is_empty() || text.lines().count() != 1 {
        return None;
    }
    let path = PathBuf::from(text);
    path.is_file().then_some(path)
}

/// A single pasted line read as a path to an image, or nothing.
///
/// This is what makes dragging a file onto the terminal work, so it has to be *quiet*: anything it
/// is unsure about is ordinary text, because inserting what was typed is the behaviour somebody is
/// expecting and swallowing it is not. One line, one path, a file that exists, and bytes that are
/// an image — four gates, and a sentence with a space in it does not get past the second.
pub fn pasted_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    // Quoted or `file://`-wrapped, which is what a drag produces on most desktops.
    let bare = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| text.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(text);
    let bare = match bare.strip_prefix("file://") {
        // `file:///tmp/a.png` — the authority is empty for a local file, and percent-escapes are
        // how a space survives the trip.
        Some(rest) => unescape(rest.strip_prefix("localhost").unwrap_or(rest)),
        // A drag also escapes spaces the shell's way, which is what makes the path a single word.
        None => bare.replace("\\ ", " "),
    };
    let path = PathBuf::from(shellexpand::tilde(&bare).into_owned());
    if !path.is_file() {
        return None;
    }
    // The extension is a hint about whether it is worth reading, not the answer. The answer is in
    // the bytes, and it is `keep` that asks them.
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    matches!(ext.as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp")).then_some(path)
}

/// `%20` and friends, for a path that came through a `file://` URL.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let hex: String = chars.clone().take(2).collect();
        match u8::from_str_radix(&hex, 16) {
            Ok(b) => {
                out.push(b as char);
                chars.next();
                chars.next();
            }
            Err(_) => out.push(c),
        }
    }
    out
}

/// Sniff, shrink if it is worth shrinking, and write it where it will still be in a week.
fn keep(store: &Path, bytes: Vec<u8>) -> Result<Attachment, String> {
    // What a provider will take, if anything. A `None` here is not the end of it: a BMP off an X11
    // clipboard is a picture nobody can send and everybody can open, and re-encoding it is a line
    // of work rather than a refusal.
    let sendable = sniff(&bytes);
    let decoded = image::load_from_memory(&bytes).map_err(|e| match sendable {
        Some(kind) => {
            format!("could not read the {}: {e}", kind.strip_prefix("image/").unwrap_or(kind))
        }
        None => "that is not an image of a kind any model here can read".to_string(),
    })?;
    let (w, h) = image::GenericImageView::dimensions(&decoded);

    // Left alone when it is already something to send and already small enough. Re-encoding costs
    // sharpness on exactly the kind of image people paste — a screenshot of text — and buys
    // nothing.
    let (bytes, media_type, w, h) = match sendable {
        Some(kind) if w <= MAX_EDGE && h <= MAX_EDGE && bytes.len() <= MAX_BYTES => {
            (bytes, kind.to_string(), w, h)
        }
        _ => shrunk(&decoded)?,
    };
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "that image is {} \u{2014} more than a turn can carry",
            human(bytes.len())
        ));
    }

    std::fs::create_dir_all(store)
        .map_err(|e| format!("could not make {}: {e}", store.display()))?;
    let ext = media_type.strip_prefix("image/").unwrap_or("png");
    let path = store.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
    let len = bytes.len();
    std::fs::write(&path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(Attachment { path, media_type, width: w, height: h, bytes: len })
}

/// Keep bytes that arrived some other way — another machine's paste, sent over the swarm — exactly
/// as a picture read off a file here is kept.
pub fn from_bytes(store: &Path, bytes: Vec<u8>) -> Result<Attachment, String> {
    keep(store, bytes)
}

/// What the bytes say they are, when they are something a provider takes.
pub fn media_type_of(bytes: &[u8]) -> Option<&'static str> {
    sniff(bytes)
}

/// Bring a picture down to something worth sending: inside [`MAX_EDGE`], inside [`MAX_BYTES`], and
/// in a format a provider accepts.
///
/// PNG first, because most of what is pasted is a screenshot and a screenshot of text is where
/// every JPEG artefact lands on a letter. JPEG when PNG will not fit, because a photograph is a
/// photograph: 1568 square of one is several megabytes of PNG and a few hundred kilobytes of JPEG,
/// and refusing a picture somebody just copied over the encoding it happened to arrive in is the
/// worse of the two answers.
fn shrunk(source: &image::DynamicImage) -> Result<(Vec<u8>, String, u32, u32), String> {
    let (w, h) = image::GenericImageView::dimensions(source);
    // `resize` fits *within* the bounds either way, so an image already inside them would be
    // scaled up by it. Nothing here ever adds pixels.
    let small = match w > MAX_EDGE || h > MAX_EDGE {
        true => source.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3),
        false => source.clone(),
    };
    let (w, h) = image::GenericImageView::dimensions(&small);
    let mut png = Vec::new();
    small
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("could not re-encode the image: {e}"))?;
    if png.len() <= MAX_BYTES {
        return Ok((png, "image/png".to_string(), w, h));
    }
    // Through RGB explicitly: JPEG has no alpha, and the encoder refuses rather than flattening.
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgb8(small.to_rgb8())
        .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .map_err(|e| format!("could not re-encode the image: {e}"))?;
    Ok((jpeg, "image/jpeg".to_string(), w, h))
}

/// Where a program is on `PATH`, or nothing.
fn which(program: &str) -> Option<PathBuf> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    std::env::var_os("PATH")?.to_str()?.split(sep).find_map(|dir| {
        let candidate = Path::new(dir).join(program);
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_rescued_out_of_stderr_and_the_everyday_complaint_is_left_in_it() {
        // Both of these are real output. The coercion error is what `osascript` prints *every
        // single time* the pasteboard holds text and it was asked for a picture — which is the
        // ordinary way pressing `^V` with no image copied goes — so repeating it to somebody would
        // be an alarming sentence generated by nothing at all going wrong.
        assert_eq!(
            denied_line(
                "0:29: execution error: Can\u{2019}t make some data into the expected type. (-1700)"
            ),
            None
        );
        // A refusal is the exception worth surfacing: the pasteboard is readable, the picture is
        // on it, and the answer is a grant that belongs to the terminal rather than to neosh.
        assert!(
            denied_line("execution error: Not authorized to send Apple events. (-1743)").is_some()
        );
        assert!(denied_line("xclip: Error: Permission denied").is_some());
        // Nothing said is nothing to report, rather than an empty complaint.
        assert_eq!(denied_line(""), None);
        assert_eq!(denied_line("\n  \n"), None);
    }

    /// A one-pixel PNG, so the tests exercise the real decoder rather than a shape.
    fn pixel() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encode");
        out
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("neosh-img-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn what_it_is_is_read_off_the_bytes_and_not_off_the_name() {
        assert_eq!(sniff(&pixel()), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0x00]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        // The one that matters: a provider told this was a PNG would reject the turn.
        assert_eq!(sniff(b"%PDF-1.4"), None, "a PDF is not one of the four");
        assert_eq!(sniff(b"hello"), None);
    }

    #[test]
    fn an_image_small_enough_already_is_written_through_untouched() {
        let dir = tmp("through");
        let bytes = pixel();
        let got = keep(&dir, bytes.clone()).expect("kept");
        assert_eq!(got.media_type, "image/png");
        assert_eq!((got.width, got.height), (1, 1));
        assert_eq!(
            std::fs::read(&got.path).expect("read back"),
            bytes,
            "re-encoding a screenshot of text is how it gets blurrier for nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn something_that_is_not_an_image_is_refused_here_rather_than_on_the_wire() {
        let dir = tmp("refuse");
        let err = keep(&dir, b"#!/bin/sh\necho hi\n".to_vec()).expect_err("refused");
        assert!(err.contains("not an image"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_huge_one_is_brought_down_to_a_size_worth_sending() {
        let dir = tmp("shrink");
        let big = image::RgbaImage::from_pixel(4000, 2000, image::Rgba([9, 9, 9, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(big)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode");
        let got = keep(&dir, bytes).expect("kept");
        assert_eq!(got.width, MAX_EDGE, "the long edge is what is clamped");
        assert_eq!(got.height, MAX_EDGE / 2, "and the aspect ratio survives it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pasted_path_is_only_a_path_when_it_is_unambiguously_one() {
        let dir = tmp("paste");
        let file = dir.join("shot.png");
        std::fs::write(&file, pixel()).expect("write");

        assert_eq!(pasted_path(file.to_str().expect("utf8")).as_ref(), Some(&file));
        // The three shapes a drag produces.
        assert_eq!(pasted_path(&format!("\"{}\"", file.display())).as_ref(), Some(&file));
        assert_eq!(pasted_path(&format!("'{}'", file.display())).as_ref(), Some(&file));
        assert_eq!(pasted_path(&format!("file://{}", file.display())).as_ref(), Some(&file));

        // And the things that are sentences. Every one of these would be text eaten by a gesture
        // nobody made.
        assert_eq!(pasted_path("have a look at shot.png"), None, "a sentence is not a path");
        assert_eq!(pasted_path(&format!("{} and this", file.display())), None);
        assert_eq!(pasted_path("/nope/missing.png"), None, "a path to nothing is text");
        assert_eq!(pasted_path(""), None);

        // A file that exists but is not one of the four we can send.
        let notes = dir.join("notes.txt");
        std::fs::write(&notes, "hello").expect("write");
        assert_eq!(pasted_path(notes.to_str().expect("utf8")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_space_survives_both_ways_a_drag_can_escape_it() {
        let dir = tmp("space");
        let file = dir.join("my shot.png");
        std::fs::write(&file, pixel()).expect("write");
        assert_eq!(
            pasted_path(&format!("file://{}", file.display().to_string().replace(' ', "%20")))
                .as_ref(),
            Some(&file)
        );
        assert_eq!(
            pasted_path(&file.display().to_string().replace(' ', "\\ ")).as_ref(),
            Some(&file)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_type_asked_for_is_one_the_clipboard_said_it_had() {
        // A JPEG copied out of an image viewer. Asking for `image/png` and nothing else is how
        // this used to come back as "nothing on the clipboard that is an image".
        let offer = Offer {
            reader: "wl-paste",
            types: vec!["text/html".into(), "image/jpeg".into(), "text/plain".into()],
        };
        assert_eq!(offer.picture(), Some("image/jpeg"));

        // Several on offer is a program that has ranked nothing, so the ranking is ours: PNG over
        // JPEG, because it is the one that has not already thrown pixels away.
        let browser = Offer {
            reader: "wl-paste",
            types: vec!["image/jpeg".into(), "image/png".into(), "text/html".into()],
        };
        assert_eq!(browser.picture(), Some("image/png"));

        // Exactly as offered, capitals and parameters and all — a request for `image/x-ms-bmp`
        // against an X11 clipboard offering `image/x-MS-bmp` gets nothing back.
        let x11 = Offer { reader: "xclip", types: vec!["TARGETS".into(), "image/x-MS-bmp".into()] };
        assert_eq!(x11.picture(), Some("image/x-MS-bmp"));
        let with_params =
            Offer { reader: "wl-paste", types: vec!["image/png;charset=binary".into()] };
        assert_eq!(with_params.picture(), Some("image/png;charset=binary"));

        // Nothing here can open an SVG, and offering to take one turns "no picture" into "a
        // picture that failed".
        let vector = Offer { reader: "wl-paste", types: vec!["image/svg+xml".into()] };
        assert_eq!(vector.picture(), None);
    }

    #[test]
    fn a_page_that_only_named_a_picture_still_gives_one_up() {
        // What a browser leaves on the clipboard when you copy an image off a page.
        let html = "<meta charset=\"utf-8\"><img src=\"https://example.com/a%20cat.png?w=800&amp;h=600\" alt=\"a cat\">";
        assert_eq!(
            img_srcs(html),
            vec!["https://example.com/a%20cat.png?w=800&h=600".to_string()],
            "the entity is part of the markup, not of the URL"
        );

        // Relative is dropped rather than guessed at: the fragment carries no base, and a wrong
        // URL is a request to somebody who did not ask for one.
        assert!(img_srcs("<img src=\"/img/logo.png\">").is_empty());

        // `srcset` and `data-src` are not `src`.
        assert!(
            img_srcs("<img srcset=\"https://example.com/2x.png 2x\" data-src=\"https://example.com/lazy.png\">")
                .is_empty()
        );
        assert_eq!(
            img_srcs(
                "<img srcset=\"https://example.com/2x.png 2x\" src=\"https://example.com/1x.png\">"
            ),
            vec!["https://example.com/1x.png".to_string()]
        );

        // Unquoted, and more than one.
        assert_eq!(
            img_srcs(
                "<img src=https://example.com/one.png><p>and</p><img src='https://example.com/two.png'>"
            ),
            vec![
                "https://example.com/one.png".to_string(),
                "https://example.com/two.png".to_string()
            ]
        );
    }

    #[test]
    fn a_line_of_text_is_only_a_picture_when_it_is_unmistakably_one() {
        assert_eq!(
            plain_named("https://example.com/cat.png").as_deref(),
            Some("https://example.com/cat.png")
        );
        assert_eq!(
            plain_named("  https://example.com/deep/path/cat.JPEG  ").as_deref(),
            Some("https://example.com/deep/path/cat.JPEG"),
            "an extension is a name, and names are not case"
        );
        // An ordinary link is not a reason to make a request to it.
        assert_eq!(plain_named("https://example.com/an/article"), None);
        assert_eq!(plain_named("have a look at https://example.com/cat.png"), None);
        assert_eq!(plain_named("a sentence"), None);
        assert_eq!(plain_named(""), None);
    }

    #[test]
    fn what_a_browser_leaves_behind_is_read_in_the_order_we_trust_it() {
        let dir = tmp("uri-list");
        let file = dir.join("shot.png");
        std::fs::write(&file, pixel()).expect("write");

        // A file manager's copy.
        assert_eq!(
            named_in("text/uri-list", &format!("# a comment\nfile://{}\n", file.display())),
            vec![format!("file://{}", file.display())]
        );
        // And it comes back as the file it names.
        let named = format!("file://{}", file.display());
        assert!(matches!(resolve(&dir, &named), Some(Ok(Clipped::Image(_)))));

        // A page's picture is somewhere else, and saying so is not fetching it.
        match resolve(&dir, "https://example.com/cat.png") {
            Some(Ok(Clipped::Remote(url))) => assert_eq!(url, "https://example.com/cat.png"),
            other => panic!("{other:?} is not a picture on somebody's server"),
        }
        // Things that name nothing get out of the way of the next candidate.
        assert!(resolve(&dir, "just some text").is_none());
        assert!(resolve(&dir, "mailto:someone@example.com").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_data_url_is_a_picture_that_needs_no_network() {
        let dir = tmp("data-url");
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pixel());
        // Wrapped, which is how a long one survives being put on a clipboard.
        let wrapped = format!("image/png;base64,{}\n{}", &encoded[..8], &encoded[8..]);
        match data_url(&dir, &wrapped) {
            Ok(Clipped::Image(got)) => {
                assert_eq!(got.media_type, "image/png");
                assert_eq!((got.width, got.height), (1, 1));
            }
            other => panic!("{other:?}"),
        }
        // The type in the URL is what somebody wrote; the bytes are what they encoded.
        let lying = format!(
            "image/png;base64,{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"#!/bin/sh\n")
        );
        assert!(data_url(&dir, &lying).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_picture_too_busy_to_be_a_png_is_sent_as_a_jpeg() {
        let dir = tmp("jpeg");
        // A photograph's worth of detail, at a size nothing will shrink: the PNG of it is over
        // what a turn can carry, and refusing it over the encoding it arrived in is the wrong
        // answer twice — it is a picture somebody just copied.
        let mut noise = image::RgbImage::new(MAX_EDGE, MAX_EDGE);
        let mut seed: u32 = 0x1234_5678;
        for pixel in noise.pixels_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let b = seed.to_le_bytes();
            *pixel = image::Rgb([b[0], b[1], b[2]]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(noise)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode");
        assert!(bytes.len() > MAX_BYTES, "the test's own premise: {} bytes", bytes.len());

        let got = keep(&dir, bytes).expect("kept");
        assert_eq!(got.media_type, "image/jpeg");
        assert_eq!((got.width, got.height), (MAX_EDGE, MAX_EDGE), "nothing was thrown away to fit");
        assert!(got.bytes <= MAX_BYTES, "{} bytes is still more than a turn can carry", got.bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn something_openable_that_nothing_can_be_sent_is_re_encoded_rather_than_refused() {
        let dir = tmp("bmp");
        // What an X11 application puts on the clipboard. No provider takes a BMP and every
        // decoder opens one.
        let mut bmp = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([7, 8, 9, 255]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
        .expect("encode");
        assert_eq!(sniff(&bmp), None, "the test's own premise");

        let got = keep(&dir, bmp).expect("kept");
        assert_eq!(got.media_type, "image/png");
        assert_eq!((got.width, got.height), (2, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_chip_says_the_things_you_cannot_see_for_yourself() {
        let a = Attachment {
            path: PathBuf::from("/x/y.png"),
            media_type: "image/png".into(),
            width: 1920,
            height: 1080,
            bytes: 421_888,
        };
        assert_eq!(a.label(), "png 1920\u{d7}1080 \u{b7} 412 KB");
    }
}
