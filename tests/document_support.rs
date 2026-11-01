use code_context::document::{
    ZipArchive, build_zip_archive, decode_text_bytes, decode_xml_entities,
    extract_archive_metadata, inflate_raw_bounded,
};

#[test]
fn round_1_zip_deflate_entities_utf16_and_metadata_work() {
    let core_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <dc:title>Architecture &amp; Design &#x2014; v2</dc:title>
  <dc:creator>Moon Choi</dc:creator>
  <dc:subject>Code Context Document Indexing</dc:subject>
  <cp:keywords>docx, pptx, trigram, search</cp:keywords>
</cp:coreProperties>"#;
    let app_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Properties><Application>Microsoft Office Word</Application><Pages>12</Pages></Properties>"#;

    let archive_bytes = build_zip_archive(&[
        ("docProps/core.xml", core_xml.as_bytes(), true),
        ("docProps/app.xml", app_xml.as_bytes(), false),
    ]);

    let archive = ZipArchive::parse(&archive_bytes).unwrap();
    assert!(archive.total_uncompressed() > 100);
    let meta = extract_archive_metadata(&archive);
    assert_eq!(meta.title.as_deref(), Some("Architecture & Design — v2"));
    assert_eq!(meta.creator.as_deref(), Some("Moon Choi"));
    assert_eq!(
        meta.subject.as_deref(),
        Some("Code Context Document Indexing")
    );
    assert_eq!(
        meta.keywords.as_deref(),
        Some("docx, pptx, trigram, search")
    );
    assert_eq!(meta.application.as_deref(), Some("Microsoft Office Word"));
    assert_eq!(meta.pages_or_slides.as_deref(), Some("12 pages"));

    let header = meta.format_header();
    assert!(header.contains("# [Document Metadata]"));
    assert!(header.contains("Title: Architecture & Design — v2"));

    // Entity decoding
    assert_eq!(
        decode_xml_entities("A &lt; B &amp; C &#65; &#xac00; &quot;ok&quot;"),
        "A < B & C A 가 \"ok\""
    );

    // UTF-16LE & UTF-16BE decoding
    let mut utf16le = vec![0xFF, 0xFE];
    for unit in "한글 문서 테스트\r\n두번째 줄".encode_utf16() {
        utf16le.extend_from_slice(&unit.to_le_bytes());
    }
    assert_eq!(
        decode_text_bytes(&utf16le).unwrap(),
        "한글 문서 테스트\n두번째 줄"
    );

    let mut utf16be = vec![0xFE, 0xFF];
    for unit in "UTF16BE Document".encode_utf16() {
        utf16be.extend_from_slice(&unit.to_be_bytes());
    }
    assert_eq!(decode_text_bytes(&utf16be).unwrap(), "UTF16BE Document");

    // Path traversal rejection
    let bad_zip = build_zip_archive(&[("../secret.xml", b"nope", false)]);
    assert!(ZipArchive::parse(&bad_zip).is_err());

    // Stored block inflate test (BTYPE=00)
    let payload = b"stored block payload";
    let len = payload.len() as u16;
    let nlen = !len;
    let mut stored = vec![0x01]; // BFINAL=1, BTYPE=00
    stored.extend_from_slice(&len.to_le_bytes());
    stored.extend_from_slice(&nlen.to_le_bytes());
    stored.extend_from_slice(payload);
    assert_eq!(inflate_raw_bounded(&stored, 1024).unwrap(), payload);
}

#[test]
fn round_2_docx_headings_lists_tables_hyperlinks_comments_and_footnotes() {
    let document_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <w:body>
    <w:p>
      <w:pPr><w:pStyle w:val="Heading1"/></w:pPr>
      <w:bookmarkStart w:id="0" w:name="sec_intro"/>
      <w:r><w:t>1. 시스템 아키텍처 개요</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t>본 문서는 </w:t></w:r>
      <w:hyperlink r:id="rId5"><w:r><w:t>Code Context 공식 저장소</w:t></w:r></w:hyperlink>
      <w:r><w:t> 설계를 설명합니다. </w:t></w:r>
      <w:del><w:r><w:delText>구버전 설명</w:delText></w:r></w:del>
    </w:p>
    <w:p>
      <w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>
      <w:r><w:t>첫 번째 핵심 요구사항</w:t></w:r>
    </w:p>
    <w:p>
      <w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr>
      <w:r><w:t>하위 세부 항목</w:t></w:r>
    </w:p>
    <w:tbl>
      <w:tr>
        <w:tc><w:p><w:r><w:t>모듈</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>역할</w:t></w:r></w:p></w:tc>
      </w:tr>
      <w:tr>
        <w:tc><w:p><w:r><w:t>resident.rs</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>Trigram 인메모리 인덱스</w:t></w:r></w:p></w:tc>
      </w:tr>
    </w:tbl>
  </w:body>
</w:document>"#;

    let rels_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId5" Target="https://github.com/dotnetpower/code-context"/>
</Relationships>"#;

    let footnotes_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>
  <w:footnote w:id="1"><w:p><w:r><w:t>RFC 1951 DEFLATE 기반 압축 해제</w:t></w:r></w:p></w:footnote>
</w:footnotes>"#;

    let comments_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:comment w:id="10" w:author="ReviewerKim" w:date="2026-11-01T09:00:00Z">
    <w:p><w:r><w:t>표의 두 번째 행에 성능 수치를 추가해 주세요.</w:t></w:r></w:p>
  </w:comment>
</w:comments>"#;

    let header_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:p><w:r><w:t>CONFIDENTIAL DESIGN SPEC</w:t></w:r></w:p>
</w:hdr>"#;

    let docx_bytes = build_zip_archive(&[
        ("word/document.xml", document_xml.as_bytes(), true),
        ("word/_rels/document.xml.rels", rels_xml.as_bytes(), true),
        ("word/footnotes.xml", footnotes_xml.as_bytes(), true),
        ("word/comments.xml", comments_xml.as_bytes(), true),
        ("word/header1.xml", header_xml.as_bytes(), false),
    ]);

    let archive = ZipArchive::parse(&docx_bytes).unwrap();
    let extracted = code_context::document::extract_docx(&archive).unwrap();
    assert!(extracted.contains("## [Document Header]\nCONFIDENTIAL DESIGN SPEC"));
    assert!(extracted.contains("# {#sec_intro} 1. 시스템 아키텍처 개요"));
    assert!(extracted.contains("[Code Context 공식 저장소](https://github.com/dotnetpower/code-context)"));
    assert!(extracted.contains("~~구버전 설명~~"));
    assert!(extracted.contains("- 첫 번째 핵심 요구사항\n  - 하위 세부 항목"));
    assert!(extracted.contains("| 모듈 | 역할 |\n| --- | --- |\n| resident.rs | Trigram 인메모리 인덱스 |"));
    assert!(extracted.contains("## [Footnotes]\n[^1]: RFC 1951 DEFLATE 기반 압축 해제"));
    assert!(extracted.contains("## [Comments]\n- [Comment #10 by ReviewerKim (2026-11-01T09:00:00Z)]: 표의 두 번째 행에 성능 수치를 추가해 주세요."));
}

