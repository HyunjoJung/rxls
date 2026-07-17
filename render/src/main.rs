//! `rxls-render` command-line bundle generator.

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use rxls::Workbook;
use rxls_render::{
    build_print_document, render_print_document_pdf, render_print_page_png, render_scene_svg,
    render_sheet_svg, Fixed, FontPack, PathCommand, PrintOptions, Rect, RenderOptions, RenderRange,
    RenderSelection, Rgb, Scene, SceneNode, TextAnchor, TextBaseline, FIXED_UNITS_PER_PIXEL,
    MAX_WORKSHEET_COLUMN, MAX_WORKSHEET_ROW,
};
use sha2::{Digest, Sha256};

const MAX_INPUT_BYTES: u64 = 512 << 20;
const MAX_BUNDLE_SHEETS: usize = 1_024;
const MAX_BUNDLE_BYTES: u64 = 256 << 20;
const MAX_MANIFEST_BYTES: usize = 16 << 20;
static STAGING_NONCE: AtomicU64 = AtomicU64::new(0);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rxls-render: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let command = parse_args(env::args_os().skip(1))?;
    let bytes = read_bounded(&command.input)?;
    let workbook = Workbook::open(&bytes)?;
    if workbook.sheets.len() > MAX_BUNDLE_SHEETS {
        return Err(format!(
            "bundle sheet limit exceeded: limit {MAX_BUNDLE_SHEETS}, required {}",
            workbook.sheets.len()
        )
        .into());
    }
    let font_pack = command
        .font_pack_manifest
        .as_ref()
        .map(FontPack::load_manifest)
        .transpose()?;
    let mut transaction = OutputTransaction::begin(&command.output_dir)?;

    let source_sha256 = sha256_hex(&bytes);
    let mut options = RenderOptions {
        selection: command
            .range
            .map(RenderSelection::Range)
            .unwrap_or(RenderSelection::Used),
        include_hidden: command.include_hidden,
        gridlines: command.gridlines,
        font_pack,
        ..RenderOptions::default()
    };
    if let Some(family) = command.default_font_family {
        options.default_font_family = family;
    } else if let Some(pack) = options.font_pack.as_ref() {
        options.default_font_family = pack.default_family().to_string();
    }
    let mut rendered = Vec::with_capacity(workbook.sheets.len());
    let mut bundle_bytes = 0_u64;

    for (index, sheet) in workbook.sheets.iter().enumerate() {
        let mut sheet_options = options.clone();
        if command.single_page_sheets {
            let source_print_gridlines = sheet
                .print_metadata()
                .print_gridlines()
                .unwrap_or_else(|| sheet.print_gridlines());
            sheet_options.gridlines &= source_print_gridlines;
        }
        let output = render_sheet_svg(&workbook, index, &sheet_options)?;
        let file = format!("sheet-{index:04}.svg");
        let svg_sha256 = sha256_hex(output.svg.as_bytes());
        let scene_sha256 = scene_sha256_hex(&output.scene);
        bundle_bytes = bundle_bytes
            .checked_add(output.svg.len() as u64)
            .ok_or("bundle output byte count overflow")?;
        if bundle_bytes > MAX_BUNDLE_BYTES {
            return Err(format!(
                "bundle output limit exceeded: limit {MAX_BUNDLE_BYTES}, required {bundle_bytes}"
            )
            .into());
        }
        fs::write(transaction.staging_dir().join(&file), output.svg.as_bytes())?;
        let visibility = if sheet.is_very_hidden() {
            "very_hidden"
        } else if sheet.is_hidden() {
            "hidden"
        } else {
            "visible"
        };
        let print = if command.print_layout {
            Some(render_print_bundle(
                &workbook,
                index,
                &sheet_options,
                &command.print_backends,
                PrintBundlePolicy {
                    single_page_sheets: command.single_page_sheets,
                    png_dpi: command.png_dpi,
                },
                transaction.staging_dir(),
                &mut bundle_bytes,
            )?)
        } else {
            None
        };
        rendered.push(BundleSheet {
            index,
            name: sheet.name.clone(),
            visibility,
            file,
            canvas_width_raw: output.scene.width.raw(),
            canvas_height_raw: output.scene.height.raw(),
            svg_bytes: output.svg.len() as u64,
            svg_sha256,
            scene_sha256,
            report_json: output.report.to_json(),
            print,
        });
    }

    let manifest = bundle_manifest_json(
        &bytes,
        &source_sha256,
        options.font_pack.as_ref().map(FontPack::pack_sha256),
        &rendered,
    );
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "render manifest limit exceeded: limit {MAX_MANIFEST_BYTES}, required {}",
            manifest.len()
        )
        .into());
    }
    bundle_bytes = bundle_bytes
        .checked_add(manifest.len() as u64)
        .ok_or("bundle output byte count overflow")?;
    if bundle_bytes > MAX_BUNDLE_BYTES {
        return Err(format!(
            "bundle output limit exceeded: limit {MAX_BUNDLE_BYTES}, required {bundle_bytes}"
        )
        .into());
    }
    fs::write(
        transaction.staging_dir().join("render-manifest.json"),
        manifest.as_bytes(),
    )?;
    transaction.commit()?;
    println!(
        "rendered {} sheet(s) into {}",
        rendered.len(),
        command.output_dir.display()
    );
    Ok(())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let file = File::open(path)?;
    let declared = file.metadata()?.len();
    if declared > MAX_INPUT_BYTES {
        return Err(format!(
            "input byte limit exceeded: limit {MAX_INPUT_BYTES}, declared {declared}"
        )
        .into());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(declared).unwrap_or(0));
    file.take(MAX_INPUT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(format!(
            "input byte limit exceeded: limit {MAX_INPUT_BYTES}, read {}",
            bytes.len()
        )
        .into());
    }
    Ok(bytes)
}

