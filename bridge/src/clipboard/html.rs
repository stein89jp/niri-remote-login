//! CF_HTML ("HTML Format") <-> `text/html`. CF_HTML is UTF-8 with an ASCII
//! description header whose decimal offsets locate the document and the
//! copied fragment inside the same byte buffer.

/// The decoded document bound, in UTF-8 bytes, on either side.
pub(crate) const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;
/// Description headers are a handful of short lines; never scan further.
pub(crate) const MAX_HEADER_BYTES: usize = 4096;
const CHARSET: &str = "<meta charset=\"utf-8\">";
const START_MARKER: &str = "<!--StartFragment-->";
const END_MARKER: &str = "<!--EndFragment-->";

#[derive(Debug, Default, PartialEq)]
struct Header {
    start_html: Option<usize>,
    end_html: Option<usize>,
    start_fragment: Option<usize>,
    end_fragment: Option<usize>,
}

/// Reads `Key:Value` lines up to the first line that is not one. A
/// negative offset (StartHTML:-1 marks an absent context) reads as None.
fn parse_header(data: &[u8]) -> Option<Header> {
    let mut header = Header::default();
    let mut version = false;
    let limit = data.len().min(MAX_HEADER_BYTES);
    let mut position = 0;
    while position < limit {
        let rest = &data[position..limit];
        let end = rest
            .iter()
            .position(|&b| b == b'\r' || b == b'\n')
            .unwrap_or(rest.len());
        let line = &rest[..end];
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            break;
        };
        let key = &line[..colon];
        if key.is_empty() || !key.iter().all(u8::is_ascii_alphanumeric) {
            break;
        }
        let value = std::str::from_utf8(&line[colon + 1..]).ok()?.trim();
        let offset = || {
            value
                .parse::<i64>()
                .ok()
                .and_then(|v| usize::try_from(v).ok())
        };
        match key {
            b"Version" => version = true,
            b"StartHTML" => header.start_html = offset(),
            b"EndHTML" => header.end_html = offset(),
            b"StartFragment" => header.start_fragment = offset(),
            b"EndFragment" => header.end_fragment = offset(),
            _ => {}
        }
        position += end;
        if data.get(position) == Some(&b'\r') {
            position += 1;
        }
        if data.get(position) == Some(&b'\n') {
            position += 1;
        }
    }
    version.then_some(header)
}

/// The document (with its context, styles and fragment markers), else the
/// bare fragment, as UTF-8 HTML for Wayland. A charset declaration is
/// prepended because Linux toolkits otherwise guess the encoding.
pub(crate) fn from_cf_html(data: &[u8]) -> Option<String> {
    let data = &data[..data.iter().position(|&b| b == 0).unwrap_or(data.len())];
    if data.len() > MAX_HTML_BYTES + MAX_HEADER_BYTES {
        return None;
    }
    let header = parse_header(data)?;
    let range = [
        (header.start_html, header.end_html),
        (header.start_fragment, header.end_fragment),
    ]
    .into_iter()
    .find_map(|range| match range {
        // Writers disagree on whether EndHTML counts a terminator.
        (Some(start), Some(end)) if start < end.min(data.len()) => Some(start..end.min(data.len())),
        _ => None,
    })?;
    let html = String::from_utf8_lossy(&data[range]);
    if html.trim().is_empty() || html.len() > MAX_HTML_BYTES {
        return None;
    }
    Some(format!("{CHARSET}{html}"))
}

