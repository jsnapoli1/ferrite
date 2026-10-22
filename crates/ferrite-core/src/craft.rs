//! Registry of the ArtCraft "Crafting Apps" (github.com/storytold) — native desktop apps
//! written in Rust that replace Word, Excel, PowerPoint, Illustrator, Photoshop, Acrobat,
//! InDesign, After Effects, AutoCAD, Pro Tools, Figma, Premiere and Lightroom.
//!
//! Every extension and MIME type below was taken from the app's own packaging
//! (`packaging/linux/ai.storyteller.<app>.{desktop,mime.xml}`), and every MCP invocation from
//! the app's `<app>-cli` usage text. Ferrite uses this to:
//!
//! * recognise Craft documents in the file tree (icons, "Open in <App>", MIME types),
//! * lock files while a desktop app edits them and keep versions of every save,
//! * drive the apps headlessly through their built-in MCP servers so the assistant (and any
//!   MCP client connected to Ferrite) can work inside Craft documents stored on the server.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CraftApp {
    pub id: &'static str,
    pub name: &'static str,
    pub replaces: &'static str,
    pub summary: &'static str,
    /// The app's own document formats: (extension, MIME type if the app declares one).
    pub native: &'static [(&'static str, Option<&'static str>)],
    /// Other formats the app opens (interchange/import).
    pub opens: &'static [&'static str],
    /// Of `native` + `opens`, the extensions this app is the default handler for in Ferrite.
    pub primary: &'static [&'static str],
    /// Headless CLI binary.
    pub cli: &'static str,
    /// How the CLI starts a stdio MCP server confined to a work folder.
    pub mcp: McpLaunch,
    pub repo: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpLaunch {
    /// `<cli> mcp --automation-read-root DIR --automation-write-root DIR`
    AutomationRoots,
    /// `<cli> mcp --in FILE`
    InFile,
    /// `<cli> mcp DIR`
    PositionalFolder,
    /// `<cli> mcp` (open documents through the app's own tools)
    Plain,
}

impl CraftApp {
    pub fn handles(&self, ext: &str) -> bool {
        self.native.iter().any(|(e, _)| *e == ext) || self.opens.contains(&ext)
    }

    pub fn mime_for_name(&self, name: &str) -> Option<&'static str> {
        let ext = extension(name)?;
        self.native.iter().find(|(e, _)| *e == ext).and_then(|(_, m)| *m)
    }

    /// Arguments that start the app's MCP server for a document in `dir` (named `file`).
    pub fn mcp_args(&self, dir: &str, file: &str) -> Vec<String> {
        let mut args = vec!["mcp".to_string()];
        match self.mcp {
            McpLaunch::AutomationRoots => {
                args.extend(["--automation-read-root".into(), dir.into(), "--automation-write-root".into(), dir.into()]);
            }
            McpLaunch::InFile => args.extend(["--in".into(), format!("{dir}/{file}")]),
            McpLaunch::PositionalFolder => args.push(dir.into()),
            McpLaunch::Plain => {}
        }
        args
    }
}

fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

