//! 基于解释器语法与真实执行事件生成 OMP 风格的命令展示。

use brush_core::parser::{
    SourceSpan,
    ast::{self, SourceLocation},
};
use pp_core::shell_display::{
    ShellPresentation, ShellRow, ShellSegment, ShellState, ShellStyle, VERSION,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

mod printer;
#[cfg(test)]
mod tests;
mod words;

const MAX_NODES: usize = 4096;
const MAX_DEPTH: u16 = 64;

#[derive(Clone)]
pub(super) struct Trace(Arc<Mutex<Model>>);

#[derive(Default)]
struct Model {
    revision: u64,
    next_unit: u64,
    next_execution: u64,
    running: bool,
    rows: Vec<ShellRow>,
    nodes: Vec<Node>,
    keys: HashMap<(u64, usize, usize, u8), u32>,
    executions: HashMap<u64, Execution>,
    root: Option<u64>,
    fallback: bool,
    prepared: bool,
}

struct Node {
    unit: u64,
    start: usize,
    end: usize,
    state: ShellState,
    live: u32,
    runs: u64,
    duration: Duration,
    status: Option<u8>,
    iterations: u64,
    looping: bool,
    leaf: bool,
    guard: bool,
    function_form: Vec<ShellSegment>,
    external_form: Vec<ShellSegment>,
    builtin_form: Vec<ShellSegment>,
}

struct Execution {
    node: u32,
    started: Instant,
}

impl Trace {
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(Model {
            running: true,
            ..Model::default()
        })))
    }

    fn with<R>(&self, f: impl FnOnce(&mut Model) -> R) -> R {
        f(&mut self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    pub(super) fn prepare(&self, source: &str) {
        let mut reader = std::io::Cursor::new(source.as_bytes());
        if let Ok(program) =
            brush_core::parser::Parser::new(&mut reader, &Default::default()).parse_program()
        {
            self.parsed(&program, Some(source), None);
            self.with(|model| model.prepared = true);
        }
    }

    pub(super) fn revision(&self) -> u64 {
        self.with(|model| model.revision)
    }

    pub(super) fn parsed(
        &self,
        program: &ast::Program,
        source: Option<&str>,
        parent: Option<u64>,
    ) -> Option<u64> {
        self.with(|model| {
            if model.prepared && parent.is_none() {
                model.prepared = false;
                return model.root;
            }
            if model.fallback || model.nodes.len() >= MAX_NODES {
                return None;
            }
            model.next_unit += 1;
            let unit = model.next_unit;
            let root = model.root.is_none();
            if root {
                model.root = Some(unit);
            }
            let parent_row =
                parent
                    .and_then(|id| model.executions.get(&id))
                    .and_then(|execution| {
                        model.rows.iter().position(|row| {
                            row.node == Some(execution.node)
                                || row
                                    .segments
                                    .iter()
                                    .any(|segment| segment.node == Some(execution.node))
                        })
                    });
            let depth = parent_row.map_or(0, |index| model.rows[index].depth.saturating_add(1));
            let insert_at = parent_row.map(|index| {
                (index + 1..model.rows.len())
                    .find(|next| model.rows[*next].depth < depth)
                    .unwrap_or(model.rows.len())
            });
            let first_row = model.rows.len();
            let first_node = model.nodes.len();
            let mut printer = printer::Printer::new(model, unit, source, depth);
            printer.program(program);
            if printer.overflow {
                // 所有展示转换都是可丢弃的；超限不能改变解释器行为。
                printer.model.rows.truncate(first_row);
                printer.model.nodes.truncate(first_node);
                printer
                    .model
                    .keys
                    .retain(|_, node| (*node as usize) < first_node);
                if root {
                    let source = source
                        .map(str::to_owned)
                        .unwrap_or_else(|| program.to_string());
                    for line in source.lines() {
                        printer.model.rows.push(row(
                            0,
                            None,
                            vec![segment(line, ShellStyle::Text, None)],
                            false,
                        ));
                    }
                    printer.model.fallback = true;
                }
            }
            if let Some(insert_at) = insert_at {
                let child = model.rows.split_off(first_row);
                model.rows.splice(insert_at..insert_at, child);
            }
            model.revision += 1;
            Some(unit)
        })
    }

    pub(super) fn begin(&self, unit: Option<u64>, command: &ast::Command) -> Option<u64> {
        self.with(|model| {
            let unit = unit?;
            let span = command.location()?;
            let node = *model
                .keys
                .get(&(unit, span.start.index, span.end.index, kind(command)))?;
            let state = &mut model.nodes[node as usize];
            state.live = state.live.saturating_add(1);
            state.runs = state.runs.saturating_add(1);
            state.state = ShellState::Running;
            model.next_execution = model.next_execution.checked_add(1)?;
            let execution = model.next_execution;
            model.executions.insert(
                execution,
                Execution {
                    node,
                    started: Instant::now(),
                },
            );
            model.revision += 1;
            Some(execution)
        })
    }

    pub(super) fn end(&self, execution: u64, status: Option<u8>) {
        self.with(|model| {
            let Some(execution) = model.executions.remove(&execution) else {
                return;
            };
            let node = &mut model.nodes[execution.node as usize];
            node.live = node.live.saturating_sub(1);
            node.duration = node.duration.saturating_add(execution.started.elapsed());
            node.status = status;
            node.state = if node.live > 0 {
                ShellState::Running
            } else {
                match status {
                    Some(0) => ShellState::Ok,
                    Some(_) => ShellState::Fail,
                    None => ShellState::Interrupted,
                }
            };
            model.revision += 1;
        });
    }

    pub(super) fn resolved(&self, execution: u64, resolved: &str) {
        if !matches!(resolved, "function" | "external" | "builtin") {
            return;
        }
        self.with(|model| {
            let Some(execution) = model.executions.get(&execution) else {
                return;
            };
            let node = execution.node;
            let alternative = match resolved {
                "external" => &model.nodes[node as usize].external_form,
                "builtin" => &model.nodes[node as usize].builtin_form,
                _ => &model.nodes[node as usize].function_form,
            };
            if alternative.is_empty() {
                return;
            }
            for row in &mut model.rows {
                let Some(first) = row.segments.iter().position(|s| s.node == Some(node)) else {
                    continue;
                };
                let count = row.segments[first..]
                    .iter()
                    .take_while(|s| s.node == Some(node))
                    .count();
                row.segments
                    .splice(first..first + count, alternative.iter().cloned());
            }
            model.revision += 1;
        });
    }

    pub(super) fn skipped(&self, unit: Option<u64>, span: Option<SourceSpan>) {
        self.with(|model| {
            let (Some(unit), Some(span)) = (unit, span) else {
                return;
            };
            for node in &mut model.nodes {
                if node.unit == unit
                    && node.start >= span.start.index
                    && node.end <= span.end.index
                    && node.runs == 0
                {
                    node.state = ShellState::Skipped;
                }
            }
            model.revision += 1;
        });
    }

    pub(super) fn iteration(&self, unit: Option<u64>, span: Option<SourceSpan>) {
        self.with(|model| {
            let (Some(unit), Some(span)) = (unit, span) else {
                return;
            };
            if let Some(node) = model.nodes.iter_mut().find(|node| {
                node.unit == unit && node.start == span.start.index && node.end == span.end.index
            }) {
                node.iterations = node.iterations.saturating_add(1);
                model.revision += 1;
            }
        });
    }

    pub(super) fn finish(&self) {
        self.with(|model| {
            if !model.running {
                return;
            }
            model.running = false;
            for node in &mut model.nodes {
                if node.live > 0 {
                    node.state = ShellState::Interrupted;
                    node.live = 0;
                }
            }
            model.executions.clear();
            model.revision += 1;
        });
    }

    pub(super) fn snapshot(&self) -> Option<ShellPresentation> {
        self.with(|model| {
            if model.rows.is_empty() {
                return None;
            }
            let mut rows = model.rows.clone();
            for row in &mut rows {
                for segment in &mut row.segments {
                    if let Some(node) = segment.node.and_then(|n| model.nodes.get(n as usize)) {
                        segment.state = display_state(node);
                    }
                }
                if let Some(node) = row.node.and_then(|n| model.nodes.get(n as usize)) {
                    row.state = display_state(node);
                    let mut meta = Vec::new();
                    if let Some(code) = node
                        .status
                        .filter(|code| *code != 0 && (!node.guard || *code != 1))
                    {
                        meta.push(format!("exit {code}"));
                    }
                    if node.leaf && node.duration >= Duration::from_millis(5) {
                        meta.push(if node.duration >= Duration::from_secs(1) {
                            format!("{:.2}s", node.duration.as_secs_f64())
                        } else {
                            format!("{}ms", node.duration.as_millis())
                        });
                    }
                    if node.iterations > 0 || node.looping && node.runs > 0 {
                        meta.push(format!("↻ {}", node.iterations));
                    }
                    row.meta = meta.join(" · ");
                }
            }
            Some(ShellPresentation {
                version: VERSION,
                revision: model.revision,
                running: model.running,
                rows,
            })
        })
    }
}

fn display_state(node: &Node) -> ShellState {
    if node.guard && node.state == ShellState::Fail && node.status == Some(1) {
        ShellState::Ok
    } else {
        node.state
    }
}

fn kind(command: &ast::Command) -> u8 {
    match command {
        ast::Command::Simple(_) => 0,
        ast::Command::Compound(..) => 1,
        ast::Command::Function(_) => 2,
        ast::Command::ExtendedTest(..) => 3,
    }
}

fn segment(text: impl Into<String>, style: ShellStyle, node: Option<u32>) -> ShellSegment {
    ShellSegment {
        text: text.into(),
        style,
        node,
        state: ShellState::Pending,
        language: None,
    }
}

fn row(depth: u16, node: Option<u32>, segments: Vec<ShellSegment>, block: bool) -> ShellRow {
    ShellRow {
        depth,
        segments,
        node,
        state: ShellState::Pending,
        block,
        meta: String::new(),
        effects: String::new(),
    }
}
