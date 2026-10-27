//! SSE 增量解析：处理任意分块边界、CRLF、注释、多行 data、`[DONE]`。

#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: String,
    pub done: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入原始字节，返回**完整**的 `data:` 负载（不含 `[DONE]`）。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();

        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\n').trim_end_matches('\r');

            if line.is_empty() {
                if !self.data.is_empty() {
                    let payload = std::mem::take(&mut self.data);
                    if payload.trim() == "[DONE]" {
                        self.done = true;
                    } else {
                        out.push(payload);
                    }
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // comment / keep-alive
            }
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest);
            }
            // event:/id:/retry: 忽略
        }
        out
    }

    /// 流结束时冲刷未以空行收尾的最后一个事件。
    pub fn finish(&mut self) -> Option<String> {
        // 最后一行可能没有换行符，按行处理一次。
        if !self.buf.is_empty() {
            let line: Vec<u8> = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\n').trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest);
            }
        }
        if self.data.is_empty() {
            return None;
        }
        let payload = std::mem::take(&mut self.data);
        if payload.trim() == "[DONE]" {
            self.done = true;
            None
        } else {
            Some(payload)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_arbitrary_chunk_boundaries() {
        let raw = "data: {\"a\":1}\n\ndata: {\"a\":2}\n\ndata: [DONE]\n\n";
        for split in 1..raw.len() {
            let mut p = SseParser::new();
            let mut got = p.feed(&raw.as_bytes()[..split]);
            got.extend(p.feed(&raw.as_bytes()[split..]));
            got.extend(p.finish());
            assert_eq!(got, vec!["{\"a\":1}", "{\"a\":2}"], "split at {split}");
            assert!(p.done, "split at {split}");
        }
    }

    #[test]
    fn handles_crlf_comments_and_multiline_data() {
        let raw = ": keep-alive\r\n\r\ndata: line1\r\ndata: line2\r\n\r\n";
        let mut p = SseParser::new();
        let got = p.feed(raw.as_bytes());
        assert_eq!(got, vec!["line1\nline2"]);
    }

    #[test]
    fn flushes_unterminated_event() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"x\":1}").is_empty());
        assert_eq!(p.finish().as_deref(), Some("{\"x\":1}"));
    }
}