struct OutputTransaction {
    output_dir: PathBuf,
    staging_dir: PathBuf,
    committed: bool,
}

impl OutputTransaction {
    fn begin(output_dir: &Path) -> Result<Self, Box<dyn Error>> {
        if output_dir.exists() {
            if !output_dir.is_dir() {
                return Err("output path exists and is not a directory".into());
            }
            if output_dir.read_dir()?.next().transpose()?.is_some() {
                return Err("output directory must be empty".into());
            }
        }
        let parent = output_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let name = output_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("output directory must have a UTF-8 final component")?;
        let staging_dir = (0..100)
            .find_map(|_| {
                let nonce = STAGING_NONCE.fetch_add(1, Ordering::Relaxed);
                let candidate = parent.join(format!(
                    ".{name}.rxls-render-stage-{}-{nonce}",
                    std::process::id()
                ));
                match fs::create_dir(&candidate) {
                    Ok(()) => Some(Ok(candidate)),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .transpose()?
            .ok_or("could not allocate a unique staging directory")?;
        Ok(Self {
            output_dir: output_dir.to_path_buf(),
            staging_dir,
            committed: false,
        })
    }

    fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }

    fn commit(&mut self) -> Result<(), Box<dyn Error>> {
        if self.output_dir.exists() {
            fs::remove_dir(&self.output_dir)?;
        }
        fs::rename(&self.staging_dir, &self.output_dir)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for OutputTransaction {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.staging_dir);
        }
    }
}

#[derive(Debug)]
struct Command {
    input: PathBuf,
    output_dir: PathBuf,
    range: Option<RenderRange>,
    include_hidden: bool,
    gridlines: bool,
    font_pack_manifest: Option<PathBuf>,
    default_font_family: Option<String>,
    print_layout: bool,
    print_backends: BTreeSet<PrintBackend>,
    single_page_sheets: bool,
    png_dpi: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PrintBackend {
    Svg,
    Pdf,
    Png,
}

fn parse_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Command, Box<dyn Error>> {
    let args: Vec<_> = args.into_iter().collect();
    if args.first().and_then(|arg| arg.to_str()) != Some("bundle") {
        return Err(usage().into());
    }
    let input = args.get(1).ok_or_else(usage).map(PathBuf::from)?;
    let mut output_dir = None;
    let mut range = None;
    let mut include_hidden = false;
    let mut gridlines = true;
    let mut font_pack_manifest = None;
    let mut default_font_family = None;
    let mut print_layout = false;
    let mut print_backends = BTreeSet::new();
    let mut single_page_sheets = false;
    let mut png_dpi = 144_u32;
    let mut index = 2;
    while index < args.len() {
        match args[index].to_str() {
            Some("--output-dir") => {
                index += 1;
                output_dir = Some(
                    args.get(index)
                        .ok_or("--output-dir requires a path")?
                        .into(),
                );
            }
            Some("--range") => {
                index += 1;
                let value = args
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or("--range requires UTF-8 A1:B2 syntax")?;
                range = Some(parse_a1_range(value)?);
            }
            Some("--include-hidden") => include_hidden = true,
            Some("--no-gridlines") => gridlines = false,
            Some("--font-pack-manifest") => {
                index += 1;
                font_pack_manifest = Some(
                    args.get(index)
                        .ok_or("--font-pack-manifest requires a path")?
                        .into(),
                );
            }
            Some("--default-font-family") => {
                index += 1;
                let family = args
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or("--default-font-family requires UTF-8 text")?;
                if family.is_empty() || family.len() > 512 || family.chars().any(char::is_control) {
                    return Err("--default-font-family is invalid".into());
                }
                default_font_family = Some(family.to_string());
            }
            Some("--print-layout") => print_layout = true,
            Some("--single-page-sheets") => {
                single_page_sheets = true;
                print_layout = true;
            }
            Some("--print-backends") => {
                index += 1;
                let value = args
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or("--print-backends requires svg,pdf,png")?;
                for backend in value.split(',') {
                    let backend = match backend {
                        "svg" => PrintBackend::Svg,
                        "pdf" => PrintBackend::Pdf,
                        "png" => PrintBackend::Png,
                        _ => return Err("--print-backends accepts only svg,pdf,png".into()),
                    };
                    print_backends.insert(backend);
                }
                if print_backends.is_empty() {
                    return Err("--print-backends must not be empty".into());
                }
                print_layout = true;
            }
            Some("--png-dpi") => {
                index += 1;
                png_dpi = args
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or("--png-dpi requires an integer")?
                    .parse()?;
                if !(36..=1_200).contains(&png_dpi) {
                    return Err("--png-dpi must be between 36 and 1200".into());
                }
            }
            Some(option) => return Err(format!("unknown option {option:?}\n{}", usage()).into()),
            None => return Err("options must be valid UTF-8".into()),
        }
        index += 1;
    }
    if print_layout && print_backends.is_empty() {
        print_backends.insert(PrintBackend::Svg);
    }
    if !print_layout && png_dpi != 144 {
        return Err("--png-dpi requires --print-layout or --print-backends".into());
    }
    Ok(Command {
        input,
        output_dir: output_dir.ok_or("missing required --output-dir DIR")?,
        range,
        include_hidden,
        gridlines,
        font_pack_manifest,
        default_font_family,
        print_layout,
        print_backends,
        single_page_sheets,
        png_dpi,
    })
}

fn usage() -> String {
    "usage: rxls-render bundle INPUT --output-dir DIR [--range A1:D20] [--include-hidden] [--no-gridlines] [--font-pack-manifest FILE] [--default-font-family FAMILY] [--print-layout] [--print-backends svg,pdf,png] [--single-page-sheets] [--png-dpi 144]".to_string()
}

fn parse_a1_range(value: &str) -> Result<RenderRange, Box<dyn Error>> {
    let mut parts = value.split(':');
    let first = parts.next().ok_or("empty range")?;
    let last = parts.next().unwrap_or(first);
    if parts.next().is_some() {
        return Err("range must contain at most one ':'".into());
    }
    let (first_row, first_col) = parse_a1_cell(first)?;
    let (last_row, last_col) = parse_a1_cell(last)?;
    if first_row > last_row || first_col > last_col {
        return Err("range start must not follow its end".into());
    }
    Ok(RenderRange::new(first_row, first_col, last_row, last_col))
}

fn parse_a1_cell(value: &str) -> Result<(u32, u16), Box<dyn Error>> {
    let value = value.trim();
    let letters_end = value
        .bytes()
        .position(|byte| !byte.is_ascii_alphabetic())
        .unwrap_or(value.len());
    if letters_end == 0 || letters_end == value.len() {
        return Err(format!("invalid A1 cell {value:?}").into());
    }
    let mut column = 0_u64;
    for &byte in &value.as_bytes()[..letters_end] {
        let digit = u64::from(byte.to_ascii_uppercase() - b'A' + 1);
        column = column
            .checked_mul(26)
            .and_then(|value| value.checked_add(digit))
            .ok_or("column overflow")?;
    }
    let row: u64 = value[letters_end..].parse()?;
    if row == 0 || row > u64::from(MAX_WORKSHEET_ROW) + 1 {
        return Err("row is outside the supported grid".into());
    }
    if column == 0 || column > u64::from(MAX_WORKSHEET_COLUMN) + 1 {
        return Err("column is outside the supported grid".into());
    }
    Ok(((row - 1) as u32, (column - 1) as u16))
}

#[derive(Debug)]
struct BundleArtifact {
    file: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug)]
struct BundlePrint {
    layout_override: Option<&'static str>,
    page_count: usize,
    page_scene_sha256: Vec<String>,
    report: BundleArtifact,
    svg_pages: Vec<BundleArtifact>,
    pdf: Option<BundleArtifact>,
    png_pages: Vec<BundleArtifact>,
    png_dpi: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
struct PrintBundlePolicy {
    single_page_sheets: bool,
    png_dpi: u32,
}

fn render_print_bundle(
    workbook: &Workbook,
    sheet_index: usize,
    render_options: &RenderOptions,
    backends: &BTreeSet<PrintBackend>,
    policy: PrintBundlePolicy,
    staging_dir: &Path,
    bundle_bytes: &mut u64,
) -> Result<BundlePrint, Box<dyn Error>> {
    let document = build_print_document(
        workbook,
        sheet_index,
        &PrintOptions {
            render: render_options.clone(),
            single_page_sheets: policy.single_page_sheets,
            ..PrintOptions::default()
        },
    )?;
    let page_dir_name = format!("sheet-{sheet_index:04}-pages");
    let page_dir = staging_dir.join(&page_dir_name);
    if backends.contains(&PrintBackend::Svg) || backends.contains(&PrintBackend::Png) {
        fs::create_dir(&page_dir)?;
    }
    let mut svg_pages = Vec::new();
    let mut png_pages = Vec::new();
    let page_scene_sha256 = document
        .pages
        .iter()
        .map(|page| scene_sha256_hex(&page.scene))
        .collect();
    for (page_index, page) in document.pages.iter().enumerate() {
        if backends.contains(&PrintBackend::Svg) {
            let bytes =
                render_scene_svg(&page.scene, render_options.limits.max_output_bytes)?.into_bytes();
            let file = format!("{page_dir_name}/page-{:04}.svg", page_index + 1);
            record_bundle_bytes(bundle_bytes, bytes.len() as u64)?;
            fs::write(staging_dir.join(&file), &bytes)?;
            svg_pages.push(BundleArtifact {
                file,
                bytes: bytes.len() as u64,
                sha256: sha256_hex(&bytes),
            });
        }
        if backends.contains(&PrintBackend::Png) {
            let bytes = render_print_page_png(page, policy.png_dpi, &document)?;
            let file = format!("{page_dir_name}/page-{:04}.png", page_index + 1);
            record_bundle_bytes(bundle_bytes, bytes.len() as u64)?;
            fs::write(staging_dir.join(&file), &bytes)?;
            png_pages.push(BundleArtifact {
                file,
                bytes: bytes.len() as u64,
                sha256: sha256_hex(&bytes),
            });
        }
    }
    let pdf = if backends.contains(&PrintBackend::Pdf) {
        let bytes = render_print_document_pdf(&document)?;
        let file = format!("sheet-{sheet_index:04}.pdf");
        record_bundle_bytes(bundle_bytes, bytes.len() as u64)?;
        fs::write(staging_dir.join(&file), &bytes)?;
        Some(BundleArtifact {
            file,
            bytes: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
        })
    } else {
        None
    };
    let report_bytes = document.report.to_json().into_bytes();
    let report_file = format!("sheet-{sheet_index:04}-pages.json");
    record_bundle_bytes(bundle_bytes, report_bytes.len() as u64)?;
    fs::write(staging_dir.join(&report_file), &report_bytes)?;
    Ok(BundlePrint {
        layout_override: document.report.layout_override.map(|value| value.code()),
        page_count: document.pages.len(),
        page_scene_sha256,
        report: BundleArtifact {
            file: report_file,
            bytes: report_bytes.len() as u64,
            sha256: sha256_hex(&report_bytes),
        },
        svg_pages,
        pdf,
        png_pages,
        png_dpi: backends
            .contains(&PrintBackend::Png)
            .then_some(policy.png_dpi),
    })
}

fn record_bundle_bytes(total: &mut u64, bytes: u64) -> Result<(), Box<dyn Error>> {
    *total = total
        .checked_add(bytes)
        .ok_or("bundle output byte count overflow")?;
    if *total > MAX_BUNDLE_BYTES {
        return Err(format!(
            "bundle output limit exceeded: limit {MAX_BUNDLE_BYTES}, required {total}"
        )
        .into());
    }
    Ok(())
}

#[derive(Debug)]
struct BundleSheet {
    index: usize,
    name: String,
    visibility: &'static str,
    file: String,
    canvas_width_raw: i64,
    canvas_height_raw: i64,
    svg_bytes: u64,
    svg_sha256: String,
    scene_sha256: String,
    report_json: String,
    print: Option<BundlePrint>,
}

fn bundle_manifest_json(
    source: &[u8],
    source_sha256: &str,
    font_pack_sha256: Option<&str>,
    sheets: &[BundleSheet],
) -> String {
    let mut out = String::new();
    out.push_str("{\"schema\":\"rxls.render.bundle.v1\",\"source\":{\"sha256\":\"");
    out.push_str(source_sha256);
    out.push_str("\",\"bytes\":");
    out.push_str(&source.len().to_string());
    out.push_str("},\"renderer\":{\"name\":\"rxls-render\",\"version\":\"");
    out.push_str(env!("CARGO_PKG_VERSION"));
    out.push_str("\",\"fixed_units_per_pixel\":");
    out.push_str(&FIXED_UNITS_PER_PIXEL.to_string());
    out.push_str(",\"font_pack_sha256\":");
    match font_pack_sha256 {
        Some(digest) => {
            out.push('"');
            out.push_str(digest);
            out.push('"');
        }
        None => out.push_str("null"),
    }
    out.push_str("},\"sheets\":[");
    for (position, sheet) in sheets.iter().enumerate() {
        if position != 0 {
            out.push(',');
        }
        out.push_str("{\"index\":");
        out.push_str(&sheet.index.to_string());
        out.push_str(",\"name\":\"");
        push_json_escaped(&mut out, &sheet.name);
        out.push_str("\",\"visibility\":\"");
        out.push_str(sheet.visibility);
        out.push_str("\",\"file\":\"");
        out.push_str(&sheet.file);
        out.push_str("\",\"canvas\":{\"width_raw\":");
        out.push_str(&sheet.canvas_width_raw.to_string());
        out.push_str(",\"height_raw\":");
        out.push_str(&sheet.canvas_height_raw.to_string());
        out.push_str("},\"svg\":{\"bytes\":");
        out.push_str(&sheet.svg_bytes.to_string());
        out.push_str(",\"sha256\":\"");
        out.push_str(&sheet.svg_sha256);
        out.push_str("\"},\"scene\":{\"sha256\":\"");
        out.push_str(&sheet.scene_sha256);
        out.push_str("\"},\"report\":");
        out.push_str(&sheet.report_json);
        if let Some(print) = sheet.print.as_ref() {
            out.push_str(",\"print\":");
            push_print_bundle_json(&mut out, print);
        }
        out.push('}');
    }
    out.push_str("]}\n");
    out
}

fn push_print_bundle_json(out: &mut String, print: &BundlePrint) {
    out.push_str("{\"schema\":\"rxls.render.print-bundle.v1\"");
    if let Some(layout_override) = print.layout_override {
        out.push_str(",\"layout_override\":\"");
        out.push_str(layout_override);
        out.push('"');
    }
    out.push_str(",\"page_count\":");
    out.push_str(&print.page_count.to_string());
    out.push_str(",\"report\":");
    push_artifact_json(out, &print.report);
    out.push_str(",\"page_scenes\":[");
    for (index, digest) in print.page_scene_sha256.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push_str("{\"index\":");
        out.push_str(&index.to_string());
        out.push_str(",\"sha256\":\"");
        out.push_str(digest);
        out.push_str("\"}");
    }
    out.push(']');
    out.push_str(",\"svg_pages\":[");
    for (index, artifact) in print.svg_pages.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        push_artifact_json(out, artifact);
    }
    out.push_str("],\"pdf\":");
    match print.pdf.as_ref() {
        Some(artifact) => push_artifact_json(out, artifact),
        None => out.push_str("null"),
    }
    out.push_str(",\"png_dpi\":");
    match print.png_dpi {
        Some(dpi) => out.push_str(&dpi.to_string()),
        None => out.push_str("null"),
    }
    out.push_str(",\"png_pages\":[");
    for (index, artifact) in print.png_pages.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        push_artifact_json(out, artifact);
    }
    out.push_str("]}");
}

