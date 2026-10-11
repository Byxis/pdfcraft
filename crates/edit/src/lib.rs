//! pdfcraft-edit — page content editing (L4). Today: page marks, Acrobat's Header & footer,
//! Watermark, Background and Bates numbering (execution plan M7.6).
//!
//! **How marks are stored.** Each mark is one content stream appended to (or, for content behind
//! the page, prepended to) the page's `/Contents`, drawn in display space (after `/Rotate`) and
//! wrapped in a marked-content artifact (§14.8.2.2) so readers and screen readers skip it:
//! `/Artifact <</Type /Pagination /Subtype /Header /PCMark /HeaderFooter>> BDC … EMC`. The
//! `/PCMark` entry (also in the stream dictionary) names the kind of mark, which is how Update and
//! Remove find them again without decompressing page content. When
//! anything is drawn on top, the page's original content is wrapped in `q … Q` (two tiny streams)
//! so its graphics state can't leak into the mark.
//!
//! Text uses standard Helvetica (WinAnsi) added to the page resources as `/PCHelv`.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use pdfcraft_cos::{Dict, Document, Object, Stream};
use pdfcraft_fonts::{helvetica_width, literal, win_ansi};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EditError {
    /// Holds the 0-based index; the message names the 1-based page a user counts (#1550).
    #[error("page {} does not exist", .0.saturating_add(1))]
    NoSuchPage(usize),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Cos(#[from] pdfcraft_cos::CosError),
}

/// The kinds of page marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    HeaderFooter,
    Watermark,
    Background,
}

impl MarkKind {
    fn tag(self) -> &'static str {
        match self {
            MarkKind::HeaderFooter => "HeaderFooter",
            MarkKind::Watermark => "Watermark",
            MarkKind::Background => "Background",
        }
    }
}

pub type Rgb = [f64; 3];

/// Header and footer settings (Acrobat's Add Header and Footer dialog).
#[derive(Clone, Debug, PartialEq)]
pub struct HeaderFooter {
    /// Left, centre and right header text, then left, centre and right footer text. Tokens:
    /// `<<1>>` page number, `<<n>>` page count, `<<1 of n>>`, `<<1/n>>`, `<<Page 1>>`,
    /// `<<Page 1 of n>>`, dates `<<m/d/yyyy>>`, `<<mm/dd/yyyy>>`, `<<d/m/yyyy>>`, `<<yyyy-mm-dd>>`,
    /// `<<m/d/yy>>`, `<<mmmm d, yyyy>>`, and Bates numbers `<<Bates Number#6#1#PRE#SUF>>`
    /// (digits, start, prefix, suffix).
    pub text: [String; 6],
    pub font_size: f64,
    pub color: Rgb,
    pub underline: bool,
    /// Margins in points: top, bottom, left, right (the text's distance from the page edges).
    pub margins: [f64; 4],
    /// The number `<<1>>` shows on the first page of the range.
    pub start_number: u32,
}

impl Default for HeaderFooter {
    /// Acrobat's defaults: 8 pt black, margins 0.5 in top and bottom, 1 in left and right.
    fn default() -> Self {
        Self { text: Default::default(), font_size: 8.0, color: [0.0; 3], underline: false, margins: [36.0, 36.0, 72.0, 72.0], start_number: 1 }
    }
}

/// A picture for a background or watermark (Acrobat: Source ▸ File): an XObject already in
/// the document, an image (drawn into a unit square) or a form (a page of a PDF).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarkSource {
    pub xobject: pdfcraft_cos::ObjRef,
    /// Its natural size in points.
    pub size: (f64, f64),
    /// An image XObject (unit square); otherwise a form whose `/Matrix` maps it to `size`.
    pub image: bool,
}

impl MarkSource {
    /// A stand-in for a picture that isn't in the document yet: a one-point square, which fills
    /// the same share of the page as any picture does. For laying out previews with
    /// [`watermark_centres`].
    pub fn placeholder() -> Self {
        Self { xobject: pdfcraft_cos::ObjRef::new(0, 0), size: (1.0, 1.0), image: true }
    }
}

/// A watermark: text, or a picture from a file (`source`; the text is then ignored).
#[derive(Clone, Debug, PartialEq)]
pub struct Watermark {
    pub text: String,
    pub source: Option<MarkSource>,
    /// Pictures: size relative to the page (1.0 fits the page).
    pub scale: f64,
    /// 0 = fit: about half the page diagonal.
    pub font_size: f64,
    pub color: Rgb,
    /// 0–1.
    pub opacity: f64,
    /// Degrees counter-clockwise (Acrobat's 45° runs from bottom-left to top-right).
    pub rotation: f64,
    /// Draw behind the page content instead of on top.
    pub behind: bool,
    /// Offset of the centre from the page centre, in points (right, up).
    pub offset: [f64; 2],
    /// Draw the watermark again and again across the page (`None`: once).
    pub repeat: Option<Repeat>,
}

