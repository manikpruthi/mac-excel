//! A full recalculation keeps the spill area an array already holds: the first-entered array keeps
//! its cells and a formula entered later that would spill over them shows #SPILL! — as Excel does,
//! and as the incremental recalculation already did. Found by the command-sequence fuzz (seed 5):
//! `recalc_all` used to clear every spill and re-lay them out in row-major order, which let the
//! later formula take the area.

use gridcraft_core::{CellError, CellRef, Value};
use gridcraft_engine::Session;
use gridcraft_engine::calc::Calc;
use serde_json::json;

#[test]
fn recalc_keeps_spill_ownership() {
    let mut s = Session::new();
    s.execute("file.new", json!({"sample": "grades"})).expect("sample opens");
    s.execute("home.deleteRows", json!({"rows": "3:5"})).ok();
    s.execute("range.fill", json!({"input": "=TRANSPOSE(K3:M8)", "range": "F23:I30"})).ok();
    s.execute("home.deleteCells", json!({"range": "F8:J12", "shift": "up"})).ok();
    s.execute("home.insertColumns", json!({"cols": "F:G"})).ok();
    s.execute("cell.set", json!({"cell": "P24", "input": "=TRANSPOSE(K8:O13)"})).ok();

    let d = s.doc().expect("document");
    let si = d.wb.sheets.iter().position(|sh| sh.name == "Grades").expect("grades sheet");
    let p24 = d.wb.sheets[si].cell(CellRef::new(23, 15)).map(|c| c.value.clone()).unwrap_or(Value::Empty);
    let k25 = d.wb.sheets[si].cell(CellRef::new(24, 10)).map(|c| c.value.clone()).unwrap_or(Value::Empty);
    assert_eq!(p24, Value::Error(CellError::Spill), "the later TRANSPOSE is blocked by the array the fill laid out first");
    assert_eq!(k25, Value::Number(0.0), "the earlier array keeps its area");

    // The invariant the command-sequence fuzz checks: a full recalculation from scratch agrees
    // with the incrementally maintained values.
    let mut fresh = (*d.wb).clone();
    Calc::new().recalc_all(&mut fresh);
    let p24b = fresh.sheets[si].cell(CellRef::new(23, 15)).map(|c| c.value.clone()).unwrap_or(Value::Empty);
    let k25b = fresh.sheets[si].cell(CellRef::new(24, 10)).map(|c| c.value.clone()).unwrap_or(Value::Empty);
    assert_eq!(p24b, Value::Error(CellError::Spill), "full recalc keeps the same spill ownership");
    assert_eq!(k25b, Value::Number(0.0), "full recalc keeps the earlier array's values");
}
