//! PivotTable source reading and aggregation, and GETPIVOTDATA's selection over it.
//!
//! The report layout (what the written cells look like) lives in the engine; reading the source
//! list, interning its items, accumulating the aggregates and selecting one cell's value are
//! pure workbook computations, so they sit here where the calculator can reach them
//! (GETPIVOTDATA evaluates a PivotTable's definition, not its laid-out cells).
//!
//! Behaviour follows Excel's documented PivotTable model; the code is original.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use super::PivotDateGroup;
use gridcraft_core::date::datetime_from_serial;
use gridcraft_core::{CellError, CellRef, MAX_COLS, RangeRef, Value, number_to_text, split_sheet};
use gridcraft_numfmt::{NumberFormat, format_value};

use crate::{PivotField, PivotFilter, PivotFunc, PivotShowAs, PivotTable, PivotValue, Sheet, Workbook};

/// Largest source block read (cells).
pub const MAX_SOURCE_CELLS: u64 = 10_000_000;
/// Most fields in the Rows plus Columns areas.
pub const MAX_AXIS_FIELDS: usize = 32;
/// Most fields in the Values area.
pub const MAX_VALUE_FIELDS: usize = 256;

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

const FIELD_NAME_ERR: &str =
    "The PivotTable field name is not valid. To create a PivotTable, you must use data that is organized as a list with labeled columns.";

// ---------------------------------------------------------------- source

/// The source list: header names, per-column number formats and the records.
#[derive(Clone, Debug)]
pub struct Source {
    pub sheet: usize,
    pub range: RangeRef,
    pub headers: Vec<String>,
    pub formats: Vec<String>,
    /// Every non-blank value in the column is a number.
    pub numeric: Vec<bool>,
    /// Numeric with a date number format.
    pub dates: Vec<bool>,
    pub rows: Vec<Vec<Value>>,
}

impl Source {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.headers.iter().position(|h| h.eq_ignore_ascii_case(name))
    }
}

/// Resolves source text (range, table name or defined name) to (sheet, range including header).
pub fn resolve_source(wb: &Workbook, text: &str, default_sheet: usize) -> Result<(usize, RangeRef), String> {
    resolve_inner(wb, text, default_sheet, 0)
}

fn resolve_inner(wb: &Workbook, text: &str, default_sheet: usize, depth: u32) -> Result<(usize, RangeRef), String> {
    const INVALID: &str = "The reference isn't valid.";
    let t = text.trim().trim_start_matches('=').trim();
    if t.is_empty() || depth > 4 {
        return Err(INVALID.into());
    }
    if let Some((si, ti)) = wb.table(t)
        && let Some(tb) = wb.sheet(si).and_then(|s| s.tables.get(ti))
    {
        if !tb.header_row {
            return Err("The table needs a header row to be used as a PivotTable source.".into());
        }
        let end_row = tb.range.end.row.saturating_sub(tb.totals_row as u32).max(tb.range.start.row);
        return Ok((si, RangeRef::new(tb.range.start, CellRef::new(end_row, tb.range.end.col))));
    }
    let (sheet, body) = split_sheet(t);
    if let Some(r) = RangeRef::parse(body) {
        let si = match sheet {
            Some(n) => wb.sheet_index(&n).ok_or_else(|| INVALID.to_string())?,
            None => default_sheet,
        };
        let sh = wb.sheet(si).ok_or_else(|| INVALID.to_string())?;
        let r = if r.is_full_cols() || r.is_full_rows() {
            let used = sh.used_range().ok_or_else(|| INVALID.to_string())?;
            r.intersection(&used).ok_or_else(|| INVALID.to_string())?
        } else {
            r
        };
        return Ok((si, r));
    }
    if sheet.is_none()
        && let Some(n) = wb.name(t, default_sheet)
    {
        let f = n.formula.clone();
        return resolve_inner(wb, &f, default_sheet, depth + 1);
    }
    Err(INVALID.into())
}

