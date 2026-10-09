//! Synchronous, bounded observation of consumed JSON/SSE/WebSocket frames.
//! This parser never drives a stream, writes PostgreSQL, or assumes an EOF.
use crate::collection::{
    ContentBlock, ContextMessage, ResponseSnapshot, WINDOW_CHARACTERS, limit_window,
};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_BUFFER_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_ITEMS: usize = 64;
const MAX_CONTENT_BLOCKS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub enum ResponseFormat {
    Json,
    Sse,
    WebSocket,
}

#[derive(Default)]
struct OutputItem {
    id: Option<String>,
    phase: Option<String>,
    blocks: BTreeMap<usize, String>,
    assistant: bool,
    truncated: bool,
}

pub struct ResponseAccumulator {
    format: ResponseFormat,
    buffer: Vec<u8>,
    response_id: Option<String>,
    items: BTreeMap<usize, OutputItem>,
    complete: bool,
    overflowed: bool,
}

impl ResponseAccumulator {
    pub fn new(format: ResponseFormat) -> Self {
        Self {
            format,
            buffer: vec![],
            response_id: None,
            items: BTreeMap::new(),
            complete: false,
            overflowed: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.buffer.len().saturating_add(bytes.len()) > MAX_BUFFER_BYTES {
            self.buffer.clear();
            self.overflowed = true;
            self.complete = false;
            return;
        }
        self.buffer.extend_from_slice(bytes);
        match self.format {
            ResponseFormat::Sse => loop {
                let lf = self
                    .buffer
                    .windows(2)
                    .position(|bytes| bytes == b"\n\n")
                    .map(|position| (position, 2));
                let crlf = self
                    .buffer
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                    .map(|position| (position, 4));
                let Some((position, delimiter)) = lf
                    .into_iter()
                    .chain(crlf)
                    .min_by_key(|(position, _)| *position)
                else {
                    break;
                };
                let event: Vec<_> = self.buffer.drain(..position + delimiter).collect();
                let Ok(event) = std::str::from_utf8(&event) else {
                    continue;
                };
                let data = event
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data:")
                            .map(|text| text.strip_prefix(' ').unwrap_or(text))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Ok(value) = serde_json::from_str(&data) {
                    self.observe(&value);
                }
            },
            ResponseFormat::Json | ResponseFormat::WebSocket => {
                if let Ok(value) = serde_json::from_slice::<Value>(&self.buffer) {
                    self.buffer.clear();
                    self.observe(&value);
                }
            }
        }
    }

    /// An explicit protocol response ID is required to link any partial output.
    pub fn snapshot(&self) -> Option<ResponseSnapshot> {
        let response_id = self.response_id.clone()?;
        let messages = self
            .items
            .values()
            .filter(|item| item.assistant && !item.blocks.is_empty())
            .map(|item| ContextMessage {
                role: "assistant".to_owned(),
                phase: item.phase.clone(),
                truncated: item.truncated,
                blocks: item
                    .blocks
                    .values()
                    .map(|text| ContentBlock::Text { text: text.clone() })
                    .collect(),
            })
            .collect::<Vec<_>>();
        Some(ResponseSnapshot {
            response_id,
            messages: limit_window(&messages),
            complete: self.complete && !self.overflowed,
        })
    }

    fn observe(&mut self, value: &Value) {
        let event = value.get("type").and_then(Value::as_str).unwrap_or("");
        let response = value.get("response").unwrap_or(value);
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            // Output item IDs are not response IDs.
            if (event.starts_with("response.") && value.get("response").is_some())
                || (!event.starts_with("response.")
                    && (response.get("output").is_some()
                        || response.get("object").and_then(Value::as_str) == Some("response")))
            {
                self.response_id = Some(id.to_owned());
            }
        }
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            for (position, item) in output.iter().enumerate().take(MAX_OUTPUT_ITEMS) {
                self.merge_item(position, item);
            }
        }
        match event {
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    let position =
                        self.output_position(value, item.get("id").and_then(Value::as_str));
                    self.merge_item(position, item);
                }
            }
            "response.content_part.added" | "response.content_part.done" => {
                if let Some(part) = value.get("part") {
                    let position =
                        self.output_position(value, value.get("item_id").and_then(Value::as_str));
                    let index = value
                        .get("content_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    if let Some(text) = part
                        .get("text")
                        .or_else(|| part.get("refusal"))
                        .and_then(Value::as_str)
                    {
                        self.write_text(position, index, text, false);
                    }
                }
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done" => {
                let position =
                    self.output_position(value, value.get("item_id").and_then(Value::as_str));
                let index = value
                    .get("content_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let delta = event.ends_with(".delta");
                if let Some(text) = value
                    .get(if delta {
                        "delta"
                    } else if event.starts_with("response.refusal") {
                        "refusal"
                    } else {
                        "text"
                    })
                    .and_then(Value::as_str)
                {
                    self.write_text(position, index, text, delta);
                }
            }
            "response.completed" => self.complete = true,
            "response.incomplete" | "response.failed" | "error" => self.complete = false,
            _ => {
                if !event.starts_with("response.") && response.get("output").is_some() {
                    self.complete = response
                        .get("status")
                        .and_then(Value::as_str)
                        .is_none_or(|status| status == "completed");
                }
            }
        }
    }

    fn output_position(&self, value: &Value, id: Option<&str>) -> usize {
        value
            .get("output_index")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .or_else(|| {
                id.and_then(|id| {
                    self.items
                        .iter()
                        .find(|(_, item)| item.id.as_deref() == Some(id))
                        .map(|(index, _)| *index)
                })
            })
            .unwrap_or(0)
    }

    fn merge_item(&mut self, position: usize, item: &Value) {
        if position >= MAX_OUTPUT_ITEMS {
            self.overflowed = true;
            return;
        }
        let entry = self.items.entry(position).or_default();
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            entry.id = Some(id.to_owned());
        }
        if let Some(phase) = item
            .get("phase")
            .and_then(Value::as_str)
            .filter(|phase| !phase.contains('\0') && phase.chars().count() <= 80)
        {
            entry.phase = Some(phase.to_owned());
        }
        if let Some(role) = item.get("role").and_then(Value::as_str) {
            entry.assistant = role == "assistant";
        }
        if let Some(content) = item.get("content").and_then(Value::as_array) {
            for (index, block) in content.iter().enumerate().take(MAX_CONTENT_BLOCKS) {
                if block
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "output_text" | "text" | "refusal"))
                    && let Some(text) = block
                        .get("text")
                        .or_else(|| block.get("refusal"))
                        .and_then(Value::as_str)
                {
                    self.write_text(position, index, text, false);
                }
            }
        }
    }

    fn write_text(&mut self, position: usize, index: usize, text: &str, append: bool) {
        if position >= MAX_OUTPUT_ITEMS || index >= MAX_CONTENT_BLOCKS {
            self.overflowed = true;
            return;
        }
        let item = self.items.entry(position).or_default();
        // output_text events are protocol evidence of an assistant text item.
        item.assistant = true;
        let block = item.blocks.entry(index).or_default();
        if append {
            block.push_str(text);
        } else {
            *block = text.to_owned();
        }
        let length = block.chars().count();
        if length > WINDOW_CHARACTERS {
            *block = block.chars().skip(length - WINDOW_CHARACTERS).collect();
            item.truncated = true;
        }
        self.trim_text_budget();
    }

    fn trim_text_budget(&mut self) {
        let mut total: usize = self
            .items
            .values()
            .flat_map(|item| item.blocks.values())
            .map(|text| text.chars().count())
            .sum();
        while total > WINDOW_CHARACTERS {
            let populated: Vec<usize> = self
                .items
                .iter()
                .filter(|(_, item)| !item.blocks.is_empty())
                .map(|(index, _)| *index)
                .collect();
            if populated.len() > 1 {
                let item = self.items.get_mut(&populated[0]).unwrap();
                total -= item
                    .blocks
                    .values()
                    .map(|text| text.chars().count())
                    .sum::<usize>();
                item.blocks.clear();
            } else {
                let Some(index) = populated.first() else {
                    break;
                };
                let item = self.items.get_mut(index).unwrap();
                let first = *item.blocks.keys().next().unwrap();
                let text = item.blocks.get_mut(&first).unwrap();
                let length = text.chars().count();
                let remove = length.min(total - WINDOW_CHARACTERS);
                *text = text.chars().skip(remove).collect();
                total -= remove;
                item.truncated = true;
                if text.is_empty() && item.blocks.len() > 1 {
                    item.blocks.remove(&first);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(accumulator: &mut ResponseAccumulator, value: Value) {
        accumulator.push(value.to_string().as_bytes());
    }

    #[test]
    fn websocket_delta_done_and_complete_replace_one_protocol_output() {
        let mut accumulator = ResponseAccumulator::new(ResponseFormat::WebSocket);
        event(
            &mut accumulator,
            json!({"type":"response.created","response":{"id":"r1"}}),
        );
        event(
            &mut accumulator,
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"a1","role":"assistant","phase":"commentary","content":[]}}),
        );
        event(
            &mut accumulator,
            json!({"type":"response.output_text.delta","item_id":"a1","content_index":0,"delta":"你"}),
        );
        event(
            &mut accumulator,
            json!({"type":"response.output_text.delta","item_id":"a1","content_index":0,"delta":"好🙂"}),
        );
        event(
            &mut accumulator,
            json!({"type":"response.output_text.done","item_id":"a1","content_index":0,"text":"你好🙂"}),
        );
        event(
            &mut accumulator,
            json!({"type":"response.completed","response":{"id":"r1","output":[{"id":"a1","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"你好🙂"}]}]}}),
        );
        let snapshot = accumulator.snapshot().unwrap();
        assert!(snapshot.complete);
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(
            snapshot.messages[0].blocks,
            vec![ContentBlock::Text {
                text: "你好🙂".to_owned()
            }]
        );
    }

    #[test]
    fn sse_split_frames_and_partial_cancellation_preserve_only_consumed_text() {
        let mut accumulator = ResponseAccumulator::new(ResponseFormat::Sse);
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\r\n\r\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"part\"}\n\n"
        );
        for chunk in body.as_bytes().chunks(7) {
            accumulator.push(chunk);
        }
        let snapshot = accumulator.snapshot().unwrap();
        assert!(!snapshot.complete);
        assert_eq!(
            snapshot.messages[0].blocks,
            vec![ContentBlock::Text {
                text: "part".to_owned()
            }]
        );
    }

    #[test]
    fn json_output_and_multiple_protocol_items_keep_order_and_phase() {
        let mut accumulator = ResponseAccumulator::new(ResponseFormat::Json);
        let body = json!({"id":"r1","status":"completed","output":[
            {"type":"reasoning","summary":[{"text":"private"}]},
            {"id":"a1","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"plan"}]},
            {"id":"a2","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"answer"}]}
        ]}).to_string();
        for chunk in body.as_bytes().chunks(13) {
            accumulator.push(chunk);
        }
        let snapshot = accumulator.snapshot().unwrap();
        assert!(snapshot.complete);
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.messages[1].phase.as_deref(), Some("final_answer"));
    }

    #[test]
    fn no_protocol_id_and_unconsumed_final_output_cannot_claim_completion() {
        let mut accumulator = ResponseAccumulator::new(ResponseFormat::WebSocket);
        event(
            &mut accumulator,
            json!({"type":"response.output_text.delta","delta":"x"}),
        );
        assert!(accumulator.snapshot().is_none());
    }

    #[test]
    fn mixed_sse_delimiters_in_one_consumed_frame_are_parsed_in_wire_order() {
        let mut accumulator = ResponseAccumulator::new(ResponseFormat::Sse);
        accumulator.push(
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\r\n\r\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"text\"}\n\n"
            )
            .as_bytes(),
        );
        let snapshot = accumulator.snapshot().unwrap();
        assert_eq!(snapshot.response_id, "r");
        assert_eq!(snapshot.messages.len(), 1);
    }

    #[test]
    fn refusal_delta_done_and_json_final_are_the_same_assistant_text() {
        let mut accumulator = ResponseAccumulator::new(ResponseFormat::WebSocket);
        event(
            &mut accumulator,
            serde_json::json!({"type":"response.created","response":{"id":"r"}}),
        );
        event(
            &mut accumulator,
            serde_json::json!({"type":"response.refusal.delta","output_index":0,"content_index":0,"delta":"cannot "}),
        );
        event(
            &mut accumulator,
            serde_json::json!({"type":"response.refusal.done","output_index":0,"content_index":0,"refusal":"cannot help"}),
        );
        event(
            &mut accumulator,
            serde_json::json!({"type":"response.completed","response":{"id":"r","output":[{"role":"assistant","content":[{"type":"refusal","refusal":"cannot help"}]}]}}),
        );
        assert_eq!(
            accumulator.snapshot().unwrap().messages[0].blocks,
            vec![ContentBlock::Text {
                text: "cannot help".to_owned()
            }]
        );
    }

    #[test]
    fn multi_block_output_retains_a_bounded_unicode_suffix() {
        let mut accumulator = ResponseAccumulator::new(ResponseFormat::Json);
        let blocks: Vec<Value> = (0..64)
            .map(|_| serde_json::json!({"type":"output_text","text":"🙂".repeat(1000)}))
            .collect();
        event(
            &mut accumulator,
            serde_json::json!({"id":"r","status":"completed","output":[{"role":"assistant","content":blocks}]}),
        );
        let snapshot = accumulator.snapshot().unwrap();
        assert!(snapshot.complete);
        assert_eq!(snapshot.messages.len(), 1);
        assert!(snapshot.messages[0].truncated);
        assert_eq!(
            snapshot.messages[0]
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => text.chars().count(),
                    _ => 0,
                })
                .sum::<usize>(),
            WINDOW_CHARACTERS
        );
    }

    #[test]
    fn event_and_output_item_ids_cannot_replace_the_protocol_response_id() {
        let mut accumulator = ResponseAccumulator::new(ResponseFormat::WebSocket);
        event(
            &mut accumulator,
            serde_json::json!({"type":"response.created","response":{"id":"protocol-r"}}),
        );
        event(
            &mut accumulator,
            serde_json::json!({"type":"response.output_text.delta","id":"event-id","item_id":"message-id","delta":"text"}),
        );
        assert_eq!(accumulator.snapshot().unwrap().response_id, "protocol-r");
    }
}
