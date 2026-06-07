/*!
 Contains data structures used to describe export types.
*/

use std::fmt::Display;

/// Represents the type of file to export iMessage data into
#[derive(PartialEq, Eq, Debug)]
pub enum ExportType {
    /// HTML file export
    Html,
    /// Text file export
    Txt,
    /// Unified Markdown timeline: a single `.md` file that groups every message
    /// across all threads by day, then by conversation thread within that day
    Timeline,
}

impl ExportType {
    /// Given user's input, return a variant if the input matches one
    pub fn from_cli(format: &str) -> Option<Self> {
        match format.to_lowercase().as_str() {
            "txt" => Some(Self::Txt),
            "html" => Some(Self::Html),
            "timeline" | "md" | "markdown" => Some(Self::Timeline),
            _ => None,
        }
    }

    /// Get the file name extension for the given export type
    pub fn extension(&self) -> &str {
        match self {
            ExportType::Html => ".html",
            ExportType::Txt => ".txt",
            ExportType::Timeline => ".md",
        }
    }
}

impl Display for ExportType {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportType::Txt => write!(fmt, "txt"),
            ExportType::Html => write!(fmt, "html"),
            ExportType::Timeline => write!(fmt, "timeline"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::export_type::ExportType;

    #[test]
    fn can_parse_html_any_case() {
        assert!(matches!(
            ExportType::from_cli("html"),
            Some(ExportType::Html)
        ));
        assert!(matches!(
            ExportType::from_cli("HTML"),
            Some(ExportType::Html)
        ));
        assert!(matches!(
            ExportType::from_cli("HtMl"),
            Some(ExportType::Html)
        ));
    }

    #[test]
    fn can_parse_txt_any_case() {
        assert!(matches!(ExportType::from_cli("txt"), Some(ExportType::Txt)));
        assert!(matches!(ExportType::from_cli("TXT"), Some(ExportType::Txt)));
        assert!(matches!(ExportType::from_cli("tXt"), Some(ExportType::Txt)));
    }

    #[test]
    fn cant_parse_invalid() {
        assert!(ExportType::from_cli("pdf").is_none());
        assert!(ExportType::from_cli("json").is_none());
        assert!(ExportType::from_cli("").is_none());
    }

    #[test]
    fn can_parse_timeline_any_case() {
        assert!(matches!(
            ExportType::from_cli("timeline"),
            Some(ExportType::Timeline)
        ));
        assert!(matches!(
            ExportType::from_cli("TIMELINE"),
            Some(ExportType::Timeline)
        ));
        assert!(matches!(
            ExportType::from_cli("md"),
            Some(ExportType::Timeline)
        ));
        assert!(matches!(
            ExportType::from_cli("MARKDOWN"),
            Some(ExportType::Timeline)
        ));
    }

    #[test]
    fn timeline_extension_is_md() {
        assert_eq!(ExportType::Timeline.extension(), ".md");
    }
}
