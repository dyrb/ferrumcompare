#[derive(Clone)]
pub struct Settings {
    pub theme: u8,
    pub language: u8,
    pub contents: bool,
    pub ignore_crlf: bool,
    pub text: fc_core::text::TextRules,
    pub backend: u8,
    pub case: u8,
    pub split: f32,
    pub include_masks: String,
    pub exclude_masks: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: 0,
            language: 0,
            contents: false,
            ignore_crlf: false,
            text: Default::default(),
            backend: 0,
            case: 0,
            split: 0.5,
            include_masks: String::new(),
            exclude_masks: String::new(),
        }
    }
}
fn path() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("FerrumCompare.ini"))
}
impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(path) = path() else { return s };
        let Ok(text) = std::fs::read_to_string(path) else { return s };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            match key {
                "theme" => s.theme = value.parse::<u8>().unwrap_or(0).min(2),
                "language" => s.language = value.parse::<u8>().unwrap_or(0).min(2),
                "contents" => s.contents = value == "1",
                "text_ignore_crlf" => s.ignore_crlf = value == "1",
                "text_ignore_spaces" => s.text.ignore_spaces = value == "1",
                "text_ignore_tabs" => s.text.ignore_tabs = value == "1",
                "text_ignore_blank_lines" => s.text.ignore_blank_lines = value == "1",
                "text_ignore_case" => s.text.ignore_case = value == "1",
                "text_ignore_endings" => s.text.ignore_line_endings = value == "1",
                "include_masks" => s.include_masks = value.into(),
                "exclude_masks" => s.exclude_masks = value.into(),
                "backend" => s.backend = value.parse::<u8>().unwrap_or(0).min(2),
                "case" => s.case = value.parse::<u8>().unwrap_or(0).min(2),
                "split" => {
                    if let Ok(v) = value.parse::<f32>()
                        && v.is_finite()
                    {
                        s.split = v.clamp(0.25, 0.75);
                    }
                }
                _ => {}
            }
        }
        s
    }
    pub fn save(&self) {
        let Some(path) = path() else { return };
        let mut text = format!(
            "theme={}\nlanguage={}\ncontents={}\ntext_ignore_crlf={}\ntext_ignore_spaces={}\ntext_ignore_tabs={}\ntext_ignore_blank_lines={}\ntext_ignore_case={}\ntext_ignore_endings={}\nbackend={}\ncase={}\nsplit={:.4}\n",
            self.theme,
            self.language,
            self.contents as u8,
            self.ignore_crlf as u8,
            self.text.ignore_spaces as u8,
            self.text.ignore_tabs as u8,
            self.text.ignore_blank_lines as u8,
            self.text.ignore_case as u8,
            self.text.ignore_line_endings as u8,
            self.backend,
            self.case,
            self.split
        );
        text.push_str(&format!("include_masks={}\nexclude_masks={}\n", self.include_masks, self.exclude_masks));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(&text) {
            let _ = std::fs::write(path, text);
        }
    }
}