fn push_artifact_json(out: &mut String, artifact: &BundleArtifact) {
    out.push_str("{\"file\":\"");
    push_json_escaped(out, &artifact.file);
    out.push_str("\",\"bytes\":");
    out.push_str(&artifact.bytes.to_string());
    out.push_str(",\"sha256\":\"");
    out.push_str(&artifact.sha256);
    out.push_str("\"}");
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes))
}

fn scene_sha256_hex(scene: &Scene) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rxls-render-scene-v2\0");
    update_string(&mut digest, &scene.title);
    update_fixed(&mut digest, scene.width.raw());
    update_fixed(&mut digest, scene.height.raw());
    update_rgb(&mut digest, scene.background);
    digest.update((scene.nodes.len() as u64).to_le_bytes());
    for node in &scene.nodes {
        match node {
            SceneNode::ClipGroup(group) => {
                digest.update([6]);
                update_rect(&mut digest, group.clip);
                digest.update((group.nodes.len() as u64).to_le_bytes());
                for child in &group.nodes {
                    let child_scene = Scene {
                        title: String::new(),
                        width: Fixed::ZERO,
                        height: Fixed::ZERO,
                        background: Rgb::BLACK,
                        nodes: vec![child.clone()],
                    };
                    update_string(&mut digest, &scene_sha256_hex(&child_scene));
                }
            }
            SceneNode::Rect(node) => {
                digest.update([0]);
                update_rect(&mut digest, node.rect);
                update_optional_rgb(&mut digest, node.fill);
                update_optional_rgb(&mut digest, node.stroke);
                update_fixed(&mut digest, node.stroke_width.raw());
            }
            SceneNode::Line(node) => {
                digest.update([1]);
                for value in [node.x1, node.y1, node.x2, node.y2, node.width] {
                    update_fixed(&mut digest, value.raw());
                }
                update_rgb(&mut digest, node.color);
            }
            SceneNode::Path(node) => {
                digest.update([4]);
                digest.update((node.commands.len() as u64).to_le_bytes());
                for command in &node.commands {
                    update_path_command(&mut digest, *command);
                }
                update_optional_rgb(&mut digest, node.fill);
                update_optional_rgb(&mut digest, node.stroke);
                update_fixed(&mut digest, node.stroke_width.raw());
            }
            SceneNode::Image(node) => {
                digest.update([5]);
                update_rect(&mut digest, node.rect);
                digest.update(node.pixel_width.to_le_bytes());
                digest.update(node.pixel_height.to_le_bytes());
                digest.update(node.rotation_mdeg.to_le_bytes());
                digest.update((node.rgba.len() as u64).to_le_bytes());
                digest.update(&node.rgba);
                match &node.alt_text {
                    Some(value) => {
                        digest.update([1]);
                        update_string(&mut digest, value);
                    }
                    None => digest.update([0]),
                }
            }
            SceneNode::Text(node) => {
                digest.update([2]);
                update_string(&mut digest, &node.text);
                update_rect(&mut digest, node.bounds);
                update_rect(&mut digest, node.clip_bounds);
                update_fixed(&mut digest, node.horizontal_padding.raw());
                update_string(&mut digest, &node.style.family);
                update_fixed(&mut digest, node.style.size.raw());
                update_rgb(&mut digest, node.style.color);
                digest.update([
                    u8::from(node.style.bold),
                    u8::from(node.style.italic),
                    u8::from(node.style.underline),
                    u8::from(node.style.strikethrough),
                    match node.style.anchor {
                        TextAnchor::Start => 0,
                        TextAnchor::Middle => 1,
                        TextAnchor::End => 2,
                    },
                    match node.style.baseline {
                        TextBaseline::Top => 0,
                        TextBaseline::Middle => 1,
                        TextBaseline::Bottom => 2,
                    },
                ]);
                digest.update(node.style.rotation_degrees.to_le_bytes());
                match &node.hyperlink {
                    Some(value) => {
                        digest.update([1]);
                        update_string(&mut digest, value);
                    }
                    None => digest.update([0]),
                }
            }
            SceneNode::GlyphRun(node) => {
                digest.update([3]);
                update_string(&mut digest, &node.text);
                update_rect(&mut digest, node.clip_bounds);
                update_rgb(&mut digest, node.color);
                digest.update(node.rotation_degrees.to_le_bytes());
                update_fixed(&mut digest, node.pivot_x.raw());
                update_fixed(&mut digest, node.pivot_y.raw());
                digest.update((node.commands.len() as u64).to_le_bytes());
                for command in &node.commands {
                    update_path_command(&mut digest, *command);
                }
                digest.update((node.clusters.len() as u64).to_le_bytes());
                for cluster in &node.clusters {
                    for value in [
                        cluster.source_start,
                        cluster.source_end,
                        cluster.command_start,
                        cluster.command_end,
                    ] {
                        digest.update(value.to_le_bytes());
                    }
                }
                digest.update((node.paints.len() as u64).to_le_bytes());
                for paint in &node.paints {
                    digest.update(paint.command_start.to_le_bytes());
                    digest.update(paint.command_end.to_le_bytes());
                    update_rgb(&mut digest, paint.color);
                }
                digest.update((node.decorations.len() as u64).to_le_bytes());
                for line in &node.decorations {
                    for value in [line.x1, line.y1, line.x2, line.y2, line.width] {
                        update_fixed(&mut digest, value.raw());
                    }
                    update_rgb(&mut digest, line.color);
                }
                match &node.hyperlink {
                    Some(value) => {
                        digest.update([1]);
                        update_string(&mut digest, value);
                    }
                    None => digest.update([0]),
                }
            }
        }
    }
    hex_digest(digest.finalize())
}

