//! Server-sent-events framing.
//!
//! Deliberately small and strict: an SSE frame is only decoded once its
//! terminating blank line has arrived, so a multi-byte UTF-8 character split
//! across two HTTP chunks can never be sliced in half. Tests split frames at
//! every byte offset, including inside a CJK character, and check the result is
//! identical to the unsplit stream.

/// One decoded SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` field, when present.
    pub event: Option<String>,
    /// `data:` field(s), joined with newlines.
    pub data: String,
}

impl SseEvent {
    /// True for the OpenAI-style end-of-stream sentinel.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// Incremental byte-level SSE parser.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes; returns every frame that is now complete.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((frame, consumed)) = next_frame(&self.buffer) {
            if let Some(event) = parse_frame(frame) {
                events.push(event);
            }
            self.buffer.drain(..consumed);
        }
        events
    }

    /// Flush a trailing frame that was not terminated by a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buffer.is_empty() {
            return None;
        }
        let leftover = std::mem::take(&mut self.buffer);
        parse_frame(&leftover)
    }
}

/// Find the next complete frame and how many bytes it occupies.
fn next_frame(buffer: &[u8]) -> Option<(&[u8], usize)> {
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index] == b'\n' {
            // "\n\n" terminates a frame.
            if buffer.get(index + 1) == Some(&b'\n') {
                return Some((&buffer[..index], index + 2));
            }
            // "\n\r\n" terminates a frame too (CRLF line endings).
            if buffer.get(index + 1) == Some(&b'\r') && buffer.get(index + 2) == Some(&b'\n') {
                return Some((&buffer[..index], index + 3));
            }
        }
        index += 1;
    }
    None
}

fn parse_frame(frame: &[u8]) -> Option<SseEvent> {
    // A frame is only decoded whole, so this is always a valid boundary.
    let text = String::from_utf8_lossy(frame);
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => data_lines.push(value),
            "event" => event = Some(value.to_string()),
            // `id` and `retry` are not needed for chat completions.
            _ => {}
        }
    }
    if data_lines.is_empty() && event.is_none() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(decoder: &mut SseDecoder, chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(decoder.push(chunk));
        }
        events
    }

    #[test]
    fn decodes_a_simple_frame() {
        let mut decoder = SseDecoder::new();
        let events = push_all(&mut decoder, &[b"data: hello\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "hello".to_string()
            }]
        );
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut decoder = SseDecoder::new();
        let events = push_all(
            &mut decoder,
            &[b": keep-alive\r\nevent: message\r\ndata: one\r\ndata: two\r\n\r\n"],
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "one\ntwo");
    }

    #[test]
    fn frames_split_anywhere_decode_identically() {
        let payload =
            "data: {\"choices\":[{\"delta\":{\"content\":\"实现世界\"}}]}\n\ndata: [DONE]\n\n";
        // Build the raw bytes and split at every possible offset.
        let bytes = payload.as_bytes();
        let reference = push_all(&mut SseDecoder::new(), &[bytes]);
        assert_eq!(reference.len(), 2);
        assert!(reference[1].is_done());

        for split in 0..bytes.len() {
            let mut decoder = SseDecoder::new();
            let events = push_all(&mut decoder, &[&bytes[..split], &bytes[split..]]);
            assert_eq!(events, reference, "split at byte {split}");
        }
    }

    #[test]
    fn multibyte_characters_split_across_chunks_are_not_mangled() {
        let text = "日本語のテキスト";
        let payload = format!("data: {{\"content\":\"{text}\"}}\n\n");
        let bytes = payload.as_bytes();
        for split in 0..bytes.len() {
            let mut decoder = SseDecoder::new();
            let events = push_all(&mut decoder, &[&bytes[..split], &bytes[split..]]);
            assert_eq!(events.len(), 1, "split at byte {split}");
            assert!(events[0].data.contains(text), "split at byte {split}");
            assert!(!events[0].data.contains('\u{FFFD}'));
        }
    }

    #[test]
    fn incomplete_frames_are_buffered_until_complete() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: partial").is_empty());
        let events = decoder.push(b"\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "partial");
    }

    #[test]
    fn trailing_frame_is_flushed_at_end_of_stream() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: last").is_empty());
        let event = decoder.finish().unwrap();
        assert_eq!(event.data, "last");
        assert!(decoder.finish().is_none());
    }

    #[test]
    fn empty_and_comment_only_frames_are_ignored() {
        let mut decoder = SseDecoder::new();
        assert!(push_all(&mut decoder, &[b"\n\n: ping\n\n"]).is_empty());
    }

    #[test]
    fn data_without_a_space_is_supported() {
        let mut decoder = SseDecoder::new();
        let events = push_all(&mut decoder, &[b"data:[DONE]\n\n"]);
        assert!(events[0].is_done());
    }
}
