use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

#[test]
fn quoted_keywords_cannot_turn_unsupported_sql_into_delete() {
    let responses = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY);",
        "INSERT INTO t VALUES (1);",
        "`DELETE` FROM t;",
        "SELECT `id` FROM `t`;",
    ]);
    assert_eq!(responses[2]["ok"], false);
    assert_eq!(responses[3]["rows"], json!([[1]]));
}

#[test]
fn unordered_limit_preserves_legacy_rows_after_reopen() {
    let data = Data::new();
    let initial = session(
        &data,
        &[
            "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(1));".into(),
            "INSERT INTO t VALUES (2,'[2]'),(1,'[1]');".into(),
            "CREATE VECTOR INDEX vi ON t(v);".into(),
            "SELECT id FROM t LIMIT 1;".into(),
            "SELECT id,v FROM t LIMIT 1;".into(),
            "SELECT id FROM t ORDER BY id LIMIT 1;".into(),
        ],
    );
    assert_eq!(initial[3]["rows"], json!([[2]]));
    assert_eq!(initial[4]["rows"], json!([[2, [2.0]]]));
    assert_eq!(initial[5]["rows"], json!([[1]]));
    let recovered = session(
        &data,
        &[
            "SELECT id FROM t LIMIT 1;".into(),
            "BEGIN;".into(),
            "DELETE FROM t WHERE id=2;".into(),
            "SELECT id FROM t LIMIT 1;".into(),
            "ROLLBACK;".into(),
            "SELECT id FROM t LIMIT 1;".into(),
        ],
    );
    assert_eq!(recovered[0]["rows"], json!([[2]]));
    assert_eq!(recovered[3]["rows"], json!([[1]]));
    assert_eq!(recovered[5]["rows"], json!([[2]]));
}

#[test]
fn malformed_insert_tail_takes_precedence_over_an_earlier_duplicate() {
    let responses = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, s VARCHAR(20));",
        "INSERT INTO t VALUES (1,'existing');",
        "BEGIN;",
        "INSERT INTO t VALUES (2,'before');",
        "INSERT INTO t VALUES (3,'temporary'),(1,'duplicate'),(4,'unclosed",
        "SELECT id FROM t ORDER BY id;",
        "COMMIT;",
        "SELECT id FROM t ORDER BY id;",
    ]);
    assert_eq!(responses[4]["error"]["code"], "unsupported_sql");
    assert_eq!(responses[5]["rows"], json!([[1], [2]]));
    assert_eq!(responses[7]["rows"], json!([[1], [2]]));
}

#[test]
fn exact_topk_resolves_distances_that_round_to_the_same_f64() {
    let responses = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2));",
        "INSERT INTO t VALUES (1,'[3e38,1]'),(2,'[3e38,0]'),(3,'[-3e38,0]');",
        "SELECT id FROM t ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 3;",
    ]);
    assert_eq!(responses[2]["rows"], json!([[2], [3], [1]]));
}

#[test]
fn exact_topk_preserves_subnormal_differences_and_alignment_boundary() {
    let responses = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, kind VARCHAR(1), v VECTOR(2));",
        "INSERT INTO t VALUES \
         (1,'x','[3.4028234663852886e38,1.401298464324817e-45]'), \
         (2,'x','[-3.4028234663852886e38,0]'), \
         (3,'x','[3.4028234663852886e38,-0.0]'), \
         (4,'g','[-4294967040,0]'),(5,'g','[2147483520,0]'),(6,'g','[4294967040,0]');",
        "CREATE VECTOR INDEX vi ON t(v);",
        "SELECT id,L2_DISTANCE(v,'[0,0]') AS d FROM t ORDER BY d LIMIT 6;",
        "SELECT id,L2_DISTANCE(v,'[-16777215,0]') AS d FROM t WHERE kind='g' ORDER BY d LIMIT 3;",
    ]);
    let maximum = f64::from(f32::MAX);
    assert_eq!(
        responses[3]["rows"],
        json!([
            [5, 2147483520.0],
            [4, 4294967040.0],
            [6, 4294967040.0],
            [2, maximum],
            [3, maximum],
            [1, maximum]
        ])
    );
    // Opposite signs at gaps 7 and 8, and same-sign subtraction at gap 8.
    assert_eq!(
        responses[4]["rows"],
        json!([[5, 2164260735.0], [4, 4278189825.0], [6, 4311744255.0]])
    );
}