#[test]
fn round_3_pptx_slides_ordering_titles_bullets_tables_notes_comments_and_smartart() {
    let slide1_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
        <p:txBody><a:p><a:r><a:t>Q4 에이전트 플랫폼 로드맵</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:nvPr><p:ph type="subTitle"/></p:nvPr></p:nvSpPr>
        <p:txBody><a:p><a:r><a:t>Code Context &amp; Test Context 통합</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:sp>
        <p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr>
        <p:txBody>
          <a:p><a:pPr lvl="0"/><a:r><a:t>문서 인덱싱 지원</a:t></a:r></a:p>
          <a:p><a:pPr lvl="1"/><a:r><a:rPr><a:hlinkClick r:id="rId1"/></a:rPr><a:t>벤치마크 리포트</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

    let slide2_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
        <p:txBody><a:p><a:r><a:t>성능 지표 비교</a:t></a:r></a:p></p:txBody>
      </p:sp>
      <p:graphicFrame>
        <a:graphic><a:graphicData>
          <a:tbl>
            <a:tr>
              <a:tc><a:txBody><a:p><a:r><a:t>지표</a:t></a:r></a:p></a:txBody></a:tc>
              <a:tc><a:txBody><a:p><a:r><a:t>개선율</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
            <a:tr>
              <a:tc><a:txBody><a:p><a:r><a:t>토큰 절감</a:t></a:r></a:p></a:txBody></a:tc>
              <a:tc><a:txBody><a:p><a:r><a:t>78%</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
          </a:tbl>
        </a:graphicData></a:graphic>
      </p:graphicFrame>
    </p:spTree>
  </p:cSld>
</p:sld>"#;

    let slide10_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld><p:spTree><p:sp>
    <p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
    <p:txBody><a:p><a:r><a:t>부록: 10번째 슬라이드</a:t></a:r></a:p></p:txBody>
  </p:sp></p:spTree></p:cSld>
</p:sld>"#;

    let slide1_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships><Relationship Id="rId1" Target="https://example.com/bench"/></Relationships>"#;

    let notes1_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld><p:spTree><p:sp><p:txBody>
    <a:p><a:r><a:t>발표자 참고: 고객 데모 시 슬라이드 1에서 아키텍처를 강조할 것</a:t></a:r></a:p>
  </p:txBody></p:sp></p:spTree></p:cSld>
</p:notes>"#;

    let comment1_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:cmLst xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cm authorId="0"><p:text>일정표를 최신화하세요</p:text></p:cm>
</p:cmLst>"#;

    let diagram1_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <dgm:ptLst><dgm:pt><dgm:t><a:p><a:r><a:t> 수집 -> 인덱싱 -> 스마트 컨텍스트 </a:t></a:r></a:p></dgm:t></dgm:pt></dgm:ptLst>
</dgm:dataModel>"#;

    let pptx_bytes = build_zip_archive(&[
        ("ppt/slides/slide10.xml", slide10_xml.as_bytes(), true),
        ("ppt/slides/slide2.xml", slide2_xml.as_bytes(), true),
        ("ppt/slides/slide1.xml", slide1_xml.as_bytes(), true),
        ("ppt/slides/_rels/slide1.xml.rels", slide1_rels.as_bytes(), false),
        ("ppt/notesSlides/notesSlide1.xml", notes1_xml.as_bytes(), true),
        ("ppt/comments/comment1.xml", comment1_xml.as_bytes(), false),
        ("ppt/diagrams/data1.xml", diagram1_xml.as_bytes(), true),
    ]);

    let archive = ZipArchive::parse(&pptx_bytes).unwrap();
    let extracted = code_context::document::extract_pptx(&archive).unwrap();

    // Ensure numerical ordering: Slide 1 -> Slide 2 -> Slide 10
    let pos1 = extracted.find("## [Slide 1] Q4 에이전트 플랫폼 로드맵").unwrap();
    let pos2 = extracted.find("## [Slide 2] 성능 지표 비교").unwrap();
    let pos10 = extracted.find("## [Slide 10] 부록: 10번째 슬라이드").unwrap();
    assert!(pos1 < pos2 && pos2 < pos10);

    assert!(extracted.contains("### Code Context & Test Context 통합"));
    assert!(extracted.contains("- 문서 인덱싱 지원\n  - [벤치마크 리포트](https://example.com/bench)"));
    assert!(extracted.contains("### [Slide 1 Notes]\n발표자 참고: 고객 데모 시 슬라이드 1에서 아키텍처를 강조할 것"));
    assert!(extracted.contains("### [Slide 1 Comments]\n- 일정표를 최신화하세요"));
    assert!(extracted.contains("| 지표 | 개선율 |\n| --- | --- |\n| 토큰 절감 | 78% |"));
    assert!(extracted.contains("## [SmartArt Diagrams]\n- 수집 -> 인덱싱 -> 스마트 컨텍스트"));
}