/// Decodes `text/html` bytes from a Wayland source: UTF-8 (with or without a
/// BOM) or UTF-16 with a BOM, as some toolkits write.
pub(crate) fn decode(bytes: &[u8]) -> Option<String> {
    let utf16 = |bytes: &[u8], little: bool| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| {
                if little {
                    u16::from_le_bytes(b)
                } else {
                    u16::from_be_bytes(b)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    let text = if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        utf16(rest, true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        utf16(rest, false)
    } else {
        let rest = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        String::from_utf8_lossy(rest).into_owned()
    };
    let text = text.replace('\0', "");
    (!text.trim().is_empty() && text.len() <= MAX_HTML_BYTES).then_some(text)
}

/// Drops one leading `<meta ... charset ...>`: Chromium and this bridge
/// prepend it for Linux, but inside a Windows fragment it is noise.
fn strip_charset(html: &str) -> &str {
    let trimmed = html.trim_start();
    if trimmed.len() >= 5
        && trimmed.as_bytes()[..5].eq_ignore_ascii_case(b"<meta")
        && let Some(end) = trimmed.find('>')
        && trimmed[..end].to_ascii_lowercase().contains("charset")
    {
        return &trimmed[end + 1..];
    }
    html
}

/// Builds CF_HTML with fixed-width offsets, so the header length is known
/// before the offsets are written. Existing fragment markers are kept; a
/// full document gets markers around its body; anything else is wrapped.
pub(crate) fn to_cf_html(html: &str) -> Vec<u8> {
    let html = strip_charset(html);
    let lower = html.to_ascii_lowercase();
    let marked = lower
        .find(&START_MARKER.to_ascii_lowercase())
        .zip(lower.rfind(&END_MARKER.to_ascii_lowercase()))
        .filter(|(start, end)| start + START_MARKER.len() <= *end);
    let body = || {
        let open = lower.find("<body")?;
        let open_end = open + lower[open..].find('>')? + 1;
        let close = lower.rfind("</body")?;
        (open_end <= close).then_some((open_end, close))
    };
    let (document, fragment) = if let Some((start, end)) = marked {
        (html.to_owned(), (start + START_MARKER.len(), end))
    } else if let Some((open_end, close)) = body() {
        let mut document = String::with_capacity(html.len() + 64);
        document.push_str(&html[..open_end]);
        document.push_str(START_MARKER);
        let start = document.len();
        document.push_str(&html[open_end..close]);
        let end = document.len();
        document.push_str(END_MARKER);
        document.push_str(&html[close..]);
        (document, (start, end))
    } else {
        let prefix = format!("<html><body>\r\n{START_MARKER}");
        let document = format!("{prefix}{html}{END_MARKER}\r\n</body>\r\n</html>");
        (document, (prefix.len(), prefix.len() + html.len()))
    };
    let header = |values: [usize; 4]| {
        format!(
            "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
            values[0], values[1], values[2], values[3]
        )
    };
    let base = header([0; 4]).len();
    let mut out = header([
        base,
        base + document.len(),
        base + fragment.0,
        base + fragment.1,
    ])
    .into_bytes();
    out.extend_from_slice(document.as_bytes());
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment_of(cf_html: &[u8]) -> &str {
        let header = parse_header(cf_html).unwrap();
        std::str::from_utf8(&cf_html[header.start_fragment.unwrap()..header.end_fragment.unwrap()])
            .unwrap()
    }

    #[test]
    fn header_offsets_select_document_then_fragment() {
        // A Windows-style buffer with CRLF lines, a SourceURL containing
        // colons, and a terminator after EndHTML.
        let document =
            "<html><body>\r\n<!--StartFragment--><b>日本</b><!--EndFragment-->\r\n</body></html>";
        let head = "Version:1.0\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\nSourceURL:https://example.test/a:b\r\n";
        let start = head.len();
        let fragment_start = start + document.find("<b>").unwrap();
        let fragment_end = start + document.find("<!--EndFragment").unwrap();
        let head = format!(
            "Version:1.0\r\nStartHTML:{start:010}\r\nEndHTML:{:010}\r\nStartFragment:{fragment_start:010}\r\nEndFragment:{fragment_end:010}\r\nSourceURL:https://example.test/a:b\r\n",
            start + document.len()
        );
        let mut data = format!("{head}{document}").into_bytes();
        data.push(0);
        assert_eq!(from_cf_html(&data).unwrap(), format!("{CHARSET}{document}"));
        // StartHTML:-1 means no context: the fragment is used instead.
        let no_context = String::from_utf8(data.clone()).unwrap().replacen(
            &format!("StartHTML:{start:010}"),
            "StartHTML:-000000001",
            1,
        );
        assert_eq!(
            from_cf_html(no_context.as_bytes()).unwrap(),
            format!("{CHARSET}<b>日本</b>")
        );
        // An EndHTML past the buffer is clamped to what arrived.
        let clamped = String::from_utf8(data[..data.len() - 1].to_vec())
            .unwrap()
            .replacen(
                &format!("EndHTML:{:010}", start + document.len()),
                &format!("EndHTML:{:010}", start + document.len() + 100),
                1,
            );
        assert_eq!(
            from_cf_html(clamped.as_bytes()).unwrap(),
            format!("{CHARSET}{document}")
        );
    }

    #[test]
    fn malformed_headers_and_offsets_are_refused() {
        for data in [
            &b""[..],
            b"<html>no header</html>",
            b"StartHTML:0\r\nEndHTML:5\r\nhello",
            b"Version:0.9\r\nStartHTML:40\r\nEndHTML:30\r\n<html></html>",
            b"Version:0.9\r\nStartHTML:999\r\nEndHTML:1000\r\n",
            b"Version:0.9\r\nStartHTML:x\r\nEndHTML:y\r\n<p>",
            b"Version:0.9\r\nStartFragment:47\r\nEndFragment:50\r\n   ",
        ] {
            assert!(from_cf_html(data).is_none(), "{data:?}");
        }
        let mut oversize = b"Version:0.9\r\nStartHTML:40\r\nEndHTML:50\r\n".to_vec();
        oversize.resize(MAX_HTML_BYTES + MAX_HEADER_BYTES + 1, b'a');
        assert!(from_cf_html(&oversize).is_none());
    }

    #[test]
    fn generated_offsets_roundtrip_for_fragments_documents_and_markers() {
        let cf = to_cf_html("<meta charset='utf-8'><i>x😀</i>");
        assert_eq!(*cf.last().unwrap(), 0);
        assert_eq!(fragment_of(&cf), "<i>x😀</i>");
        let html = from_cf_html(&cf).unwrap();
        assert!(html.starts_with(CHARSET) && html.contains("<!--StartFragment--><i>x😀</i>"));
        // Converting back keeps the existing markers.
        assert_eq!(fragment_of(&to_cf_html(&html)), "<i>x😀</i>");
        let document = "<HTML><head><style>p{}</style></head><BODY class=a><p>1</p></BODY></HTML>";
        let cf = to_cf_html(document);
        assert_eq!(fragment_of(&cf), "<p>1</p>");
        let header = parse_header(&cf).unwrap();
        assert_eq!(
            &cf[header.start_html.unwrap()..header.end_html.unwrap()],
            document
                .replace("<p>1</p>", "<!--StartFragment--><p>1</p><!--EndFragment-->")
                .as_bytes()
        );
        assert_eq!(header.end_html.unwrap() + 1, cf.len());
    }

    #[test]
    fn wayland_html_bytes_decode_from_utf8_and_utf16() {
        assert_eq!(decode(b"\xef\xbb\xbf<p>a</p>").unwrap(), "<p>a</p>");
        let mut utf16 = vec![0xff, 0xfe];
        for unit in "<p>é</p>".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode(&utf16).unwrap(), "<p>é</p>");
        assert!(decode(b"").is_none());
        assert!(decode(b" \0 ").is_none());
        assert!(decode(&vec![b'a'; MAX_HTML_BYTES + 1]).is_none());
    }
}
