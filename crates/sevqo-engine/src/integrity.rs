//! Chunk/Clip/vec_map/file 四者的一致性检查器（方案 2.4）。
//!
//! 只修复能够唯一确定的数据关系，绝不猜测或覆盖未知数据：
//!
//! - persisted 单元丢失有效映射 → 就地回 pending（自愈，等价于压缩保险丝）
//! - 活映射指向已删除的单元/文件 → 失效（留给压缩回收）
//! - done 文件仍有未持久化单元 → 回 stale 重新排队
//! - 其余任何可疑状态只记录、不改动

use crate::db::Db;
use crate::Result;

#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityReport {
    /// persisted 却无有效向量映射的单元数（已自动修复为 pending）
    pub fixed_stale_units: usize,
    /// 活映射指向已不存在单元/文件的行数（已自动失效，待压缩回收）
    pub fixed_orphan_rows: usize,
    /// done 但必需单元缺失的文件数（已自动回 stale）
    pub fixed_incomplete_done: usize,
    /// 无法自动修复、需要人工判断的问题
    pub problems: Vec<String>,
}

impl IntegrityReport {
    pub fn is_clean(&self) -> bool {
        self.fixed_stale_units == 0
            && self.fixed_orphan_rows == 0
            && self.fixed_incomplete_done == 0
            && self.problems.is_empty()
    }
}

/// 执行一致性检查并自动修复可确定的关系。在启动阶段调用一次；
/// 修复全部走幂等 UPDATE，重复执行没有副作用。
pub fn check_and_repair(db: &Db) -> Result<IntegrityReport> {
    let mut report = IntegrityReport::default();
    let g = db.write_conn();

    // 1) persisted 单元没有有效映射：状态撒了谎，回 pending 让补齐轮重建。
    report.fixed_stale_units += g.execute(
        "UPDATE chunk SET status='pending',retry_count=0,error=NULL \
         WHERE status='persisted' AND (vec_row IS NULL OR vec_row NOT IN (SELECT row_id FROM vec_map WHERE alive=1))",
        [],
    )?;
    report.fixed_stale_units += g.execute(
        "UPDATE clip SET status='pending',retry_count=0,error=NULL 
         WHERE status='persisted' AND (vec_row IS NULL OR vec_row NOT IN (SELECT row_id FROM vec_map WHERE alive=1))",
        [],
    )?;

    // 2) 活映射的单元已经不存在（chunk/clip 被删，或 image 的文件被删）。
    report.fixed_orphan_rows += g.execute(
        "UPDATE vec_map SET alive=0 WHERE alive=1 AND ( 
           (target_kind='chunk' AND ref_id NOT IN (SELECT id FROM chunk)) 
        OR (target_kind='clip' AND ref_id NOT IN (SELECT id FROM clip)) 
        OR (target_kind='image' AND file_id NOT IN (SELECT id FROM file)))",
        [],
    )?;

    // 3) done 文件必需单元缺失：聚合状态与事实不符，回 stale。
    report.fixed_incomplete_done += g.execute(
        "UPDATE file SET state='stale' WHERE state='done' 
         AND kind IN ('video','document','code') 
         AND ( id IN (SELECT file_id FROM chunk WHERE status!='persisted') 
            OR id IN (SELECT file_id FROM clip WHERE status!='persisted') )",
        [],
    )?;

    // 4) 无法自动修复的：done 文件的向量映射行号越界之类，交给启动的
    //    generation 校验报告；这里只记录双向一致性的可疑信号。
    let suspicious: i64 = g.query_row(
        "SELECT count(*) FROM vec_map WHERE alive=1 
         AND ( (target_kind='chunk' AND ref_id IN (SELECT id FROM chunk WHERE status!='persisted')) 
            OR (target_kind='clip' AND ref_id IN (SELECT id FROM clip WHERE status!='persisted')) )",
        [],
        |r| r.get(0),
    )?;
    if suspicious > 0 {
        report.problems.push(format!(
            "{suspicious} 条有效映射对应的单元仍标记为未持久化（将在补齐轮重新编码）"
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::scan::Scanned;
    use crate::{EmbedRowKind, FileState, Kind};

    fn doc_db() -> (tempfile::TempDir, Db, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let root = db.add_root("/tmp/d", "d").unwrap();
        let seen = crate::db::now_ns();
        let s = Scanned {
            root_id: root,
            path: "/tmp/d/a.md".into(),
            name: "a.md".into(),
            ext: "md".into(),
            kind: Kind::Document,
            size: 1,
            mtime_ns: seen,
        };
        let id = db.upsert_file(&s, seen).unwrap().0;
        (dir, db, id)
    }

    #[test]
    fn persisted_unit_without_mapping_is_requeued() {
        let (_d, db, id) = doc_db();
        let chunks = db.sync_chunks(id, &["一".into(), "二".into()]).unwrap();
        db.set_file_state(id, FileState::Done, None).unwrap();
        // 谎称 persisted 但没有任何映射/落盘
        db.mark_units_persisted(&[(EmbedRowKind::Chunk, chunks[0].0)])
            .unwrap();

        let report = check_and_repair(&db).unwrap();

        assert_eq!(report.fixed_stale_units, 1, "{report:?}");
        let status: String = db
            .read()
            .unwrap()
            .query_row(
                "SELECT status FROM chunk WHERE id=?",
                params_like(chunks[0].0),
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending", "自愈后必须重新可补");
        // 文件仍是 done 且单元 pending → 检查器把它回 stale
        assert_eq!(report.fixed_incomplete_done, 1, "{report:?}");
        assert_eq!(db.file(id).unwrap().state, FileState::Stale);
    }

    #[test]
    fn orphan_alive_rows_are_tombstoned() {
        let (_d, db, id) = doc_db();
        let chunks = db.sync_chunks(id, &["一".into()]).unwrap();
        db.set_file_state(id, FileState::Done, None).unwrap();
        // 手工删掉 chunk 行，模拟「单元已删除但映射还在」
        let cid = chunks[0].0;
        db.read()
            .unwrap()
            .execute("DELETE FROM chunk WHERE id=?", params_like(cid))
            .unwrap();
        let row = db.reserve_vec_row(EmbedRowKind::Chunk, cid, id).unwrap();
        db.activate_vec_rows(&[row]).unwrap();
        assert_eq!(db.alive_vec_rows().unwrap(), 1);

        let report = check_and_repair(&db).unwrap();

        assert_eq!(report.fixed_orphan_rows, 1, "{report:?}");
        assert_eq!(db.alive_vec_rows().unwrap(), 0);
    }

    #[test]
    fn clean_state_reports_nothing() {
        let (_d, db, id) = doc_db();
        let chunks = db.sync_chunks(id, &["一".into()]).unwrap();
        let row = db
            .reserve_vec_row(EmbedRowKind::Chunk, chunks[0].0, id)
            .unwrap();
        db.activate_vec_rows(&[row]).unwrap();
        db.mark_units_persisted(&[(EmbedRowKind::Chunk, chunks[0].0)])
            .unwrap();
        db.set_file_state(id, FileState::Done, None).unwrap();

        let report = check_and_repair(&db).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(db.alive_vec_rows().unwrap(), 1, "健康状态不得误伤");
    }

    fn params_like(v: i64) -> [i64; 1] {
        [v]
    }
}
