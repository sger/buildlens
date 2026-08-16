use super::lexer::{Lexer, Token};
use super::model::{DvtLocation, IdeActivityLog, IdeMessage, IdeSection};

const SECTION_CLASSES: &[&str] = &[
    "IDEActivityLogSection",
    "IDEActivityLogCommandInvocationSection",
    "IDEActivityLogMajorGroupSection",
    "IDECommandLineBuildLog",
];
const UNIT_TEST_SECTION: &str = "IDEActivityLogUnitTestSection";
const MESSAGE_CLASSES: &[&str] = &[
    "IDEActivityLogMessage",
    "IDEClangDiagnosticActivityLogMessage",
    "IDEDiagnosticActivityLogMessage",
];
const ACTION_MESSAGE: &str = "IDEActivityLogActionMessage";
const ANALYZER_RESULT_MESSAGE: &str = "IDEActivityLogAnalyzerResultMessage";

/// Attachments only exist in log format version 11 and later.
const ATTACHMENTS_MIN_VERSION: u64 = 11;
/// Newest format version this parser has been validated against.
const MAX_KNOWN_VERSION: u64 = 12;

pub struct ParseOptions {
    /// Keep large section `text` bodies and `commandDetailDesc` strings.
    pub keep_text: bool,
}

pub fn parse(bytes: &[u8], options: &ParseOptions) -> (IdeActivityLog, Vec<String>) {
    let mut log = IdeActivityLog::default();
    let mut lexer = match Lexer::new(bytes) {
        Ok(lexer) => lexer,
        Err(error) => return (log, vec![error.to_string()]),
    };
    let mut parser = Parser {
        lexer: &mut lexer,
        classes: Vec::new(),
        warnings: Vec::new(),
        fatal: false,
        keep_text: options.keep_text,
        version: 0,
        is_command_line_log: false,
        peeked: None,
    };
    log.version = parser.read_int().unwrap_or(0);
    if log.version > MAX_KNOWN_VERSION {
        parser.warnings.push(format!(
            "activity log format version {} is newer than the last validated version {MAX_KNOWN_VERSION}; parsing may be incomplete",
            log.version
        ));
    }
    parser.version = log.version;
    log.main_section = parser.read_section();
    let warnings = parser.warnings;
    (log, warnings)
}

struct Parser<'a, 'b> {
    lexer: &'b mut Lexer<'a>,
    classes: Vec<String>,
    warnings: Vec<String>,
    /// Once set, every read returns defaults so the tree parsed so far unwinds intact.
    fatal: bool,
    keep_text: bool,
    version: u64,
    is_command_line_log: bool,
    peeked: Option<Token<'a>>,
}