/// Reads the source list.
pub fn read_source(wb: &Workbook, text: &str, default_sheet: usize) -> Result<Source, String> {
    let (si, r) = resolve_source(wb, text, default_sheet)?;
    let sh = wb.sheet(si).ok_or("The reference isn't valid.")?;
    if r.count() > MAX_SOURCE_CELLS {
        return Err("The source range is too large for a PivotTable.".into());
    }
    let mut headers: Vec<String> = Vec::new();
    // Names taken so far (lower-case), and the next suffix to try for each repeated header, so
    // that thousands of equal headers don't take cubic time.
    let mut taken: HashSet<String> = HashSet::new();
    let mut next_suffix: HashMap<String, usize> = HashMap::new();
    let mut formats = Vec::new();
    for c in r.start.col..=r.end.col {
        let h = value_text(wb, sh, CellRef::new(r.start.row, c)).trim().to_string();
        if h.is_empty() {
            return Err(FIELD_NAME_ERR.into());
        }
        let key = h.to_ascii_lowercase();
        let mut n = h.clone();
        let mut k = next_suffix.get(&key).copied().unwrap_or(2);
        while taken.contains(&n.to_ascii_lowercase()) {
            n = format!("{h}{k}");
            k += 1;
        }
        next_suffix.insert(key, k);
        taken.insert(n.to_ascii_lowercase());
        headers.push(n);
        let fmt = if r.end.row > r.start.row {
            wb.styles.get(sh.style_id(CellRef::new(r.start.row + 1, c))).num_fmt.as_str().to_string()
        } else {
            "General".into()
        };
        formats.push(fmt);
    }
    let w = headers.len();
    let mut rows = Vec::new();
    let mut numeric = vec![true; w];
    let mut any = vec![false; w];
    if r.end.row > r.start.row {
        for row in r.start.row + 1..=r.end.row {
            let rec: Vec<Value> = (r.start.col..=r.end.col).map(|c| sh.value(CellRef::new(row, c)).scalar()).collect();
            if rec.iter().all(Value::is_empty) {
                continue;
            }
            for (i, v) in rec.iter().enumerate() {
                if !v.is_empty() {
                    if let Some(a) = any.get_mut(i) {
                        *a = true;
                    }
                    if !v.is_number()
                        && let Some(n) = numeric.get_mut(i)
                    {
                        *n = false;
                    }
                }
            }
            rows.push(rec);
        }
    }
    for (n, a) in numeric.iter_mut().zip(any.iter()) {
        *n = *n && *a;
    }
    let dates = numeric.iter().zip(formats.iter()).map(|(n, f)| *n && NumberFormat::parse(f).is_date()).collect();
    Ok(Source { sheet: si, range: r, headers, formats, numeric, dates, rows })
}

/// A cell's displayed text, as the pivot reads headers and item labels.
fn value_text(wb: &Workbook, sh: &Sheet, at: CellRef) -> String {
    let v = sh.value(at);
    let code = wb.styles.get(sh.style_id(at)).num_fmt.as_str().to_string();
    formatted(&v, &code, wb)
}

/// A value as its number format shows it (General for anything else).
fn formatted(v: &Value, code: &str, wb: &Workbook) -> String {
    format_value(v, &parsed(code), wb.date_system).text
}

/// Parsed number formats are cached by code.
fn parsed(code: &str) -> NumberFormat {
    static CACHE: LazyLock<Mutex<HashMap<String, NumberFormat>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    let lock = || CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(f) = lock().get(code) {
        return f.clone();
    }
    let f = NumberFormat::parse(code);
    let mut m = lock();
    if m.len() > 4096 {
        m.clear();
    }
    m.insert(code.to_string(), f.clone());
    f
}

// ---------------------------------------------------------------- items

/// One interned item of a field: the value, its label, and how it compares.
#[derive(Clone, Debug)]
pub struct Item {
    pub value: Value,
    pub label: String,
    class: u8,
    num: f64,
    text: String,
    pub first: usize,
    /// The value is a number shown with the source format.
    pub formatted: bool,
}

#[derive(Default)]
pub struct FieldItems {
    items: Vec<Item>,
    index: HashMap<String, u32>,
}

pub fn cmp_items(a: &Item, b: &Item) -> Ordering {
    a.class.cmp(&b.class).then(a.num.total_cmp(&b.num)).then_with(|| a.text.cmp(&b.text))
}

/// (identity key, item) for a source value under a field's date grouping.
fn make_item(wb: &Workbook, v: &Value, group: PivotDateGroup, fmt: &str, first: usize) -> (String, Item) {
    let it = |value: Value, label: String, class: u8, num: f64, formatted: bool| Item {
        text: label.to_lowercase(),
        value,
        label,
        class,
        num,
        first,
        formatted,
    };
    match v {
        Value::Empty => ("b".into(), it(Value::text("(blank)"), "(blank)".into(), 4, 0.0, false)),
        Value::Number(n) => {
            if group != PivotDateGroup::None
                && let Some(dt) = datetime_from_serial(wb.date_system, *n)
            {
                let mon = MONTHS.get(dt.month.saturating_sub(1) as usize).copied().unwrap_or("Jan");
                let (label, key) = match group {
                    PivotDateGroup::Years => (dt.year.to_string(), dt.year as f64),
                    PivotDateGroup::Quarters => {
                        let q = (dt.month.saturating_sub(1)) / 3 + 1;
                        (format!("Qtr{q}"), q as f64)
                    }
                    PivotDateGroup::Months => (mon.to_string(), dt.month as f64),
                    _ => (format!("{}-{mon}", dt.day), (dt.month * 100 + dt.day) as f64),
                };
                return (format!("g{key}"), it(Value::text(label.as_str()), label, 0, key, false));
            }
            let label = formatted(v, fmt, wb);
            (format!("n{}", n.to_bits()), it(v.clone(), label, 0, *n, true))
        }
        Value::Text(t) => (format!("t{}", t.to_lowercase()), it(v.clone(), t.to_string(), 1, 0.0, false)),
        Value::Bool(b) => {
            let l = if *b { "TRUE" } else { "FALSE" };
            (format!("l{l}"), it(v.clone(), l.into(), 2, *b as u8 as f64, false))
        }
        Value::Error(e) => (format!("e{}", e.as_str()), it(v.clone(), e.as_str().into(), 3, 0.0, false)),
        Value::Array(_) => make_item(wb, &v.scalar(), group, fmt, first),
    }
}

