//! 上游 SSE 流的帧解析。
//!
//! 三家上游的流式接口都是 SSE：若干 `data: <载荷>` 行、以空行分帧。
//! 这里只做与格式无关的拆帧——载荷怎么翻译是各家 adapter 的事。

use bytes::Bytes;
use futures_util::{Stream as FutureStream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

/// 把上游字节流解析成 SSE `data:` 载荷流。
///
/// - 多行 data 会按 SSE 规范拼接；
/// - 空行标志一帧结束，此时产出一个载荷；
/// - `data: [DONE]` 原样产出，由上层按各家约定处理（只有 OpenAI 用它）。
pub fn sse_data_stream<S>(stream: S) -> impl FutureStream<Item = crate::error::Result<String>>
where
    S: FutureStream<Item = reqwest::Result<Bytes>> + Unpin,
{
    SseParser {
        inner: stream,
        buffer: String::new(),
        frame: Vec::new(),
    }
}

struct SseParser<S> {
    inner: S,
    /// 跨 chunk 的行缓冲（SSE 行可能被任意切分）。
    buffer: String,
    /// 当前帧积累的 data 行。
    frame: Vec<String>,
}

impl<S> FutureStream for SseParser<S>
where
    S: FutureStream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = crate::error::Result<String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 先把缓冲里能凑成的完整行消化掉。
            while let Some(pos) = self.buffer.find('\n') {
                let line = self.buffer[..pos].trim_end_matches('\r').to_string();
                self.buffer.drain(..=pos);

                if line.is_empty() {
                    // 空行 = 帧结束。
                    if !self.frame.is_empty() {
                        let payload = self.frame.join("\n");
                        self.frame.clear();
                        return Poll::Ready(Some(Ok(payload)));
                    }
                    continue;
                }

                if let Some(payload) = line.strip_prefix("data:") {
                    // SSE 规范：冒号后的一个空格属于语法，不是内容。
                    self.frame
                        .push(payload.strip_prefix(' ').unwrap_or(payload).to_string());
                }
                // 其它行（event:/id:/注释）与翻译无关，忽略。
            }

            match self.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    match std::str::from_utf8(&bytes) {
                        Ok(text) => self.buffer.push_str(text),
                        Err(_) => {
                            return Poll::Ready(Some(Err(crate::error::Error::UpstreamUnavailable(
                                "上游流包含非 UTF-8 数据".to_string(),
                            ))));
                        }
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(crate::error::Error::UpstreamUnavailable(
                        format!("上游流中断: {error}"),
                    ))));
                }
                Poll::Ready(None) => {
                    // 上游断流：把残余当作最后一帧发出去（如果有）。
                    if !self.frame.is_empty() {
                        let payload = self.frame.join("\n");
                        self.frame.clear();
                        return Poll::Ready(Some(Ok(payload)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::stream;

    fn collect(frames: Vec<&str>) -> Vec<String> {
        let bytes: Vec<reqwest::Result<Bytes>> = frames
            .into_iter()
            .map(|frame| Ok(Bytes::from(frame.to_string())))
            .collect();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut parser = sse_data_stream(stream::iter(bytes));
            let mut payloads = Vec::new();
            while let Some(item) = parser.next().await {
                payloads.push(item.unwrap());
            }
            payloads
        })
    }

    #[test]
    fn splits_frames_on_blank_lines() {
        let payloads = collect(vec![
            "data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n",
        ]);

        assert_eq!(payloads, vec!["{\"a\":1}", "{\"b\":2}", "[DONE]"]);
    }

    #[test]
    fn handles_chunks_split_mid_line() {
        // 一个 SSE 帧被任意切开的情形。
        let payloads = collect(vec![
            "data: {\"a\":",
            "1}\n",
            "\ndata: {\"b\":2}\n\n",
        ]);

        assert_eq!(payloads, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn handles_crlf_and_multi_line_data() {
        let payloads = collect(vec!["data: line1\r\ndata: line2\r\n\r\n"]);

        assert_eq!(payloads, vec!["line1\nline2"]);
    }

    #[test]
    fn ignores_comments_and_event_fields() {
        let payloads = collect(vec![": ping\n\nevent: delta\ndata: x\n\n"]);

        assert_eq!(payloads, vec!["x"]);
    }
}