fn update_path_command(digest: &mut Sha256, command: PathCommand) {
    match command {
        PathCommand::MoveTo { x, y } => {
            digest.update([0]);
            update_fixed(digest, x.raw());
            update_fixed(digest, y.raw());
        }
        PathCommand::LineTo { x, y } => {
            digest.update([1]);
            update_fixed(digest, x.raw());
            update_fixed(digest, y.raw());
        }
        PathCommand::QuadraticTo {
            control_x,
            control_y,
            x,
            y,
        } => {
            digest.update([2]);
            for value in [control_x, control_y, x, y] {
                update_fixed(digest, value.raw());
            }
        }
        PathCommand::CubicTo {
            control1_x,
            control1_y,
            control2_x,
            control2_y,
            x,
            y,
        } => {
            digest.update([3]);
            for value in [control1_x, control1_y, control2_x, control2_y, x, y] {
                update_fixed(digest, value.raw());
            }
        }
        PathCommand::Close => digest.update([4]),
    }
}

fn update_string(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value.as_bytes());
}

fn update_fixed(digest: &mut Sha256, value: i64) {
    digest.update(value.to_le_bytes());
}

fn update_rgb(digest: &mut Sha256, value: Rgb) {
    digest.update([value.red, value.green, value.blue]);
}

fn update_optional_rgb(digest: &mut Sha256, value: Option<Rgb>) {
    match value {
        Some(value) => {
            digest.update([1]);
            update_rgb(digest, value);
        }
        None => digest.update([0]),
    }
}