#[test]
fn vector_elements_must_be_json_numbers() {
    let responses = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2));",
        r#"INSERT INTO t VALUES (1,'[{"$serde_json::private::Number":"1.0"},0]');"#,
        "SELECT id FROM t;",
    ]);
    assert_eq!(responses[1]["ok"], false);
    assert_eq!(responses[2]["rows"], json!([]));
}

#[test]
fn flat_vector_index_tracks_mutations_and_recovery() {
    let data = Data::new();
    let responses = session(
        &data,
        &[
            "CREATE TABLE t (s VARCHAR(16), v VECTOR(2), id BIGINT PRIMARY KEY);".into(),
            "INSERT INTO t VALUES ('甲😀','[1,0]',1),('甲','[2,0]',2);".into(),
            "CREATE VECTOR INDEX vi ON t(v);".into(),
            "SELECT id FROM t ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;".into(),
            "BEGIN;".into(),
            "UPDATE t SET s='different',v='[0,0]' WHERE id=2;".into(),
            "DELETE FROM t WHERE id=1;".into(),
            "INSERT INTO t VALUES ('新','[1,0]',3);".into(),
            "SELECT id FROM t ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;".into(),
            "COMMIT;".into(),
        ],
    );
    assert_eq!(responses[3]["rows"], json!([[1], [2]]));
    assert_eq!(responses[8]["rows"], json!([[2], [3]]));
    let recovered = session(
        &data,
        &["SELECT id,v FROM t ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;".into()],
    );
    assert_eq!(
        recovered[0]["rows"],
        json!([[2, [0.0, 0.0]], [3, [1.0, 0.0]]])
    );
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Data(PathBuf);
impl Data {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mv-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Data {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn session(data: &Data, sql: &[String]) -> Vec<Value> {
    session_with_hot_budget(data, sql, 8 << 20)
}
fn session_with_hot_budget(data: &Data, sql: &[String], budget: usize) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_minivectordb"))
        .arg("--data-dir")
        .arg(&data.0)
        .env("MINIVECTORDB_SORT_BYTES", "4096")
        .env("MINIVECTORDB_HOT_BYTES", budget.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for (i, query) in sql.iter().enumerate() {
        writeln!(
            input,
            "{}",
            json!({"id":i,"op":"execute","sql":query,"search_mode":"exact"})
        )
        .unwrap();
    }
    writeln!(input, "{}", json!({"id":"end","op":"shutdown"})).unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut lines: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(lines.remove(0), json!({"type":"ready"}));
    assert_eq!(lines.pop().unwrap(), json!({"id":"end","ok":true}));
    lines
}
fn run(sql: &[&str]) -> Vec<Value> {
    session(
        &Data::new(),
        &sql.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}

#[test]
fn decimal_vector_rounding_does_not_round_through_f64() {
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2));",
        "INSERT INTO t VALUES (1,'[1.000000059604644775390626,0]');",
        "SELECT v FROM t;",
    ]);
    assert_eq!(r[2]["rows"], json!([[[1.0000001192092896_f64, 0.0]]]));
}
#[test]
fn bigint_extrema_survive_storage_and_comparison() {
    let data = Data::new();
    session(&data,&["CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT);".into(),"INSERT INTO t VALUES (-9223372036854775808,9223372036854775807),(9007199254740993,-9007199254740993);".into()]);
    let r = session(&data, &["SELECT * FROM t ORDER BY id;".into()]);
    assert_eq!(
        r[0]["rows"],
        json!([
            [-9223372036854775808_i64, 9223372036854775807_i64],
            [9007199254740993_i64, -9007199254740993_i64]
        ])
    );
}
#[test]
fn failed_statement_keeps_prior_transaction_change() {
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT);",
        "INSERT INTO t VALUES (1,1);",
        "BEGIN;",
        "UPDATE t SET n=9;",
        "INSERT INTO t VALUES (2,2),(1,3);",
        "COMMIT;",
        "SELECT * FROM t;",
    ]);
    assert_eq!(r[4]["ok"], false);
    assert_eq!(r[6]["rows"], json!([[1, 9]]));
}
#[test]
fn ordinary_sort_ties_always_use_primary_key_ascending() {
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT);",
        "INSERT INTO t VALUES (3,8),(1,8),(2,9);",
        "SELECT id FROM t ORDER BY n DESC;",
    ]);
    assert_eq!(r[2]["rows"], json!([[2], [1], [3]]));
}
#[test]
fn deleted_primary_key_can_be_reinserted_in_transaction() {
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT);",
        "INSERT INTO t VALUES (1,1);",
        "DELETE FROM t;",
        "BEGIN;",
        "INSERT INTO t VALUES (1,9);",
        "SELECT * FROM t;",
        "COMMIT;",
        "SELECT * FROM t;",
    ]);
    assert_eq!(r[5]["rows"], json!([[1, 9]]));
    assert_eq!(r[7]["rows"], json!([[1, 9]]));
}
#[test]
fn external_merge_and_large_topk_return_all_rows_in_order() {
    let data = Data::new();
    let values = (0..3000)
        .rev()
        .map(|n| format!("({n},{})", n % 7))
        .collect::<Vec<_>>()
        .join(",");
    let r = session(
        &data,
        &[
            "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT);".into(),
            format!("INSERT INTO t VALUES {values};"),
            "SELECT id FROM t ORDER BY n DESC LIMIT 2000;".into(),
            "SELECT id FROM t ORDER BY id;".into(),
        ],
    );
    let mut expected: Vec<_> = (0..3000).collect();
    expected.sort_by_key(|n| (std::cmp::Reverse(n % 7), *n));
    assert_eq!(
        r[2]["rows"],
        json!(expected[..2000].iter().map(|n| vec![n]).collect::<Vec<_>>())
    );
    assert_eq!(
        r[3]["rows"],
        json!((0..3000).map(|n| vec![n]).collect::<Vec<_>>())
    );
}
#[test]
fn ddl_in_transaction_rolls_back_on_shutdown() {
    let data = Data::new();
    session(
        &data,
        &[
            "BEGIN;".into(),
            "CREATE TABLE t (id BIGINT PRIMARY KEY);".into(),
            "INSERT INTO t VALUES (1);".into(),
        ],
    );
    let r = session(&data, &["SELECT * FROM t;".into()]);
    assert_eq!(r[0]["ok"], false);
}
#[test]
fn invalid_types_fail_on_empty_tables() {
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT, v VECTOR(2));",
        "UPDATE t SET n='bad';",
        "SELECT missing FROM t LIMIT 0;",
        "SELECT id FROM t WHERE n='bad';",
        "SELECT id FROM t ORDER BY v;",
    ]);
    assert!(r[1..].iter().all(|r| r["ok"] == false));
}
#[test]
fn filtered_exact_ties_and_vector_projection_are_current() {
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, s VARCHAR(2), v VECTOR(2));",
        "INSERT INTO t VALUES (1,'no','[0,0]'),(8,'ok','[3,4]'),(3,'ok','[-3,-4]');",
        "SELECT id,L2_DISTANCE(v,'[0,0]') AS d FROM t WHERE s='ok' ORDER BY d LIMIT 2;",
        "BEGIN;",
        "UPDATE t SET v='[0,0]' WHERE id=8;",
        "SELECT id FROM t WHERE s='ok' ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 1;",
        "ROLLBACK;",
    ]);
    assert_eq!(r[2]["rows"], json!([[3, 5.0], [8, 5.0]]));
    assert_eq!(r[5]["rows"], json!([[8]]));
}