impl FieldItems {
    pub fn intern(&mut self, wb: &Workbook, v: &Value, group: PivotDateGroup, fmt: &str, first: usize) -> u32 {
        let (key, item) = make_item(wb, v, group, fmt, first);
        if let Some(i) = self.index.get(&key) {
            return *i;
        }
        let i = self.items.len() as u32;
        self.items.push(item);
        self.index.insert(key, i);
        i
    }
    pub fn get(&self, i: u32) -> Option<&Item> {
        self.items.get(i as usize)
    }
    pub fn label(&self, i: u32) -> String {
        self.get(i).map(|x| x.label.clone()).unwrap_or_default()
    }
}

/// The label of a source value under a date grouping (for filters and field lists).
pub fn item_label(wb: &Workbook, v: &Value, group: PivotDateGroup, fmt: &str) -> String {
    make_item(wb, v, group, fmt, 0).1.label
}

/// Distinct item labels of a source column, sorted like the field list shows them.
pub fn distinct_labels(wb: &Workbook, src: &Source, col: usize, group: PivotDateGroup, limit: usize) -> Vec<String> {
    let mut fi = FieldItems::default();
    let fmt = src.formats.get(col).map(String::as_str).unwrap_or("General");
    for (i, r) in src.rows.iter().enumerate() {
        if let Some(v) = r.get(col) {
            fi.intern(wb, v, group, fmt, i);
        }
        if fi.items.len() > 100_000 {
            break;
        }
    }
    let mut items = fi.items;
    items.sort_by(cmp_items);
    items.into_iter().take(limit).map(|i| i.label).collect()
}

/// Excel's caption verb for a function ("Sum of Sales").
pub fn func_caption(f: PivotFunc) -> &'static str {
    match f {
        PivotFunc::Sum => "Sum",
        PivotFunc::Count => "Count",
        PivotFunc::Average => "Average",
        PivotFunc::Max => "Max",
        PivotFunc::Min => "Min",
        PivotFunc::Product => "Product",
        PivotFunc::CountNumbers => "Count Numbers",
        PivotFunc::StdDev => "StdDev",
        PivotFunc::StdDevP => "StdDevp",
        PivotFunc::Var => "Var",
        PivotFunc::VarP => "Varp",
    }
}

// ---------------------------------------------------------------- aggregation

/// One accumulator over a set of records.
#[derive(Clone, Debug, Default)]
pub struct Acc {
    count: u64,
    nums: u64,
    sum: f64,
    prod: f64,
    min: f64,
    max: f64,
    mean: f64,
    m2: f64,
    err: Option<CellError>,
}

impl Acc {
    pub fn add(&mut self, v: &Value) {
        match v {
            Value::Empty => {}
            Value::Number(n) => {
                let n = *n;
                self.count += 1;
                self.nums += 1;
                self.sum += n;
                if self.nums == 1 {
                    self.prod = n;
                    self.min = n;
                    self.max = n;
                } else {
                    self.prod *= n;
                    self.min = self.min.min(n);
                    self.max = self.max.max(n);
                }
                let d = n - self.mean;
                self.mean += d / self.nums as f64;
                self.m2 += d * (n - self.mean);
            }
            Value::Error(e) => {
                self.count += 1;
                if self.err.is_none() {
                    self.err = Some(*e);
                }
            }
            _ => self.count += 1,
        }
    }
    pub fn result(&self, f: PivotFunc) -> Value {
        let n = self.nums as f64;
        match f {
            PivotFunc::Count => return Value::number(self.count as f64),
            PivotFunc::CountNumbers => return Value::number(n),
            _ => {}
        }
        if let Some(e) = self.err {
            return Value::Error(e);
        }
        let div0 = Value::Error(CellError::Div0);
        match f {
            PivotFunc::Sum => Value::number(self.sum),
            PivotFunc::Average => {
                if self.nums == 0 {
                    div0
                } else {
                    Value::number(self.sum / n)
                }
            }
            PivotFunc::Max => Value::number(if self.nums == 0 { 0.0 } else { self.max }),
            PivotFunc::Min => Value::number(if self.nums == 0 { 0.0 } else { self.min }),
            PivotFunc::Product => Value::number(if self.nums == 0 { 0.0 } else { self.prod }),
            PivotFunc::StdDev | PivotFunc::Var => {
                if self.nums < 2 {
                    div0
                } else {
                    let var = (self.m2 / (n - 1.0)).max(0.0);
                    Value::number(if f == PivotFunc::Var { var } else { var.sqrt() })
                }
            }
            PivotFunc::StdDevP | PivotFunc::VarP => {
                if self.nums < 1 {
                    div0
                } else {
                    let var = (self.m2 / n).max(0.0);
                    Value::number(if f == PivotFunc::VarP { var } else { var.sqrt() })
                }
            }
            PivotFunc::Count | PivotFunc::CountNumbers => Value::Empty,
        }
    }
}

