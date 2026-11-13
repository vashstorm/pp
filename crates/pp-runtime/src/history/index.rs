use super::schema::Store;
#[cfg(test)]
use super::schema::unavailable;
use anyhow::Result;
use pp_core::cancellation::CancellationToken;
#[cfg(test)]
use rusqlite::TransactionBehavior;
use std::path::Path;

impl Store {
    /// 同步会话目录投影：委托 persistence 索引模块在同一连接上执行逐文件事务，
    /// 返回逐文件脱敏诊断。取消时由索引模块回滚当前文件事务并返回 Cancelled。
    pub fn sync_sessions(
        &mut self,
        dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>> {
        crate::agent::persistence::index::sync(&mut self.conn, dir, cancellation)
    }

    /// 验收帮助（仅测试）：删除派生表数据后重新同步，验证投影可完整重建
    /// 且不触碰 input_history。DELETE sessions 经外键级联清空 prompts，
    /// 触发器同步移除 FTS 文档。
    #[cfg(test)]
    pub(crate) fn rebuild_sessions(&mut self, dir: &Path) -> Result<Vec<String>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        tx.execute("DELETE FROM sessions", [])
            .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        self.sync_sessions(dir, &CancellationToken::new())
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{count, cwd, match_rows, temp_store};

    #[test]
    fn sqlite_fts_triggers_cascade_and_derived_rebuild_preserve_inputs() {
        let (root, mut store) = temp_store("history.db");
        let a = cwd(root.path(), "project-a");
        store
            .append_input(&a, "修复模型切换失败 needle_42", 1000)
            .unwrap();

        store
            .conn
            .execute(
                "INSERT INTO sessions(session_id,cwd,created_at_ms,updated_at_secs,
                 updated_at_nanos,summary,preview,bytes,done,device,inode,
                 summary_folded,preview_folded,cwd_folded)
                 VALUES ('session-1',?1,10,20,30,'示例标题','首条预览内容',42,1,7,8,
                 '示例标题','首条预览内容',?2)",
                rusqlite::params![a.to_str().unwrap(), a.to_str().unwrap()],
            )
            .unwrap();
        let session_rowid = store.conn.last_insert_rowid();
        store
            .conn
            .execute(
                "INSERT INTO session_prompts(session_rowid,entry_offset,text,text_folded)
                 VALUES (?1,0,'首条 无关键词的记录','首条 无关键词的记录')",
                [session_rowid],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO session_prompts(session_rowid,entry_offset,text,text_folded)
                 VALUES (?1,1,'修复模型切换失败 needle_42','修复模型切换失败 needle_42')",
                [session_rowid],
            )
            .unwrap();

        // trigram 短语匹配：长词命中（含仅出现在第二条 prompt 的词）。
        assert_eq!(match_rows(&store.conn, "prompt_fts", "\"模型切\""), [2]);
        assert_eq!(match_rows(&store.conn, "session_fts", "\"示例标题\""), [1]);
        assert_eq!(match_rows(&store.conn, "session_fts", "\"预览内容\""), [1]);
        assert_eq!(
            match_rows(&store.conn, "session_fts", "\"project-a\"").len(),
            1
        );

        // 更新触发器：先删 OLD 再插 NEW，新旧内容检索一致。
        store
            .conn
            .execute(
                "UPDATE sessions SET summary='新标题了', summary_folded='新标题了' WHERE rowid=?1",
                [session_rowid],
            )
            .unwrap();
        assert!(match_rows(&store.conn, "session_fts", "\"示例标题\"").is_empty());
        assert_eq!(match_rows(&store.conn, "session_fts", "\"新标题了\""), [1]);
        store
            .conn
            .execute(
                "UPDATE session_prompts SET text='改写后的新内容', text_folded='改写后的新内容'
                 WHERE session_rowid=?1 AND entry_offset=0",
                [session_rowid],
            )
            .unwrap();
        assert!(match_rows(&store.conn, "prompt_fts", "\"无关键词\"").is_empty());
        assert_eq!(match_rows(&store.conn, "prompt_fts", "\"改写后\""), [1]);

        // 直接删除单条 prompt：FTS 文档同步移除。
        store
            .conn
            .execute(
                "DELETE FROM session_prompts WHERE session_rowid=?1 AND entry_offset=1",
                [session_rowid],
            )
            .unwrap();
        assert!(match_rows(&store.conn, "prompt_fts", "\"needle_42\"").is_empty());

        // 删除会话：FK 级联清空 prompts，两个 FTS 索引无残留；input_history 不受影响。
        store
            .conn
            .execute("DELETE FROM sessions WHERE rowid=?1", [session_rowid])
            .unwrap();
        assert_eq!(
            count(&store.conn, "SELECT COUNT(*) FROM session_prompts"),
            0
        );
        assert_eq!(count(&store.conn, "SELECT COUNT(*) FROM session_fts"), 0);
        assert_eq!(count(&store.conn, "SELECT COUNT(*) FROM prompt_fts"), 0);
        assert_eq!(
            store.load_inputs(&a, None, 100).unwrap()[0].text,
            "修复模型切换失败 needle_42"
        );

        // 重建派生投影后的索引一致性验收：外部内容 FTS 的 integrity-check 必须通过。
        store
            .conn
            .execute(
                "INSERT INTO session_fts(session_fts) VALUES ('integrity-check')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO prompt_fts(prompt_fts) VALUES ('integrity-check')",
                [],
            )
            .unwrap();
    }
}