#[test]
fn round_4_xlsx_shared_strings_sheets_formulas_comments_and_csv_parsing() {
    let shared_strings_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="3" uniqueCount="3">
  <si><t>서비스명</t></si>
  <si><t>월간 비용(USD)</t></si>
  <si><r><t>AKS </t></r><r><t>클러스터</t></r></si>
</sst>"#;

    let workbook_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="인프라 예산" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#;

    let workbook_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;

    let sheet1_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="s"><v>1</v></c>
      <c r="C1" t="inlineStr"><is><t>활성 여부</t></is></c>
    </row>
    <row r="2">
      <c r="A2" t="s"><v>2</v></c>
      <c r="B2"><f>SUM(1200,300)</f><v>1500</v></c>
      <c r="C2" t="b"><v>1</v></c>
    </row>
  </sheetData>
</worksheet>"#;

    let comments1_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <commentList>
    <comment ref="B2" authorId="0"><text><r><t>예비비 300달러 포함</t></r></text></comment>
  </commentList>
</comments>"#;

    let xlsx_bytes = build_zip_archive(&[
        ("xl/sharedStrings.xml", shared_strings_xml.as_bytes(), true),
        ("xl/workbook.xml", workbook_xml.as_bytes(), true),
        ("xl/_rels/workbook.xml.rels", workbook_rels.as_bytes(), false),
        ("xl/worksheets/sheet1.xml", sheet1_xml.as_bytes(), true),
        ("xl/comments1.xml", comments1_xml.as_bytes(), true),
    ]);

    let archive = ZipArchive::parse(&xlsx_bytes).unwrap();
    let extracted = code_context::document::extract_xlsx(&archive).unwrap();
    assert!(extracted.contains("## [Sheet: 인프라 예산]"));
    assert!(extracted.contains("| 서비스명 | 월간 비용(USD) | 활성 여부 |"));
    assert!(extracted.contains("| AKS 클러스터 | 1500 (=SUM(1200,300)) | TRUE |"));
    assert!(extracted.contains("## [Sheet Comments]\n- [B2]: 예비비 300달러 포함"));

    // RFC 4180 CSV with quoted multiline field and escaped quotes
    let csv_raw = "이름,설명,상태\r\n\"보리 \"\"AI\"\" 엔진\",\"줄바꿈 포함\n설명 텍스트\",정상\r\n";
    let csv_rows = code_context::document::parse_delimited_records(csv_raw, ',');
    assert_eq!(csv_rows.len(), 2);
    assert_eq!(csv_rows[1][0], "보리 \"AI\" 엔진");
    assert_eq!(csv_rows[1][1], "줄바꿈 포함 설명 텍스트");
}

#[test]
fn round_5_odf_odt_odp_ods_epub_and_svg_extraction() {
    // 1. ODT test
    let odt_content = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:xlink="http://www.w3.org/1999/xlink">
  <office:body><office:text>
    <text:h text:outline-level="2">오픈도큐먼트 설계 섹션</text:h>
    <text:p>ODF 본문<text:s text:c="2"/>공백 및 <text:a xlink:href="https://oasis-open.org">OASIS 표준</text:a></text:p>
    <table:table table:name="요약표">
      <table:table-row>
        <table:table-cell><text:p>항목</text:p></table:table-cell>
        <table:table-cell><text:p>값</text:p></table:table-cell>
      </table:table-row>
      <table:table-row>
        <table:table-cell><text:p>포맷</text:p></table:table-cell>
        <table:table-cell><text:p>ODT</text:p></table:table-cell>
        <table:table-cell table:number-columns-repeated="1024"/>
      </table:table-row>
    </table:table>
  </office:text></office:body>
</office:document-content>"#;
    let odt_zip = build_zip_archive(&[("content.xml", odt_content.as_bytes(), true)]);
    let odt_text =
        code_context::document::extract_odf(&ZipArchive::parse(&odt_zip).unwrap(), "odt").unwrap();
    assert!(odt_text.contains("## 오픈도큐먼트 설계 섹션"));
    assert!(odt_text.contains("[OASIS 표준](https://oasis-open.org)"));
    assert!(odt_text.contains("| 포맷 | ODT |"));

    // 2. ODP test
    let odp_content = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0">
  <office:body><office:presentation>
    <draw:page draw:name="아키텍처 개요">
      <draw:frame><draw:text-box><text:p>인메모리 Trigram 검색</text:p></draw:text-box></draw:frame>
      <presentation:notes><draw:frame><draw:text-box><text:p>ODP 발표자 노트 텍스트</text:p></draw:text-box></draw:frame></presentation:notes>
    </draw:page>
  </office:presentation></office:body>
</office:document-content>"#;
    let odp_zip = build_zip_archive(&[("content.xml", odp_content.as_bytes(), true)]);
    let odp_text =
        code_context::document::extract_odf(&ZipArchive::parse(&odp_zip).unwrap(), "odp").unwrap();
    assert!(odp_text.contains("## [Slide 1] 아키텍처 개요"));
    assert!(odp_text.contains("인메모리 Trigram 검색"));
    assert!(odp_text.contains("### [Slide 1 Notes]\nODP 발표자 노트 텍스트"));

    // 3. EPUB test
    let container_xml = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
    let opf_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <metadata><dc:title>핸드북 가이드</dc:title><dc:creator>홍길동</dc:creator></metadata>
  <manifest><item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;
    let ch1_xhtml = r#"<?xml version="1.0" encoding="UTF-8"?>
<html><head><title>제1장 시작하기</title></head><body><h1>제1장 시작하기</h1><p>EPUB 본문 문장입니다.</p></body></html>"#;
    let epub_zip = build_zip_archive(&[
        ("META-INF/container.xml", container_xml.as_bytes(), false),
        ("OEBPS/content.opf", opf_xml.as_bytes(), true),
        ("OEBPS/ch1.xhtml", ch1_xhtml.as_bytes(), true),
    ]);
    let epub_text =
        code_context::document::extract_epub(&ZipArchive::parse(&epub_zip).unwrap()).unwrap();
    assert!(epub_text.contains("Title: 핸드북 가이드"));
    assert!(epub_text.contains("## [Chapter 1] 제1장 시작하기"));
    assert!(epub_text.contains("EPUB 본문 문장입니다."));

    // 4. SVG diagram text extraction
    let svg = r#"<svg><title>시스템 흐름도</title><style>.cls{fill:red;}</style><text><tspan>클라이언트</tspan> <tspan>데몬 IPC</tspan></text></svg>"#;
    let svg_text = code_context::document::extract_svg_or_xhtml_text(svg);
    assert!(svg_text.contains("시스템 흐름도"));
    assert!(svg_text.contains("클라이언트 데몬 IPC"));
    assert!(!svg_text.contains("fill:red"));
}