/// Where a value field's numbers come from.
#[derive(Clone, Copy, PartialEq)]
pub enum ValSrc {
    /// A source column.
    Col(usize),
    /// A calculated field (index into the PivotTable's calculated fields).
    Calc(usize),
}

pub type AggMap = HashMap<(Vec<u32>, Vec<u32>), Vec<Acc>>;

/// A report filter (or, for a field also in Rows/Columns, its hidden-items filter), with the
/// source column, and whether the field is on no axis (a pure report filter).
pub type ReportFilter<'a> = (usize, &'a PivotFilter, PivotDateGroup, bool);

pub fn report_filters<'a>(pt: &'a PivotTable, src: &Source) -> (Vec<ReportFilter<'a>>, Vec<ReportFilter<'a>>) {
    let axis_group =
        |name: &str| pt.rows.iter().chain(pt.columns.iter()).find(|field| field.source_col.eq_ignore_ascii_case(name)).map(|field| field.date_group);
    let filters: Vec<_> = pt
        .filters
        .iter()
        .filter_map(|filter| {
            let column = src.col(&filter.source_col)?;
            let group = axis_group(&filter.source_col);
            Some((column, filter, group.unwrap_or_default(), group.is_none()))
        })
        .collect();
    let shown = filters.iter().copied().filter(|filter| filter.3).collect();
    (filters, shown)
}

/// Why Show Details turns a cell down when it isn't a value cell of a PivotTable (the front end
/// edits that cell instead).
pub const NOT_A_VALUE_CELL: &str = "select a value cell in a PivotTable";

/// What [`detail_records`] and [`detail_cells`] ask of a report computation: the row and column
/// item paths of the value cell at `at`, the paths of every record the filters let through (when
/// `paths` is set), and every value cell (when `cells` is set).
#[derive(Default)]
pub struct Probe {
    pub at: Option<(usize, usize)>,
    pub hit: Option<(Vec<u32>, Vec<u32>)>,
    pub paths: Option<Vec<ProbePath>>,
    pub cells: Option<HashSet<(usize, usize)>>,
}

/// One source record's row and column item paths, and its index in the source.
pub type ProbePath = (usize, Vec<u32>, Vec<u32>);

pub struct AggregateInput<'a> {
    pub wb: &'a Workbook,
    pub pt: &'a PivotTable,
    pub src: &'a Source,
    pub rows: &'a [(usize, &'a PivotField)],
    pub cols: &'a [(usize, &'a PivotField)],
    pub vals: &'a [(ValSrc, &'a PivotValue)],
    pub extra: &'a [usize],
    pub filters: &'a [ReportFilter<'a>],
    pub filter_fmts: &'a [String],
    pub row_fmts: &'a [String],
    pub col_fmts: &'a [String],
    pub empty: bool,
}

#[derive(Default)]
pub struct AggregateData {
    pub row_items: Vec<FieldItems>,
    pub col_items: Vec<FieldItems>,
    pub aggs: AggMap,
    pub row_set: HashSet<Vec<u32>>,
    pub col_set: HashSet<Vec<u32>>,
    pub col_leaves: HashSet<Vec<u32>>,
}

