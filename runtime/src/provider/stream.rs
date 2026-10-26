use super::types::{ProviderError, StreamEvent};

/// Upper bound for a single buffered line, guarding against a misbehaving endpoint.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Splits a byte stream into complete UTF-8 lines; partial lines (and split code points) stay buffered.
#[derive(Default)]
pub struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, ProviderError> {
        self.buf.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=i).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(String::from_utf8(line).map_err(|_| ProviderError::protocol("stream contained invalid UTF-8"))?);
        }
        if self.buf.len() > MAX_LINE_BYTES {
            return Err(ProviderError::protocol("stream line exceeded size limit"));
        }
        Ok(lines)
    }

    pub fn finish(&mut self) -> Result<Option<String>, ProviderError> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        let rest = std::mem::take(&mut self.buf);
        String::from_utf8(rest).map(Some).map_err(|_| ProviderError::protocol("stream contained invalid UTF-8"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Minimal Server-Sent Events parser (event + multi-line data fields).
#[derive(Default)]
pub struct SseParser {
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn feed_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').map_or((line, ""), |(f, v)| (f, v.strip_prefix(' ').unwrap_or(v)));
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }

    pub fn flush(&mut self) -> Option<SseEvent> {
        self.dispatch()
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        Some(SseEvent { event, data: std::mem::take(&mut self.data).join("\n") })
    }
}

/// Converts provider wire lines into normalized stream events. One instance per response.
pub trait StreamDecoder: Send {
    fn feed_line(&mut self, line: &str) -> Result<Vec<StreamEvent>, ProviderError>;
    /// Called at end of body; must emit `Done` if the provider never signalled completion.
    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_buffer_handles_split_chunks_and_crlf() {
        let mut b = LineBuffer::default();
        assert!(b.push(b"hel").unwrap().is_empty());
        assert_eq!(b.push(b"lo\r\nwor").unwrap(), vec!["hello"]);
        assert_eq!(b.push(b"ld\n\n").unwrap(), vec!["world", ""]);
        assert_eq!(b.finish().unwrap(), None);
    }

    #[test]
    fn line_buffer_keeps_split_utf8_code_points() {
        let bytes = "é\n".as_bytes();
        let mut b = LineBuffer::default();
        assert!(b.push(&bytes[..1]).unwrap().is_empty());
        assert_eq!(b.push(&bytes[1..]).unwrap(), vec!["é"]);
    }

    #[test]
    fn sse_parser_joins_data_and_ignores_comments() {
        let mut p = SseParser::default();
        for l in [": ping", "event: message_start", "data: {\"a\":", "data: 1}"] {
            assert!(p.feed_line(l).is_none());
        }
        assert_eq!(p.feed_line(""), Some(SseEvent { event: Some("message_start".into()), data: "{\"a\":\n1}".into() }));
        assert!(p.feed_line("").is_none());
        p.feed_line("data:[DONE]");
        assert_eq!(p.flush().unwrap().data, "[DONE]");
    }
}
