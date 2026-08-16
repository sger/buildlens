#[derive(Debug, Default, Clone)]
pub struct IdeActivityLog {
    pub version: u64,
    pub main_section: Option<IdeSection>,
}

#[derive(Debug, Default, Clone)]
pub struct IdeSection {
    /// Kept for diagnostics and future subclass-specific mapping.
    #[allow(dead_code)]
    pub class_name: String,
    pub section_type: u64,
    pub domain_type: String,
    pub title: String,
    pub signature: String,
    /// CFAbsoluteTime (seconds since 2001-01-01).
    pub time_started: Option<f64>,
    pub time_stopped: Option<f64>,
    pub sub_sections: Vec<IdeSection>,
    pub text: Option<String>,
    pub messages: Vec<IdeMessage>,
    pub was_cancelled: bool,
    pub is_quiet: bool,
    pub was_fetched_from_cache: bool,
    pub subtitle: String,
    pub location: Option<DvtLocation>,
    pub command_detail_desc: Option<String>,
    pub unique_identifier: String,
    pub localized_result_string: String,
    pub xcbuild_signature: String,
}

#[derive(Debug, Default, Clone)]
pub struct IdeMessage {
    /// Kept for diagnostics and future subclass-specific mapping.
    #[allow(dead_code)]
    pub class_name: String,
    pub title: String,
    pub short_title: String,
    pub time_emitted: u64,
    pub sub_messages: Vec<IdeMessage>,
    pub severity: u64,
    pub message_type: String,
    pub location: Option<DvtLocation>,
    pub category_ident: String,
}

#[derive(Debug, Default, Clone)]
pub struct DvtLocation {
    pub document_url: String,
    /// Kept for diagnostics and future subclass-specific mapping.
    #[allow(dead_code)]
    pub timestamp: Option<f64>,
    pub starting_line: Option<u64>,
    pub starting_column: Option<u64>,
    pub ending_line: Option<u64>,
    pub ending_column: Option<u64>,
}