fn aggregate_record(
    input: &AggregateInput<'_>,
    data: &mut AggregateData,
    probe: &mut Option<&mut Probe>,
    ri: usize,
    rec: &[Value],
) -> Result<(), String> {
    for ((c, field, group, _), fmt) in input.filters.iter().zip(input.filter_fmts) {
        if let Some(selected) = &field.selected {
            let label = item_label(input.wb, rec.get(*c).unwrap_or(&Value::Empty), *group, fmt);
            if !selected.iter().any(|value| value.eq_ignore_ascii_case(&label)) {
                return Ok(());
            }
        }
    }
    if input.empty {
        return Ok(());
    }
    let mut row_path = Vec::with_capacity(input.rows.len());
    for (((column, field), items), fmt) in input.rows.iter().zip(data.row_items.iter_mut()).zip(input.row_fmts) {
        row_path.push(items.intern(input.wb, rec.get(*column).unwrap_or(&Value::Empty), field.date_group, fmt, ri));
    }
    let mut col_path = Vec::with_capacity(input.cols.len());
    for (((column, field), items), fmt) in input.cols.iter().zip(data.col_items.iter_mut()).zip(input.col_fmts) {
        col_path.push(items.intern(input.wb, rec.get(*column).unwrap_or(&Value::Empty), field.date_group, fmt, ri));
    }
    for row_depth in 0..=row_path.len() {
        for col_depth in 0..=col_path.len() {
            let accs = data
                .aggs
                .entry((row_path[..row_depth].to_vec(), col_path[..col_depth].to_vec()))
                .or_insert_with(|| vec![Acc::default(); input.vals.len() + input.extra.len()]);
            for (acc, (source, _)) in accs.iter_mut().zip(input.vals) {
                if let ValSrc::Col(column) = source {
                    acc.add(rec.get(*column).unwrap_or(&Value::Empty));
                }
            }
            for (acc, column) in accs.iter_mut().skip(input.vals.len()).zip(input.extra) {
                acc.add(rec.get(*column).unwrap_or(&Value::Empty));
            }
        }
    }
    let shown_depth = input
        .cols
        .iter()
        .zip(data.col_items.iter())
        .zip(col_path.iter())
        .position(|(((_, field), items), &item)| !field.collapsed_items.is_empty() && is_collapsed(field, &items.label(item)))
        .map_or(col_path.len(), |depth| depth + 1);
    data.col_leaves.insert(col_path[..shown_depth].to_vec());
    if let Some(paths) = probe.as_deref_mut().and_then(|value| value.paths.as_mut()) {
        paths.push((ri, row_path.clone(), col_path.clone()));
    }
    data.row_set.insert(row_path);
    data.col_set.insert(col_path);
    if !input.cols.is_empty() && data_cols_exceed_sheet(data.col_leaves.len(), input.pt.grand_totals_rows, input.vals.len()) {
        return Err("The PivotTable report will not fit on the sheet.".into());
    }
    if data.aggs.len() > 5_000_000 {
        return Err("The PivotTable report is too large.".into());
    }
    Ok(())
}

pub fn aggregate_source(input: AggregateInput<'_>, probe: &mut Option<&mut Probe>) -> Result<AggregateData, String> {
    let mut data = AggregateData {
        row_items: input.rows.iter().map(|_| FieldItems::default()).collect(),
        col_items: input.cols.iter().map(|_| FieldItems::default()).collect(),
        ..AggregateData::default()
    };
    for (ri, record) in input.src.rows.iter().enumerate() {
        aggregate_record(&input, &mut data, probe, ri, record)?;
    }
    Ok(data)
}

/// True when `col_groups` distinct column leaves (plus the grand-total column) times `values`
/// value fields (at least one data column per group) already exceed the sheet width. This is a
/// lower bound on the final column count.
pub fn data_cols_exceed_sheet(col_groups: usize, grand_total: bool, values: usize) -> bool {
    (col_groups + usize::from(grand_total)).saturating_mul(values.max(1)) > MAX_COLS as usize
}

/// Whether a field's item is collapsed (its detail is hidden).
pub fn is_collapsed(field: &PivotField, label: &str) -> bool {
    field.collapsed_items.iter().any(|c| c.eq_ignore_ascii_case(label))
}

// ---------------------------------------------------------------- GETPIVOTDATA

/// One GETPIVOTDATA field/item pair: the field's source column name and the items that satisfy
/// it (several for Excel's `{"Mar","Apr"}`).
#[derive(Clone, Debug)]
pub struct GetPair {
    pub field: String,
    pub items: Vec<Value>,
}

/// Which area a pair's field is in.
enum Axis {
    Row(usize),
    Col(usize),
    Filter(usize),
}