fn update_rect(digest: &mut Sha256, value: Rect) {
    for fixed in [value.x, value.y, value.width, value.height] {
        update_fixed(digest, fixed.raw());
    }
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    let mut out = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in digest.as_ref() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn push_json_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < '\u{20}' => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let byte = ch as u8;
                out.push_str("\\u00");
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
            ch => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn command_line_accepts_explicit_font_pack_and_family() {
        let command = parse_args(
            [
                "bundle",
                "input.xlsx",
                "--output-dir",
                "output",
                "--font-pack-manifest",
                "fonts/manifest.json",
                "--default-font-family",
                "Noto Sans CJK KR",
                "--range",
                "B2:D9",
                "--no-gridlines",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .unwrap();
        assert_eq!(command.input, PathBuf::from("input.xlsx"));
        assert_eq!(command.output_dir, PathBuf::from("output"));
        assert_eq!(
            command.font_pack_manifest,
            Some(PathBuf::from("fonts/manifest.json"))
        );
        assert_eq!(
            command.default_font_family.as_deref(),
            Some("Noto Sans CJK KR")
        );
        assert_eq!(command.range, Some(RenderRange::new(1, 1, 8, 3)));
        assert!(!command.gridlines);
        assert!(!command.print_layout);
        assert!(command.print_backends.is_empty());
        assert!(!command.single_page_sheets);
    }

    #[test]
    fn bundle_manifest_records_the_exact_verified_pack_identity() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let manifest = bundle_manifest_json(b"source", "source-digest", Some(digest), &[]);
        assert!(manifest.contains(&format!("\"font_pack_sha256\":\"{digest}\"")));
        assert!(!manifest.contains("manifest.json"));
    }

    #[test]
    fn command_line_accepts_selectable_print_backends_and_dpi() {
        let command = parse_args(
            [
                "bundle",
                "input.xlsx",
                "--output-dir",
                "output",
                "--print-backends",
                "svg,pdf,png,pdf",
                "--png-dpi",
                "300",
                "--single-page-sheets",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .unwrap();
        assert!(command.print_layout);
        assert_eq!(
            command.print_backends,
            BTreeSet::from([PrintBackend::Svg, PrintBackend::Pdf, PrintBackend::Png])
        );
        assert_eq!(command.png_dpi, 300);
        assert!(command.single_page_sheets);
    }
}