#[test]
fn equality_after_vector_observes_staged_rows_and_full_projection() {
    let r = run(&[
        "CREATE TABLE t (v VECTOR(2), label VARCHAR(4), n BIGINT, id BIGINT PRIMARY KEY);",
        "INSERT INTO t VALUES ('[0,0]','别',9007199254740993,1),('[3,4]','猫😀',9007199254740993,3),('[-3,-4]','猫😀',9007199254740993,2),('[0,0]','猫😀',9007199254740992,4);",
        "CREATE VECTOR INDEX vi ON t(v);",
        "SELECT label,id,v FROM t WHERE n=9007199254740993 AND label='猫😀' AND n=9007199254740993 ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
        "BEGIN;",
        "UPDATE t SET label='猫😀' WHERE id=1;",
        "DELETE FROM t WHERE id=2;",
        "INSERT INTO t VALUES ('[0,1]','猫😀',9007199254740993,5);",
        "SELECT id FROM t WHERE label='猫😀' AND n=9007199254740993 ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
        "SELECT id FROM t WHERE label='猫😀' AND label='别' ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
        "ROLLBACK;",
        "SELECT id FROM t WHERE label='猫😀' AND n>=9007199254740993 ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
    ]);
    assert_eq!(
        r[3]["rows"],
        json!([["猫😀", 2, [-3.0, -4.0]], ["猫😀", 3, [3.0, 4.0]]])
    );
    assert_eq!(r[8]["rows"], json!([[1], [5]]));
    assert_eq!(r[9]["rows"], json!([]));
    assert_eq!(r[11]["rows"], json!([[2], [3]]));
}