/// GETPIVOTDATA's value: the aggregate `data_field` reports over the source records whose row,
/// column and report-filter items match `pairs` — a field left out of the pairs totals over all
/// its items, as Excel's subtotals and grand totals do. `Ok(None)`: no such value field, no such
/// field or item, or no record matches — Excel's `#REF!`. A calculated value field is not
/// resolved here (it needs the calculator; the refresh writes its cells, so the report on the
/// sheet still shows it).
///
/// The pairs match the items as the report shows them (labels, date-grouped captions, formatted
/// numbers), and the report filters apply first, so a combination a filter hides is `#REF!` as
/// Excel documents it.
pub fn select(wb: &Workbook, pt: &PivotTable, pivot_sheet: usize, data_field: &str, pairs: &[GetPair]) -> Result<Option<Value>, String> {
    let src = read_source(wb, &pt.source, pivot_sheet)?;
    let rows: Vec<(usize, &PivotField)> = pt.rows.iter().filter_map(|f| src.col(&f.source_col).map(|c| (c, f))).collect();
    let cols: Vec<(usize, &PivotField)> = pt.columns.iter().filter_map(|f| src.col(&f.source_col).map(|c| (c, f))).collect();
    let vals: Vec<(ValSrc, &PivotValue)> = pt.values.iter().filter_map(|v| src.col(&v.source_col).map(|c| (ValSrc::Col(c), v))).collect();
    if rows.len() + cols.len() > MAX_AXIS_FIELDS {
        return Err(format!("A PivotTable can have at most {MAX_AXIS_FIELDS} row and column fields."));
    }
    let field = data_field.trim();
    let Some(vi) = vals.iter().position(|(_, v)| v.name.eq_ignore_ascii_case(field) || v.source_col.eq_ignore_ascii_case(field)) else {
        return Ok(None);
    };
    // The pairs, as axes. A field of the Values area is not a pair field.
    let mut cons: Vec<(Axis, Vec<Value>)> = Vec::new();
    for p in pairs {
        if p.items.is_empty() {
            return Ok(None);
        }
        let axis = rows
            .iter()
            .position(|(_, f)| f.source_col.eq_ignore_ascii_case(&p.field))
            .map(Axis::Row)
            .or_else(|| cols.iter().position(|(_, f)| f.source_col.eq_ignore_ascii_case(&p.field)).map(Axis::Col))
            .or_else(|| pt.filters.iter().position(|f| f.source_col.eq_ignore_ascii_case(&p.field)).map(Axis::Filter));
        let Some(axis) = axis else { return Ok(None) };
        cons.push((axis, p.items.clone()));
    }
    let (filters, _) = report_filters(pt, &src);
    let fmt_of = |c: usize| src.formats.get(c).cloned().unwrap_or_else(|| "General".into());
    // The aggregates over the records the filters and a subset of the pairs let through. The
    // show-values-as percentages divide by totals with some pairs dropped: the grand total by
    // all of them, a row total by the column pairs, a column total by the row pairs.
    let aggregate = |skip_rows: bool, skip_cols: bool| -> Option<Vec<Acc>> {
        let mut accs = vec![Acc::default(); vals.len()];
        let mut any = false;
        'rec: for rec in &src.rows {
            for (c, filter, group, _) in &filters {
                if let Some(selected) = &filter.selected
                    && !selected.iter().any(|v| v.eq_ignore_ascii_case(&item_label(wb, rec.get(*c).unwrap_or(&Value::Empty), *group, &fmt_of(*c))))
                {
                    continue 'rec;
                }
            }
            for (axis, items) in &cons {
                if (matches!(axis, Axis::Row(_)) && skip_rows) || (matches!(axis, Axis::Col(_)) && skip_cols) {
                    continue;
                }
                let (v, group, fmt) = match axis {
                    Axis::Row(i) => {
                        let (c, f) = rows[*i];
                        (rec.get(c).unwrap_or(&Value::Empty), f.date_group, fmt_of(c))
                    }
                    Axis::Col(i) => {
                        let (c, f) = cols[*i];
                        (rec.get(c).unwrap_or(&Value::Empty), f.date_group, fmt_of(c))
                    }
                    Axis::Filter(i) => {
                        let (c, _, g, _) = &filters[*i];
                        (rec.get(*c).unwrap_or(&Value::Empty), *g, fmt_of(*c))
                    }
                };
                if !items.iter().any(|it| pair_item_matches(wb, it, v, group, &fmt)) {
                    continue 'rec;
                }
            }
            any = true;
            for (acc, (source, _)) in accs.iter_mut().zip(&vals) {
                if let ValSrc::Col(c) = source {
                    acc.add(rec.get(*c).unwrap_or(&Value::Empty));
                }
            }
        }
        if any { Some(accs) } else { None }
    };
    let full = aggregate(false, false);
    // A combination with no records behind it is #REF!, as Excel's missing totals are.
    if full.is_none() && !cons.is_empty() {
        return Ok(None);
    }
    let value = |accs: Option<&Vec<Acc>>| {
        // No records at all aggregate like the records that match nothing: the totals of nothing.
        accs.and_then(|a| a.get(vi)).map(|a| a.result(vals[vi].1.func)).unwrap_or_else(|| Acc::default().result(vals[vi].1.func))
    };
    let raw = value(full.as_ref());
    let shown = match vals[vi].1.show_as {
        PivotShowAs::Normal => raw,
        PivotShowAs::PercentOfGrandTotal => ratio(raw, Some(value(aggregate(true, true).as_ref()))),
        PivotShowAs::PercentOfColumnTotal => ratio(raw, Some(value(aggregate(true, false).as_ref()))),
        PivotShowAs::PercentOfRowTotal => ratio(raw, Some(value(aggregate(false, true).as_ref()))),
        // Running totals and ranks need the report's item order; the raw aggregate is honest.
        PivotShowAs::RunningTotal | PivotShowAs::Rank => raw,
    };
    Ok(Some(shown))
}

