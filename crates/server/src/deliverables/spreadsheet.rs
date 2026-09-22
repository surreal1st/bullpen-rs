//! XLSX builder — port of spreadsheet section in deliverables.ts.

use super::zip::{ZipEntry, build_zip};

pub const XLSX_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

pub(crate) fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn col_letter(index: usize) -> String {
    let mut s = String::new();
    let mut x = index + 1;
    while x > 0 {
        let rem = (x - 1) % 26;
        s.insert(0, (b'A' + rem as u8) as char);
        x = (x - 1) / 26;
    }
    s
}

fn xlsx_cell(col_index: usize, value: &serde_json::Value) -> String {
    let col = col_letter(col_index);
    if let Some(n) = value.as_f64().filter(|n| n.is_finite()) {
        return format!("<c r=\"{col}\"><v>{n}</v></c>");
    }
    let owned = value.to_string();
    let text = value.as_str().unwrap_or(&owned);
    format!(
        "<c r=\"{col}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
        xml_escape(text)
    )
}

pub fn build_xlsx(rows: &[Vec<serde_json::Value>]) -> Vec<u8> {
    let rows_xml: String = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let cells: String = row
                .iter()
                .enumerate()
                .map(|(j, v)| xlsx_cell(j, v))
                .collect();
            format!("<row r=\"{}\">{cells}</row>", i + 1)
        })
        .collect();

    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

    let workbook = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

    let workbook_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;

    let sheet = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows_xml}</sheetData></worksheet>"#
    );

    build_zip(&[
        ZipEntry {
            name: "[Content_Types].xml".into(),
            data: content_types.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "_rels/.rels".into(),
            data: root_rels.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "xl/workbook.xml".into(),
            data: workbook.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "xl/_rels/workbook.xml.rels".into(),
            data: workbook_rels.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "xl/worksheets/sheet1.xml".into(),
            data: sheet.into_bytes(),
        },
    ])
}

pub fn coerce_table(content: &serde_json::Value) -> Option<Vec<Vec<serde_json::Value>>> {
    let value = if let Some(s) = content.as_str() {
        serde_json::from_str(s).ok()?
    } else {
        content.clone()
    };
    let arr = value.as_array()?;
    let mut rows = Vec::new();
    for row in arr {
        let row_arr = row.as_array()?;
        rows.push(
            row_arr
                .iter()
                .map(|cell| {
                    if cell.is_number() {
                        cell.clone()
                    } else {
                        serde_json::Value::String(cell.as_str().unwrap_or("").to_string())
                    }
                })
                .collect(),
        );
    }
    Some(rows)
}
