use std::{
    ffi::OsString,
    io::Write,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::{
    process::Command,
    task,
    time::{self, timeout, timeout_at},
};
use tracing::info;

use crate::errors::rejected;

mod xml_package;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub chunk_id: String,
    pub ordinal: usize,
    pub body: String,
    pub content_hash: String,
}

/// Office and e-book formats converted by pandoc: Slack filetype, MIME type,
/// pandoc reader, and the signature the file must start with.
const PANDOC_FORMATS: &[(&str, &str, &str, &[u8])] = &[
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
        ZIP_SIGNATURE,
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
        ZIP_SIGNATURE,
    ),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
        ZIP_SIGNATURE,
    ),
    (
        "odt",
        "application/vnd.oasis.opendocument.text",
        "odt",
        ZIP_SIGNATURE,
    ),
    ("epub", "application/epub+zip", "epub", ZIP_SIGNATURE),
    ("rtf", "application/rtf", "rtf", b"{\\rtf"),
    ("rtf", "text/rtf", "rtf", b"{\\rtf"),
];
const ZIP_SIGNATURE: &[u8] = b"PK\x03\x04";
/// Caps pandoc's heap, as pandoc recommends for untrusted input.
const PANDOC_MAX_HEAP: &str = "-M512m";

/// Returns the pandoc reader for a file's Slack filetype or MIME type.
pub fn pandoc_reader(filetype: &str, mimetype: &str) -> Option<&'static str> {
    PANDOC_FORMATS
        .iter()
        .find(|(known_filetype, known_mimetype, _, _)| {
            *known_filetype == filetype || *known_mimetype == mimetype
        })
        .map(|(_, _, reader, _)| *reader)
}

pub async fn extract_pdf_text(
    pdf: Vec<u8>,
    extraction_timeout: Duration,
    max_output_bytes: usize,
) -> Result<String> {
    convert(
        "pdftotext",
        "PDF",
        &pdf,
        extraction_timeout,
        max_output_bytes,
        |input, output| {
            vec![
                "-layout".into(),
                "-nopgbrk".into(),
                input.into(),
                output.into(),
            ]
        },
    )
    .await
}

/// Converts a document in one of pandoc's input formats to plain text.
/// Sandboxed pandoc reads no other files and makes no network requests. The
/// extraction timeout covers any package repair and pandoc together.
pub async fn extract_pandoc_text(
    document: Vec<u8>,
    reader: &str,
    extraction_timeout: Duration,
    max_output_bytes: usize,
) -> Result<String> {
    let deadline = Instant::now()
        .checked_add(extraction_timeout)
        .context("extraction timeout is too long")?;
    let signature = PANDOC_FORMATS
        .iter()
        .find(|(_, _, known_reader, _)| *known_reader == reader)
        .map(|(_, _, _, signature)| *signature)
        .with_context(|| format!("unsupported pandoc reader {reader}"))?;
    if !document.starts_with(signature) {
        return Err(rejected(format!("file is not a {reader} document")));
    }
    // Office opens packages whose XML parts start with a byte order mark or end
    // in NUL padding; pandoc rejects them, so such parts are repaired first.
    let document = if signature == ZIP_SIGNATURE {
        let format = reader.to_owned();
        // Stops the blocking repair if this extraction is dropped or times out.
        let cancel = CancelOnDrop::default();
        let limit = xml_package::Deadline::new(deadline, cancel.0.clone());
        let mut repair =
            task::spawn_blocking(move || match xml_package::repair(&document, &limit) {
                Some((repaired, parts)) => {
                    info!(
                        event = "company_context_package_xml_repaired",
                        format, parts
                    );
                    repaired
                }
                None => document,
            });
        // Waiting for a free blocking thread counts against the deadline too.
        match timeout_at(time::Instant::from_std(deadline), &mut repair).await {
            Ok(repaired) => repaired.context("repair package XML parts")?,
            Err(_) => {
                repair.abort();
                bail!("pandoc timed out before it started");
            }
        }
    } else {
        document
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("pandoc timed out before it started");
    }
    convert(
        "pandoc",
        &format!("{reader} document"),
        &document,
        remaining,
        max_output_bytes,
        |input, output| {
            vec![
                "+RTS".into(),
                PANDOC_MAX_HEAP.into(),
                "-RTS".into(),
                "--sandbox".into(),
                "--from".into(),
                reader.into(),
                "--to".into(),
                "plain".into(),
                "--wrap=none".into(),
                "--output".into(),
                output.into(),
                input.into(),
            ]
        },
    )
    .await
}

