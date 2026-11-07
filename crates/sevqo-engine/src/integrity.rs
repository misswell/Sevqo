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
         WHERE status='persisted' AND (
            vec_row IS NULL
            OR vec_row NOT IN (SELECT row_id FROM vec_map WHERE alive=1
                               AND target_kind='chunk' AND ref_id=chunk.id AND file_id=chunk.file_id))",
        [],
    )?;
    report.fixed_stale_units += g.execute(
        "UPDATE clip SET status='pending',retry_count=0,error=NULL 
         WHERE status='persisted' AND (
            vec_row IS NULL
            OR vec_row NOT IN (SELECT row_id FROM vec_map WHERE alive=1
                               AND target_kind='clip' AND ref_id=clip.id AND file_id=clip.file_id))",
        [],
    )?;

    // 1b) image 单元 = 文件本身：活映射必须 ref_id=file_id 且文件仍存在。
    report.fixed_stale_units += g.execute(
        "UPDATE vec_map SET alive=0 WHERE alive=1 AND target_kind='image' AND (
            ref_id != file_id OR file_id NOT IN (SELECT id FROM file))",
        [],
    )?;
    // 1c) 同一 (target,ref) 挂多条活映射：无法确定哪条为真，全部失效并让单元重嵌。
    report.fixed_stale_units += g.execute(
        "UPDATE vec_map SET alive=0 WHERE alive=1 AND target_kind!='image' AND (
            SELECT count(*) FROM vec_map v2 WHERE v2.alive=1
              AND v2.target_kind=vec_map.target_kind AND v2.ref_id=vec_map.ref_id) > 1",
        [],
    )?;
    report.fixed_stale_units += g.execute(
        "UPDATE chunk SET status='pending',retry_count=0,error=NULL WHERE status='persisted'
          AND id IN (SELECT ref_id FROM vec_map WHERE alive=0 AND target_kind='chunk'
                     AND ref_id IN (SELECT ref_id FROM vec_map WHERE alive=1 AND target_kind='chunk'))",
        [],
    )?;
    report.fixed_stale_units += g.execute(
        "UPDATE clip SET status='pending',retry_count=0,error=NULL WHERE status='persisted'
          AND id IN (SELECT ref_id FROM vec_map WHERE alive=0 AND target_kind='clip'
                     AND ref_id IN (SELECT ref_id FROM vec_map WHERE alive=1 AND target_kind='clip'))",
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
    // image 的单元就是文件本身：done 却没有任何有效映射 = 完整性缺口。
    report.fixed_incomplete_done += g.execute(
        "UPDATE file SET state='stale' WHERE state='done' AND kind='image' 
         AND id NOT IN (SELECT file_id FROM vec_map WHERE alive=1 AND target_kind='image')",
        [],
    )?;

    // 4) 无法自动修复的：done 文件的向量映射行号越界之类，交给启动的
    //    generation 校验报告；这里只记录双向一致性的可疑信号。
    let unknown_kind: i64 = g.query_row(
        "SELECT count(*) FROM vec_map WHERE target_kind NOT IN ('image','clip','chunk')",
        [],
        |r| r.get(0),
    )?;
    if unknown_kind > 0 {
        report.problems.push(format!(
            "{unknown_kind} 条映射的 target_kind 无法识别，需要人工检查"
        ));
    }
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

    /// 归属校验（P1-04）：Chunk A 指向的 row 虽然存在活映射，但那映射属于
    /// Chunk B——必须判 A 失联，而不是把 B 的向量当成 A 的。
    #[test]
    fn persisted_unit_with_wrong_owner_map_is_requeued() {
        let (_d, db, id) = doc_db();
        let chunks = db.sync_chunks(id, &["甲".into(), "乙".into()]).unwrap();
        db.set_file_state(id, FileState::Done, None).unwrap();
        let (a, b) = (chunks[0].0, chunks[1].0);
        // 乙先正常入库
        let row_b = db.reserve_vec_row(EmbedRowKind::Chunk, b, id).unwrap();
        db.activate_vec_rows(&[row_b]).unwrap();
        db.mark_units_persisted(&[(EmbedRowKind::Chunk, b)])
            .unwrap();
        // 甲谎称 persisted，但 vec_row 指向乙的映射行
        db.read()
            .unwrap()
            .execute(
                "UPDATE chunk SET status='persisted',vec_row=? WHERE id=?",
                params_like2(row_b, a),
            )
            .unwrap();

        let report = check_and_repair(&db).unwrap();

        assert!(report.fixed_stale_units >= 1, "{report:?}");
        let status_a: String = db
            .read()
            .unwrap()
            .query_row("SELECT status FROM chunk WHERE id=?", [a], |r| r.get(0))
            .unwrap();
        assert_eq!(status_a, "pending", "归属错误的单元必须重新嵌入");
        let status_b: String = db
            .read()
            .unwrap()
            .query_row("SELECT status FROM chunk WHERE id=?", [b], |r| r.get(0))
            .unwrap();
        assert_eq!(status_b, "persisted", "归属正确的单元不受影响");
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

    fn params_like2(a: i64, b: i64) -> [i64; 2] {
        [a, b]
    }
}