#[test]
fn batched_topk_uses_current_committed_versions_and_transaction_fallback() {
    let data = Data::new();
    let values = (1..=4200)
        .map(|n| {
            format!(
                "({n},'{}','[{},0,0,0]')",
                if n % 2 == 0 { "yes" } else { "no" },
                n % 13
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let topk = "SELECT id FROM t WHERE c='yes' ORDER BY L2_DISTANCE(v,'[0,0,0,0]') LIMIT 10;";
    let replies = session(
        &data,
        &[
            "CREATE TABLE t (id BIGINT PRIMARY KEY, c VARCHAR(3), v VECTOR(4));".into(),
            format!("INSERT INTO t VALUES {values};"),
            "CREATE VECTOR INDEX vi ON t(v);".into(),
            topk.into(),
            "UPDATE t SET v='[0,0,0,0]' WHERE id=2;".into(),
            "DELETE FROM t WHERE id=26;".into(),
            "INSERT INTO t VALUES (26,'yes','[-1,0,0,0]');".into(),
            topk.into(),
            "BEGIN;".into(),
            "UPDATE t SET v='[0,0,0,0]' WHERE id=4;".into(),
            "DELETE FROM t WHERE id=2;".into(),
            topk.into(),
            "ROLLBACK;".into(),
            topk.into(),
            "BEGIN;".into(),
            "UPDATE t SET v='[0,0,0,0]' WHERE id=26;".into(),
            "COMMIT;".into(),
            topk.into(),
        ],
    );
    assert_eq!(
        replies[3]["rows"],
        json!([
            [26],
            [52],
            [78],
            [104],
            [130],
            [156],
            [182],
            [208],
            [234],
            [260]
        ])
    );
    assert_eq!(
        replies[7]["rows"],
        json!([
            [2],
            [52],
            [78],
            [104],
            [130],
            [156],
            [182],
            [208],
            [234],
            [260]
        ])
    );
    assert_eq!(
        replies[11]["rows"],
        json!([
            [4],
            [52],
            [78],
            [104],
            [130],
            [156],
            [182],
            [208],
            [234],
            [260]
        ])
    );
    assert_eq!(replies[13]["rows"], replies[7]["rows"]);
    assert_eq!(
        replies[17]["rows"],
        json!([
            [2],
            [26],
            [52],
            [78],
            [104],
            [130],
            [156],
            [182],
            [208],
            [234]
        ])
    );
    let recovered = session(&data, &[topk.into()]);
    assert_eq!(recovered[0]["rows"], replies[17]["rows"]);
}

#[test]
fn batched_topk_reads_vectors_and_rows_larger_than_its_window_and_batch_budget() {
    let v0 = std::iter::repeat_n("0", 2053).collect::<Vec<_>>().join(",");
    let v1 = format!(
        "1,{}",
        std::iter::repeat_n("0", 2052).collect::<Vec<_>>().join(",")
    );
    let content = "x".repeat(196608);
    let replies=session(&Data::new(),&[
        "CREATE TABLE t (content VARCHAR(262144), v VECTOR(2053), c VARCHAR(1), id BIGINT PRIMARY KEY);".into(),
        format!("INSERT INTO t VALUES ('{content}','[{v1}]','x',1),('{content}','[{v0}]','y',2);"),
        "CREATE VECTOR INDEX vi ON t(v);".into(),
        format!("SELECT id,L2_DISTANCE(v,'[{v0}]') AS d FROM t ORDER BY d LIMIT 2;"),
        format!("SELECT id FROM t WHERE c='y' ORDER BY L2_DISTANCE(v,'[{v0}]') LIMIT 2;"),
    ]);
    assert_eq!(replies[3]["rows"], json!([[2, 0.0], [1, 1.0]]));
    assert_eq!(replies[4]["rows"], json!([[2]]));
}

#[test]
fn screened_topk_refines_the_whole_tied_boundary() {
    let mut rows = vec!["(1,'[3.4028234663852886e38,1.401298464324817e-45]')".to_owned()];
    for id in 2..34 {
        rows.push(format!("({id},'[-3.4028234663852886e38,0]')"));
    }
    let insert = format!("INSERT INTO t VALUES {};", rows.join(","));
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2));",
        &insert,
        "CREATE VECTOR INDEX vi ON t(v);",
        "SELECT id,L2_DISTANCE(v,'[0,0]') AS d FROM t ORDER BY d LIMIT 2;",
        "SELECT id FROM t WHERE id >= 4 ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
        "SELECT id FROM t WHERE id < 1 ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
    ]);
    let d = f64::from(f32::MAX);
    assert_eq!(r[3]["rows"], json!([[2, d], [3, d]]));
    assert_eq!(r[4]["rows"], json!([[4], [5]]));
    assert_eq!(r[5]["rows"], json!([]));
}