impl<'a> Parser<'a, '_> {
    fn fail(&mut self, reason: String) {
        if !self.fatal {
            self.fatal = true;
            self.warnings.push(format!(
                "{reason} at byte {}; keeping partial result",
                self.lexer.position()
            ));
        }
    }

    fn next(&mut self) -> Option<Token<'a>> {
        if self.fatal {
            return None;
        }
        if let Some(token) = self.peeked.take() {
            return Some(token);
        }
        match self.lexer.next_token() {
            Ok(Some(token)) => Some(token),
            Ok(None) => {
                self.fail("unexpected end of stream".into());
                None
            }
            Err(error) => {
                self.fail(error.to_string());
                None
            }
        }
    }

    /// Looks ahead without treating end-of-stream as a failure.
    fn peek(&mut self) -> Option<&Token<'a>> {
        if self.fatal {
            return None;
        }
        if self.peeked.is_none() {
            self.peeked = match self.lexer.next_token() {
                Ok(token) => token,
                Err(error) => {
                    self.fail(error.to_string());
                    None
                }
            };
        }
        self.peeked.as_ref()
    }

    fn read_int(&mut self) -> Option<u64> {
        match self.next()? {
            Token::Int(value) => Some(value),
            Token::Null => Some(0),
            other => {
                self.fail(format!("expected int, found {other:?}"));
                None
            }
        }
    }

    fn read_bool(&mut self) -> bool {
        self.read_int().is_some_and(|value| value != 0)
    }

    fn read_double(&mut self) -> Option<f64> {
        match self.next()? {
            Token::Double(value) => value,
            Token::Null => None,
            other => {
                self.fail(format!("expected double, found {other:?}"));
                None
            }
        }
    }

    /// Some sections carry an extra int before a string field. The int is only
    /// present when the very next token is one — a null or string token *is*
    /// the field. Peeking alone is ambiguous, because a null string may be
    /// followed by an int-encoded boolean, so consume and dispatch instead.
    fn read_string_after_optional_int(&mut self) -> Option<String> {
        match self.next()? {
            Token::Int(_) => self.read_string(),
            Token::String(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            Token::Null => None,
            other => {
                self.fail(format!("expected string, found {other:?}"));
                None
            }
        }
    }

    fn read_string(&mut self) -> Option<String> {
        match self.next()? {
            Token::String(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            Token::Null => None,
            other => {
                self.fail(format!("expected string, found {other:?}"));
                None
            }
        }
    }

    /// Consumes class registrations, then resolves the next instance's class name.
    /// Returns None for a Null token (no instance).
    fn read_instance_class(&mut self) -> Option<String> {
        loop {
            match self.next()? {
                Token::ClassName(name) => self.classes.push(name.to_owned()),
                Token::ClassRef(index) => {
                    let position = (index as usize).checked_sub(1)?;
                    match self.classes.get(position) {
                        Some(name) => return Some(name.clone()),
                        None => {
                            self.fail(format!("class reference {index} has no registration"));
                            return None;
                        }
                    }
                }
                Token::Null => return None,
                other => {
                    self.fail(format!("expected class instance, found {other:?}"));
                    return None;
                }
            }
        }
    }

    fn read_section(&mut self) -> Option<IdeSection> {
        let class_name = self.read_instance_class()?;
        if class_name == "DBGConsoleLog" {
            self.fail("DBGConsoleLog sections are not supported".into());
            return None;
        }
        let is_unit_test = class_name == UNIT_TEST_SECTION;
        if !is_unit_test && !SECTION_CLASSES.contains(&class_name.as_str()) {
            self.fail(format!("unknown SLF section class '{class_name}'"));
            return None;
        }
        let is_command_line_root = class_name == "IDECommandLineBuildLog";
        if is_command_line_root {
            self.is_command_line_log = true;
        }
        let mut section = IdeSection {
            class_name,
            ..Default::default()
        };
        section.section_type = self.read_int().unwrap_or(0);
        section.domain_type = self.read_string().unwrap_or_default();
        section.title = self.read_string().unwrap_or_default();
        section.signature = self.read_string().unwrap_or_default();
        section.time_started = self.read_double();
        section.time_stopped = self.read_double();
        section.sub_sections = self.read_section_list();
        let text = self.read_string_after_optional_int();
        section.text = if self.keep_text { text } else { None };
        section.messages = self.read_message_list();
        section.was_cancelled = self.read_bool();
        section.is_quiet = self.read_bool();
        section.was_fetched_from_cache = self.read_bool();
        section.subtitle = self.read_string_after_optional_int().unwrap_or_default();
        section.location = self.read_location();
        let command_detail = self.read_string();
        section.command_detail_desc = if self.keep_text { command_detail } else { None };
        section.unique_identifier = self.read_string().unwrap_or_default();
        section.localized_result_string = self.read_string().unwrap_or_default();
        section.xcbuild_signature = self.read_string().unwrap_or_default();
        if self.version >= ATTACHMENTS_MIN_VERSION {
            self.skip_attachment_list();
        }
        if is_unit_test {
            for _ in 0..6 {
                self.read_string();
            }
        }
        // Only the outermost IDECommandLineBuildLog carries a trailing int, and
        // only in some Xcode versions — reaching the end of the stream here is
        // a complete parse, not a failure.
        if is_command_line_root && self.peek().is_some() {
            self.read_int();
        }
        // A fatal error mid-section still yields the fields parsed so far,
        // so ancestors keep their partial tree.
        Some(section)
    }

    fn read_section_list(&mut self) -> Vec<IdeSection> {
        let count = match self.next() {
            Some(Token::List(count)) => count,
            Some(Token::Null) | None => return Vec::new(),
            Some(other) => {
                self.fail(format!("expected section list, found {other:?}"));
                return Vec::new();
            }
        };
        let mut sections = Vec::new();
        for _ in 0..count {
            if self.fatal {
                break;
            }
            match self.read_section() {
                Some(section) => sections.push(section),
                None => break,
            }
        }
        sections
    }

    fn read_message_list(&mut self) -> Vec<IdeMessage> {
        let count = match self.next() {
            Some(Token::List(count)) => count,
            Some(Token::Null) | None => return Vec::new(),
            Some(other) => {
                self.fail(format!("expected message list, found {other:?}"));
                return Vec::new();
            }
        };
        let mut messages = Vec::new();
        for _ in 0..count {
            if self.fatal {
                break;
            }
            match self.read_message() {
                Some(message) => messages.push(message),
                None => break,
            }
        }
        messages
    }

    fn read_message(&mut self) -> Option<IdeMessage> {
        let class_name = self.read_instance_class()?;
        let is_action = class_name == ACTION_MESSAGE;
        let is_analyzer_result = class_name == ANALYZER_RESULT_MESSAGE;
        if !is_action && !is_analyzer_result && !MESSAGE_CLASSES.contains(&class_name.as_str()) {
            self.fail(format!("unknown SLF message class '{class_name}'"));
            return None;
        }
        let mut message = IdeMessage {
            class_name,
            ..Default::default()
        };
        message.title = self.read_string().unwrap_or_default();
        message.short_title = self.read_string().unwrap_or_default();
        message.time_emitted = self.read_int().unwrap_or(0);
        self.read_int(); // rangeEndInSectionText
        self.read_int(); // rangeStartInSectionText
        message.sub_messages = self.read_message_list();
        message.severity = self.read_int().unwrap_or(0);
        message.message_type = self.read_string().unwrap_or_default();
        message.location = self.read_location();
        message.category_ident = self.read_string().unwrap_or_default();
        self.skip_location_list(); // secondaryLocations
        self.read_string(); // additionalDescription
        if is_action {
            self.read_string(); // action
        }
        if is_analyzer_result {
            self.read_string(); // resultType
            self.read_int(); // keyEventIndex
        }
        if self.fatal { None } else { Some(message) }
    }

    fn read_location(&mut self) -> Option<DvtLocation> {
        let class_name = self.read_instance_class()?;
        let is_text = class_name == "DVTTextDocumentLocation";
        if !is_text && class_name != "DVTDocumentLocation" {
            self.fail(format!("unknown SLF location class '{class_name}'"));
            return None;
        }
        let mut location = DvtLocation {
            document_url: self.read_string().unwrap_or_default(),
            timestamp: self.read_double(),
            ..Default::default()
        };
        if is_text {
            location.starting_line = self.read_int();
            location.starting_column = self.read_int();
            location.ending_line = self.read_int();
            location.ending_column = self.read_int();
            self.read_int(); // characterRangeEnd
            self.read_int(); // characterRangeStart
            self.read_int(); // locationEncoding
        }
        if self.fatal { None } else { Some(location) }
    }

    fn skip_location_list(&mut self) {
        let count = match self.next() {
            Some(Token::List(count)) => count,
            Some(Token::Null) | None => return,
            Some(other) => {
                self.fail(format!("expected location list, found {other:?}"));
                return;
            }
        };
        for _ in 0..count {
            if self.fatal || self.read_location().is_none() {
                break;
            }
        }
    }

    fn skip_attachment_list(&mut self) {
        let count = match self.next() {
            Some(Token::List(count)) => count,
            Some(Token::Null) | None => return,
            Some(other) => {
                self.fail(format!("expected attachment list, found {other:?}"));
                return;
            }
        };
        for _ in 0..count {
            if self.fatal {
                break;
            }
            let Some(class_name) = self.read_instance_class() else {
                break;
            };
            if !class_name.ends_with("IDEActivityLogSectionAttachment") {
                self.fail(format!("unknown SLF attachment class '{class_name}'"));
                break;
            }
            self.read_string(); // identifier
            self.read_int(); // majorVersion
            self.read_int(); // minorVersion
            match self.next() {
                Some(Token::Json(_)) | Some(Token::Null) | None => {}
                Some(other) => {
                    self.fail(format!("expected attachment payload, found {other:?}"));
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn double(value: f64) -> String {
        format!("{:x}^", value.to_bits().swap_bytes())
    }

    fn string(value: &str) -> String {
        format!("{}\"{}", value.len(), value)
    }

    /// A minimal IDEActivityLogSection instance body (fields after the class ref),
    /// with the given title, cache flag, and pre-rendered subsection bodies.
    fn section_body(title: &str, cached: bool, children: &[String]) -> String {
        let subsections = if children.is_empty() {
            "-".to_string()
        } else {
            // Children reuse class registration 1.
            format!(
                "{}({}",
                children.len(),
                children
                    .iter()
                    .map(|child| format!("1@{child}"))
                    .collect::<String>()
            )
        };
        [
            "0#".to_string(),                               // sectionType
            string("domain"),                               // domainType
            string(title),                                  // title
            string(title),                                  // signature
            double(1.0),                                    // timeStartedRecording
            double(3.5),                                    // timeStoppedRecording
            subsections,                                    // subSections
            "-".to_string(),                                // text
            "-".to_string(),                                // messages
            "0#".to_string(),                               // wasCancelled
            "0#".to_string(),                               // isQuiet
            (if cached { "1#" } else { "0#" }).to_string(), // wasFetchedFromCache
            "-".to_string(),                                // subtitle
            "-".to_string(),                                // location
            "-".to_string(),                                // commandDetailDesc
            string("UID"),                                  // uniqueIdentifier
            "-".to_string(),                                // localizedResultString
            "-".to_string(),                                // xcbuildSignature
            "-".to_string(),                                // attachments (version 12)
        ]
        .concat()
    }

    fn log_with(children: &[String]) -> String {
        format!(
            "SLF012#{}1@{}",
            "21%IDEActivityLogSection",
            section_body("Build App", false, children)
        )
    }

    #[test]
    fn parses_minimal_section_tree() {
        let step = section_body("Build target App", false, &[]);
        let input = log_with(&[step]);
        let (log, warnings) = parse(input.as_bytes(), &ParseOptions { keep_text: false });
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(log.version, 12);
        let main = log.main_section.expect("main section");
        assert_eq!(main.title, "Build App");
        assert_eq!(main.unique_identifier, "UID");
        assert_eq!(main.time_started, Some(1.0));
        assert_eq!(main.time_stopped, Some(3.5));
        assert_eq!(main.sub_sections.len(), 1);
        assert_eq!(main.sub_sections[0].title, "Build target App");
    }

    #[test]
    fn cache_flag_round_trips() {
        let step = section_body("CompileSwift", true, &[]);
        let input = log_with(&[step]);
        let (log, _) = parse(input.as_bytes(), &ParseOptions { keep_text: false });
        assert!(log.main_section.unwrap().sub_sections[0].was_fetched_from_cache);
    }

    #[test]
    fn unknown_class_becomes_warning_not_panic() {
        let input = "SLF012#12%MysteryClass1@";
        let (log, warnings) = parse(input.as_bytes(), &ParseOptions { keep_text: false });
        assert!(log.main_section.is_none());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("MysteryClass"))
        );
    }

    #[test]
    fn corrupt_tail_keeps_partial_tree() {
        let good = section_body("Build target Kept", false, &[]);
        let full = log_with(&[good.clone(), good]);
        let truncated = &full[..full.len() - 30];
        let (log, warnings) = parse(truncated.as_bytes(), &ParseOptions { keep_text: false });
        assert!(!warnings.is_empty());
        let main = log.main_section.expect("partial main section survives");
        assert_eq!(main.sub_sections[0].title, "Build target Kept");
    }
}