#[test]
fn round_6_pdf_rtf_ipynb_and_xliff_extraction() {
    // 1. PDF with FlateDecode stream + literal & UTF-16BE strings
    let stream_content = b"BT /F1 12 Tf (Code Context PDF Engine) Tj T* [(Fast ) -200 (Trigram Search)] TJ ET";
    let compressed_stream = code_context::document::zlib_compress_fixed(stream_content);
    let mut pdf_bytes = Vec::new();
    pdf_bytes.extend_from_slice(b"%PDF-1.7\n1 0 obj << /Title (PDF Specification Guide) /Author (Moon Choi) >> endobj\n");
    pdf_bytes.extend_from_slice(b"2 0 obj << /Filter /FlateDecode /Length ");
    pdf_bytes.extend_from_slice(compressed_stream.len().to_string().as_bytes());
    pdf_bytes.extend_from_slice(b" >>\nstream\r\n");
    pdf_bytes.extend_from_slice(&compressed_stream);
    pdf_bytes.extend_from_slice(b"\r\nendstream\nendobj\n%%EOF\n");

    let pdf_text = code_context::document::extract_pdf(&pdf_bytes).unwrap();
    assert!(pdf_text.contains("Title: PDF Specification Guide"));
    assert!(pdf_text.contains("Author: Moon Choi"));
    assert!(pdf_text.contains("## [Page 1]"));
    assert!(pdf_text.contains("Code Context PDF Engine"));
    assert!(pdf_text.contains("Fast Trigram Search"));

    // 2. RTF with fonttbl skip and Korean Unicode escapes (\u54620?\u44544? = 한글)
    let rtf_raw = b"{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Arial;}}{\\colortbl;\\red0\\green0\\blue0;}\\pard RTF \\u54620?\\u44544? Document\\par Second Line\\tab Tabbed\\par}";
    let rtf_text = code_context::document::extract_rtf(rtf_raw).unwrap();
    assert!(rtf_text.contains("RTF 한글 Document"));
    assert!(rtf_text.contains("Second Line\tTabbed"));
    assert!(!rtf_text.contains("Arial"));

    // 3. Jupyter Notebook (.ipynb)
    let ipynb_raw = r##"{
      "metadata": {"kernelspec": {"language": "python"}},
      "cells": [
        {"cell_type": "markdown", "source": ["# 실험 노트북\n", "토큰 절감 측정 결과"]},
        {
          "cell_type": "code",
          "source": ["def compute_savings():\n", "    return 0.78\n"],
          "outputs": [{"output_type": "stream", "text": ["savings = 78%\n"]}]
        }
      ]
    }"##
    .as_bytes();
    let ipynb_text = code_context::document::extract_ipynb(ipynb_raw).unwrap();
    assert!(ipynb_text.contains("## [Notebook Cell 1: markdown]\n# 실험 노트북"));
    assert!(ipynb_text.contains("## [Notebook Cell 2: code (python)]\ndef compute_savings():"));
    assert!(ipynb_text.contains("### [Cell Output]\nsavings = 78%"));

    // 4. XLIFF localization document
    let xliff_raw = r#"<xliff version="1.2"><file><body>
      <trans-unit id="welcome_msg">
        <source>Welcome to Code Context</source>
        <target>코드 컨텍스트에 오신 것을 환영합니다</target>
      </trans-unit>
    </body></file></xliff>"#;
    let xliff_text = code_context::document::extract_xliff_or_tmx(xliff_raw);
    assert!(xliff_text.contains("### [Translation Unit: welcome_msg]"));
    assert!(xliff_text.contains("Target: 코드 컨텍스트에 오신 것을 환영합니다"));
}

#[test]
fn round_7_document_definition_ranking_and_boilerplate_detection() {
    use code_context::document::{
        is_document_boilerplate_line, is_document_definition_line,
        matches_extension_case_insensitive,
    };

    for def_line in [
        "# 아키텍처 개요",
        "## [Slide 3] 성능 벤치마크",
        "## [Sheet: 예산안]",
        "## [Page 2]",
        "## [Notebook Cell 1: markdown]",
        "\\section{시스템 모델}",
        "\\newcommand{\\cctx}{Code Context}",
        "@article{choi2026cctx,",
        "= Typst 최상위 제목",
        "#let theme = \"dark\"",
        "* Org Mode Headline",
        "#+TITLE: 설계 문서",
        "[[sec-architecture]]",
        ".. _ overview-anchor:",
        ".. function:: extract_docx(archive)",
        "Title: 엔터프라이즈 아키텍처",
    ] {
        assert!(
            is_document_definition_line(def_line),
            "expected definition line: {def_line}"
        );
    }

    for non_def in [
        "일반 본문 텍스트 줄입니다.",
        "| 데이터1 | 데이터2 |",
        "use std::collections::BTreeMap;",
    ] {
        assert!(
            !is_document_definition_line(non_def),
            "expected non-definition: {non_def}"
        );
    }

    for boilerplate in [
        "| --- | --- |",
        "---",
        "\\usepackage{amsmath}",
        "#import \"@preview/polylux:0.3.1\": *",
        "include::chapter1.adoc[]",
    ] {
        assert!(
            is_document_boilerplate_line(boilerplate),
            "expected boilerplate: {boilerplate}"
        );
    }

    assert!(matches_extension_case_insensitive("report.DOCX", ".docx"));
    assert!(matches_extension_case_insensitive("slides.Pptx", ".pptx"));
}

