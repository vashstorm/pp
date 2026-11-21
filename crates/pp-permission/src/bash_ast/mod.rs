//! 基于 Brush AST 的 Bash 命令静态分析，用于精确安全审计与命令级前缀授权。

mod classify;
mod wrappers;

use brush_core::parser::{
    Parser,
    ast::{self, CommandPrefixOrSuffixItem as Item},
    word::{self, WordPiece, WordPieceWithSource},
};
use classify::{
    AUTH_FILES, fallback_critical, is_auth_path, is_fork_bomb, is_remote_fetch_leaf,
    is_shell_interpreter_leaf,
};
use std::{collections::HashSet, io::Cursor};
use wrappers::{strip_privilege_prefix, strip_transparent_wrappers, xargs_command_start};

const MAX_NODES: usize = 4096;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArgToken {
    Literal(String),
    Dynamic,
}

impl ArgToken {
    pub(super) fn as_literal(&self) -> Option<&str> {
        match self {
            Self::Literal(value) => Some(value.as_str()),
            Self::Dynamic => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeafCommand {
    /// 剥离环境赋值与透明包装器后的归一化词序列；首词已取 basename。
    pub(crate) tokens: Vec<ArgToken>,
    /// 保留词原文（供 `rm -r ~` 等检查 ~ 路径或绝对路径）。
    pub(super) raw_args: Vec<String>,
    /// 是否存在指向非 `/dev/null` 的写文件重定向。
    pub(crate) has_write_redirect: bool,
    /// 命令名在当前脚本内被定义为 shell function，不按内置/只读工具信任。
    pub(crate) is_function_call: bool,
    /// 存在无法静态剥离的动态执行（例如 `bash -c "$VAR"`、`eval` 或动态包装参数）。
    pub(crate) dynamic_exec: bool,
}

impl LeafCommand {
    pub(crate) fn program(&self) -> Option<&str> {
        self.tokens.first().and_then(ArgToken::as_literal)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProgramAnalysis {
    pub(crate) leaves: Vec<LeafCommand>,
    pub(crate) has_write_redirection: bool,
    pub(crate) critical_reason: Option<&'static str>,
    pub(crate) unanalyzable: bool,
}

impl ProgramAnalysis {
    /// 当命令含多条未授权命令时提取去重后的建议前缀列表；
    /// 任一未授权叶子无法推导出安全前缀时返回 `None`（仅可单次放行）。
    pub(crate) fn suggested_prefixes<'a>(
        leaves: impl IntoIterator<Item = &'a LeafCommand>,
    ) -> Option<Vec<Vec<String>>> {
        let mut prefixes = Vec::new();
        for leaf in leaves {
            let prefix = leaf.suggested_prefix()?;
            if !prefixes.contains(&prefix) {
                prefixes.push(prefix);
            }
        }
        (!prefixes.is_empty()).then_some(prefixes)
    }
}

pub(crate) fn analyze_bash_arguments(arguments: &str) -> Option<ProgramAnalysis> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let command = value.get("command")?.as_str()?;
    Some(analyze_command(command))
}

pub(crate) fn analyze_command(command: &str) -> ProgramAnalysis {
    let mut analyzer = Analyzer {
        analysis: ProgramAnalysis::default(),
        functions: HashSet::new(),
        nodes: 0,
    };
    if is_fork_bomb(command) {
        analyzer.analysis.critical_reason = Some("fork bomb pattern");
    }
    analyzer.parse_and_visit(command, 0);
    analyzer.analysis
}

struct Analyzer {
    analysis: ProgramAnalysis,
    functions: HashSet<String>,
    nodes: usize,
}

impl Analyzer {
    fn tick(&mut self, depth: usize) -> bool {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            self.analysis.unanalyzable = true;
            false
        } else {
            true
        }
    }

    fn set_critical(&mut self, reason: &'static str) {
        if self.analysis.critical_reason.is_none() {
            self.analysis.critical_reason = Some(reason);
        }
    }

    fn parse_and_visit(&mut self, source: &str, depth: usize) {
        if !self.tick(depth) {
            return;
        }
        let mut reader = Cursor::new(source.as_bytes());
        let Ok(program) = Parser::new(&mut reader, &Default::default()).parse_program() else {
            self.analysis.unanalyzable = true;
            if let Some(reason) = fallback_critical(source) {
                self.set_critical(reason);
            }
            return;
        };
        for list in &program.complete_commands {
            self.visit_list(list, depth + 1);
        }
    }

    fn visit_list(&mut self, list: &ast::CompoundList, depth: usize) {
        if !self.tick(depth) {
            return;
        }
        for item in &list.0 {
            for (_, pipeline) in &item.0 {
                self.visit_pipeline(pipeline, depth + 1);
            }
        }
    }

    fn visit_pipeline(&mut self, pipeline: &ast::Pipeline, depth: usize) {
        if !self.tick(depth) {
            return;
        }
        let mut saw_remote_fetch = false;
        for command in &pipeline.seq {
            let start_leaves = self.analysis.leaves.len();
            self.visit_command(command, depth + 1);
            let new_leaves = &self.analysis.leaves[start_leaves..];
            let has_shell = new_leaves.iter().any(is_shell_interpreter_leaf);
            let has_fetch = new_leaves.iter().any(is_remote_fetch_leaf);
            if saw_remote_fetch && has_shell {
                self.set_critical("remote script piped into a shell");
            }
            if has_fetch {
                saw_remote_fetch = true;
            }
        }
    }

    fn visit_command(&mut self, command: &ast::Command, depth: usize) {
        if !self.tick(depth) {
            return;
        }
        match command {
            ast::Command::Simple(simple) => self.visit_simple(simple, depth + 1),
            ast::Command::Compound(compound, redirects) => {
                if let Some(redirects) = redirects {
                    self.visit_redirects(&redirects.0, depth + 1);
                }
                self.visit_compound(compound, depth + 1);
            }
            ast::Command::Function(func) => {
                self.functions.insert(func.fname.value.clone());
                self.visit_compound(&func.body.0, depth + 1);
            }
            ast::Command::ExtendedTest(test, redirects) => {
                if let Some(redirects) = redirects {
                    self.visit_redirects(&redirects.0, depth + 1);
                }
                self.visit_extended_test(&test.expr, depth + 1);
            }
        }
    }

    fn visit_compound(&mut self, compound: &ast::CompoundCommand, depth: usize) {
        if !self.tick(depth) {
            return;
        }
        match compound {
            ast::CompoundCommand::BraceGroup(group) => self.visit_list(&group.list, depth + 1),
            ast::CompoundCommand::Subshell(subshell) => self.visit_list(&subshell.list, depth + 1),
            ast::CompoundCommand::IfClause(clause) => {
                self.visit_list(&clause.condition, depth + 1);
                self.visit_list(&clause.then, depth + 1);
                for els in clause.elses.iter().flatten() {
                    if let Some(cond) = &els.condition {
                        self.visit_list(cond, depth + 1);
                    }
                    self.visit_list(&els.body, depth + 1);
                }
            }
            ast::CompoundCommand::WhileClause(clause)
            | ast::CompoundCommand::UntilClause(clause) => {
                self.visit_list(&clause.0, depth + 1);
                self.visit_list(&clause.1.list, depth + 1);
            }
            ast::CompoundCommand::ForClause(clause) => {
                if let Some(values) = &clause.values {
                    for word in values {
                        self.scan_word_substitutions(&word.value, depth + 1);
                    }
                }
                self.visit_list(&clause.body.list, depth + 1);
            }
            ast::CompoundCommand::ArithmeticForClause(clause) => {
                self.visit_list(&clause.body.list, depth + 1);
            }
            ast::CompoundCommand::CaseClause(clause) => {
                self.scan_word_substitutions(&clause.value.value, depth + 1);
                for case in &clause.cases {
                    for pattern in &case.patterns {
                        self.scan_word_substitutions(&pattern.value, depth + 1);
                    }
                    if let Some(body) = &case.cmd {
                        self.visit_list(body, depth + 1);
                    }
                }
            }
            ast::CompoundCommand::Coprocess(clause) => {
                self.visit_command(&clause.body, depth + 1);
            }
            ast::CompoundCommand::Arithmetic(arith) => {
                self.scan_word_substitutions(&arith.expr.value, depth + 1);
            }
        }
    }

    fn visit_extended_test(&mut self, expr: &ast::ExtendedTestExpr, depth: usize) {
        if !self.tick(depth) {
            return;
        }
        match expr {
            ast::ExtendedTestExpr::And(a, b) | ast::ExtendedTestExpr::Or(a, b) => {
                self.visit_extended_test(a, depth + 1);
                self.visit_extended_test(b, depth + 1);
            }
            ast::ExtendedTestExpr::Not(a) | ast::ExtendedTestExpr::Parenthesized(a) => {
                self.visit_extended_test(a, depth + 1);
            }
            ast::ExtendedTestExpr::UnaryTest(_, word) => {
                self.scan_word_substitutions(&word.value, depth + 1);
            }
            ast::ExtendedTestExpr::BinaryTest(_, left, right) => {
                self.scan_word_substitutions(&left.value, depth + 1);
                self.scan_word_substitutions(&right.value, depth + 1);
            }
        }
    }

    fn visit_simple(&mut self, simple: &ast::SimpleCommand, depth: usize) {
        if !self.tick(depth) {
            return;
        }
        let mut raw_tokens = Vec::new();
        let mut raw_strings = Vec::new();
        let mut redirects = Vec::new();

        if let Some(prefix) = &simple.prefix {
            for item in &prefix.0 {
                match item {
                    Item::AssignmentWord(assign, _) => {
                        self.visit_assignment(&assign.value, depth + 1);
                    }
                    Item::IoRedirect(redir) => redirects.push(redir),
                    Item::Word(word) => {
                        raw_strings.push(word.value.clone());
                        raw_tokens.push(self.parse_word_token(&word.value, depth + 1));
                    }
                    Item::ProcessSubstitution(_, sub) => {
                        self.visit_list(&sub.list, depth + 1);
                        raw_strings.push(sub.to_string());
                        raw_tokens.push(ArgToken::Dynamic);
                    }
                }
            }
        }
        if let Some(word) = &simple.word_or_name {
            raw_strings.push(word.value.clone());
            raw_tokens.push(self.parse_word_token(&word.value, depth + 1));
        }
        if let Some(suffix) = &simple.suffix {
            for item in &suffix.0 {
                match item {
                    Item::IoRedirect(redir) => redirects.push(redir),
                    Item::AssignmentWord(assign, word) => {
                        self.visit_assignment(&assign.value, depth + 1);
                        raw_strings.push(word.value.clone());
                        raw_tokens.push(self.parse_word_token(&word.value, depth + 1));
                    }
                    Item::Word(word) => {
                        raw_strings.push(word.value.clone());
                        raw_tokens.push(self.parse_word_token(&word.value, depth + 1));
                    }
                    Item::ProcessSubstitution(_, sub) => {
                        self.visit_list(&sub.list, depth + 1);
                        raw_strings.push(sub.to_string());
                        raw_tokens.push(ArgToken::Dynamic);
                    }
                }
            }
        }

        let has_write_redirect = self.visit_redirects_refs(&redirects, depth + 1);

        if raw_tokens.is_empty() {
            // 纯环境变量赋值 + 写重定向（如 `FOO=1 > /etc/passwd`）必须产生叶子以阻止只读放行。
            if has_write_redirect {
                self.analysis.leaves.push(LeafCommand {
                    tokens: Vec::new(),
                    raw_args: Vec::new(),
                    has_write_redirect: true,
                    is_function_call: false,
                    dynamic_exec: false,
                });
            }
            return;
        }

        let (mut tokens, raw_args, dynamic_exec) =
            strip_transparent_wrappers(&raw_tokens, &raw_strings);
        if dynamic_exec {
            self.analysis.unanalyzable = true;
        }

        let is_function_call = tokens
            .first()
            .and_then(ArgToken::as_literal)
            .is_some_and(|name| self.functions.contains(name));

        if let Some(ArgToken::Literal(first)) = tokens.first_mut() {
            let base = first.rsplit('/').next().unwrap_or(first).to_owned();
            *first = base;
        } else {
            self.analysis.unanalyzable = true;
        }

        let leaf = LeafCommand {
            tokens,
            raw_args,
            has_write_redirect,
            is_function_call,
            dynamic_exec,
        };

        if let Some(reason) = classify::leaf_critical_reason(&leaf) {
            self.set_critical(reason);
        }

        // `find -exec` / `xargs` / `bash -c` 等嵌套执行提取
        self.visit_nested_exec(&leaf, depth + 1);

        self.analysis.leaves.push(leaf);
    }

    fn visit_assignment(&mut self, value: &ast::AssignmentValue, depth: usize) {
        match value {
            ast::AssignmentValue::Scalar(word) => {
                self.scan_word_substitutions(&word.value, depth + 1);
            }
            ast::AssignmentValue::Array(items) => {
                for (_, word) in items {
                    self.scan_word_substitutions(&word.value, depth + 1);
                }
            }
        }
    }

    fn visit_redirects(&mut self, redirects: &[ast::IoRedirect], depth: usize) -> bool {
        let refs: Vec<&ast::IoRedirect> = redirects.iter().collect();
        self.visit_redirects_refs(&refs, depth)
    }

    fn visit_redirects_refs(&mut self, redirects: &[&ast::IoRedirect], depth: usize) -> bool {
        let mut writes = false;
        for redir in redirects {
            match redir {
                ast::IoRedirect::File(_, kind, target) => {
                    let is_write = matches!(
                        kind,
                        ast::IoFileRedirectKind::Write
                            | ast::IoFileRedirectKind::Append
                            | ast::IoFileRedirectKind::Clobber
                            | ast::IoFileRedirectKind::ReadAndWrite
                    );
                    match target {
                        ast::IoFileRedirectTarget::Filename(word)
                        | ast::IoFileRedirectTarget::Duplicate(word) => {
                            let tok = self.parse_word_token(&word.value, depth + 1);
                            if is_write {
                                let is_dev_null =
                                    matches!(&tok, ArgToken::Literal(p) if p == "/dev/null");
                                if !is_dev_null {
                                    writes = true;
                                    self.analysis.has_write_redirection = true;
                                }
                                if let ArgToken::Literal(path) = &tok {
                                    if is_auth_path(path) {
                                        self.set_critical("write to system authentication files");
                                    }
                                } else if AUTH_FILES.iter().any(|auth| word.value.contains(auth)) {
                                    self.set_critical("write to system authentication files");
                                }
                            }
                        }
                        ast::IoFileRedirectTarget::ProcessSubstitution(_, sub) => {
                            self.visit_list(&sub.list, depth + 1);
                            if is_write {
                                writes = true;
                                self.analysis.has_write_redirection = true;
                            }
                        }
                        ast::IoFileRedirectTarget::Fd(_) => {}
                    }
                }
                ast::IoRedirect::OutputAndError(word, _) => {
                    let tok = self.parse_word_token(&word.value, depth + 1);
                    let is_dev_null = matches!(&tok, ArgToken::Literal(p) if p == "/dev/null");
                    if !is_dev_null {
                        writes = true;
                        self.analysis.has_write_redirection = true;
                    }
                    if matches!(&tok, ArgToken::Literal(path) if is_auth_path(path)) {
                        self.set_critical("write to system authentication files");
                    }
                }
                ast::IoRedirect::HereString(_, word) => {
                    self.scan_word_substitutions(&word.value, depth + 1);
                }
                ast::IoRedirect::HereDocument(_, doc) => {
                    if doc.requires_expansion {
                        self.scan_word_substitutions(&doc.doc.value, depth + 1);
                    }
                }
            }
        }
        writes
    }

    fn parse_word_token(&mut self, raw: &str, depth: usize) -> ArgToken {
        let Ok(pieces) = word::parse(raw, &Default::default()) else {
            self.analysis.unanalyzable = true;
            return ArgToken::Dynamic;
        };
        self.scan_pieces_substitutions(&pieces, depth);
        if let Some(lit) = literal_pieces(&pieces) {
            ArgToken::Literal(lit)
        } else {
            ArgToken::Dynamic
        }
    }

    fn scan_word_substitutions(&mut self, raw: &str, depth: usize) {
        if let Ok(pieces) = word::parse(raw, &Default::default()) {
            self.scan_pieces_substitutions(&pieces, depth);
        }
    }

    fn scan_pieces_substitutions(&mut self, pieces: &[WordPieceWithSource], depth: usize) {
        for piece in pieces {
            match &piece.piece {
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => {
                    self.scan_pieces_substitutions(inner, depth);
                }
                WordPiece::CommandSubstitution(sub)
                | WordPiece::BackquotedCommandSubstitution(sub) => {
                    self.parse_and_visit(sub, depth + 1);
                }
                _ => {}
            }
        }
    }

    fn visit_nested_exec(&mut self, leaf: &LeafCommand, depth: usize) {
        let (effective_tokens, _) = strip_privilege_prefix(&leaf.tokens, &leaf.raw_args);
        let Some(ArgToken::Literal(raw_program)) = effective_tokens.first() else {
            return;
        };
        let program = raw_program.rsplit('/').next().unwrap_or(raw_program);
        let args = &effective_tokens[1..];

        if matches!(program, "bash" | "sh" | "zsh" | "dash" | "ksh") {
            let mut idx = 0;
            while idx < args.len() {
                match &args[idx] {
                    ArgToken::Literal(flag)
                        if flag == "--command"
                            || (flag.starts_with('-')
                                && !flag.starts_with("--")
                                && flag[1..].contains('c')) =>
                    {
                        match args.get(idx + 1) {
                            Some(ArgToken::Literal(script)) => {
                                self.parse_and_visit(script, depth + 1);
                            }
                            _ => {
                                self.analysis.unanalyzable = true;
                            }
                        }
                        break;
                    }
                    ArgToken::Literal(s) if s == "--" => break,
                    _ => idx += 1,
                }
            }
        } else if matches!(program, "eval" | "source" | ".") {
            if args.len() == 1
                && let Some(ArgToken::Literal(script)) = args.first()
                && program == "eval"
            {
                self.parse_and_visit(script, depth + 1);
            } else {
                self.analysis.unanalyzable = true;
            }
        } else if program == "find" {
            let mut idx = 0;
            while idx < args.len() {
                if matches!(&args[idx], ArgToken::Literal(s) if matches!(s.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
                {
                    let inner: Vec<&ArgToken> = args[idx + 1..]
                        .iter()
                        .take_while(
                            |tok| !matches!(tok, ArgToken::Literal(s) if s == ";" || s == "+"),
                        )
                        .collect();
                    if let Some(ArgToken::Literal(cmd)) = inner.first() {
                        let mut tokens: Vec<ArgToken> = inner.into_iter().cloned().collect();
                        if let Some(ArgToken::Literal(first)) = tokens.first_mut() {
                            *first = first.rsplit('/').next().unwrap_or(cmd).to_owned();
                        }
                        let sub_leaf = LeafCommand {
                            tokens,
                            raw_args: Vec::new(),
                            has_write_redirect: false,
                            is_function_call: false,
                            dynamic_exec: false,
                        };
                        if let Some(reason) = classify::leaf_critical_reason(&sub_leaf) {
                            self.set_critical(reason);
                        }
                        self.analysis.leaves.push(sub_leaf);
                    } else {
                        self.analysis.unanalyzable = true;
                    }
                }
                idx += 1;
            }
        } else if program == "xargs"
            && let Some(start) = xargs_command_start(args)
        {
            let tokens: Vec<ArgToken> = args[start..].to_vec();
            if !tokens.is_empty() {
                let mut normalized = tokens;
                if let Some(ArgToken::Literal(first)) = normalized.first_mut() {
                    *first = first.rsplit('/').next().unwrap_or(first).to_owned();
                }
                let sub_leaf = LeafCommand {
                    tokens: normalized,
                    raw_args: Vec::new(),
                    has_write_redirect: false,
                    is_function_call: false,
                    dynamic_exec: false,
                };
                if let Some(reason) = classify::leaf_critical_reason(&sub_leaf) {
                    self.set_critical(reason);
                }
                self.analysis.leaves.push(sub_leaf);
            }
        }
    }
}

fn literal_pieces(pieces: &[WordPieceWithSource]) -> Option<String> {
    let mut output = String::new();
    for piece in pieces {
        match &piece.piece {
            WordPiece::Text(s) | WordPiece::SingleQuotedText(s) => output.push_str(s),
            WordPiece::EscapeSequence(s) => output.push_str(s.strip_prefix('\\').unwrap_or(s)),
            WordPiece::DoubleQuotedSequence(inner) => {
                output.push_str(&literal_pieces(inner)?);
            }
            _ => return None,
        }
    }
    Some(output)
}
