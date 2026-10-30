//! 子 Agent 控制面板；关闭面板不改变任务生命周期。
use crate::subagent::Binding;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph},
};
use serde_json::Value;

#[derive(Default)]
pub(super) struct Panel {
    pub binding: Option<Binding>,
    pub roles: bool,
    pub selected: usize,
    pub detail: bool,
    pub scroll: u16,
    pub message: Option<String>,
    pub error: Option<String>,
    pub wake_parent: bool,
}
impl Panel {
    fn snapshot(&self) -> Value {
        self.binding.as_ref().map_or_else(
            || serde_json::json!({"agents":[],"runs":[]}),
            Binding::snapshots,
        )
    }
    fn selected_id(&self) -> Option<String> {
        ordered_agents(&self.snapshot()["agents"]).get(self.selected)?["id"]
            .as_str()
            .map(str::to_owned)
    }
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let snapshot = self.snapshot();
        let agents = ordered_agents(&snapshot["agents"]);
        let mut lines = vec![Line::from(if self.roles {
            "Role definitions (Project > User > Built-in)"
        } else {
            "↑↓ Select · Enter History · e Follow-up · c Cancel · x Stop Agent · r Resume · Esc Back"
        })];
        if self.roles {
            if let Some(binding) = &self.binding {
                match binding.roles() {
                    Ok(roles) => {
                        for role in roles {
                            lines.push(Line::from(format!(
                                "{} — {} [{}]",
                                role.name, role.description, role.source
                            )));
                        }
                    }
                    Err(e) => lines.push(Line::from(e.to_string())),
                }
            }
        } else if let Some(text) = &self.message {
            lines.push(Line::from(
                "Enter follow-up input; Enter to send, Esc to go back (no local commands)",
            ));
            lines.push(Line::from(text.clone()));
        } else if self.detail {
            if let (Some(binding), Some(id)) = (&self.binding, self.selected_id()) {
                match binding.uri(
                    "read",
                    &format!("history://{id}"),
                    None,
                    &Default::default(),
                ) {
                    Ok(value) => {
                        for entry in value["history"].as_str().unwrap_or("").lines() {
                            if let Ok(entry) = serde_json::from_str::<Value>(entry) {
                                let event = &entry["event"];
                                lines.push(Line::from(format!("{}", event)));
                            }
                        }
                    }
                    Err(e) => lines.push(Line::from(e.to_string())),
                }
            }
        } else {
            for (index, agent) in agents.iter().enumerate() {
                let requests = snapshot["runs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r["agent_id"] == agent["id"])
                    .map(|r| r["requests"].as_u64().unwrap_or(0))
                    .sum::<u64>();
                let runs = snapshot["runs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r["agent_id"] == agent["id"])
                    .collect::<Vec<_>>();
                let tokens = runs
                    .iter()
                    .map(|r| r["tokens"].as_u64().unwrap_or(0))
                    .sum::<u64>();
                let cost = runs
                    .iter()
                    .map(|r| r["cost"].as_f64())
                    .collect::<Option<Vec<_>>>()
                    .map(|rates| format!("${:.6}", rates.iter().sum::<f64>()))
                    .unwrap_or_else(|| "cost —".into());
                lines.push(Line::from(format!(
                    "{} {}{} {} [{}] {} · {} requests · {tokens} tokens · {cost}",
                    if index == self.selected { "›" } else { " " },
                    "  ".repeat(
                        agent["spec"]["depth"]
                            .as_u64()
                            .unwrap_or(1)
                            .saturating_sub(1)
                            .min(8) as usize
                    ),
                    agent["id"].as_str().unwrap_or(""),
                    agent["name"].as_str().unwrap_or(""),
                    agent["status"].as_str().unwrap_or(""),
                    agent["spec"]["model"].as_str().unwrap_or(""),
                    requests
                )));
            }
            if agents.is_empty() {
                lines.push(Line::from(
                    "No subagents yet. Spawned agents will appear here.",
                ));
            }
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(error.clone()));
        }
        let lines = lines
            .into_iter()
            .map(|line| {
                Line::from(super::transcript_style::display_text(&line.to_string()).into_owned())
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).scroll((self.scroll, 0)).block(
                Block::default().borders(Borders::ALL).title(if self.roles {
                    " Agents "
                } else {
                    " Subagents "
                }),
            ),
            area,
        );
    }
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if let Some(text) = &mut self.message {
            match key.code {
                KeyCode::Esc => self.message = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                KeyCode::Enter => {
                    let text = text.clone();
                    if let (Some(binding), Some(id)) = (&self.binding, self.selected_id()) {
                        self.error = binding
                            .control(&id, "send", Some(&text))
                            .err()
                            .map(|e| e.to_string());
                        self.wake_parent = self.error.is_none();
                        self.message = None;
                    }
                }
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                if self.detail {
                    self.detail = false;
                    self.scroll = 0;
                } else {
                    return true;
                }
            }
            KeyCode::Up => {
                if self.detail {
                    self.scroll = self.scroll.saturating_sub(1);
                } else {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if self.detail {
                    self.scroll = self.scroll.saturating_add(1);
                } else {
                    self.selected = (self.selected + 1).min(
                        self.snapshot()["agents"]
                            .as_array()
                            .map_or(0, |a| a.len().saturating_sub(1)),
                    );
                }
            }
            KeyCode::Enter => {
                self.detail = true;
                self.scroll = 0;
            }
            KeyCode::Char('e') => self.message = Some(String::new()),
            KeyCode::Char(c @ ('c' | 'x' | 'r')) => {
                if let (Some(binding), Some(id)) = (&self.binding, self.selected_id()) {
                    self.error = binding
                        .control(
                            &id,
                            match c {
                                'c' => "cancel",
                                'x' => "kill",
                                _ => "revive",
                            },
                            None,
                        )
                        .err()
                        .map(|e| e.to_string());
                }
            }
            _ => {}
        }
        false
    }
}

fn ordered_agents(value: &Value) -> Vec<Value> {
    let all = value.as_array().cloned().unwrap_or_default();
    fn visit(
        parent: &str,
        all: &[Value],
        seen: &mut std::collections::BTreeSet<String>,
        out: &mut Vec<Value>,
    ) {
        let mut children = all
            .iter()
            .filter(|a| a["parent"] == parent)
            .collect::<Vec<_>>();
        children.sort_by_key(|a| {
            a["id"]
                .as_str()
                .and_then(|s| s.strip_prefix('a'))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        for child in children {
            if let Some(id) = child["id"].as_str()
                && seen.insert(id.to_owned())
            {
                out.push(child.clone());
                visit(id, all, seen, out);
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    visit("Main", &all, &mut seen, &mut out);
    for agent in all {
        if agent["id"]
            .as_str()
            .is_some_and(|id| seen.insert(id.to_owned()))
        {
            out.push(agent);
        }
    }
    out
}
#[cfg(test)]
mod tests {
    #[test]
    fn tree_selection_orders_children_with_their_parent() {
        let rows = super::ordered_agents(
            &serde_json::json!([{"id":"a10","parent":"Main"},{"id":"a2","parent":"Main"},{"id":"a1","parent":"Main"},{"id":"a3","parent":"a1"}]),
        );
        assert_eq!(
            rows.iter()
                .map(|r| r["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a1", "a3", "a2", "a10"]
        );
    }
}
