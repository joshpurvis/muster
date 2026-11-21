//! A picture's size in pixels, read from its header (PNG, JPEG, GIF, WebP) or its `<svg>` tag,
//! without decoding it. `post_pr` uses it to leave out pictures too tall to read (src/prs.rs
//! `too_tall`). `None` means the header doesn't say: the caller treats the picture as it did before.

/// Width and height as the window shows them (a JPEG's EXIF rotation applied).
pub fn size(mime: &str, b: &[u8]) -> Option<(u32, u32)> {
    let (w, h) = match mime {
        "image/png" => png(b)?,
        "image/jpeg" => jpeg(b)?,
        "image/gif" => gif(b)?,
        "image/webp" => webp(b)?,
        "image/svg+xml" => svg(b)?,
        _ => return None,
    };
    (w > 0 && h > 0).then_some((w, h))
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?) as u32)
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as u32)
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let x = b.get(at..at + 3)?;
    Some(x[0] as u32 | (x[1] as u32) << 8 | (x[2] as u32) << 16)
}

/// The IHDR chunk, which the format puts first (13 bytes long).
fn png(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(8..16)? != b"\0\0\0\x0dIHDR" {
        return None;
    }
    let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
    Some((w, h))
}

/// The logical screen size.
fn gif(b: &[u8]) -> Option<(u32, u32)> {
    Some((le16(b, 6)?, le16(b, 8)?))
}

/// The first chunk: VP8X (extended: the canvas), VP8 (lossy) or VP8L (lossless).
fn webp(b: &[u8]) -> Option<(u32, u32)> {
    match b.get(12..16)? {
        b"VP8X" => {
            if u32::from_le_bytes(b.get(16..20)?.try_into().ok()?) < 10 {
                return None;
            }
            Some((le24(b, 24)? + 1, le24(b, 27)? + 1))
        }
        b"VP8 " => {
            if b.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            Some((le16(b, 26)? & 0x3fff, le16(b, 28)? & 0x3fff))
        }
        b"VP8L" => {
            if *b.get(20)? != 0x2f {
                return None;
            }
            let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        _ => None,
    }
}

/// The first frame header (SOFn), walking the segments up to the image data (an EXIF segment may
/// come before or after it). An EXIF orientation of 5 to 8 (turned a quarter) swaps width and
/// height, as the window draws it.
fn jpeg(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    let mut turned = false;
    let mut frame = None;
    while i + 1 < b.len() {
        if b[i] != 0xff {
            break;
        }
        let m = b[i + 1];
        match m {
            // Fill bytes, and markers without a length.
            0xff => i += 1,
            0x01 | 0xd0..=0xd8 => i += 2,
            // The end, or the image data.
            0xd9 | 0xda => break,
            _ => {
                let Some(len) = be16(b, i + 2).map(|l| l as usize).filter(|l| *l >= 2) else { break };
                let Some(seg) = b.get(i + 4..i + 2 + len) else { break };
                if frame.is_none() && matches!(m, 0xc0..=0xcf) && !matches!(m, 0xc4 | 0xc8 | 0xcc) {
                    let (Some(h), Some(w)) = (be16(seg, 1), be16(seg, 3)) else { break };
                    frame = Some((w, h));
                }
                if m == 0xe1 && let Some(o) = exif_orientation(seg) {
                    turned = (5..=8).contains(&o);
                }
                i += 2 + len;
            }
        }
    }
    frame.map(|(w, h)| if turned { (h, w) } else { (w, h) })
}

/// Tag 0x0112 of an APP1 Exif segment's first IFD.
fn exif_orientation(seg: &[u8]) -> Option<u16> {
    let t = seg.strip_prefix(b"Exif\0\0")?;
    let little = match t.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let x: [u8; 2] = t.get(at..at + 2)?.try_into().ok()?;
        Some(if little { u16::from_le_bytes(x) } else { u16::from_be_bytes(x) })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let x: [u8; 4] = t.get(at..at + 4)?.try_into().ok()?;
        Some(if little { u32::from_le_bytes(x) } else { u32::from_be_bytes(x) })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    (0..count.min(256)).map(|k| ifd + 2 + k * 12).find(|&e| u16_at(e) == Some(0x0112)).and_then(|e| u16_at(e + 8))
}

/// The root `<svg>` tag's width and height when both are absolute lengths (px, in, cm, mm, pt, pc
/// or a plain number), else its viewBox. Only their ratio matters here, so a small viewBox is
/// scaled up before rounding.
fn svg(b: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(b).ok()?;
    let attrs = attributes(root_tag(text)?);
    let get = |name: &str| attrs.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
    let (w, h) = match (get("width").and_then(length), get("height").and_then(length)) {
        (Some(w), Some(h)) => (w, h),
        _ => {
            let mut vb = get("viewBox")?.split(|c: char| c.is_whitespace() || c == ',').filter(|s| !s.is_empty());
            let mut next = || vb.next().and_then(|s| s.parse::<f64>().ok());
            let (_, _, w, h) = (next()?, next()?, next()?, next()?);
            if vb.next().is_some() || !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
                return None;
            }
            (w, h)
        }
    };
    let k = if w.min(h) < 100.0 { 100.0 / w.min(h) } else { 1.0 };
    Some(((w * k).round().min(u32::MAX as f64) as u32, (h * k).round().min(u32::MAX as f64) as u32))
}

