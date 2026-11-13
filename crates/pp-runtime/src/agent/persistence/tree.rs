//! 基于已提交的原生记录重建逻辑会话树，节点只保存日志游标。
mod build;
mod select;
mod view;

use super::*;
use pp_core::todo::TodoPhase;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::agent) struct Selection {
    #[serde(deserialize_with = "required_leaf")]
    pub leaf: Option<String>,
}

fn required_leaf<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::deserialize(de)
}

pub(in crate::agent) struct Prepared {
    pub session: Session,
    pub model_reference: String,
    pub replay: Vec<ReplayRecord>,
    pub selection: Selection,
    pub draft: Option<String>,
    /// 目标 cursor 之前最后一条 State/Workflow 记录折叠出的历史 todo 投影：
    /// 普通 `todos` 无来源，计划投影带 `todo_plan_id`；空 + None 为无清单。
    /// 只保留最新一份，不逐节点复制整个清单。
    pub todos: Vec<TodoPhase>,
    pub todo_plan_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Assistant,
    Tools,
}

#[derive(Clone, Copy, Debug)]
struct Cursor {
    offset: u64,
    len: usize,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub parent: Option<usize>,
    pub kind: Kind,
    pub text: String,
    native: Cursor,
    before: Cursor,
    replay: (u64, u64),
}

#[derive(Default, Debug)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub leaf: Option<usize>,
    pending: (u64, u64),
}

pub struct View {
    pub tree: Tree,
    pub records: Vec<Vec<ReplayRecord>>,
}

#[cfg(test)]
mod fixtures {
    use super::*;
    use serde_json::json;

    pub(super) fn turn(
        journal: &mut SessionJournal,
        session: &mut Session,
        metadata: &Metadata<'_>,
        user: &str,
        answer: &str,
        tools: bool,
    ) {
        match session {
            Session::Completion(request) => request
                .messages
                .push(serde_json::from_value(json!({"role":"user","content":user})).unwrap()),
            Session::Response(request) => super::super::super::turn::response_input(request)
                .push(serde_json::from_value(json!({"role":"user","content":user})).unwrap()),
        }
        journal
            .sent(
                metadata,
                (&*session).into(),
                &WorkflowState::default(),
                Some(user),
                None,
            )
            .unwrap();
        journal.event(&AgentEvent::AssistantStarted);
        journal.event(&AgentEvent::AssistantFinished {
            text: answer.into(),
        });
        if tools {
            for id in ["call-a", "call-b"] {
                journal.event(&AgentEvent::ToolStarted {
                    id: id.into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                });
                journal.event(&AgentEvent::ToolFinished {
                    id: id.into(),
                    result: json!({"ok":true,"content":format!("result for {id}")}),
                });
            }
        }
        match session {
            Session::Completion(request) => {
                let value = if tools {
                    json!({"role":"assistant","content":answer,"tool_calls":[{"id":"call-a","type":"function","function":{"name":"read","arguments":"{}"}},{"id":"call-b","type":"function","function":{"name":"read","arguments":"{}"}}]})
                } else {
                    json!({"role":"assistant","content":answer})
                };
                request
                    .messages
                    .push(serde_json::from_value(value).unwrap());
                if tools {
                    for id in ["call-a", "call-b"] {
                        request.messages.push(
                            serde_json::from_value(
                                json!({"role":"tool","tool_call_id":id,"content":"result"}),
                            )
                            .unwrap(),
                        );
                    }
                }
            }
            Session::Response(request) => {
                let input = super::super::super::turn::response_input(request);
                input.push(serde_json::from_value(json!({"type":"message","id":"message-a","role":"assistant","status":"completed","content":[{"type":"output_text","text":answer,"annotations":[]}]})).unwrap());
                if tools {
                    for id in ["call-a", "call-b"] {
                        input.push(serde_json::from_value(json!({"type":"function_call","call_id":id,"name":"read","arguments":"{}"})).unwrap());
                    }
                    for id in ["call-a", "call-b"] {
                        input.push(serde_json::from_value(json!({"type":"function_call_output","call_id":id,"output":"result"})).unwrap());
                    }
                }
            }
        }
        journal.commit((&*session).into()).unwrap();
    }

    pub(super) fn text(session: &Session) -> String {
        serde_json::to_string(session).unwrap()
    }

    pub(super) fn metadata(dir: &std::path::Path) -> Metadata<'_> {
        Metadata {
            role_route: None,
            cwd: dir,
            model_reference: "fixture/model",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "system",
            prompt_source: None,
        }
    }

    pub(super) fn users(records: &[ReplayRecord]) -> Vec<&str> {
        records
            .iter()
            .filter_map(|record| match record {
                ReplayRecord::User(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    pub(super) fn answers(records: &[ReplayRecord]) -> Vec<&str> {
        records
            .iter()
            .filter_map(|record| match record {
                ReplayRecord::Event(AgentEvent::AssistantFinished { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    pub(super) fn assert_unknown_target(journal: &mut SessionJournal, id: &str) {
        assert_eq!(
            journal.prepare_tree(id).err().unwrap().to_string(),
            "unknown session tree target"
        );
    }
}