pub static APPS: &[CraftApp] = &[
    CraftApp {
        id: "wordcraft",
        name: "WordCraft",
        replaces: "Microsoft Word",
        summary: "Write and design documents; open and save Word files",
        native: &[("docx", Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"))],
        opens: &["docm", "dotx", "dotm", "doc", "odt", "rtf", "md", "tex", "html", "htm", "txt"],
        primary: &["docx", "docm", "dotx", "dotm", "doc", "odt", "rtf", "tex"],
        cli: "wordcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/wordcraft",
    },
    CraftApp {
        id: "gridcraft",
        name: "GridCraft",
        replaces: "Microsoft Excel",
        summary: "Calculate, analyse and chart data in workbooks",
        native: &[("xlsx", Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"))],
        opens: &["xlsm", "xlsb", "ods", "csv", "tsv"],
        primary: &["xlsx", "xlsm", "xlsb", "ods", "tsv"],
        cli: "gridcraft-cli",
        mcp: McpLaunch::InFile,
        repo: "https://github.com/storytold/gridcraft",
    },
    CraftApp {
        id: "deckcraft",
        name: "DeckCraft",
        replaces: "Microsoft PowerPoint",
        summary: "Create and present slide decks",
        native: &[("deckcraft", Some("application/x-deckcraft")), ("slidecraft", Some("application/x-deckcraft"))],
        opens: &["pptx", "potx", "ppsx"],
        primary: &["deckcraft", "slidecraft", "pptx", "potx", "ppsx"],
        cli: "deckcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/deckcraft",
    },
    CraftApp {
        id: "vectorcraft",
        name: "VectorCraft",
        replaces: "Adobe Illustrator",
        summary: "Draw and edit vector illustrations, SVG and PDF",
        native: &[
            ("vectorcraft", Some("application/x-vectorcraft")),
            ("drawcraft", Some("application/x-vectorcraft")),
            ("vctemplate", Some("application/x-vectorcraft-template")),
        ],
        opens: &["svg", "svgz", "ai", "ait", "eps", "emf", "wmf", "afdesign", "afpub", "pdf"],
        primary: &["vectorcraft", "drawcraft", "vctemplate", "svg", "svgz", "ai", "ait", "eps", "emf", "wmf", "afdesign", "afpub"],
        cli: "vectorcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/vectorcraft",
    },
    CraftApp {
        id: "photocraft",
        name: "PhotoCraft",
        replaces: "Adobe Photoshop",
        summary: "Edit photos and layered PSD documents",
        native: &[("pcraft", Some("application/x-photocraft"))],
        opens: &["psd", "psb", "pdn", "ora", "kra", "png", "jpg", "jpeg", "tif", "tiff", "webp", "gif", "bmp", "tga", "ico", "pnm", "pbm", "pgm", "ppm", "exr", "hdr", "avif", "qoi"],
        primary: &["pcraft", "psd", "psb", "pdn", "ora", "kra", "png", "jpg", "jpeg", "tif", "tiff", "webp", "gif", "bmp", "tga", "ico", "pnm", "pbm", "pgm", "ppm", "exr", "hdr", "avif", "qoi"],
        cli: "photocraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/photocraft",
    },
    CraftApp {
        id: "pdfcraft",
        name: "PdfCraft",
        replaces: "Adobe Acrobat",
        summary: "Read, organize, combine, split and secure PDFs",
        native: &[("pdf", Some("application/pdf"))],
        opens: &[],
        primary: &["pdf"],
        cli: "pdfcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/pdfcraft",
    },
    CraftApp {
        id: "designcraft",
        name: "DesignCraft",
        replaces: "Adobe InDesign",
        summary: "Lay out magazines, books and print documents",
        native: &[("designcraft", Some("application/x-designcraft"))],
        opens: &["idml"],
        primary: &["designcraft", "idml"],
        cli: "designcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/designcraft",
    },
    CraftApp {
        id: "effectcraft",
        name: "EffectCraft",
        replaces: "Adobe After Effects",
        summary: "Motion graphics and visual effects compositor",
        // The app declares no MIME type for its project files.
        native: &[("ecproj", None)],
        opens: &["lottie"],
        primary: &["ecproj", "lottie"],
        cli: "effectcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/effectcraft",
    },
    CraftApp {
        id: "cadcraft",
        name: "CADCraft",
        replaces: "AutoCAD",
        summary: "Draft, dimension and plot 2D drawings; open DXF and DWG files",
        native: &[("dxf", Some("image/vnd.dxf")), ("dwg", Some("image/vnd.dwg"))],
        opens: &[],
        primary: &["dxf", "dwg"],
        cli: "cadcraft-cli",
        mcp: McpLaunch::Plain,
        repo: "https://github.com/storytold/cadcraft",
    },
    CraftApp {
        id: "soundcraft",
        name: "SoundCraft",
        replaces: "Avid Pro Tools",
        summary: "Record, edit and mix audio and MIDI",
        native: &[("scraft", Some("application/x-soundcraft-session"))],
        opens: &["wav", "aif", "aiff", "flac", "mp3", "mid", "midi"],
        primary: &["scraft", "wav", "aif", "aiff", "flac", "mp3", "mid", "midi"],
        cli: "soundcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/soundcraft",
    },
    CraftApp {
        id: "figcraft",
        name: "FigCraft",
        replaces: "Figma",
        summary: "Design and prototype user interfaces",
        native: &[("figcraft", Some("application/x-figcraft"))],
        opens: &["fig", "svg", "png", "jpg", "jpeg"],
        primary: &["figcraft", "fig"],
        cli: "figcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/figcraft",
    },
    CraftApp {
        id: "filmcraft",
        name: "FilmCraft",
        replaces: "Adobe Premiere Pro",
        summary: "Edit video, color and sound",
        native: &[("fcproj", Some("application/x-filmcraft"))],
        opens: &["mp4", "m4v", "mov", "mkv", "webm", "avi", "mxf", "ts", "mpg", "mpeg", "3gp", "edl", "fcpxml", "otio", "aaf", "omf"],
        primary: &["fcproj", "mp4", "m4v", "mov", "mkv", "webm", "avi", "mxf", "mpg", "mpeg", "3gp", "edl", "fcpxml", "otio", "aaf", "omf"],
        cli: "filmcraft-cli",
        mcp: McpLaunch::AutomationRoots,
        repo: "https://github.com/storytold/filmcraft",
    },
    CraftApp {
        id: "lightcraft",
        name: "LightCraft",
        replaces: "Adobe Lightroom",
        summary: "Organise photos and develop raw files, non-destructively",
        native: &[("lcpreset", None)],
        opens: &["dng", "arw", "cr2", "cr3", "nef", "raf", "orf", "rw2", "rw", "pef", "heic", "heif", "jpg", "jpeg", "png", "tif", "tiff", "webp"],
        primary: &["lcpreset", "dng", "arw", "cr2", "cr3", "nef", "raf", "orf", "rw2", "rw", "pef", "heic", "heif"],
        cli: "lightcraft-cli",
        mcp: McpLaunch::PositionalFolder,
        repo: "https://github.com/storytold/lightcraft",
    },
];

pub fn app(id: &str) -> Option<&'static CraftApp> {
    APPS.iter().find(|a| a.id == id)
}

/// The default Craft app for a file name, if any.
pub fn app_for_name(name: &str) -> Option<&'static CraftApp> {
    let ext = extension(name)?;
    APPS.iter().find(|a| a.primary.contains(&ext.as_str()))
}

/// Every Craft app able to open a file name (for "Open with…").
pub fn apps_for_name(name: &str) -> Vec<&'static CraftApp> {
    match extension(name) {
        Some(ext) => APPS.iter().filter(|a| a.handles(&ext)).collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_primary_extension_has_one_owner() {
        let mut seen = std::collections::HashMap::new();
        for a in APPS {
            for e in a.primary {
                assert!(a.handles(e), "{} lists {e} as primary but cannot open it", a.id);
                if let Some(other) = seen.insert(*e, a.id) {
                    panic!("{e} claimed by both {other} and {}", a.id);
                }
            }
        }
        assert_eq!(APPS.len(), 13);
    }

    #[test]
    fn lookups() {
        assert_eq!(app_for_name("Report.DOCX").unwrap().id, "wordcraft");
        assert_eq!(app_for_name("logo.svg").unwrap().id, "vectorcraft");
        assert_eq!(app_for_name("cut.fcproj").unwrap().id, "filmcraft");
        assert_eq!(app_for_name("deck.deckcraft").unwrap().mime_for_name("deck.deckcraft"), Some("application/x-deckcraft"));
        assert!(app_for_name("notes.md").is_none());
        let svg: Vec<_> = apps_for_name("a.svg").iter().map(|a| a.id).collect();
        assert_eq!(svg, vec!["vectorcraft", "figcraft"]);
        assert_eq!(app("gridcraft").unwrap().mcp_args("/w", "a.xlsx"), vec!["mcp", "--in", "/w/a.xlsx"]);
    }
}
