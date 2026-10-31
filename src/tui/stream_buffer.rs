//! 只合并显示层的相邻文本；控制事件由调用方先刷新缓冲。
use crate::events::AgentEvent;
const LIMIT: usize = 64 * 1024;
#[derive(Default)]
pub(super) struct StreamBuffer {
    pending: Option<AgentEvent>,
}
impl StreamBuffer {
    pub fn is_full(&self) -> bool {
        matches!(&self.pending, Some(AgentEvent::ThinkingDelta { text } | AgentEvent::AssistantDelta { text, .. }) if text.len() >= LIMIT)
    }
    pub fn take(&mut self) -> Option<AgentEvent> {
        self.pending.take()
    }
    pub fn push(&mut self, event: AgentEvent) -> Option<AgentEvent> {
        let compatible = matches!(
            (&self.pending, &event),
            (
                Some(AgentEvent::ThinkingDelta { .. }),
                AgentEvent::ThinkingDelta { .. }
            )
        ) || matches!((&self.pending, &event),
                (Some(AgentEvent::AssistantDelta { refusal: a, .. }), AgentEvent::AssistantDelta { refusal: b, .. }) if a == b);
        if compatible {
            let pending = self.pending.as_mut().expect("已有同类型文本");
            match (pending, event) {
                (AgentEvent::ThinkingDelta { text: a }, AgentEvent::ThinkingDelta { text: b })
                | (
                    AgentEvent::AssistantDelta { text: a, .. },
                    AgentEvent::AssistantDelta { text: b, .. },
                ) => {
                    a.push_str(&b);
                    if a.len() >= LIMIT {
                        return self.take();
                    }
                    return None;
                }
                _ => unreachable!(),
            }
        }
        if self.pending.is_none()
            && matches!(&event, AgentEvent::ThinkingDelta { text } | AgentEvent::AssistantDelta { text, .. } if text.len() >= LIMIT)
        {
            return Some(event);
        }
        self.pending.replace(event)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stream_buffer_preserves_types_refusals_unicode_and_limit() {
        let mut buffer = StreamBuffer::default();
        for text in ["中", "文🙂"] {
            assert!(
                buffer
                    .push(AgentEvent::ThinkingDelta { text: text.into() })
                    .is_none()
            );
        }
        assert!(
            matches!(buffer.push(AgentEvent::AssistantDelta { text: "ok".into(), refusal: false }), Some(AgentEvent::ThinkingDelta { text }) if text == "中文🙂")
        );
        assert!(
            matches!(buffer.push(AgentEvent::AssistantDelta { text: "no".into(), refusal: true }), Some(AgentEvent::AssistantDelta { text, refusal: false }) if text == "ok")
        );
        assert!(
            matches!(buffer.take(), Some(AgentEvent::AssistantDelta { text, refusal: true }) if text == "no")
        );
        buffer.push(AgentEvent::ThinkingDelta { text: "a".into() });
        assert!(
            matches!(buffer.push(AgentEvent::ThinkingDelta { text: "x".repeat(LIMIT) }), Some(AgentEvent::ThinkingDelta { text }) if text.len() == LIMIT + 1)
        );
        assert!(buffer.take().is_none());
    }
}
