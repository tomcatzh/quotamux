use std::pin::Pin;

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SseDecodeError {
    FrameTooLarge,
    ReadTimeout,
    Transport,
}

impl SseDecodeError {
    pub const fn safe_message(self) -> &'static str {
        match self {
            Self::FrameTooLarge => "upstream SSE frame exceeded 2 MiB",
            Self::ReadTimeout => "upstream SSE read timed out",
            Self::Transport => "upstream SSE transport failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn encode(&self) -> Bytes {
        let mut output = String::new();
        if let Some(event) = &self.event {
            output.push_str("event: ");
            output.push_str(event);
            output.push('\n');
        }
        for line in self.data.lines() {
            output.push_str("data: ");
            output.push_str(line);
            output.push('\n');
        }
        output.push('\n');
        Bytes::from(output)
    }

    pub fn json(event: impl Into<String>, value: &serde_json::Value) -> Self {
        Self {
            event: Some(event.into()),
            data: value.to_string(),
        }
    }
}

pub struct SseDecoder {
    stream: ByteStream,
    buffer: BytesMut,
    complete: bool,
}

impl SseDecoder {
    pub fn new(stream: ByteStream) -> Self {
        Self {
            stream,
            buffer: BytesMut::new(),
            complete: false,
        }
    }

    pub async fn next_event(&mut self) -> Result<Option<SseEvent>, SseDecodeError> {
        loop {
            if let Some((end, separator_len)) = find_separator(&self.buffer) {
                let frame = self.buffer.split_to(end);
                let _ = self.buffer.split_to(separator_len);
                if let Some(event) = parse_frame(&frame) {
                    return Ok(Some(event));
                }
                continue;
            }
            if self.complete {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                let frame = self.buffer.split().freeze();
                return Ok(parse_frame(&frame));
            }
            match self.stream.next().await {
                Some(Ok(bytes)) => {
                    self.buffer.extend_from_slice(&bytes);
                    if self.buffer.len() > 2 * 1024 * 1024 {
                        return Err(SseDecodeError::FrameTooLarge);
                    }
                }
                Some(Err(error)) if error.is_timeout() => {
                    return Err(SseDecodeError::ReadTimeout);
                }
                Some(Err(_)) => return Err(SseDecodeError::Transport),
                None => self.complete = true,
            }
        }
    }
}

fn find_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| (index, 2))
        })
}

fn parse_frame(frame: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(frame);
    let mut event = None;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(SseEvent {
            event,
            data: data.join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[tokio::test]
    async fn decodes_fragmented_events_and_ignores_comments() {
        let chunks = stream::iter(vec![
            Ok(Bytes::from(": keep-alive\n\ndata: {\"a\"")),
            Ok(Bytes::from(":1}\n\nevent: done\ndata: [DONE]\n\n")),
        ]);
        let mut decoder = SseDecoder::new(Box::pin(chunks));
        assert_eq!(
            decoder.next_event().await.unwrap().unwrap().data,
            "{\"a\":1}"
        );
        let done = decoder.next_event().await.unwrap().unwrap();
        assert_eq!(done.event.as_deref(), Some("done"));
        assert_eq!(done.data, "[DONE]");
        assert!(decoder.next_event().await.unwrap().is_none());
    }
}
