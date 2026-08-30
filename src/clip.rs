//! Builds the OSC 52 escape sequence that asks the terminal emulator to set
//! the local system clipboard — the only channel that reaches back through
//! SSH. Inside tmux the sequence is wrapped in a DCS passthrough envelope so
//! tmux forwards it to the outer terminal untouched.

/// Base64 payload cap (≈ 75 KB of text). Larger copies skip OSC 52 rather
/// than truncate — the internal register keeps the full text either way —
/// because tmux and many terminals cap OSC string length, and a silently
/// clipped clipboard is worse than none.
const MAX_PAYLOAD: usize = 100_000;

/// The full escape sequence that puts `text` on the system clipboard, or
/// `None` when the payload would exceed what terminals reliably accept.
pub fn osc52(text: &str, in_tmux: bool) -> Option<String> {
    let payload = text.len().div_ceil(3) * 4;
    if payload > MAX_PAYLOAD {
        return None;
    }
    let mut seq = String::with_capacity(payload + 24);
    // A passthrough must double every ESC of the inner sequence; base64 and
    // the BEL terminator are ESC-free, so only the OSC leader needs it.
    if in_tmux {
        seq.push_str("\x1bPtmux;\x1b\x1b]52;c;");
    } else {
        seq.push_str("\x1b]52;c;");
    }
    base64(text.as_bytes(), &mut seq);
    seq.push('\x07');
    if in_tmux {
        seq.push_str("\x1b\\");
    }
    Some(seq)
}

fn base64(bytes: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() {
                ALPHABET[(n >> shift) as usize & 63] as char
            } else {
                '='
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_the_rfc_4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in vectors {
            let mut out = String::new();
            base64(input.as_bytes(), &mut out);
            assert_eq!(out, expected, "encoding {input:?}");
        }
    }

    #[test]
    fn osc52_builds_the_plain_sequence() {
        assert_eq!(osc52("foo", false).unwrap(), "\x1b]52;c;Zm9v\x07");
    }

    #[test]
    fn tmux_passthrough_doubles_every_escape() {
        assert_eq!(
            osc52("foo", true).unwrap(),
            "\x1bPtmux;\x1b\x1b]52;c;Zm9v\x07\x1b\\"
        );
    }

    #[test]
    fn an_oversized_copy_skips_the_sequence() {
        let text = "x".repeat(MAX_PAYLOAD);
        assert!(osc52(&text, false).is_none());
        assert!(osc52(&text, true).is_none());
    }
}