#[test]
fn round_8_smart_document_context_clamping_slides_headings_tables_and_latex() {
    use code_context::document::clamp_document_snippet;

    // 1. Slide clamping in PPTX extracted lines
    let pptx_lines: Vec<String> = vec![
        "## [Slide 1] 개요",
        "- 소개 항목",
        "",
        "## [Slide 2] 핵심 아키텍처",
        "- Trigram 인덱스",
        "- 스마트 클램핑",
        "",
        "## [Slide 3] 결론",
        "- 요약",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        clamp_document_snippet("deck.pptx", &pptx_lines, 4, 4),
        Some((3, 5))
    );

    // 2. Markdown table header preservation
    let md_table_lines: Vec<String> = vec![
        "# 1. 개요",
        "설명 문단",
        "## 1.1 성능 표",
        "| 포맷 | 지원 여부 |",
        "| --- | --- |",
        "| DOCX | 지원 |",
        "| PPTX | 지원 |",
        "다음 문단",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        clamp_document_snippet("README.md", &md_table_lines, 6, 6),
        Some((2, 6))
    );

    // 3. Fenced code block integrity
    let fenced_lines: Vec<String> = vec![
        "## 다이어그램",
        "```mermaid",
        "graph TD",
        "  A --> B",
        "```",
        "## 다음 섹션",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        clamp_document_snippet("arch.md", &fenced_lines, 3, 3),
        Some((0, 4))
    );

    // 4. LaTeX environment clamping
    let tex_lines: Vec<String> = vec![
        "\\section{Introduction}",
        "\\begin{theorem}",
        "Let $T$ be a trigram index.",
        "\\end{theorem}",
        "After theorem.",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        clamp_document_snippet("paper.tex", &tex_lines, 2, 2),
        Some((1, 3))
    );
}

#[test]
fn round_9_workspace_resident_watched_live_parity_and_exclusions_for_documents() {
    use code_context::catalog::Spec;
    use code_context::engine::{Backend, Engine};
    use code_context::policy::MemoryPolicy;
    use code_context::search::Search;
    use code_context::watched::Watched;
    use code_context::workspace::Workspace;
    use serde_json::json;

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("docs")).unwrap();
    std::fs::create_dir_all(root.path().join(".ipynb_checkpoints")).unwrap();

    // Create a real DOCX file
    let doc_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>엔터프라이즈 보안 정책</w:t></w:r></w:p>
    <w:p><w:r><w:t>제로 트러스트 네트워크 아키텍처 필수 적용</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
    let docx_bytes = build_zip_archive(&[("word/document.xml", doc_xml.as_bytes(), true)]);
    std::fs::write(root.path().join("docs/security_policy.docx"), &docx_bytes).unwrap();

    // Create a real PPTX file with 2 slides
    let s1 = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld><p:spTree>
    <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>1분기 아키텍처 리뷰</a:t></a:r></a:p></p:txBody></p:sp>
    <p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>레거시 마이그레이션 일정</a:t></a:r></a:p></p:txBody></p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;
    let s2 = r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld><p:spTree>
    <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>2분기 확장성 계획</a:t></a:r></a:p></p:txBody></p:sp>
    <p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>멀티 리전 페일오버 자동화</a:t></a:r></a:p></p:txBody></p:sp>
  </p:spTree></p:cSld>
</p:sld>"#;
    let pptx_bytes = build_zip_archive(&[
        ("ppt/slides/slide1.xml", s1.as_bytes(), true),
        ("ppt/slides/slide2.xml", s2.as_bytes(), true),
    ]);
    std::fs::write(root.path().join("docs/roadmap.pptx"), &pptx_bytes).unwrap();

    // Office lock file & LaTeX build artifacts (must be excluded by default)
    std::fs::write(root.path().join("docs/~$security_policy.docx"), b"lock").unwrap();
    std::fs::write(root.path().join("docs/paper.aux"), b"aux").unwrap();
    std::fs::write(
        root.path().join(".ipynb_checkpoints/checkpoint.ipynb"),
        b"{}",
    )
    .unwrap();

    let globs = vec![
        "*.docx".to_string(),
        "*.pptx".to_string(),
        "*.aux".to_string(),
        "*.ipynb".to_string(),
    ];
    let workspace = Workspace::new(root.path(), vec![".".into()], globs.clone()).unwrap();

    // Verify Live, Ram, and Index backends all return identical matches on DOCX and PPTX
    for backend in [Backend::Live, Backend::Ram, Backend::Index] {
        let engine = Engine::new(workspace.clone(), backend).unwrap();
        let res = engine
            .search(&Search {
                pattern: "제로 트러스트".into(),
                regex: false,
                ignore_case: false,
                limit: 20,
            })
            .unwrap();
        assert_eq!(res.total_matches, 1);
        assert_eq!(res.matches[0].path, "docs/security_policy.docx");
        assert_eq!(res.matches[0].line, 2);
    }

    // Verify Watched code_files, code_grep, code_context, code_read, and live update recovery
    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec![".".into()],
        globs,
        excludes: vec![],
        default_excludes: true,
        prewarm: false,
        policy: MemoryPolicy::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    let files = watched.call("code_files", json!({"glob": "*"})).unwrap();
    assert_eq!(
        files["paths"],
        json!(["docs/roadmap.pptx", "docs/security_policy.docx"])
    );

    // Context search on Slide 2 should clamp to Slide 2 only (not Slide 1)
    let ctx = watched
        .call(
            "code_context",
            json!({"pattern": "멀티 리전 페일오버", "radius": 10}),
        )
        .unwrap();
    assert_eq!(ctx["search"]["total_matches"], 1);
    let snippet_lines = ctx["snippets"][0]["lines"].as_array().unwrap();
    assert_eq!(snippet_lines[0], "## [Slide 2] 2분기 확장성 계획");
    assert_eq!(snippet_lines[1], "- 멀티 리전 페일오버 자동화");

    // Modify DOCX on disk and verify transparent code_read recovery
    let updated_doc_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>엔터프라이즈 보안 정책 v2</w:t></w:r></w:p>
    <w:p><w:r><w:t>양자 내성 암호화 적용</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
    let updated_docx = build_zip_archive(&[("word/document.xml", updated_doc_xml.as_bytes(), true)]);
    std::fs::write(root.path().join("docs/security_policy.docx"), &updated_docx).unwrap();

    let read_res = watched
        .call(
            "code_read",
            json!({"ranges": [{"path": "docs/security_policy.docx", "start": 1, "end": 2}]}),
        )
        .unwrap();
    assert_eq!(
        read_res[0]["lines"],
        json!(["# 엔터프라이즈 보안 정책 v2", "양자 내성 암호화 적용"])
    );
}

