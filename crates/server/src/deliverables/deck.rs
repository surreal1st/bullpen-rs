//! PPTX builder — port of deck section in deliverables.ts.

use super::spreadsheet::xml_escape;
use super::zip::{ZipEntry, build_zip};

pub const PPTX_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckSlide {
    pub heading: String,
    pub body: Vec<String>,
}

fn heading_line(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    if !rest.starts_with('#') {
        return None;
    }
    let hashes = rest.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = rest[hashes..].trim_start();
    Some(if after.is_empty() { "Untitled" } else { after })
}

pub fn parse_outline(text: &str) -> Vec<DeckSlide> {
    let mut slides: Vec<DeckSlide> = Vec::new();
    let mut current: Option<DeckSlide> = None;

    for raw in text.replace("\r\n", "\n").split('\n') {
        let line = raw.trim();
        if let Some(heading) = heading_line(line) {
            current = Some(DeckSlide {
                heading: heading.to_string(),
                body: vec![],
            });
            slides.push(current.as_ref().unwrap().clone());
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if current.is_none() {
            current = Some(DeckSlide {
                heading: line.to_string(),
                body: vec![],
            });
            slides.push(current.as_ref().unwrap().clone());
            continue;
        }
        let bullet = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line)
            .to_string();
        if let Some(ref mut s) = current {
            s.body.push(bullet);
            if let Some(last) = slides.last_mut() {
                *last = s.clone();
            }
        }
    }

    if slides.is_empty() && !text.trim().is_empty() {
        slides.push(DeckSlide {
            heading: text.trim().chars().take(80).collect(),
            body: vec![],
        });
    }
    slides
}

fn paragraphs_xml(lines: &[String]) -> String {
    if lines.is_empty() {
        return "<a:p><a:endParaRPr lang=\"en-US\"/></a:p>".into();
    }
    lines
        .iter()
        .map(|line| {
            format!(
                "<a:p><a:r><a:rPr lang=\"en-US\" dirty=\"0\"/><a:t>{}</a:t></a:r></a:p>",
                xml_escape(line)
            )
        })
        .collect()
}

fn slide_xml(slide: &DeckSlide) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
<p:sp>
<p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
<p:spPr><a:xfrm><a:off x="457200" y="274638"/><a:ext cx="8229600" cy="1143000"/></a:xfrm></p:spPr>
<p:txBody><a:bodyPr/><a:lstStyle/>{}</p:txBody>
</p:sp>
<p:sp>
<p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
<p:spPr><a:xfrm><a:off x="457200" y="1600200"/><a:ext cx="8229600" cy="4525963"/></a:xfrm></p:spPr>
<p:txBody><a:bodyPr/><a:lstStyle/>{}</p:txBody>
</p:sp>
</p:spTree></p:cSld>
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:sld>"#,
        paragraphs_xml(std::slice::from_ref(&slide.heading)),
        paragraphs_xml(&slide.body),
    )
}

const SLIDE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#;

const SLIDE_LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="title" preserve="1">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
</p:spTree></p:cSld>
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:sldLayout>"#;

const SLIDE_LAYOUT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#;

const SLIDE_MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
</p:spTree></p:cSld>
<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>"#;

const SLIDE_MASTER_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>"#;

const THEME: &str = include_str!("pptx_theme.xml");

pub fn build_pptx(slides: &[DeckSlide]) -> Vec<u8> {
    let slide_overrides: String = slides
        .iter()
        .enumerate()
        .map(|(i, _)| {
            format!(
                "<Override PartName=\"/ppt/slides/slide{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>",
                i + 1
            )
        })
        .collect();

    let content_types = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
{slide_overrides}
</Types>"#
    );

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#;

    let sld_id_lst: String = slides
        .iter()
        .enumerate()
        .map(|(i, _)| format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 256 + i, i + 2))
        .collect();

    let presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:sldIdLst>{sld_id_lst}</p:sldIdLst>
<p:sldSz cx="9144000" cy="6858000"/>
<p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#
    );

    let presentation_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
{}
</Relationships>"#,
        slides
            .iter()
            .enumerate()
            .map(|(i, _)| format!(
                r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{}.xml"/>"#,
                i + 2,
                i + 1
            ))
            .collect::<Vec<_>>()
            .join("")
    );

    let mut entries = vec![
        ZipEntry {
            name: "[Content_Types].xml".into(),
            data: content_types.into_bytes(),
        },
        ZipEntry {
            name: "_rels/.rels".into(),
            data: root_rels.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "ppt/presentation.xml".into(),
            data: presentation.into_bytes(),
        },
        ZipEntry {
            name: "ppt/_rels/presentation.xml.rels".into(),
            data: presentation_rels.into_bytes(),
        },
        ZipEntry {
            name: "ppt/slideMasters/slideMaster1.xml".into(),
            data: SLIDE_MASTER.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "ppt/slideMasters/_rels/slideMaster1.xml.rels".into(),
            data: SLIDE_MASTER_RELS.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "ppt/slideLayouts/slideLayout1.xml".into(),
            data: SLIDE_LAYOUT.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
            data: SLIDE_LAYOUT_RELS.as_bytes().to_vec(),
        },
        ZipEntry {
            name: "ppt/theme/theme1.xml".into(),
            data: THEME.as_bytes().to_vec(),
        },
    ];

    for (i, slide) in slides.iter().enumerate() {
        entries.push(ZipEntry {
            name: format!("ppt/slides/slide{}.xml", i + 1),
            data: slide_xml(slide).into_bytes(),
        });
        entries.push(ZipEntry {
            name: format!("ppt/slides/_rels/slide{}.xml.rels", i + 1),
            data: SLIDE_RELS.as_bytes().to_vec(),
        });
    }

    build_zip(&entries)
}
