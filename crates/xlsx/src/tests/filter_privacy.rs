//! "Remove personal information from file properties on save" (`workbookPr filterPrivacy`):
//! the setting, the author and company, and note authors survive an open + save (#3089).

use std::io::Read;
use std::sync::Arc;

use gridcraft_core::CellRef;
use gridcraft_model::{Comment, DocProps, Sheet, Workbook};

use crate::{read_xlsx, write_xlsx};

fn part(bytes: &[u8], name: &str) -> String {
    let mut s = String::new();
    zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap().by_name(name).unwrap().read_to_string(&mut s).unwrap();
    s
}

fn book(privacy: bool) -> Workbook {
    let mut sheet = Sheet::new("Data");
    sheet.comments.insert(
        CellRef::new(0, 0),
        Comment {
            author: "Ann".into(),
            text: "A thread".into(),
            replies: vec![("Bob".into(), "A reply".into())],
            threaded: true,
            resolved: false,
            visible: false,
            dates: vec![],
            note_box: None,
        },
    );
    sheet.comments.insert(
        CellRef::new(1, 1),
        Comment {
            author: "Ann".into(),
            text: "A plain note".into(),
            replies: vec![],
            threaded: false,
            resolved: false,
            visible: false,
            dates: vec![],
            note_box: None,
        },
    );
    Workbook {
        sheets: vec![Arc::new(sheet)],
        filter_privacy: privacy,
        props: DocProps { author: "Ann".into(), company: "Acme".into(), ..Default::default() },
        ..Workbook::default()
    }
}

#[test]
fn privacy_setting_is_saved_and_read_back() {
    for privacy in [true, false] {
        let bytes = write_xlsx(&book(privacy)).unwrap();
        let wb = part(&bytes, "xl/workbook.xml");
        assert_eq!(wb.contains("filterPrivacy=\"1\""), privacy, "{wb}");
        let (back, _) = read_xlsx(&bytes).unwrap();
        assert_eq!(back.filter_privacy, privacy);
    }
}

#[test]
fn privacy_omits_the_author_company_and_people() {
    let bytes = write_xlsx(&book(true)).unwrap();
    let core = part(&bytes, "docProps/core.xml");
    let app = part(&bytes, "docProps/app.xml");
    let comments = part(&bytes, "xl/comments1.xml");
    let threaded = part(&bytes, "xl/threadedComments/threadedComment1.xml");
    assert!(!core.contains("Ann"), "{core}");
    assert!(!core.contains("dc:creator"), "{core}");
    assert!(!app.contains("Acme"), "{app}");
    assert!(!app.contains("Company"), "{app}");
    assert!(!comments.contains("Ann"), "{comments}");
    assert!(comments.contains("<author>Author</author>"), "{comments}");
    assert!(!threaded.contains("Ann") && !threaded.contains("Bob"), "{threaded}");
    let people = part(&bytes, "xl/persons/person.xml");
    assert!(people.contains("displayName=\"Author\""), "{people}");
    assert!(!people.contains("Ann") && !people.contains("Bob"), "{people}");
}

#[test]
fn without_privacy_the_properties_and_authors_are_kept() {
    let bytes = write_xlsx(&book(false)).unwrap();
    let core = part(&bytes, "docProps/core.xml");
    let app = part(&bytes, "docProps/app.xml");
    let comments = part(&bytes, "xl/comments1.xml");
    assert!(core.contains("<dc:creator>Ann</dc:creator>"), "{core}");
    assert!(app.contains("<Company>Acme</Company>"), "{app}");
    assert!(comments.contains("<author>Ann</author>"), "{comments}");
}