/// An absolute length in CSS pixels ("390", "390px", "4in"); `None` for relative ones (%, em).
fn length(v: &str) -> Option<f64> {
    let v = v.trim();
    let split = v.find(|c: char| c.is_ascii_alphabetic() || c == '%').unwrap_or(v.len());
    let per = match &v[split..] {
        "" | "px" => 1.0,
        "in" => 96.0,
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        _ => return None,
    };
    v[..split].trim().parse::<f64>().ok().map(|x| x * per).filter(|x| x.is_finite() && *x > 0.0)
}

/// The root element's attribute text, after comments, processing instructions and a doctype; `None`
/// when the root isn't `<svg>`.
fn root_tag(text: &str) -> Option<&str> {
    let mut rest = text;
    loop {
        rest = &rest[rest.find('<')?..];
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = &r[r.find("-->")? + 3..];
        } else if let Some(r) = rest.strip_prefix("<?") {
            rest = &r[r.find("?>")? + 2..];
        } else if let Some(r) = rest.strip_prefix("<!") {
            // A doctype, maybe with an internal subset in [...] that holds '>' of its own.
            let gt = r.find('>')?;
            let end = match r.find('[') {
                Some(open) if open < gt => { let close = open + r[open..].find(']')?; close + r[close..].find('>')? }
                _ => gt,
            };
            rest = &r[end + 1..];
        } else {
            let r = &rest[1..];
            let name_end = r.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(r.len());
            let name = &r[..name_end];
            return (name == "svg" || name.ends_with(":svg")).then(|| tag_text(&r[name_end..]));
        }
    }
}

/// The attributes of a tag's text, in order: `name="value"` or `name='value'` (at most 256).
fn attributes(mut s: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    while out.len() < 256 {
        s = s.trim_start();
        let n = s.find(|c: char| c == '=' || c == '/' || c.is_whitespace()).unwrap_or(s.len());
        if n == 0 {
            break;
        }
        let name = &s[..n];
        s = s[n..].trim_start();
        let Some(r) = s.strip_prefix('=') else { continue };
        let r = r.trim_start();
        let Some(q) = r.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
        let Some(end) = r[1..].find(q) else { break };
        out.push((name, &r[1..1 + end]));
        s = &r[2 + end..];
    }
    out
}