#[test]
fn round_10_default_globs_structure_inspector_and_token_benchmark() {
    use code_context::catalog::Spec;
    use code_context::document::inspect_structure;
    use code_context::policy::MemoryPolicy;
    use code_context::registration::{DEFAULT_SOURCE_GLOBS, TEXT_GLOBS};
    use code_context::watched::Watched;
    use serde_json::json;

    for expected_glob in [
        "*.docx", "*.pptx", "*.xlsx", "*.odt", "*.odp", "*.ods", "*.epub", "*.pdf", "*.rtf",
        "*.ipynb", "*.csv", "*.tsv", "*.tex", "*.bib", "*.typ", "*.adoc", "*.org", "*.vtt",
    ] {
        assert!(
            DEFAULT_SOURCE_GLOBS.contains(&expected_glob),
            "DEFAULT_SOURCE_GLOBS missing {expected_glob}"
        );
        assert!(
            TEXT_GLOBS.contains(&expected_glob),
            "TEXT_GLOBS missing {expected_glob}"
        );
    }

    let root = tempfile::tempdir().unwrap();
    // Build a 10-slide PPTX deck and a multi-section DOCX to measure clamping token savings
    let mut slide_entries_buf: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 1..=10 {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld><p:spTree>
    <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>슬라이드 {i}: 아키텍처 모듈 {i}</a:t></a:r></a:p></p:txBody></p:sp>
    <p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody>
      <a:p><a:pPr lvl="0"/><a:r><a:t>핵심 키워드 slide_target_{i} 설명 라인 A</a:t></a:r></a:p>
      <a:p><a:pPr lvl="1"/><a:r><a:t>세부 구현 지표 및 레이턴시 0.{i}ms 달성</a:t></a:r></a:p>
      <a:p><a:pPr lvl="1"/><a:r><a:t>메모리 예산 가드 및 증분 해시 검증 완료</a:t></a:r></a:p>
    </p:txBody></p:sp>
  </p:spTree></p:cSld>
</p:sld>"#
        );
        slide_entries_buf.push((format!("ppt/slides/slide{i}.xml"), xml.into_bytes()));
    }
    let borrowed: Vec<(&str, &[u8], bool)> = slide_entries_buf
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice(), true))
        .collect();
    let pptx_bytes = build_zip_archive(&borrowed);
    std::fs::write(root.path().join("architecture_deck.pptx"), &pptx_bytes).unwrap();

    let extracted_pptx = code_context::document::extract_pptx(
        &ZipArchive::parse(&pptx_bytes).unwrap(),
    )
    .unwrap();
    let outline = inspect_structure("architecture_deck.pptx", &extracted_pptx);
    assert_eq!(outline.len(), 10);
    assert!(outline[0].contains("## [Slide 1] 슬라이드 1: 아키텍처 모듈 1"));
    assert!(outline[9].contains("## [Slide 10] 슬라이드 10: 아키텍처 모듈 10"));

    let mut watched = Watched::new(Spec {
        root: root.path().to_path_buf(),
        scopes: vec![".".into()],
        globs: DEFAULT_SOURCE_GLOBS.iter().map(|s| (*s).to_string()).collect(),
        excludes: vec![],
        default_excludes: true,
        prewarm: false,
        policy: MemoryPolicy::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    let ctx = watched
        .call(
            "code_context",
            json!({"pattern": "slide_target_5", "radius": 15}),
        )
        .unwrap();
    let snippet_lines = ctx["snippets"][0]["lines"].as_array().unwrap();
    let clamped_chars: usize = snippet_lines
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::len)
        .sum();
    let full_chars = extracted_pptx.len();
    // Clamped single slide should use < 20% of full 10-slide extracted text
    assert!(
        clamped_chars * 5 < full_chars,
        "expected >=80% token reduction: clamped={clamped_chars} vs full={full_chars}"
    );
}

