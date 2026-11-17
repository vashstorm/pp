//! `execute_inner` 各工具分支的实现体：父模块 `tools` 只保留门禁与薄分发。
//!
//! 子模块用 `impl Tools` 块访问祖先模块的私有字段与方法，不新增 getter。
//! `bash`／`edit`／`read` 的模块名加 `_tool` 后缀，避免遮蔽 `tools::bash`／`tools::edit`／`tools::read`。

mod ask;
mod bash_tool;
mod edit_tool;
mod eval_tool;
mod read_tool;
mod subagent;
mod write;

use super::*;
