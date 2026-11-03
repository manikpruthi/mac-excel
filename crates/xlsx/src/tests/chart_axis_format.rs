//! A typed axis number format (`c:numFmt sourceLinked="0"`) is read, drawn and written back, as a
//! scatter chart's date X axis needs (#3170).

use std::io::Read;

use crate::{chart::read_chart, read_xlsx, write_xlsx, xml::parse};
use gridcraft_model::Workbook;

const SCATTER: &str = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea>
    <c:scatterChart><c:scatterStyle val="marker"/><c:ser><c:idx val="0"/><c:order val="0"/>
    <c:xVal><c:numLit><c:ptCount val="2"/><c:pt idx="0"><c:v>45200</c:v></c:pt><c:pt idx="1"><c:v>45400</c:v></c:pt></c:numLit></c:xVal>
    <c:yVal><c:numLit><c:ptCount val="2"/><c:pt idx="0"><c:v>2</c:v></c:pt><c:pt idx="1"><c:v>4</c:v></c:pt></c:numLit></c:yVal>
    </c:ser><c:axId val="1"/><c:axId val="2"/></c:scatterChart>
    <c:valAx><c:axId val="1"/><c:axPos val="b"/><c:numFmt formatCode="mmm-yy" sourceLinked="0"/></c:valAx>
    <c:valAx><c:axId val="2"/><c:axPos val="l"/><c:numFmt formatCode="0.0" sourceLinked="0"/></c:valAx>
    </c:plotArea></c:chart></c:chartSpace>"#;

#[test]
fn typed_axis_formats_are_read_from_a_scatter_chart() {
    let c = read_chart(&parse(SCATTER.as_bytes()).unwrap()).unwrap();
    assert_eq!(c.x_number_format.as_deref(), Some("mmm-yy"));
    assert_eq!(c.y_number_format.as_deref(), Some("0.0"));
}

#[test]
fn typed_axis_formats_survive_xlsx_save_and_reopen() {
    let mut c = read_chart(&parse(SCATTER.as_bytes()).unwrap()).unwrap();
    c.id = 1;
    let mut wb = Workbook::new();
    wb.sheet_mut(0).unwrap().charts.push(c);
    let bytes = write_xlsx(&wb).unwrap();

    let (back, _) = read_xlsx(&bytes).unwrap();
    assert_eq!(back.sheets[0].charts[0].x_number_format.as_deref(), Some("mmm-yy"));
    assert_eq!(back.sheets[0].charts[0].y_number_format.as_deref(), Some("0.0"));

    let mut xml = String::new();
    zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap().by_name("xl/charts/chart1.xml").unwrap().read_to_string(&mut xml).unwrap();
    assert!(xml.contains("<c:numFmt formatCode=\"mmm-yy\" sourceLinked=\"0\"/>"), "{xml}");
    assert!(xml.contains("<c:numFmt formatCode=\"0.0\" sourceLinked=\"0\"/>"), "{xml}");
}

#[test]
fn a_source_linked_general_axis_is_left_to_the_cells() {
    let xml = SCATTER.replace("formatCode=\"mmm-yy\" sourceLinked=\"0\"", "formatCode=\"General\" sourceLinked=\"1\"");
    let c = read_chart(&parse(xml.as_bytes()).unwrap()).unwrap();
    assert_eq!(c.x_number_format, None);
}
