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
fn exact_topk_resolves_distances_that_round_to_the_same_f64() {
    let responses = run(&[
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2));",
        "INSERT INTO t VALUES (1,'[3e38,1]'),(2,'[3e38,0]'),(3,'[-3e38,0]');",
        "SELECT id FROM t ORDER BY L2_DISTANCE(v,'[0,0]') LIMIT 3;",
    ]);
    assert_eq!(responses[2]["rows"], json!([[2], [3], [1]]));
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_minivectordb"))
        .arg("--data-dir")
        .arg(&data.0)
        .env("MINIVECTORDB_SORT_BYTES", "4096")
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