/// A watermark drawn as a grid of copies. One copy sits at the watermark's centre (the page
/// centre plus its `offset`); the others follow at its size plus `spacing` in each direction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Repeat {
    /// Gap between neighbouring copies, in points (horizontal, vertical).
    pub spacing: [f64; 2],
    /// Shift every other row by half a step, like bricks.
    pub stagger: bool,
}

impl Default for Repeat {
    fn default() -> Self {
        Self { spacing: [36.0, 36.0], stagger: false }
    }
}

/// The most copies of a repeated watermark on one page. More is refused, not cut short.
pub const MAX_REPEATS: usize = 1000;
/// The largest spacing or offset a repeated watermark accepts, in points.
const MAX_REPEAT_EXTENT: f64 = 1.0e6;

impl Default for Watermark {
    fn default() -> Self {
        Self {
            text: String::new(),
            source: None,
            scale: 0.5,
            font_size: 0.0,
            color: [0.6, 0.6, 0.6],
            opacity: 0.5,
            rotation: 45.0,
            behind: false,
            offset: [0.0; 2],
            repeat: None,
        }
    }
}

/// Where the copies of a watermark go on one page.
struct Layout {
    /// The font size of a text watermark (0 for a picture).
    font_size: f64,
    /// The centre of every copy, in page points.
    centres: Vec<(f64, f64)>,
}

/// Where every copy of `wm` goes on a page of `page` points (width, height): the centre of each,
/// in points with y up. It is what `add_watermark` draws, so a dialog can preview it, and it
/// fails the same way (a repeat that needs too many copies).
pub fn watermark_centres(wm: &Watermark, page: (f64, f64)) -> Result<Vec<(f64, f64)>, EditError> {
    let lines: Vec<&str> = wm.text.trim().lines().collect();
    layout_for(wm, &lines, page).map(|l| l.centres)
}

/// The font size and the copies of the watermark on one page.
fn layout_for(wm: &Watermark, lines: &[&str], page: (f64, f64)) -> Result<Layout, EditError> {
    let (copy, font_size) = mark_box(wm, lines, page);
    let anchor = (page.0 / 2.0 + wm.offset[0], page.1 / 2.0 + wm.offset[1]);
    let centres = match wm.repeat {
        Some(rep) => tile_centres(page, anchor, copy, rep)?,
        None => vec![anchor],
    };
    Ok(Layout { font_size, centres })
}

/// One copy of the watermark on a page of `page` points: the box it fills once turned by the
/// rotation, and the font size of a text watermark (0 for a picture).
fn mark_box(wm: &Watermark, lines: &[&str], page: (f64, f64)) -> ((f64, f64), f64) {
    if let Some(src) = &wm.source {
        let (dw, dh, _) = picture_size(src, page, wm.scale);
        return (rotated_box((dw, dh), wm.rotation), 0.0);
    }
    let widest = lines.iter().map(|l| helvetica_width(l, 1.0)).fold(0.0, f64::max).max(0.01);
    let (w, h) = page;
    let size = if wm.font_size > 0.0 { wm.font_size } else { ((w * w + h * h).sqrt() * 0.5 / widest).clamp(6.0, 300.0) };
    let block = (widest * size, size * 1.15 * lines.len() as f64);
    (rotated_box(block, wm.rotation), size)
}

/// The size of the box that turning a `size` rectangle by `degrees` fills.
fn rotated_box(size: (f64, f64), degrees: f64) -> (f64, f64) {
    let (s, c) = degrees.to_radians().sin_cos();
    (size.0 * c.abs() + size.1 * s.abs(), size.0 * s.abs() + size.1 * c.abs())
}