/// Sets its flag when dropped, telling blocking work that its caller has gone.
#[derive(Default)]
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Runs a converter from a temporary input file to a temporary output file.
/// Both files are removed when the conversion finishes or fails.
async fn convert(
    program: &str,
    label: &str,
    document: &[u8],
    extraction_timeout: Duration,
    max_output_bytes: usize,
    args: impl FnOnce(&Path, &Path) -> Vec<OsString>,
) -> Result<String> {
    let mut input = NamedTempFile::new().with_context(|| format!("create {label} input file"))?;
    input
        .write_all(document)
        .with_context(|| format!("write {label} input file"))?;
    input
        .flush()
        .with_context(|| format!("flush {label} input file"))?;
    let output = NamedTempFile::new().with_context(|| format!("create {label} output file"))?;

    let mut command = Command::new(program);
    command
        .args(args(input.path(), output.path()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let result = timeout(extraction_timeout, command.output())
        .await
        .with_context(|| format!("{program} timed out"))?
        .with_context(|| format!("run {program}"))?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(rejected(format!(
            "{program} failed: {}",
            stderr.trim().chars().take(500).collect::<String>()
        )));
    }
    let output_size = tokio::fs::metadata(output.path())
        .await
        .with_context(|| format!("inspect extracted {label} text"))?
        .len();
    if output_size > max_output_bytes as u64 {
        return Err(rejected(format!(
            "extracted {label} text exceeds the configured byte limit"
        )));
    }
    let text = tokio::fs::read(output.path())
        .await
        .with_context(|| format!("read extracted {label} text"))?;
    let text = String::from_utf8(text)
        .map_err(|error| rejected(format!("extracted {label} text is not UTF-8: {error}")))?;
    let normalized = normalize_text(&text);
    if normalized.is_empty() {
        return Err(rejected(format!("{label} contains no extractable text")));
    }
    Ok(normalized)
}

pub fn extract_google_doc_text(document: Vec<u8>, max_output_bytes: usize) -> Result<String> {
    extract_plain_text(document, max_output_bytes, "exported Google Doc")
}

/// Decodes a plain-text file, such as a Slack snippet.
pub fn extract_plain_text(
    document: Vec<u8>,
    max_output_bytes: usize,
    label: &str,
) -> Result<String> {
    if document.len() > max_output_bytes {
        return Err(rejected(format!(
            "{label} exceeds the configured byte limit"
        )));
    }
    let text = String::from_utf8(document)
        .map_err(|error| rejected(format!("{label} is not UTF-8: {error}")))?;
    if text.contains('\0') {
        return Err(rejected(format!("{label} is not text")));
    }
    // Drive's plain-text export starts with a UTF-8 byte order mark.
    let normalized = normalize_text(text.strip_prefix('\u{feff}').unwrap_or(&text));
    if normalized.is_empty() {
        return Err(rejected(format!("{label} contains no extractable text")));
    }
    Ok(normalized)
}

pub fn chunk_text(text: &str, max_chars: usize) -> Vec<Chunk> {
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let mut bodies = Vec::new();
    let mut current = String::new();
    for paragraph in paragraphs {
        if paragraph.chars().count() > max_chars {
            if !current.is_empty() {
                bodies.push(std::mem::take(&mut current));
            }
            split_long_text(paragraph, max_chars, &mut bodies);
            continue;
        }
        let separator = if current.is_empty() { 0 } else { 2 };
        if current.chars().count() + separator + paragraph.chars().count() > max_chars {
            bodies.push(std::mem::take(&mut current));
        } else if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }
    if !current.trim().is_empty() {
        bodies.push(current);
    }

    bodies
        .into_iter()
        .enumerate()
        .filter_map(|(ordinal, body)| {
            let body = body.trim().to_owned();
            if body.is_empty() {
                return None;
            }
            Some(Chunk {
                chunk_id: format!("{ordinal:06}"),
                ordinal,
                content_hash: hex_sha256(body.as_bytes()),
                body,
            })
        })
        .collect()
}

pub fn hex_sha256(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

pub(crate) fn split_long_text(text: &str, max_chars: usize, output: &mut Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    for chunk in chars.chunks(max_chars) {
        output.push(chunk.iter().collect());
    }
}

fn normalize_text(text: &str) -> String {
    // Byte order marks inside a document carry no text, and PostgreSQL text
    // cannot hold NUL.
    text.replace(['\u{feff}', '\0'], "")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::is_rejected;

    #[test]
    fn google_doc_text_is_utf8_normalized_and_nonempty() {
        assert_eq!(
            extract_google_doc_text("\u{feff}heading\r\nbody  \r\n".into(), 100).unwrap(),
            "heading\nbody"
        );
        assert!(extract_google_doc_text(vec![0xff], 100).is_err());
        assert!(extract_google_doc_text("\u{feff}  \n".into(), 100).is_err());
        assert!(extract_google_doc_text(b"too large".to_vec(), 3).is_err());
    }

    #[test]
    fn extracted_text_drops_byte_order_marks_and_nuls() {
        assert_eq!(
            normalize_text("\u{feff}Apples\u{feff}\0\0\r\nPears\0"),
            "Apples\nPears"
        );
    }

    #[test]
    fn binary_files_are_not_plain_text() {
        assert!(is_rejected(
            &extract_plain_text(b"a\0b".to_vec(), 100, "file").unwrap_err()
        ));
    }

    #[tokio::test]
    async fn office_documents_are_converted_to_plain_text() {
        let timeout = Duration::from_secs(30);
        // The signature is checked before pandoc runs.
        let error = extract_pandoc_text(b"plain text".to_vec(), "docx", timeout, 1_000)
            .await
            .unwrap_err();
        assert!(is_rejected(&error), "{error}");

        let directory = tempfile::tempdir().unwrap();
        let docx = directory.path().join("report.docx");
        let Ok(status) = std::process::Command::new("pandoc")
            .args(["--from", "markdown", "--output"])
            .arg(&docx)
            .stdin(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"# Quarterly report\n\nRevenue grew **12%**.\n")?;
                child.wait()
            })
        else {
            eprintln!("skipping: install pandoc to test Office document conversion");
            return;
        };
        assert!(status.success());
        let text = extract_pandoc_text(std::fs::read(&docx).unwrap(), "docx", timeout, 1_000)
            .await
            .unwrap();
        assert_eq!(text, "Quarterly report\n\nRevenue grew 12%.");
        let error = extract_pandoc_text(std::fs::read(&docx).unwrap(), "docx", timeout, 10)
            .await
            .unwrap_err();
        assert!(is_rejected(&error), "{error}");
    }

    /// The installed pandoc's major and minor version.
    fn pandoc_version() -> Option<(u32, u32)> {
        let output = std::process::Command::new("pandoc")
            .arg("--version")
            .output()
            .ok()?;
        let stdout = String::from_utf8(output.stdout).ok()?;
        let version = stdout.lines().next()?.strip_prefix("pandoc ")?;
        let mut numbers = version.split('.').map(|number| number.parse().ok());
        Some((numbers.next()??, numbers.next()??))
    }

    /// An openpyxl-style workbook: package-absolute relationship targets and
    /// inline strings, which pandoc reads from 3.14 on.
    fn workbook(sheet_prefix: &[u8], sheet_suffix: &[u8]) -> Vec<u8> {
        let sheet = [
            sheet_prefix,
            br#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Item</t></is></c><c r="B1" t="inlineStr"><is><t>Count</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>Apples</t></is></c><c r="B2"><v>3</v></c></row></sheetData></worksheet>"#,
            sheet_suffix,
        ]
        .concat();
        xml_package::test_package::build(&[
            (
                "[Content_Types].xml",
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="/xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="/xl/worksheets/sheet1.xml"/></Relationships>"#,
            ),
            ("xl/worksheets/sheet1.xml", &sheet),
        ])
    }

    #[test]
    fn the_extraction_timeout_covers_waiting_for_a_blocking_thread() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            // Occupy the only blocking thread for longer than the timeout.
            let (started, running) = tokio::sync::oneshot::channel();
            let _busy = task::spawn_blocking(move || {
                started.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(3));
            });
            running.await.unwrap();
            let began = Instant::now();
            let error = extract_pandoc_text(
                workbook(b"", b"\0\0"),
                "xlsx",
                Duration::from_millis(100),
                1_000,
            )
            .await
            .unwrap_err();
            assert!(
                began.elapsed() < Duration::from_secs(1),
                "{:?}",
                began.elapsed()
            );
            assert!(!is_rejected(&error), "{error}");
            assert_eq!(error.to_string(), "pandoc timed out before it started");
        });
        runtime.shutdown_background();
    }

    #[tokio::test]
    async fn the_extraction_timeout_covers_the_package_repair() {
        let error = extract_pandoc_text(workbook(b"", b"\0\0"), "xlsx", Duration::ZERO, 1_000)
            .await
            .unwrap_err();
        // The repair used up the deadline, so pandoc never started; a timeout
        // is retried, unlike a rejected document.
        assert!(!is_rejected(&error), "{error}");
        assert_eq!(error.to_string(), "pandoc timed out before it started");
    }

    #[tokio::test]
    async fn documents_with_whitespace_before_the_root_and_nul_padding_are_converted() {
        let directory = tempfile::tempdir().unwrap();
        let docx = directory.path().join("report.docx");
        let Ok(status) = std::process::Command::new("pandoc")
            .args(["--from", "markdown", "--output"])
            .arg(&docx)
            .stdin(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"# Quarterly report\n\nRevenue grew **12%**.\n")?;
                child.wait()
            })
        else {
            eprintln!("skipping: install pandoc to test Office document conversion");
            return;
        };
        assert!(status.success());
        let parts: Vec<(String, Vec<u8>)> =
            xml_package::test_package::parts(&std::fs::read(&docx).unwrap())
                .into_iter()
                .map(|(name, contents)| {
                    if name == "word/document.xml" {
                        let whitespace = [&b"\r\n"[..], &[b' '; 8_192]].concat();
                        (name, [&whitespace[..], &contents, b"\0\0\0"].concat())
                    } else {
                        (name, contents)
                    }
                })
                .collect();
        let parts: Vec<(&str, &[u8])> = parts
            .iter()
            .map(|(name, contents)| (name.as_str(), contents.as_slice()))
            .collect();
        let text = extract_pandoc_text(
            xml_package::test_package::build(&parts),
            "docx",
            Duration::from_secs(30),
            1_000,
        )
        .await
        .unwrap();
        assert_eq!(text, "Quarterly report\n\nRevenue grew 12%.");
    }

    #[tokio::test]
    async fn spreadsheets_with_bom_or_nul_padded_parts_are_converted() {
        // Older releases cannot read the openpyxl-style workbook at all.
        match pandoc_version() {
            Some(version) if version >= (3, 14) => {}
            found => {
                eprintln!("skipping: spreadsheet conversion needs pandoc 3.14, found {found:?}");
                return;
            }
        }
        let timeout = Duration::from_secs(30);
        for (prefix, suffix) in [
            (&b""[..], &b""[..]),
            (b"\xEF\xBB\xBF", b""),
            (b"", b"\n\0\0\0\0"),
        ] {
            let text = extract_pandoc_text(workbook(prefix, suffix), "xlsx", timeout, 1_000)
                .await
                .unwrap();
            let words: Vec<&str> = text.split_whitespace().collect();
            assert!(
                words.windows(2).any(|pair| pair == ["Apples", "3"]),
                "{text:?}"
            );
            assert!(words.contains(&"Item"), "{text:?}");
        }
    }

    #[test]
    fn chunks_are_stable_and_bounded() {
        let text = "alpha beta gamma\n\ndelta epsilon zeta\n\neta theta iota";
        let chunks = chunk_text(text, 25);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.chunk_id.as_str())
                .collect::<Vec<_>>(),
            vec!["000000", "000001", "000002"]
        );
        assert!(chunks.iter().all(|chunk| chunk.body.chars().count() <= 25));
        assert_eq!(chunks, chunk_text(text, 25));
    }

    #[test]
    fn splits_large_unicode_paragraphs_on_character_boundaries() {
        let chunks = chunk_text("éééééééé", 5);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.body.as_str())
                .collect::<Vec<_>>(),
            vec!["ééééé", "ééé"]
        );
    }
}