/// Whether one pair item matches a record's value for its field: text by the label the report
/// shows (case-insensitively, as the field list matches), a number by its value, a blank by the
/// record's blank.
fn pair_item_matches(wb: &Workbook, item: &Value, v: &Value, group: PivotDateGroup, fmt: &str) -> bool {
    match item {
        Value::Text(t) => item_label(wb, v, group, fmt).eq_ignore_ascii_case(t),
        Value::Number(n) => match v {
            Value::Number(x) if group == PivotDateGroup::None => x == n,
            _ => item_label(wb, v, group, fmt).eq_ignore_ascii_case(&number_to_text(*n)),
        },
        Value::Empty => v.is_empty(),
        Value::Bool(b) => item_label(wb, v, group, fmt).eq_ignore_ascii_case(if *b { "TRUE" } else { "FALSE" }),
        _ => false,
    }
}

/// The ratio two aggregates show (a zero total is `#DIV/0!`, an error propagates), as the
/// show-values-as percentages compute it.
pub fn ratio(a: Value, b: Option<Value>) -> Value {
    match (a, b) {
        (Value::Error(error), _) => Value::Error(error),
        (_, Some(Value::Error(error))) => Value::Error(error),
        (Value::Number(_), Some(Value::Number(0.0))) => Value::Error(CellError::Div0),
        (Value::Number(x), Some(Value::Number(y))) => Value::number(x / y),
        (value, _) => value,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gridcraft_core::{CellRef, Value};

    use super::{GetPair, select};
    use crate::{Cell, PivotField, PivotFunc, PivotShowAs, PivotTable, PivotValue, Sheet, StyleId, Workbook};

    fn workbook(rows: &[Vec<Value>]) -> Workbook {
        let mut wb = Workbook::new();
        let mut sh = Sheet::new("Data");
        for (r, row) in rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                sh.cells.set(CellRef::new(r as u32, c as u32), Cell { value: v.clone(), formula: None, style: StyleId::default(), py: None });
            }
        }
        wb.sheets = vec![Arc::new(sh)];
        wb
    }

    fn pivot(rows: &[&str], cols: &[&str], values: &[(&str, PivotFunc)], show_as: PivotShowAs) -> PivotTable {
        PivotTable {
            source: "Data!$A$1:$D$6".into(),
            rows: rows.iter().map(|f| PivotField { source_col: f.to_string(), ..Default::default() }).collect(),
            columns: cols.iter().map(|f| PivotField { source_col: f.to_string(), ..Default::default() }).collect(),
            values: values
                .iter()
                .map(|(f, func)| PivotValue {
                    source_col: f.to_string(),
                    func: *func,
                    name: format!("{:?} of {f}", func),
                    show_as,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn data() -> Vec<Vec<Value>> {
        vec![
            vec![Value::text("Region"), Value::text("Product"), Value::text("Sales"), Value::text("Units")],
            vec![Value::text("East"), Value::text("A"), Value::Number(10.0), Value::Number(1.0)],
            vec![Value::text("East"), Value::text("B"), Value::Number(20.0), Value::Number(2.0)],
            vec![Value::text("West"), Value::text("A"), Value::Number(30.0), Value::Number(3.0)],
            vec![Value::text("West"), Value::text("B"), Value::Number(40.0), Value::Number(4.0)],
            vec![Value::text("East"), Value::text("A"), Value::Number(5.0), Value::Number(1.0)],
        ]
    }

    #[test]
    fn grand_totals_row_column_and_combined_pairs() {
        let wb = workbook(&data());
        let pt = pivot(&["Region"], &["Product"], &[("Sales", PivotFunc::Sum)], PivotShowAs::Normal);
        // Grand total, by caption and by root.
        assert_eq!(select(&wb, &pt, 0, "Sum of Sales", &[]).unwrap(), Some(Value::Number(105.0)));
        assert_eq!(select(&wb, &pt, 0, "Sales", &[]).unwrap(), Some(Value::Number(105.0)));
        // One row pair, one column pair, both.
        let r = |f: &str, i: &str| GetPair { field: f.into(), items: vec![Value::text(i)] };
        assert_eq!(select(&wb, &pt, 0, "Sales", &[r("Region", "East")]).unwrap(), Some(Value::Number(35.0)));
        assert_eq!(select(&wb, &pt, 0, "Sales", &[r("Product", "A")]).unwrap(), Some(Value::Number(45.0)));
        assert_eq!(select(&wb, &pt, 0, "Sales", &[r("Region", "East"), r("Product", "B")]).unwrap(), Some(Value::Number(20.0)));
        // Pairs in any order.
        assert_eq!(select(&wb, &pt, 0, "Sales", &[r("Product", "B"), r("Region", "West")]).unwrap(), Some(Value::Number(40.0)));
        // Several items for one field.
        assert_eq!(
            select(&wb, &pt, 0, "Sales", &[GetPair { field: "Region".into(), items: vec![Value::text("East"), Value::text("West")] }]).unwrap(),
            Some(Value::Number(105.0))
        );
        // Another value field and function.
        let pt = pivot(&["Region"], &[], &[("Units", PivotFunc::Count)], PivotShowAs::Normal);
        assert_eq!(select(&wb, &pt, 0, "Count of Units", &[]).unwrap(), Some(Value::Number(5.0)));
    }

    #[test]
    fn missing_and_hidden_combinations_are_ref() {
        let wb = workbook(&data());
        let pt = pivot(&["Region"], &["Product"], &[("Sales", PivotFunc::Sum)], PivotShowAs::Normal);
        let r = |f: &str, i: &str| GetPair { field: f.into(), items: vec![Value::text(i)] };
        // No such value field, pair field or item.
        assert_eq!(select(&wb, &pt, 0, "Cost", &[]).unwrap(), None);
        assert_eq!(select(&wb, &pt, 0, "Sales", &[r("Color", "Red")]).unwrap(), None);
        assert_eq!(select(&wb, &pt, 0, "Sales", &[r("Region", "North")]).unwrap(), None);
        // An empty items list describes nothing.
        assert_eq!(select(&wb, &pt, 0, "Sales", &[GetPair { field: "Region".into(), items: vec![] }]).unwrap(), None);
        // A report filter that hides the records makes the combination #REF!.
        let mut pt = pivot(&["Region"], &[], &[("Sales", PivotFunc::Sum)], PivotShowAs::Normal);
        pt.filters = vec![crate::PivotFilter { source_col: "Region".into(), selected: Some(vec!["East".into()]) }];
        assert_eq!(select(&wb, &pt, 0, "Sales", &[]).unwrap(), Some(Value::Number(35.0)));
        assert_eq!(select(&wb, &pt, 0, "Sales", &[r("Region", "West")]).unwrap(), None);
    }

    #[test]
    fn show_values_as_percentages_of_totals() {
        let wb = workbook(&data());
        let pt = pivot(&["Region"], &[], &[("Sales", PivotFunc::Sum)], PivotShowAs::PercentOfGrandTotal);
        let r = |f: &str, i: &str| GetPair { field: f.into(), items: vec![Value::text(i)] };
        let east = select(&wb, &pt, 0, "Sales", &[r("Region", "East")]).unwrap().unwrap();
        assert!((east.as_f64().unwrap_or(0.0) - 35.0 / 105.0).abs() < 1e-12, "{east:?}");
        let pt = pivot(&["Region"], &["Product"], &[("Sales", PivotFunc::Sum)], PivotShowAs::PercentOfColumnTotal);
        let east_a = select(&wb, &pt, 0, "Sales", &[r("Region", "East"), r("Product", "A")]).unwrap().unwrap();
        assert!((east_a.as_f64().unwrap_or(0.0) - 15.0 / 45.0).abs() < 1e-12, "{east_a:?}");
        let pt = pivot(&["Region"], &["Product"], &[("Sales", PivotFunc::Sum)], PivotShowAs::PercentOfRowTotal);
        let east_a = select(&wb, &pt, 0, "Sales", &[r("Region", "East"), r("Product", "A")]).unwrap().unwrap();
        assert!((east_a.as_f64().unwrap_or(0.0) - 15.0 / 35.0).abs() < 1e-12, "{east_a:?}");
    }

    #[test]
    fn an_empty_source_aggregates_like_an_empty_report() {
        let wb = workbook(&[vec![Value::text("Region"), Value::text("Sales")]]);
        let mut pt = pivot(&["Region"], &[], &[("Sales", PivotFunc::Sum)], PivotShowAs::Normal);
        pt.source = "Data!$A$1:$B$1".into();
        assert_eq!(select(&wb, &pt, 0, "Sales", &[]).unwrap(), Some(Value::Number(0.0)));
    }

    #[test]
    fn source_reading_matches_the_report_engine() {
        let wb = workbook(&data());
        let read = super::read_source(&wb, "Data!$A$1:$D$6", 0).unwrap();
        assert_eq!(read.sheet, 0);
        assert_eq!(read.headers, vec!["Region", "Product", "Sales", "Units"]);
        assert_eq!(read.rows.len(), 5);
        assert_eq!(read.numeric, vec![false, false, true, true]);
        assert_eq!(read.dates, vec![false, false, false, false]);
    }
}
