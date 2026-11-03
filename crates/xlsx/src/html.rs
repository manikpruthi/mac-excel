//! HTML tables saved with a spreadsheet extension: Excel's old "HTML format" save (an `.xls` that
//! is an HTML document) and `.html`/`.htm` workbook files. The first table becomes the sheet, as
//! Excel opens it (#3166). Styling and everything outside the table are dropped; values, spans
//! (as merges) and `<br>` line breaks are kept.

use std::collections::HashSet;
use std::sync::Arc;

use gridcraft_core::parse::parse_input;
use gridcraft_core::{CellRef, DateSystem, MAX_COLS, MAX_ROWS, RangeRef};
use gridcraft_model::{Cell, NumFmt, Sheet, StyleId, StyleTable, Workbook};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::{IoError, ReadReport};

/// Cells one table may hold, occupied positions of a span included (the bound the BIFF reader
/// applies to a sheet).
const MAX_HTML_CELLS: usize = 4_000_000;
/// Merges one table may need.
const MAX_HTML_MERGES: usize = 100_000;
/// Bytes of HTML one file may hold.
const MAX_HTML_BYTES: usize = 256 * 1024 * 1024;

/// Reads the first table of an HTML document as a workbook.
pub fn read_html(bytes: &[u8]) -> Result<(Workbook, ReadReport), IoError> {
    if bytes.len() > MAX_HTML_BYTES {
        return Err(IoError::TooLarge(format!("the HTML is {} bytes, over the limit of {} MiB", bytes.len(), MAX_HTML_BYTES >> 20)));
    }
    let text = crate::csv::decode_text(bytes);
    let mut wb = Workbook::new();
    let date_system = wb.date_system;
    let mut sheet = Sheet::new("Sheet1");
    let mut styles = std::mem::take(&mut wb.styles);
    let mut report = ReadReport::default();
    let mut r = Reader::from_str(&text);
    let mut table = 0i32;
    let mut row: u32 = 0;
    let mut col: u32 = 0;
    let mut had_tr = false;
    let mut cell = String::new();
    let mut in_cell = false;
    let mut cs: u32 = 1;
    let mut rs: u32 = 1;
    let mut cells: usize = 0;
    let mut merges: Vec<RangeRef> = Vec::new();
    // Positions a rowspan or colspan still occupies in later rows.
    let mut pending: HashSet<(u32, u32)> = HashSet::new();
    let mut fmt_cache: Vec<(&'static str, StyleId)> = Vec::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                let name = local(e.name().as_ref());
                if table == 0 {
                    if name == "table" {
                        table = 1;
                    }
                    continue;
                }
                match name.as_str() {
                    "table" => table += 1,
                    "tr" if table == 1 => {
                        if had_tr {
                            row += 1;
                        }
                        had_tr = true;
                        col = 0;
                    }
                    "td" | "th" if table == 1 => {
                        open_cell(e.attributes(), row, &mut col, &pending, &mut cell, &mut in_cell, &mut cs, &mut rs);
                    }
                    "br" if table == 1 && in_cell => cell.push('\n'),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                if table == 0 {
                    continue;
                }
                let name = local(e.name().as_ref());
                match name.as_str() {
                    "td" | "th" if table == 1 => {
                        open_cell(e.attributes(), row, &mut col, &pending, &mut cell, &mut in_cell, &mut cs, &mut rs);
                        flush_cell(
                            &mut sheet,
                            &mut styles,
                            &mut cells,
                            &mut merges,
                            &mut pending,
                            &mut fmt_cache,
                            row,
                            col,
                            cs,
                            rs,
                            &cell,
                            date_system,
                        )?;
                        col += cs;
                        in_cell = false;
                    }
                    "br" if table == 1 && in_cell => cell.push('\n'),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if table == 1
                    && in_cell
                    && let Ok(s) = t.decode()
                {
                    // A defined entity is decoded; an undefined one (&nbsp; in an XHTML-ish save)
                    // stays literal rather than losing the rest of the cell.
                    match quick_xml::escape::unescape(&s) {
                        Ok(u) => cell.push_str(&u),
                        Err(_) => cell.push_str(&s),
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if table == 1
                    && in_cell
                    && let Ok(s) = t.decode()
                {
                    cell.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let name = local(e.name().as_ref());
                if name == "table" {
                    table -= 1;
                    if table == 0 {
                        break;
                    }
                } else if (name == "td" || name == "th") && table == 1 && in_cell {
                    flush_cell(&mut sheet, &mut styles, &mut cells, &mut merges, &mut pending, &mut fmt_cache, row, col, cs, rs, &cell, date_system)?;
                    col += cs;
                    in_cell = false;
                }
            }
            Ok(Event::Eof) => break,
            // An entity reference: resolved when it is predefined or numeric, kept literal
            // otherwise (&nbsp; and friends), as a lenient read of an HTML table.
            Ok(Event::GeneralRef(e)) => {
                if table == 1
                    && in_cell
                    && let Ok(n) = e.decode()
                {
                    let c = if e.is_char_ref() {
                        let h = n.strip_prefix('#').unwrap_or(&n);
                        let code = if let Some(x) = h.strip_prefix('x') { u32::from_str_radix(x, 16) } else { h.parse::<u32>() }.ok();
                        code.and_then(char::from_u32)
                    } else {
                        match n.as_ref() {
                            "amp" => Some('&'),
                            "lt" => Some('<'),
                            "gt" => Some('>'),
                            "quot" => Some('"'),
                            "apos" => Some('\''),
                            "nbsp" => Some('\u{a0}'),
                            _ => None,
                        }
                    };
                    match c {
                        Some(c) => cell.push(c),
                        None => {
                            cell.push('&');
                            cell.push_str(&n);
                            cell.push(';');
                        }
                    }
                }
            }
            // Damaged markup ends the table; the values read so far stay.
            Err(_) => break,
            _ => {}
        }
    }
    if in_cell {
        flush_cell(&mut sheet, &mut styles, &mut cells, &mut merges, &mut pending, &mut fmt_cache, row, col, cs, rs, &cell, date_system)?;
    }
    sheet.merges = merges;
    wb.styles = styles;
    wb.sheets = vec![Arc::new(sheet)];
    if table == 0 {
        report.warn("the HTML document has no table; the sheet is empty");
    }
    Ok((wb, report))
}

/// Starts a cell: skips the positions a span from an earlier row still occupies, and reads its
/// colspan and rowspan.
#[allow(clippy::too_many_arguments)]
fn open_cell(
    attrs: quick_xml::events::attributes::Attributes,
    row: u32,
    col: &mut u32,
    pending: &HashSet<(u32, u32)>,
    cell: &mut String,
    in_cell: &mut bool,
    cs: &mut u32,
    rs: &mut u32,
) {
    while pending.contains(&(row, *col)) {
        *col += 1;
    }
    *in_cell = true;
    cell.clear();
    (*cs, *rs) = (1, 1);
    for a in attrs {
        let Ok(a) = a else { continue };
        let an = local(a.key.as_ref());
        let v = a.normalized_value(quick_xml::XmlVersion::Explicit1_0).ok().unwrap_or_default();
        if an == "colspan" {
            *cs = v.parse::<u32>().unwrap_or(1).clamp(1, MAX_COLS);
        } else if an == "rowspan" {
            *rs = v.parse::<u32>().unwrap_or(1).clamp(1, MAX_ROWS);
        }
    }
}

/// Writes one cell and records its span: the value lands on the span's top-left, the positions
/// it covers are occupied for later rows, and a span of more than one cell becomes a merge.
#[allow(clippy::too_many_arguments)]
fn flush_cell(
    sheet: &mut Sheet,
    styles: &mut StyleTable,
    cells: &mut usize,
    merges: &mut Vec<RangeRef>,
    pending: &mut HashSet<(u32, u32)>,
    fmt_cache: &mut Vec<(&'static str, StyleId)>,
    row: u32,
    col: u32,
    cs: u32,
    rs: u32,
    text: &str,
    date_system: DateSystem,
) -> Result<(), IoError> {
    if *cells >= MAX_HTML_CELLS {
        return Err(IoError::TooLarge(format!("the HTML table holds more than {MAX_HTML_CELLS} cells")));
    }
    let t = text.trim();
    if !t.is_empty() && row < MAX_ROWS && col < MAX_COLS {
        let p = parse_input(t, date_system);
        let style = match p.format {
            Some(code) => match fmt_cache.iter().find(|(c, _)| *c == code) {
                Some((_, id)) => *id,
                None => {
                    let id = styles.derive(StyleId::DEFAULT, |s| s.num_fmt = NumFmt::new(code));
                    fmt_cache.push((code, id));
                    id
                }
            },
            None => StyleId::DEFAULT,
        };
        sheet.cells.set(CellRef::new(row, col), Cell { value: p.value, formula: None, style, py: None });
    }
    *cells += 1;
    if cs > 1 || rs > 1 {
        let end_r = row.saturating_add(rs - 1).min(MAX_ROWS - 1);
        let end_c = col.saturating_add(cs - 1).min(MAX_COLS - 1);
        if merges.len() < MAX_HTML_MERGES {
            merges.push(RangeRef::new(CellRef::new(row, col), CellRef::new(end_r, end_c)));
        }
        for rr in row..=end_r {
            for cc in col..=end_c {
                if (rr, cc) != (row, col) {
                    pending.insert((rr, cc));
                    *cells += 1;
                }
            }
        }
    }
    Ok(())
}

/// A tag or attribute name, lower-cased (HTML tags are case-insensitive).
fn local(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridcraft_core::Value;

    fn one(html: &str) -> Workbook {
        read_html(html.as_bytes()).unwrap().0
    }

    #[test]
    fn the_first_table_becomes_the_sheet() {
        let wb = one(
            "<html><body><table><tr><th>Name</th><th>Qty</th></tr><tr><td>Apples</td><td>3</td></tr><tr><td>Pears</td><td>5</td></tr></table></body></html>",
        );
        let sh = wb.sheet(0).unwrap();
        assert_eq!(sh.value(CellRef::new(0, 0)), Value::text("Name"));
        assert_eq!(sh.value(CellRef::new(0, 1)), Value::text("Qty"));
        assert_eq!(sh.value(CellRef::new(1, 1)), Value::Number(3.0));
        assert_eq!(sh.value(CellRef::new(2, 1)), Value::Number(5.0));
        assert_eq!(sh.value(CellRef::new(3, 0)), Value::Empty);
    }

    #[test]
    fn titles_outside_the_table_are_dropped_and_spans_merge() {
        let wb = one(
            "<html><head><title>t</title></head><body><p>hi</p><table><tr><td colspan=\"2\">wide</td></tr><tr><td>a</td><td>b</td></tr></table></body></html>",
        );
        let sh = wb.sheet(0).unwrap();
        assert_eq!(sh.value(CellRef::new(0, 0)), Value::text("wide"));
        assert_eq!(sh.value(CellRef::new(0, 1)), Value::Empty);
        assert_eq!(sh.value(CellRef::new(1, 0)), Value::text("a"));
        assert_eq!(sh.value(CellRef::new(1, 1)), Value::text("b"));
        assert!(sh.merges.contains(&RangeRef::new(CellRef::new(0, 0), CellRef::new(0, 1))), "{sh:?}");
    }

    #[test]
    fn entities_line_breaks_and_numbers() {
        let wb = one("<table><tr><td>1 &amp; 2</td><td>4.50</td><td>a<br>b</td></tr></table>");
        let sh = wb.sheet(0).unwrap();
        assert_eq!(sh.value(CellRef::new(0, 0)), Value::text("1 & 2"));
        assert_eq!(sh.value(CellRef::new(0, 1)), Value::Number(4.5));
        assert_eq!(sh.value(CellRef::new(0, 2)), Value::text("a\nb"));
    }

    #[test]
    fn rowspan_skips_occupied_cells_and_a_missing_table_warns() {
        let wb = one("<table><tr><td rowspan=\"2\">x</td><td>1</td></tr><tr><td>2</td></tr></table>");
        let sh = wb.sheet(0).unwrap();
        assert_eq!(sh.value(CellRef::new(0, 0)), Value::text("x"));
        assert_eq!(sh.value(CellRef::new(0, 1)), Value::Number(1.0));
        // A rowspan covers its span with a merge; the covered cell itself stays empty.
        assert_eq!(sh.value(CellRef::new(1, 0)), Value::Empty);
        assert_eq!(sh.value(CellRef::new(1, 1)), Value::Number(2.0));
        assert!(sh.merges.contains(&RangeRef::new(CellRef::new(0, 0), CellRef::new(1, 0))), "{sh:?}");
        let (wb, report) = read_html("<html><body><p>no table</p></body></html>".as_bytes()).unwrap();
        assert_eq!(wb.sheet(0).unwrap().used_range(), None);
        assert!(report.warnings.iter().any(|w| w.contains("no table")), "{report:?}");
    }
}