#[test]
fn round_11_markdown_frontmatter_fences_setext_alerts_admonitions_tables_and_slug_anchors() {
    use code_context::catalog::Spec;
    use code_context::document::{
        clamp_document_snippet, document_definition_priority, github_heading_slug,
        inspect_structure, is_agent_instruction_markdown, is_document_boilerplate_line,
        is_markdown_path, resolve_markdown_anchor_range,
    };
    use code_context::policy::MemoryPolicy;
    use code_context::registration::{DEFAULT_SOURCE_GLOBS, TEXT_GLOBS};
    use code_context::watched::Watched;
    use serde_json::json;

    // 1. Markdown extension & special filename detection
    for md_path in [
        "README.md",
        "docs/guide.mdx",
        "notes.markdown",
        "spec.mdown",
        "doc.mkd",
        "analysis.qmd",
        "report.rmd",
        "README",
        "CHANGELOG",
        "LICENSE",
        "CONTRIBUTING",
        "SECURITY",
    ] {
        assert!(is_markdown_path(md_path), "{md_path}");
    }
    for agent_doc in [
        "AGENTS.md",
        "CLAUDE.md",
        "SKILL.md",
        ".github/copilot-instructions.md",
        "vscode-extension/prompts/code-context.instructions.md",
        "prompts/review.prompt.md",
        "agents/security.agent.md",
    ] {
        assert!(is_agent_instruction_markdown(agent_doc), "{agent_doc}");
    }
    assert!(DEFAULT_SOURCE_GLOBS.contains(&"*.markdown"));
    assert!(DEFAULT_SOURCE_GLOBS.contains(&"*.qmd"));
    assert!(TEXT_GLOBS.contains(&"*.rmd"));
    assert!(TEXT_GLOBS.contains(&"*.mdown"));
    assert!(TEXT_GLOBS.contains(&"*.mkd"));

    // 2. Tiered Markdown definition priorities & boilerplate classification
    assert_eq!(document_definition_priority("# Top Title"), 4);
    assert_eq!(document_definition_priority("   ## Indented H2 ##"), 4);
    assert_eq!(document_definition_priority("applyTo: \"**\""), 4);
    assert_eq!(document_definition_priority("### Subheading"), 3);
    assert_eq!(document_definition_priority("> [!IMPORTANT]"), 3);
    assert_eq!(document_definition_priority(":::warning"), 3);
    assert_eq!(document_definition_priority("[^arch]: Architecture footnote"), 3);
    assert_eq!(document_definition_priority("<summary>Expand details</summary>"), 3);
    assert_eq!(
        document_definition_priority("[repo-link]: https://github.com/dotnetpower/code-context"),
        2
    );
    assert_eq!(
        document_definition_priority("- **CACHE_MISS**: Nonblocking root warmup signal"),
        2
    );
    assert_eq!(document_definition_priority("    # 4-space indented code block"), 0);

    for boilerplate in [
        "| --- | :---: | ---: |",
        "--- | ---",
        "---",
        "====",
        "```",
        ":::",
        "<!-- markdownlint-disable MD013 -->",
        "[![CI](https://img.shields.io/badge/build-passing-brightgreen)](https://example.com)",
        "- [Installation](#installation)",
        "<hr/>",
    ] {
        assert!(is_document_boilerplate_line(boilerplate), "{boilerplate}");
    }

    // 3. Complex Markdown document with YAML frontmatter (...), fenced code with # comments,
    //    Setext heading, GFM Alert, ::: admonition, <details>, pipeless GFM table, and $$ math
    let complex_md = concat!(
        "---\n",
        "name: cctx-guide\n",
        "description: \"Comprehensive guide\"\n",
        "applyTo: \"**\"\n",
        "...\n",
        "\n",
        "# Code Context Guide\n",
        "\n",
        "## Installation and Setup {#install-setup}\n",
        "Run the installer script:\n",
        "```bash\n",
        "# This is a shell comment inside a code fence, NOT a heading!\n",
        "bash scripts/install.sh\n",
        "```\n",
        "Verify with `cctx ping`.\n",
        "\n",
        "Architecture Overview\n",
        "---------------------\n",
        "Setext H2 section body.\n",
        "\n",
        "> [!WARNING]\n",
        "> Never poll on CACHE_MISS.\n",
        "> Continue with permitted host tools.\n",
        "\n",
        ":::tip\n",
        "Keep read batches under 150 lines.\n",
        ":::\n",
        "\n",
        "<details>\n",
        "<summary>Advanced Memory Formula</summary>\n",
        "$$\n",
        "B_{target} = \\min(B_{max}, M_{avail} - R)\n",
        "$$\n",
        "</details>\n",
        "\n",
        "## Performance Table\n",
        "\n",
        "Metric | Baseline | Native\n",
        "--- | ---: | ---:\n",
        "Latency | 3.899 ms | 0.260 ms\n",
        "Speedup | 1.00x | 15.01x\n"
    );

    // 4. Fence-aware structure inspection & slug generation
    assert_eq!(github_heading_slug("## Installation and Setup {#install-setup}"), "install-setup");
    assert_eq!(github_heading_slug("## Windows Agent Sessions"), "windows-agent-sessions");
    let structure = inspect_structure("docs/guide.md", complex_md);
    assert!(
        !structure.iter().any(|s| s.contains("shell comment")),
        "code fence # comment must not appear in structure: {structure:?}"
    );
    assert!(structure.iter().any(|s| s.contains("[frontmatter] name: cctx-guide")));
    assert!(structure.iter().any(|s| s.contains("(#install-setup)")));
    assert!(structure.iter().any(|s| s.contains("Architecture Overview (#architecture-overview)")));

    // 5. Anchor range resolution
    let (inst_start, inst_end) =
        resolve_markdown_anchor_range(complex_md, "install-setup").unwrap();
    let md_lines: Vec<String> = complex_md.lines().map(str::to_owned).collect();
    assert_eq!(md_lines[inst_start - 1], "## Installation and Setup {#install-setup}");
    assert_eq!(md_lines[inst_end - 1], "Verify with `cctx ping`.");

    // 6. Clamping tests: frontmatter, fence comment, GFM alert, ::: admonition, <details>, pipeless table
    let fm_clamp = clamp_document_snippet("docs/guide.md", &md_lines, 2, 2).unwrap();
    assert_eq!(fm_clamp.0, 0);
    assert_eq!(md_lines[fm_clamp.1], "# Code Context Guide");

    let alert_idx = md_lines
        .iter()
        .position(|l| l.contains("Never poll on CACHE_MISS"))
        .unwrap();
    let alert_clamp =
        clamp_document_snippet("docs/guide.md", &md_lines, alert_idx, alert_idx).unwrap();
    assert_eq!(md_lines[alert_clamp.0], "> [!WARNING]");
    assert_eq!(md_lines[alert_clamp.1], "> Continue with permitted host tools.");

    let tip_idx = md_lines
        .iter()
        .position(|l| l.contains("Keep read batches under 150 lines"))
        .unwrap();
    let tip_clamp = clamp_document_snippet("docs/guide.md", &md_lines, tip_idx, tip_idx).unwrap();
    assert_eq!(md_lines[tip_clamp.0], ":::tip");
    assert_eq!(md_lines[tip_clamp.1], ":::");

    let math_idx = md_lines
        .iter()
        .position(|l| l.contains("B_{target}"))
        .unwrap();
    let details_clamp =
        clamp_document_snippet("docs/guide.md", &md_lines, math_idx, math_idx).unwrap();
    assert_eq!(md_lines[details_clamp.0], "<details>");
    assert_eq!(md_lines[details_clamp.1], "</details>");

    let tbl_idx = md_lines
        .iter()
        .position(|l| l.contains("15.01x"))
        .unwrap();
    let tbl_clamp = clamp_document_snippet("docs/guide.md", &md_lines, tbl_idx, tbl_idx).unwrap();
    assert_eq!(md_lines[tbl_clamp.0], "## Performance Table");
    assert_eq!(md_lines[tbl_clamp.1], "Speedup | 1.00x | 15.01x");

    // 7. End-to-end Watched code_read with path.md#anchor
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("docs/guide.md"), complex_md).unwrap();
    let mut watched = Watched::new(Spec {
        root: dir.path().to_path_buf(),
        scopes: vec![".".into()],
        globs: vec!["*.md".into()],
        excludes: vec![],
        default_excludes: true,
        prewarm: false,
        policy: MemoryPolicy::default(),
        allow_worktrees: false,
        managed_parent: None,
    })
    .unwrap();

    let anchor_read = watched
        .call(
            "code_read",
            json!({
                "ranges": [
                    {"path": "docs/guide.md#install-setup", "start": 1, "end": 120},
                    {"path": "[Perf](docs/guide.md#performance-table)", "start": 1, "end": 120}
                ]
            }),
        )
        .unwrap();
    assert_eq!(anchor_read[0]["path"], "docs/guide.md");
    assert_eq!(
        anchor_read[0]["lines"][0],
        "## Installation and Setup {#install-setup}"
    );
    assert_eq!(anchor_read[1]["lines"][0], "## Performance Table");
}