/// Where the copies of a repeated mark go: the centres, in page points (y up), of the copies
/// whose `size` box touches the page. One copy is centred on `anchor`; the others are `size` plus
/// the spacing away, with every other row shifted by half a step when `rep.stagger` is set.
/// More than [`MAX_REPEATS`] copies is an error, because dropping some would change what the
/// user asked for.
fn tile_centres(page: (f64, f64), anchor: (f64, f64), size: (f64, f64), rep: Repeat) -> Result<Vec<(f64, f64)>, EditError> {
    let bad = || EditError::Invalid("invalid repeat settings".into());
    let finite = |v: f64, max: f64| v.is_finite() && v.abs() <= max;
    if !(rep.spacing.iter().all(|s| finite(*s, MAX_REPEAT_EXTENT) && *s >= 0.0)
        && finite(anchor.0, MAX_REPEAT_EXTENT)
        && finite(anchor.1, MAX_REPEAT_EXTENT)
        && size.0.is_finite()
        && size.1.is_finite())
    {
        return Err(bad());
    }
    // A box of nothing would need endless copies; the cap below catches the small ones too.
    let (bw, bh) = (size.0.max(0.01), size.1.max(0.01));
    let (px, py) = (bw + rep.spacing[0], bh + rep.spacing[1]);
    let too_many = || {
        EditError::Invalid(format!("the repeated watermark would need more than {MAX_REPEATS} copies on a page; increase the spacing or the size"))
    };
    // The rows (and, for each, the columns) whose copies touch the page.
    let first_row = ((-bh / 2.0 - anchor.1) / py).ceil();
    let last_row = ((page.1 + bh / 2.0 - anchor.1) / py).floor();
    let rows = last_row - first_row + 1.0;
    if rows > MAX_REPEATS as f64 {
        return Err(too_many());
    }
    // Both ends are within a few million after the checks above, so they fit an i64.
    let (first_row, last_row) = (first_row as i64, last_row as i64);
    let columns = |row: i64| -> (i64, i64) {
        let shift = if rep.stagger && row.rem_euclid(2) == 1 { px / 2.0 } else { 0.0 };
        let first = ((-bw / 2.0 - anchor.0 - shift) / px).ceil();
        let last = ((page.0 + bw / 2.0 - anchor.0 - shift) / px).floor();
        (first as i64, last as i64)
    };
    let mut total = 0usize;
    for row in first_row..=last_row {
        let (a, b) = columns(row);
        total = total.saturating_add(usize::try_from((b - a + 1).max(0)).unwrap_or(usize::MAX));
        if total > MAX_REPEATS {
            return Err(too_many());
        }
    }
    let mut centres = Vec::with_capacity(total);
    for row in first_row..=last_row {
        let shift = if rep.stagger && row.rem_euclid(2) == 1 { px / 2.0 } else { 0.0 };
        let (a, b) = columns(row);
        for col in a..=b {
            centres.push((anchor.0 + shift + col as f64 * px, anchor.1 + row as f64 * py));
        }
    }
    Ok(centres)
}

/// A background behind the page content: a solid colour, or a picture from a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Background {
    pub color: Rgb,
    pub opacity: f64,
    pub source: Option<MarkSource>,
    /// Pictures: size relative to the page (1.0 fits the page).
    pub scale: f64,
}

impl Default for Background {
    fn default() -> Self {
        Self { color: [1.0, 1.0, 0.85], opacity: 1.0, source: None, scale: 1.0 }
    }
}

/// The size a picture mark is drawn at, in points: fitted to the page at `scale`. Returns its
/// width, height and the factor from its natural size.
fn picture_size(src: &MarkSource, page: (f64, f64), scale: f64) -> (f64, f64, f64) {
    let (sw, sh) = (src.size.0.max(0.01), src.size.1.max(0.01));
    let k = (page.0 / sw).min(page.1 / sh) * if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    (sw * k, sh * k, k)
}

/// Drawing a picture mark: fitted to the page at `scale`, centred (plus `offset`), turned by
/// `rotation` degrees. Returns the content and the resource name it uses.
fn picture(src: &MarkSource, page: (f64, f64), scale: f64, rotation: f64, offset: [f64; 2]) -> String {
    let (w, h) = page;
    let (dw, dh, k) = picture_size(src, page, scale);
    let (s, c) = rotation.to_radians().sin_cos();
    let (cx, cy) = (w / 2.0 + offset[0], h / 2.0 + offset[1]);
    // Centre, rotate, then place the picture's box (unit square for images).
    let place = if src.image {
        format!("{} 0 0 {} {} {} cm", n(dw), n(dh), n(-dw / 2.0), n(-dh / 2.0))
    } else {
        format!("{} 0 0 {} {} {} cm", n(k), n(k), n(-dw / 2.0), n(-dh / 2.0))
    };
    format!("{} {} {} {} {} {} cm\n{place}\n/PCPic{} Do\n", n(c), n(s), n(-s), n(c), n(cx), n(cy), src.xobject.num)
}