/// The opening tag's attributes: up to the first `>` outside quotes.
fn tag_text(rest: &str) -> &str {
    let mut quote = None;
    for (i, c) in rest.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '>') => return &rest[..i],
            _ => {}
        }
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b
    }

    /// A JPEG start: SOI, an optional APP1 Exif segment with `orientation`, then SOF0.
    fn jpeg_of(w: u16, h: u16, orientation: Option<u16>, big_endian: bool) -> Vec<u8> {
        let mut b = vec![0xff, 0xd8];
        if let Some(o) = orientation {
            let mut t: Vec<u8> = if big_endian { b"MM\0\x2a\0\0\0\x08".to_vec() } else { b"II\x2a\0\x08\0\0\0".to_vec() };
            let e16 = |x: u16| if big_endian { x.to_be_bytes() } else { x.to_le_bytes() };
            let e32 = |x: u32| if big_endian { x.to_be_bytes() } else { x.to_le_bytes() };
            t.extend(e16(1));
            t.extend(e16(0x0112));
            t.extend(e16(3));
            t.extend(e32(1));
            t.extend(e16(o));
            t.extend([0, 0]);
            t.extend(e32(0));
            let mut seg = b"Exif\0\0".to_vec();
            seg.extend(t);
            b.extend([0xff, 0xe1]);
            b.extend(((seg.len() + 2) as u16).to_be_bytes());
            b.extend(seg);
        }
        // An APP0 (JFIF) and a DQT-ish segment first, as real files have.
        b.extend([0xff, 0xe0, 0, 4, 0, 0]);
        b.extend([0xff, 0xff, 0xdb, 0, 3, 0]);
        b.extend([0xff, 0xc2, 0, 11, 8]);
        b.extend(h.to_be_bytes());
        b.extend(w.to_be_bytes());
        b.extend([3, 1, 0x22, 0]);
        b
    }

    #[test]
    fn sizes_from_every_kind_of_header() {
        assert_eq!(size("image/png", &png_of(390, 8000)), Some((390, 8000)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend([0x86, 0x01, 0x40, 0x1f]);
        assert_eq!(size("image/gif", &gif), Some((390, 8000)));
        assert_eq!(size("image/jpeg", &jpeg_of(390, 8000, None, false)), Some((390, 8000)));
        // A phone photo stored sideways with "turn a quarter" in its EXIF is shown upright.
        assert_eq!(size("image/jpeg", &jpeg_of(8000, 390, Some(6), false)), Some((390, 8000)));
        assert_eq!(size("image/jpeg", &jpeg_of(8000, 390, Some(8), true)), Some((390, 8000)));
        assert_eq!(size("image/jpeg", &jpeg_of(8000, 390, Some(1), true)), Some((8000, 390)));
        let riff = |chunk: &[u8], rest: &[u8]| {
            let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
            b.extend(chunk);
            b.extend(rest);
            b
        };
        let mut x = vec![10, 0, 0, 0, 0, 0, 0, 0];
        x.extend([0x85, 0x01, 0x00, 0x3f, 0x1f, 0x00]);
        assert_eq!(size("image/webp", &riff(b"VP8X", &x)), Some((390, 8000)));
        let mut lossy = vec![0; 4];
        lossy.extend([0, 0, 0, 0x9d, 0x01, 0x2a]);
        lossy.extend(390u16.to_le_bytes());
        lossy.extend(8000u16.to_le_bytes());
        assert_eq!(size("image/webp", &riff(b"VP8 ", &lossy)), Some((390, 8000)));
        let bits: u32 = (390 - 1) | (8000 - 1) << 14;
        let mut lossless = vec![0; 4];
        lossless.push(0x2f);
        lossless.extend(bits.to_le_bytes());
        assert_eq!(size("image/webp", &riff(b"VP8L", &lossless)), Some((390, 8000)));
        let svg = |t: &str| size("image/svg+xml", t.as_bytes());
        assert_eq!(svg(r#"<svg xmlns="http://www.w3.org/2000/svg" width="390" height="8000">"#), Some((390, 8000)));
        assert_eq!(svg(r#"<?xml version="1.0"?><svg stroke-width="2" width='390px' height="8000px" >"#), Some((390, 8000)));
        assert_eq!(svg(r#"<svg width="100%" viewBox="0 0 39,800">"#), Some((100, 2051)));
        assert_eq!(svg(r#"<svg data-x="a>b" viewBox="0 0 1600 1000"><rect width="5" height="900"/></svg>"#), Some((1600, 1000)));
        // The root tag, not one in a comment, a doctype or another attribute's value.
        assert_eq!(svg(r#"<?xml version="1.0"?><!-- <svg width="1600" height="1000"/> --><!DOCTYPE svg [<!ENTITY a "<svg width='9' height='1'>">]>
            <svg aria-label="width='1600' height='1000'" width="390" height="8000">"#), Some((390, 8000)));
        assert_eq!(svg(r#"<svg:svg xmlns:svg="http://www.w3.org/2000/svg" width = "390" height='8000'>"#), Some((390, 8000)));
        // Absolute units become pixels; the ratio is what counts.
        assert_eq!(svg(r#"<svg width="1in" height="4in">"#), Some((100, 400)));
        assert_eq!(svg(r#"<svg width="10cm" height="10mm">"#), Some((1000, 100)));
        // A JPEG's EXIF segment after its frame header still turns it.
        let mut late = jpeg_of(8000, 390, None, false);
        let exif = jpeg_of(1, 1, Some(6), false);
        let app1 = exif.windows(2).position(|w| w == [0xff, 0xe1]).unwrap();
        let len = u16::from_be_bytes([exif[app1 + 2], exif[app1 + 3]]) as usize;
        late.extend(&exif[app1..app1 + 2 + len]);
        late.extend([0xff, 0xda, 0, 2]);
        assert_eq!(size("image/jpeg", &late), Some((390, 8000)));
    }

    #[test]
    fn headers_that_dont_say_give_none() {
        for (mime, b) in [
            ("image/png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec()),
            ("image/png", png_of(0, 8000)),
            ("image/png", b"\x89PNG\r\n\x1a\n\0\0\0\rIDAT\0\0\0\x01\0\0\0\x02".to_vec()),
            ("image/gif", b"GIF89a\x01".to_vec()),
            ("image/jpeg", vec![0xff, 0xd8, 0xff]),
            ("image/jpeg", vec![0xff, 0xd8, 0xff, 0xe0, 0x40, 0x00, 1, 2]),
            ("image/jpeg", vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x00, 0xff, 0xc0]),
            ("image/jpeg", vec![0xff, 0xd8, 0xff, 0xda, 0, 2, 0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1]),
            ("image/jpeg", vec![0xff, 0xd8, 0x00, 0x01]),
            ("image/webp", b"RIFF\0\0\0\0WEBPVP8 \0\0\0\0\0\0\0\x01\x02\x03\x01\0\x01\0".to_vec()),
            ("image/webp", b"RIFF\0\0\0\0WEBPABCD".to_vec()),
            ("image/svg+xml", br#"<svg width="100%" height="100%">"#.to_vec()),
            ("image/svg+xml", br#"<svg viewBox="0 0 10">"#.to_vec()),
            ("image/svg+xml", br#"<svg viewBox="0 0 10 20 30">"#.to_vec()),
            ("image/svg+xml", br#"<svg width="2em" height="9em">"#.to_vec()),
            ("image/svg+xml", br#"<html><svg width="390" height="8000">"#.to_vec()),
            ("image/svg+xml", br#"<!-- <svg width="390" height="8000"> -->"#.to_vec()),
            ("image/png", b"\x89PNG\r\n\x1a\n\0\0\0\0IHDR\0\0\x01\x86\0\0\x1f\x40".to_vec()),
            ("image/webp", b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0\x85\x01\0\x3f\x1f\0".to_vec()),
            ("image/svg+xml", br#"<svg viewBox="0 0 NaN 10">"#.to_vec()),
            ("image/svg+xml", b"\xff\xfe<svg".to_vec()),
            ("image/svg+xml", br#"<svg stroke-width="3" data-height="9">"#.to_vec()),
            ("image/bmp", png_of(1, 1)),
        ] {
            assert_eq!(size(mime, &b), None, "{mime} {b:?}");
        }
        // A huge EXIF count or offset is read safely.
        let mut b = vec![0xff, 0xd8, 0xff, 0xe1, 0, 16];
        b.extend(b"Exif\0\0II\x2a\0\xff\xff\xff\x7f");
        b.extend([0xff, 0xc0, 0, 11, 8, 0, 8, 0, 4, 3, 1, 0x22, 0]);
        assert_eq!(size("image/jpeg", &b), Some((4, 8)));
    }
}