#[test]
fn hot_snapshot_projects_current_wal_rows_across_tables_and_commits() {
    let t = "SELECT id,c,body FROM t WHERE c >= '甲' AND n >= 7 ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 1;";
    let u = "SELECT id,c,body FROM u WHERE c >= '甲' AND n >= 7 ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 1;";
    let sql: Vec<_> = [
        "CREATE TABLE t (id BIGINT PRIMARY KEY,c VARCHAR(4),body VARCHAR(12),n BIGINT,v VECTOR(2));",
        "CREATE TABLE u (id BIGINT PRIMARY KEY,c VARCHAR(4),body VARCHAR(12),n BIGINT,v VECTOR(2));",
        "INSERT INTO t VALUES (1,'甲','first',7,'[1,0]'),(2,'甲','second',9,'[2,0]');",
        "INSERT INTO u VALUES (1,'甲😀','other',7,'[0,0]');",
        "CREATE VECTOR INDEX ti ON t(v);",
        "CREATE VECTOR INDEX ui ON u(v);",
        t, u, t,
        "UPDATE t SET c='a' WHERE id=1;", t,
        "DELETE FROM t WHERE id=1;",
        "INSERT INTO t VALUES (1,'甲','reinserted',7,'[0,0]');", t,
        "BEGIN;",
        "UPDATE t SET n=0 WHERE id=1;", t,
        "ROLLBACK;", t,
        "BEGIN;",
        "UPDATE t SET body='committed', v='[0,0]' WHERE id=2;",
        "UPDATE t SET v='[2,0]' WHERE id=1;",
        "COMMIT;", t,
        "CREATE VECTOR INDEX ti2 ON t(v);", t,
    ].into_iter().map(str::to_owned).collect();
    for budget in [8 << 20, 1 << 20] {
        let data = Data::new();
        let r = session_with_hot_budget(&data, &sql, budget);
        assert!(r.iter().all(|x| x["ok"] == true), "{r:?}");
        assert_eq!(r[6]["rows"], json!([[1, "甲", "first"]]));
        assert_eq!(r[7]["rows"], json!([[1, "甲😀", "other"]]));
        assert_eq!(r[8]["rows"], r[6]["rows"]);
        assert_eq!(r[10]["rows"], json!([[2, "甲", "second"]]));
        assert_eq!(r[13]["rows"], json!([[1, "甲", "reinserted"]]));
        assert_eq!(r[16]["rows"], r[10]["rows"]);
        assert_eq!(r[18]["rows"], r[13]["rows"]);
        assert_eq!(r[23]["rows"], json!([[2, "甲", "committed"]]));
        assert_eq!(r[25]["rows"], r[23]["rows"]);
        let recovered = session_with_hot_budget(&data, &[t.into(), u.into()], budget);
        assert_eq!(recovered[0]["rows"], r[23]["rows"]);
        assert_eq!(recovered[1]["rows"], r[7]["rows"]);
    }
}