/// Register a mark picture in the page's own resources (`/PCPic<num>`).
fn add_picture_resource(doc: &mut Document, page: &pdfcraft_model::Page, src: &MarkSource) -> Result<(), EditError> {
    let p = doc.get(page.obj).as_dict().cloned().unwrap_or_default();
    let mut res = p.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let mut xo = res.get(b"XObject").map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()).unwrap_or_default();
    xo.set(format!("PCPic{}", src.xobject.num).into_bytes(), Object::Ref(src.xobject));
    res.set(b"XObject".to_vec(), Object::Dict(xo));
    doc.update_dict(page.obj, |d| d.set(b"Resources".to_vec(), Object::Dict(res)))?;
    Ok(())
}

/// Values tokens need that come from outside the document.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Context {
    /// Today's date: (year, month 1–12, day 1–31).
    pub date: (i64, u32, u32),
}

fn n(v: f64) -> String {
    let s = format!("{:.3}", if v.abs() < 5e-4 { 0.0 } else { v });
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

fn rgb(c: Rgb) -> String {
    let c = c.map(|x| x.clamp(0.0, 1.0));
    format!("{} {} {} rg", n(c[0]), n(c[1]), n(c[2]))
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// Expand `<<…>>` tokens for page `index` (0-based within the range) of `count`.
pub fn expand(template: &str, page_number: u32, count: usize, bates: u64, cx: &Context) -> String {
    let (y, m, d) = cx.date;
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("<<") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find(">>") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = &after[..end];
        let p = page_number;
        let value = match token {
            "1" => p.to_string(),
            "n" => count.to_string(),
            "1 of n" => format!("{p} of {count}"),
            "1/n" => format!("{p}/{count}"),
            "Page 1" => format!("Page {p}"),
            "Page 1 of n" => format!("Page {p} of {count}"),
            "m/d" => format!("{m}/{d}"),
            "m/d/yy" => format!("{m}/{d}/{:02}", y.rem_euclid(100)),
            "m/d/yyyy" => format!("{m}/{d}/{y}"),
            "mm/dd/yy" => format!("{m:02}/{d:02}/{:02}", y.rem_euclid(100)),
            "mm/dd/yyyy" => format!("{m:02}/{d:02}/{y}"),
            "d/m/yy" => format!("{d}/{m}/{:02}", y.rem_euclid(100)),
            "d/m/yyyy" => format!("{d}/{m}/{y}"),
            "dd/mm/yyyy" => format!("{d:02}/{m:02}/{y}"),
            "yyyy-mm-dd" => format!("{y}-{m:02}-{d:02}"),
            "mmmm d, yyyy" => format!("{} {d}, {y}", MONTHS[(m.clamp(1, 12) - 1) as usize]),
            t if t.starts_with("Bates Number") => {
                // Bates Number#digits#start#prefix#suffix (start is applied by the caller).
                let parts: Vec<&str> = t.split('#').collect();
                let digits = parts.get(1).and_then(|d| d.parse::<usize>().ok()).unwrap_or(6).clamp(1, 15);
                format!("{}{:0digits$}{}", parts.get(3).unwrap_or(&""), bates, parts.get(4).unwrap_or(&""))
            }
            _ => format!("<<{token}>>"),
        };
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// The Bates start number in a template (`<<Bates Number#digits#start#…>>`), if any.
fn bates_start(template: &str) -> Option<u64> {
    let i = template.find("<<Bates Number")?;
    let t = &template[i + 2..];
    let t = &t[..t.find(">>")?];
    t.split('#').nth(2).and_then(|s| s.parse().ok()).or(Some(1))
}

fn page_list(doc: &Document) -> Vec<pdfcraft_model::Page> {
    pdfcraft_model::pages(doc)
}

fn check(pages: &[usize], count: usize) -> Result<(), EditError> {
    match pages.iter().find(|p| **p >= count) {
        Some(p) => Err(EditError::NoSuchPage(*p)),
        None => Ok(()),
    }
}

/// The page's `/Contents` as a list of stream references (inline content is promoted).
fn contents(doc: &mut Document, page: &pdfcraft_model::Page) -> Result<Vec<Object>, EditError> {
    let obj = doc.get(page.obj);
    let d = obj.as_dict().cloned().unwrap_or_default();
    Ok(match d.get(b"Contents").cloned() {
        None => Vec::new(),
        Some(Object::Ref(r)) => match doc.get(r).as_array() {
            Some(a) => a.clone(),
            None => vec![Object::Ref(r)],
        },
        Some(Object::Array(a)) => a,
        Some(other) => vec![Object::Ref(doc.add(other))],
    })
}

/// Add the font (and an opacity state) to the page's own resources.
fn add_resources(doc: &mut Document, page: &pdfcraft_model::Page, opacity: Option<f64>, content: Option<&[u8]>) -> Result<(), EditError> {
    let mut res = page.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let mut fonts = res.get(b"Font").map(|f| doc.resolve(f)).and_then(|f| f.as_dict().cloned()).unwrap_or_default();
    for (name, base) in [(&b"PCHelv"[..], "Helvetica"), (b"PCTimes", "Times-Roman"), (b"PCCour", "Courier")] {
        // Helvetica always (marks use it); the others only when the content names them.
        if name != b"PCHelv" && !content.is_some_and(|c| c.windows(name.len() + 1).any(|w| w[0] == b'/' && &w[1..] == name)) {
            continue;
        }
        let mut font = Dict::new();
        font.set(b"Type".to_vec(), Object::name("Font"));
        font.set(b"Subtype".to_vec(), Object::name("Type1"));
        font.set(b"BaseFont".to_vec(), Object::name(base));
        font.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
        fonts.set(name.to_vec(), Object::Dict(font));
    }
    res.set(b"Font".to_vec(), Object::Dict(fonts));
    if let Some(o) = opacity {
        let mut gs = res.get(b"ExtGState").map(|g| doc.resolve(g)).and_then(|g| g.as_dict().cloned()).unwrap_or_default();
        let mut s = Dict::new();
        s.set(b"Type".to_vec(), Object::name("ExtGState"));
        s.set(b"CA".to_vec(), Object::Real(o));
        s.set(b"ca".to_vec(), Object::Real(o));
        let name = format!("PCGS{}", (o * 100.0).round() as i64);
        gs.set(name.into_bytes(), Object::Dict(s));
        res.set(b"ExtGState".to_vec(), Object::Dict(gs));
    }
    // The page gets its own (possibly copied) resources; shared dictionaries stay untouched.
    doc.update_dict(page.obj, |d| d.set(b"Resources".to_vec(), Object::Dict(res)))?;
    Ok(())
}

const WRAP_OPEN: &[u8] = b"q %PdfCraft\n";
const WRAP_CLOSE: &[u8] = b"Q %PdfCraft\n";

#[cfg(test)]
fn stream_bytes(doc: &Document, o: &Object) -> Option<Vec<u8>> {
    match &*doc.resolve(o) {
        Object::Stream(s) => s.decoded().ok(),
        _ => None,
    }
}

/// The `/PCMark` tag in a content stream's dictionary: a mark kind, or `Wrap` for the q/Q
/// wrapper. Read from the dictionary, so finding marks never decompresses page content.
fn stream_tag(doc: &Document, o: &Object) -> Option<String> {
    let obj = doc.resolve(o);
    obj.as_dict()?.name(b"PCMark").map(|n| String::from_utf8_lossy(n).into_owned())
}

fn tagged(tag: &str) -> Dict {
    let mut d = Dict::new();
    d.set(b"PCMark".to_vec(), Object::name(tag));
    d
}

/// Put a mark's content on a page: behind (prepended) or on top (appended, after wrapping the
/// original content in q/Q).
fn place(doc: &mut Document, page: &pdfcraft_model::Page, kind: MarkKind, content: Vec<u8>, behind: bool) -> Result<(), EditError> {
    place_tagged(doc, page, kind.tag(), content, behind)
}

/// Put content on a page, tagged `tag` (see [`place`]).
fn place_tagged(doc: &mut Document, page: &pdfcraft_model::Page, tag: &str, content: Vec<u8>, behind: bool) -> Result<(), EditError> {
    let mut list = contents(doc, page)?;
    let mark = Object::Ref(doc.add(Object::Stream(Stream::flate(tagged(tag), &content))));
    if behind {
        list.insert(0, mark);
    } else {
        let wrapped = list.iter().any(|o| stream_tag(doc, o).as_deref() == Some("Wrap"));
        let original = list.iter().any(|o| stream_tag(doc, o).is_none());
        if !wrapped && original {
            let open = Object::Ref(doc.add(Object::Stream(Stream::from_raw(tagged("Wrap"), WRAP_OPEN.to_vec()))));
            let close = Object::Ref(doc.add(Object::Stream(Stream::from_raw(tagged("Wrap"), WRAP_CLOSE.to_vec()))));
            // Behind-marks stay outside the wrapper.
            let split = list.iter().position(|o| stream_tag(doc, o).is_none()).unwrap_or(list.len());
            list.insert(split, open);
            list.push(close);
        }
        list.push(mark);
    }
    doc.update_dict(page.obj, |d| d.set(b"Contents".to_vec(), Object::Array(list)))?;
    Ok(())
}

/// Draw `content` on top of a page (0-based) as permanent page content, with standard Helvetica
/// available as `/PCHelv`. The stream is tagged `tag` (not a mark kind, so Remove never takes
/// it away); the original content is wrapped in q/Q first. Used for applied redaction boxes.
pub fn stamp(doc: &mut Document, page: usize, tag: &str, content: Vec<u8>) -> Result<(), EditError> {
    let all = page_list(doc);
    check(&[page], all.len())?;
    add_resources(doc, &all[page], None, Some(&content))?;
    let p = page_list(doc).swap_remove(page);
    place_tagged(doc, &p, tag, content, false)
}

fn begin(kind: MarkKind, subtype: &str, matrix: [f64; 6]) -> String {
    format!(
        "q\n/Artifact <</Type /Pagination /Subtype /{subtype} /PCMark /{}>> BDC\n{} {} {} {} {} {} cm\n",
        kind.tag(),
        n(matrix[0]),
        n(matrix[1]),
        n(matrix[2]),
        n(matrix[3]),
        n(matrix[4]),
        n(matrix[5])
    )
}

const END: &str = "EMC\nQ\n";

/// Refuses page text the standard-14 fonts (WinAnsiEncoding) can't draw: written anyway, each
/// such character would become a `?` on the page (#125). `lines` must split the text the way the
/// caller draws it, so a `\r` that would be drawn is refused too.
pub(crate) fn drawable<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<(), EditError> {
    match lines.into_iter().find_map(pdfcraft_fonts::first_non_win_ansi) {
        Some(c) => Err(undrawable(c)),
        None => Ok(()),
    }
}

/// The refusal for page text with `c`, which nothing here can draw.
pub(crate) fn undrawable(c: char) -> EditError {
    EditError::Invalid(undrawable_message(c))
}

/// The wording of the refusal for text with `c`, which the standard fonts can't draw.
pub fn undrawable_message(c: char) -> String {
    format!("the standard fonts can't draw \"{c}\" (U+{:04X}); only Western European characters can be added as text for now", u32::from(c))
}

fn text_op(x: f64, y: f64, text: &str) -> Vec<u8> {
    let mut v = format!("1 0 0 1 {} {} Tm ", n(x), n(y)).into_bytes();
    v.extend(literal(&win_ansi(text)));
    v.extend_from_slice(b" Tj\n");
    v
}

/// Add a header and footer to `pages` (0-based). With `replace`, existing headers and footers on
/// those pages are removed first (Acrobat's Replace Existing).
pub fn add_header_footer(doc: &mut Document, pages: &[usize], hf: &HeaderFooter, replace: bool, cx: &Context) -> Result<(), EditError> {
    let all = page_list(doc);
    check(pages, all.len())?;
    if hf.text.iter().all(|t| t.trim().is_empty()) {
        return Err(EditError::Invalid("type the header or footer text first".into()));
    }
    // Tokens only add ASCII (Bates prefixes and suffixes are part of the template), so checking
    // the templates, split as `expand`'s output is, covers every page.
    hf.text.iter().try_for_each(|t| drawable(t.lines()))?;
    if !(hf.font_size.is_finite() && hf.font_size > 0.0 && hf.font_size <= 200.0 && hf.margins.iter().all(|m| m.is_finite() && *m >= 0.0)) {
        return Err(EditError::Invalid("invalid font size or margins".into()));
    }
    // Page numbers count up from start_number, so the last selected page needs the most room.
    // Refuse the whole run before drawing anything: headers applied to some pages and then a
    // failed page number would leave the document half changed.
    let last_number = u64::from(hf.start_number)
        .checked_add(pages.len().saturating_sub(1) as u64)
        .ok_or_else(|| EditError::Invalid("the header page numbers are too large".into()))?;
    if last_number > u64::from(u32::MAX) {
        return Err(EditError::Invalid(format!(
            "the header start number {} leaves no room for {} pages (page numbers stop at {})",
            hf.start_number,
            pages.len(),
            u32::MAX
        )));
    }
    if replace {
        remove_marks(doc, pages, MarkKind::HeaderFooter)?;
    }
    let all = page_list(doc);
    let count = all.len();
    let bates0 = hf.text.iter().find_map(|t| bates_start(t)).unwrap_or(1);
    for (k, &i) in pages.iter().enumerate() {
        let page = &all[i];
        let (w, h) = page.display_size(doc);
        let number =
            u32::try_from(u64::from(hf.start_number) + k as u64).map_err(|_| EditError::Invalid("the header page numbers are too large".into()))?;
        let bates = bates0 + k as u64;
        let size = hf.font_size;
        let [top, bottom, left, right] = hf.margins;
        let mut body: Vec<u8> = format!("BT\n/PCHelv {} Tf\n{}\n", n(size), rgb(hf.color)).into_bytes();
        let mut underlines = String::new();
        for (slot, template) in hf.text.iter().enumerate() {
            if template.trim().is_empty() {
                continue;
            }
            let lines: Vec<String> = expand(template, number, count, bates, cx).lines().map(str::to_owned).collect();
            let header = slot < 3;
            let line_h = size * 1.2;
            for (li, line) in lines.iter().enumerate() {
                let tw = helvetica_width(line, size);
                let x = match slot % 3 {
                    0 => left,
                    1 => (w - tw) / 2.0,
                    _ => w - right - tw,
                };
                // Headers hang below the top margin; footers sit on the bottom margin.
                let y = if header { h - top - size * 0.8 - li as f64 * line_h } else { bottom + (lines.len() - 1 - li) as f64 * line_h };
                body.extend(text_op(x, y, line));
                if hf.underline {
                    underlines.push_str(&format!("{} {} {} {} re f\n", n(x), n(y - size * 0.15), n(tw), n((size * 0.06).max(0.4))));
                }
            }
        }
        body.extend_from_slice(b"ET\n");
        if !underlines.is_empty() {
            body.extend(format!("{}\n{underlines}", rgb(hf.color)).bytes());
        }
        let mut content = begin(MarkKind::HeaderFooter, "Header", page.view_matrix(doc)).into_bytes();
        content.extend(body);
        content.extend_from_slice(END.as_bytes());
        add_resources(doc, page, None, None)?;
        let page = &page_list(doc)[i];
        place(doc, page, MarkKind::HeaderFooter, content, false)?;
    }
    Ok(())
}

/// Add a text watermark to `pages`.
pub fn add_watermark(doc: &mut Document, pages: &[usize], wm: &Watermark, replace: bool) -> Result<(), EditError> {
    let all = page_list(doc);
    check(pages, all.len())?;
    let text = wm.text.trim();
    if text.is_empty() && wm.source.is_none() {
        return Err(EditError::Invalid("type the watermark text first".into()));
    }
    if wm.source.is_none() {
        drawable(text.lines())?;
    }
    if !(wm.opacity.is_finite()
        && wm.rotation.is_finite()
        && wm.font_size.is_finite()
        && wm.font_size >= 0.0
        && wm.offset.iter().all(|o| o.is_finite()))
    {
        return Err(EditError::Invalid("invalid watermark settings".into()));
    }
    let lines: Vec<&str> = text.lines().collect();
    // Where every copy goes on every page, worked out before anything changes, so a repeat that
    // is refused (too many copies, say) leaves the document as it was.
    let layouts: Vec<Layout> = pages.iter().map(|&i| layout_for(wm, &lines, all[i].display_size(doc))).collect::<Result<_, EditError>>()?;
    if replace {
        remove_marks(doc, pages, MarkKind::Watermark)?;
    }
    let opacity = wm.opacity.clamp(0.0, 1.0);
    for (&i, layout) in pages.iter().zip(&layouts) {
        let page = page_list(doc)[i].clone();
        let (w, h) = page.display_size(doc);
        // Each copy of a repeated watermark is drawn in its own q ... Q.
        let repeated = wm.repeat.is_some();
        if let Some(src) = &wm.source {
            let mut content = begin(MarkKind::Watermark, "Watermark", page.view_matrix(doc));
            content.push_str(&format!("/PCGS{} gs\n", (opacity * 100.0).round() as i64));
            for &(x, y) in &layout.centres {
                if repeated {
                    content.push_str("q\n");
                }
                content.push_str(&picture(src, (w, h), wm.scale, wm.rotation, [x - w / 2.0, y - h / 2.0]));
                if repeated {
                    content.push_str("Q\n");
                }
            }
            content.push_str(END);
            add_resources(doc, &page, Some(opacity), None)?;
            add_picture_resource(doc, &page_list(doc)[i].clone(), src)?;
            let page = &page_list(doc)[i];
            place(doc, page, MarkKind::Watermark, content.into_bytes(), wm.behind)?;
            continue;
        }
        let size = layout.font_size;
        let (s, c) = wm.rotation.to_radians().sin_cos();
        let mut content = begin(MarkKind::Watermark, "Watermark", page.view_matrix(doc)).into_bytes();
        content.extend(format!("/PCGS{} gs\n", (opacity * 100.0).round() as i64).bytes());
        for &(cx, cy) in &layout.centres {
            if repeated {
                content.extend_from_slice(b"q\n");
            }
            content.extend(
                format!("{} {} {} {} {} {} cm\nBT\n/PCHelv {} Tf\n{}\n", n(c), n(s), n(-s), n(c), n(cx), n(cy), n(size), rgb(wm.color)).bytes(),
            );
            let line_h = size * 1.15;
            let total = line_h * (lines.len() as f64 - 1.0);
            for (li, line) in lines.iter().enumerate() {
                let tw = helvetica_width(line, size);
                content.extend(text_op(-tw / 2.0, total / 2.0 - li as f64 * line_h - size * 0.35, line));
            }
            content.extend_from_slice(b"ET\n");
            if repeated {
                content.extend_from_slice(b"Q\n");
            }
        }
        content.extend_from_slice(END.as_bytes());
        add_resources(doc, &page, Some(opacity), None)?;
        let page = &page_list(doc)[i];
        place(doc, page, MarkKind::Watermark, content, wm.behind)?;
    }
    Ok(())
}

/// Add a solid-colour background behind the content of `pages`.
pub fn add_background(doc: &mut Document, pages: &[usize], bg: &Background, replace: bool) -> Result<(), EditError> {
    let all = page_list(doc);
    check(pages, all.len())?;
    if replace {
        remove_marks(doc, pages, MarkKind::Background)?;
    }
    let opacity = if bg.opacity.is_finite() { bg.opacity.clamp(0.0, 1.0) } else { 1.0 };
    for &i in pages {
        let page = page_list(doc)[i].clone();
        let (w, h) = page.display_size(doc);
        let mut content = begin(MarkKind::Background, "Background", page.view_matrix(doc));
        match &bg.source {
            Some(src) => {
                content.push_str(&format!("/PCGS{} gs\n", (opacity * 100.0).round() as i64));
                content.push_str(&picture(src, (w, h), bg.scale, 0.0, [0.0; 2]));
            }
            None => content.push_str(&format!("/PCGS{} gs\n{}\n0 0 {} {} re f\n", (opacity * 100.0).round() as i64, rgb(bg.color), n(w), n(h))),
        }
        content.push_str(END);
        add_resources(doc, &page, Some(opacity), None)?;
        if let Some(src) = &bg.source {
            add_picture_resource(doc, &page_list(doc)[i].clone(), src)?;
        }
        let page = &page_list(doc)[i];
        place(doc, page, MarkKind::Background, content.into_bytes(), true)?;
    }
    Ok(())
}

/// Remove marks of `kind` from `pages`; returns how many were removed. When no mark is left on
/// top of a page, the q/Q wrapper goes too, restoring the original content list.
pub fn remove_marks(doc: &mut Document, pages: &[usize], kind: MarkKind) -> Result<usize, EditError> {
    let all = page_list(doc);
    check(pages, all.len())?;
    let mut removed = 0;
    for &i in pages {
        let page = &all[i];
        let list = contents(doc, page)?;
        let before = list.len();
        let mut kept: Vec<Object> = list.into_iter().filter(|o| stream_tag(doc, o).as_deref() != Some(kind.tag())).collect();
        removed += before - kept.len();
        // Marks on top come after the closing wrapper; without any, the wrapper has no purpose.
        let is_wrap = |o: &Object| stream_tag(doc, o).as_deref() == Some("Wrap");
        let close = kept.iter().rposition(&is_wrap);
        let marks_on_top = close.is_some_and(|c| kept[c + 1..].iter().any(|o| stream_tag(doc, o).is_some_and(|t| t != "Wrap")));
        if !marks_on_top {
            kept.retain(|o| !is_wrap(o));
        }
        if before != kept.len() {
            doc.update_dict(page.obj, |d| d.set(b"Contents".to_vec(), Object::Array(kept)))?;
        }
    }
    Ok(removed)
}

/// Which kinds of marks the document has (for enabling Update/Remove).
pub fn marks_present(doc: &Document) -> Vec<MarkKind> {
    added::scan_markers(doc).marks
}

pub mod objects;
pub use objects::{EditableObject, MAX_MOVE_OBJECTS, ObjectKind, ObjectTarget, editable_objects, move_objects};

mod flatten;
pub use flatten::{flatten, flatten_fill_sign};
pub mod images;
pub use images::{ImageChange, PageImage, change_image, page_images, reading_images, rect_to_rect, turn_about_centre};
pub mod bidi;
pub mod indic;
pub mod text;
pub use text::{
    BlockStyle, LineEdit, RangeStyle, TextBlock, TextLine, clear_block, reading_blocks, replace_block, replace_line, rewrite_block, text_blocks,
    text_lines,
};
pub mod added;
pub use added::{
    Added, AddedImage, AddedText, Align, Content, Family, MarkerScan, add_content, delete_content, first_undrawable, list_added, scan_markers,
    update_content,
};

#[cfg(test)]
mod tests;
