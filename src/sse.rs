//! Incremental parser for `text/event-stream` bodies.
//!
//! Works on raw bytes so multi-byte UTF-8 characters split across chunks stay intact.

pub struct SseEvent {
    #[allow(dead_code)] // kept for debugging; payloads carry their own `type`
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// Bytes of `buf` already searched for an event boundary.
    scanned: usize,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((end, sep_len)) = self.find_boundary() {
            let raw: Vec<u8> = self.buf.drain(..end + sep_len).collect();
            self.scanned = 0;
            if let Some(event) = parse_event(&String::from_utf8_lossy(&raw[..end])) {
                events.push(event);
            }
        }
        // Re-check the last few bytes next time in case a separator straddles chunks.
        self.scanned = self.buf.len().saturating_sub(3);
        events
    }

    fn find_boundary(&self) -> Option<(usize, usize)> {
        let start = self.scanned.min(self.buf.len());
        let hay = &self.buf[start..];
        for i in 0..hay.len() {
            if hay[i..].starts_with(b"\n\n") {
                return Some((start + i, 2));
            }
            if hay[i..].starts_with(b"\r\n\r\n") {
                return Some((start + i, 4));
            }
        }
        None
    }
}

fn parse_event(block: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_across_chunks() {
        let mut parser = SseParser::default();
        let body = "event: a\ndata: {\"t\":\"안녕\"}\n\n: comment\n\ndata: x\r\ndata: y\r\n\r\n";
        let bytes = body.as_bytes();
        let mut events = Vec::new();
        // Feed one byte at a time to split UTF-8 sequences and separators.
        for b in bytes {
            events.extend(parser.push(std::slice::from_ref(b)));
        }
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("a"));
        assert_eq!(events[0].data, "{\"t\":\"안녕\"}");
        assert_eq!(events[1].data, "x\ny");
    }
}