#[test]
fn oversized_hot_build_row_falls_back_to_exact_wal_scan() {
    let body = "x".repeat(400_000);
    let insert = format!("INSERT INTO t VALUES (1,'{body}','[0,0]'),(2,'small','[1,0]');");
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY,body VARCHAR(500000),v VECTOR(2));",
        &insert,
        "CREATE VECTOR INDEX vi ON t(v);",
        "SELECT id FROM t ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
        "SELECT id FROM t WHERE body='small' ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
    ]);
    assert_eq!(r[3]["rows"], json!([[1], [2]]));
    assert_eq!(r[4]["rows"], json!([[2]]));
}

#[test]
fn hot_distance_projection_uses_its_own_query_vector() {
    let r = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY,v VECTOR(2));",
        "INSERT INTO t VALUES (1,'[0,0]'),(2,'[1,0]');",
        "CREATE VECTOR INDEX vi ON t(v);",
        "SELECT id,L2_DISTANCE(v,'[0,0]') AS d0,L2_DISTANCE(v,'[1,0]') AS d1 FROM t ORDER BY d0 LIMIT 2;",
        "SELECT id,v,L2_DISTANCE(v,'[1,0]') AS d1 FROM t ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 2;",
    ]);
    assert_eq!(r[3]["rows"], json!([[1, 0.0, 1.0], [2, 1.0, 0.0]]));
    assert_eq!(
        r[4]["rows"],
        json!([[1, [0.0, 0.0], 1.0], [2, [1.0, 0.0], 0.0]])
    );
}