#[test]
fn round_12_jsonl_vtt_srt_xliff2_ipynb_errors_svg_and_inline_slug_stripping() {
    use code_context::document::{
        clamp_document_snippet, extract_ipynb, extract_jsonl, extract_vtt_or_srt,
        extract_xliff_or_tmx, format_markdown_table, github_heading_slug, inspect_structure,
    };

    // 1. JSON Lines (.jsonl) extraction & structure inspection
    let raw_jsonl = concat!(
        "{\"id\":\"evt-1\",\"type\":\"tool_call\",\"tool\":\"cctxNativeContext\"}\n",
        "{\"id\":\"evt-2\",\"type\":\"tool_result\",\"status\":\"ok\"}\n"
    );
    let extracted_jsonl = extract_jsonl(raw_jsonl);
    assert!(extracted_jsonl.contains("Count: 2 records"));
    assert!(extracted_jsonl.contains("## [Record 1]"));
    assert!(extracted_jsonl.contains("tool: cctxNativeContext"));
    let jsonl_outline = inspect_structure("logs/events.jsonl", raw_jsonl);
    assert_eq!(
        jsonl_outline,
        vec![
            "L1: [Record 1] evt-1".to_string(),
            "L2: [Record 2] evt-2".to_string()
        ]
    );

    // 2. WebVTT & SRT extraction + raw SRT cue clamping
    let raw_vtt = concat!(
        "WEBVTT\n\n",
        "NOTE Intro segment\n\n",
        "cue-1\n",
        "00:00:01.000 --> 00:00:04.000\n",
        "<v Alice>Welcome to Code Context!</v>\n\n",
        "2\n",
        "00:00:05.000 --> 00:00:08.000\n",
        "Sub-millisecond trigram search.\n"
    );
    let vtt_out = extract_vtt_or_srt(raw_vtt);
    assert!(vtt_out.contains("### [Note] Intro segment"));
    assert!(vtt_out.contains("### [Cue cue-1: 00:00:01.000 --> 00:00:04.000]"));
    assert!(vtt_out.contains("[Alice]: Welcome to Code Context!"));

    let srt_lines: Vec<String> = vec![
        "1",
        "00:00:01,000 --> 00:00:03,000",
        "First subtitle line",
        "",
        "2",
        "00:00:04,000 --> 00:00:06,000",
        "Matched second subtitle line",
        "Second cue continuation",
        "",
        "3",
        "00:00:07,000 --> 00:00:09,000",
        "Third subtitle",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        clamp_document_snippet("captions.srt", &srt_lines, 6, 6),
        Some((4, 7))
    );

    // 3. XLIFF 2.0 <unit> with <note> and <segment>
    let xliff2 = r#"<xliff version="2.0"><file id="f1"><unit id="greeting_btn">
      <notes><note>Button label on home screen</note></notes>
      <segment><source>Start Now</source><target>지금 시작</target></segment>
    </unit></file></xliff>"#;
    let xliff2_out = extract_xliff_or_tmx(xliff2);
    assert!(xliff2_out.contains("### [Translation Unit: greeting_btn]"));
    assert!(xliff2_out.contains("Target: 지금 시작"));
    assert!(xliff2_out.contains("Note: Button label on home screen"));

    // 4. Jupyter Notebook (.ipynb) with execution_count & ANSI-colored error traceback
    let ipynb_err = r#"{
      "metadata": {"kernelspec": {"language": "python"}},
      "cells": [{
        "cell_type": "code",
        "execution_count": 7,
        "source": ["1 / 0\n"],
        "outputs": [{
          "output_type": "error",
          "ename": "ZeroDivisionError",
          "evalue": "division by zero",
          "traceback": ["\u001b[0;31mZeroDivisionError\u001b[0m: division by zero at line 1"]
        }]
      }]
    }"#;
    let ipynb_out = extract_ipynb(ipynb_err.as_bytes()).unwrap();
    assert!(ipynb_out.contains("## [Notebook Cell 1: code (python) In[7]]"));
    assert!(ipynb_out.contains("### [Cell Error] ZeroDivisionError: division by zero"));
    assert!(ipynb_out.contains("ZeroDivisionError: division by zero at line 1"));
    assert!(!ipynb_out.contains("\u{1b}"));

    // 5. Markdown heading slug with inline code backticks, bold, and HTML tags
    assert_eq!(
        github_heading_slug("## Using `code_context` in **VS Code** <code>v0.17</code>"),
        "using-code_context-in-vs-code-v017"
    );

    // 6. Table cell pipe & newline escaping
    let tbl = format_markdown_table(&[
        vec!["Header".into(), "Expression".into()],
        vec!["Union".into(), "a | b\nc".into()],
    ]);
    assert_eq!(tbl[2], "| Union | a \\| b c |");

    // 7. SVG structure inspection
    let svg_src = "<svg>\n  <title>Architecture Diagram</title>\n  <g id=\"cache-layer\">\n    <text>Trigram Index</text>\n  </g>\n</svg>";
    let svg_outline = inspect_structure("diagram.svg", svg_src);
    assert!(svg_outline.iter().any(|s| s.contains("[svg:title] Architecture Diagram")));
    assert!(svg_outline.iter().any(|s| s.contains("[svg:id] #cache-layer")));

    // 8. Unit anchors (#cell-2, #slide-2), Org-mode heading slug, and Mermaid definition priority
    assert_eq!(
        github_heading_slug("** Org Mode Subheading"),
        "org-mode-subheading"
    );
    assert_eq!(
        code_context::document::document_definition_priority("sequenceDiagram"),
        4
    );
    let nb_extracted = "## [Notebook Cell 1: markdown]\n# Intro\n\n## [Notebook Cell 2: code (python) In[1]]\nx = 42\nprint(x)\n\n## [Notebook Cell 3: markdown]\n# End\n";
    let (c2_start, c2_end) =
        code_context::document::resolve_markdown_anchor_range(nb_extracted, "#cell-2").unwrap();
    let nb_lines: Vec<&str> = nb_extracted.lines().collect();
    assert_eq!(nb_lines[c2_start - 1], "## [Notebook Cell 2: code (python) In[1]]");
    assert_eq!(nb_lines[c2_end - 1], "print(x)");
}








