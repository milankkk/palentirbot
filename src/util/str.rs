use std::borrow::Cow;

/// Truncate `s` to at most `max_chars` Unicode scalar values in a single pass.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Truncate `s` to at most `max_chars` total Unicode scalar values, appending `ellipsis` if cut.
pub fn truncate_with_ellipsis<'a>(s: &'a str, max_chars: usize, ellipsis: &str) -> Cow<'a, str> {
    if s.char_indices().nth(max_chars).is_none() {
        return Cow::Borrowed(s);
    }

    let keep = max_chars.saturating_sub(ellipsis.chars().count());
    let head = truncate_chars(s, keep);
    let mut out = String::with_capacity(head.len() + ellipsis.len());
    out.push_str(head);
    out.push_str(ellipsis);
    Cow::Owned(out)
}

/// Decode `%XX` percent-encoded sequences as UTF-8, lossily replacing invalid byte sequences.
pub fn percent_decode_utf8(input: &str) -> String {
    if !input.contains('%') {
        return input.to_owned();
    }

    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8(out)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[inline]
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