#[test]
fn compact_commits_recover_multiple_blocks_and_mix_with_legacy_rows() {
    let data = Data::new();
    let suffix = "重复后缀".repeat(8);
    let rows = (1..=520)
        .map(|id| format!("('文档{id:04}{suffix}',{id},'[1,-0.0,0]','甲')"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = [
        "CREATE TABLE t (body VARCHAR(500000), id BIGINT PRIMARY KEY, v VECTOR(3), c VARCHAR(2));"
            .into(),
        "BEGIN;".into(),
        format!("INSERT INTO t VALUES {rows};"),
        "COMMIT;".into(),
        "CREATE VECTOR INDEX vi ON t(v);".into(),
    ];
    let result = session(&data, &sql);
    assert!(result.iter().all(|r| r["ok"] == true), "{result:?}");
    let wal = fs::read(data.0.join("commits.mv1")).unwrap();
    assert!(wal.windows(8).any(|bytes| bytes == b"MVTX0002"));
    assert!(wal.len() < 30_000, "shared text must shrink: {}", wal.len());

    let project = "SELECT id,body,v FROM t WHERE c='甲' ORDER BY L2_DISTANCE(v,'[1,0,0]') LIMIT 2;";
    for budget in [0, 8 << 20] {
        let reply = session_with_hot_budget(
            &data,
            &[
                project.into(),
                "SELECT id FROM t ORDER BY L2_DISTANCE(v,'[1,0,0]') LIMIT 2;".into(),
            ],
            budget,
        );
        assert_eq!(
            reply[0]["rows"],
            json!([
                [1, format!("文档0001{suffix}"), [1.0, -0.0, 0.0]],
                [2, format!("文档0002{suffix}"), [1.0, -0.0, 0.0]]
            ])
        );
        assert_eq!(reply[1]["rows"], json!([[1], [2]]));
    }
    let large = "z".repeat(300_000);
    let reply = session(
        &data,
        &[
            format!("UPDATE t SET body='{large}',v='[0,0,0]' WHERE id=1;"),
            "DELETE FROM t WHERE id=2;".into(),
            "INSERT INTO t VALUES ('重插',2,'[1,0,0]','甲');".into(),
        ],
    );
    assert!(reply.iter().all(|r| r["ok"] == true), "{reply:?}");
    let reply = session(&data, &[project.into()]);
    assert_eq!(
        reply[0]["rows"],
        json!([
            [2, "重插", [1.0, 0.0, 0.0]],
            [3, format!("文档0003{suffix}"), [1.0, -0.0, 0.0]]
        ])
    );
}

#[test]
fn spill_from_failed_statement_keeps_prior_transaction_and_recovery() {
    let data = Data::new();
    let wide = "a".repeat(1 << 20);
    let commands = [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, body VARCHAR(1048576));".to_owned(),
        "INSERT INTO t VALUES (1,'old');".to_owned(),
        "BEGIN;".to_owned(),
        "UPDATE t SET body='noop' WHERE id=9;".to_owned(),
        "INSERT INTO t VALUES (2,'discard'),(1,'duplicate');".to_owned(),
        "INSERT INTO t VALUES (2,'retained');".to_owned(),
        format!("INSERT INTO t VALUES (3,'{wide}'),(1,'duplicate');"),
        "SELECT id,body FROM t ORDER BY id;".to_owned(),
        "COMMIT;".to_owned(),
    ];
    let replies = session(&data, &commands);
    for i in [4, 6] {
        assert_eq!(replies[i]["error"]["code"], "duplicate_key");
    }
    let expected = json!([[1, "old"], [2, "retained"]]);
    assert_eq!(replies[7]["rows"], expected);
    assert_eq!(replies[8]["ok"], true);
    let reopened = session(&data, &["SELECT id,body FROM t ORDER BY id;".into()]);
    assert_eq!(reopened[0]["rows"], expected);
}

#[test]
fn a_late_insert_syntax_error_never_publishes_staged_rows() {
    let data = Data::new();
    let result = session(
        &data,
        &[
            "CREATE TABLE t (id BIGINT PRIMARY KEY, s VARCHAR(24));".into(),
            "BEGIN;".into(),
            "INSERT INTO t VALUES (1,'earlier');".into(),
            "/* prefix */ INSERT INTO `T` (s,id) VALUES ('can''t',2),('逗号,括号)',3),('unfinished"
                .into(),
            "SELECT id,s FROM t ORDER BY id;".into(),
            "INSERT INTO t VALUES (4,'after') /* still valid */;".into(),
            "COMMIT;".into(),
        ],
    );
    assert_eq!(result[3]["error"]["code"], "unsupported_sql");
    assert_eq!(result[4]["rows"], json!([[1, "earlier"]]));
    assert_eq!(result[5]["affected_rows"], 1);
    let reply = session(&data, &["SELECT id,s FROM t ORDER BY id;".into()]);
    assert_eq!(reply[0]["rows"], json!([[1, "earlier"], [4, "after"]]));
}
