use fc_core::State;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

static LANGUAGE: AtomicU8 = AtomicU8::new(0);
pub fn set(choice: u8) {
    LANGUAGE.store(choice, Relaxed);
}
pub fn ru() -> bool {
    match LANGUAGE.load(Relaxed) {
        1 => false,
        2 => true,
        _ => unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() & 0x3ff == 0x19 },
    }
}
macro_rules! strings {
    ($($id:ident = $en:expr, $ru:expr;)*) => {
        #[derive(Clone, Copy)] pub enum S { $($id,)* }
        pub fn t(s: S) -> &'static str { match s { $(S::$id => if ru() { $ru } else { $en },)* } }
    };
}
strings! {
    File = "File", "Файл";
    View = "View", "Вид";
    Help = "Help", "Справка";
    Left = "Left:", "Слева:";
    Right = "Right:", "Справа:";
    Compare = "Compare", "Сравнить";
    Contents = "Check file contents", "Проверять содержимое";
    Stop = "Stop", "Остановить";
    Swap = "Swap sides", "Поменять местами";
    Search = "Filter by name or relative path", "Фильтр по имени или относительному пути";
    FolderFilters = "Analysis filters...", "Фильтры анализа...";
    IncludeMasks = "Include files (for example, *.cpp;*.hpp; empty = all)", "Включать файлы (например, *.cpp;*.hpp; пусто = все)";
    ExcludeMasks = "Exclude names (for example, .git;.vs;target;build)", "Исключать имена (например, .git;.vs;target;build)";
    MasksHelp = "Excluded directories are not traversed. The search field only hides results; it accepts masks and !exclusions.", "Исключённые папки не обходятся. Поле поиска только скрывает результаты; поддерживает маски и !исключения.";
    Apply = "Apply", "Применить";
    Cancel = "Cancel", "Отмена";
    AnalysisLimited = "Analysis filters active", "Область анализа ограничена масками";
    All = "All results", "Все результаты";
    Changes = "Differences", "Различия";
    Equal = "Verified matches", "Проверенные совпадения";
    Uncertain = "Uncertain results", "Неопределённые результаты";
    Auto = "Auto", "Автоматически";
    Walk = "Directory enumeration", "Обход каталогов";
    Mft = "NTFS MFT", "NTFS MFT";
    Name = "Name", "Имя";
    State = "Result", "Результат";
    Size = "Size", "Размер";
    Modified = "Modified", "Изменён";
    Ready = "Choose two folders, or drop files or folders onto the left and right sides", "Выберите две папки или перетащите файлы либо папки на левую и правую стороны";
    DropLeft = "Open on the left", "Открыть слева";
    DropRight = "Open on the right", "Открыть справа";
    DropPair = "Compare the two items", "Сравнить два объекта";
    DropData = "Drop files or folders from File Explorer", "Перетащите файлы или папки из Проводника";
    DropCount = "Drop one item per side, or two items together", "Перетащите по одному объекту на сторону или сразу два объекта";
    DropBusy = "Wait for the operation to finish, or stop it first", "Дождитесь завершения операции или сначала остановите её";
    DropMove = "This source only allows moving; choose a copy drag instead", "Источник разрешает только перемещение; используйте перетаскивание с копированием";
    DropKinds = "Choose two files or two folders.", "Выберите два файла или две папки.";
    DropOther = "Drop an item of the same type on the other side to compare", "Для сравнения перетащите объект того же типа на другую сторону";
    DropArchive = "Archive contents are not supported yet. Extract the archives and compare the folders.", "Сравнение содержимого архивов пока не поддерживается. Распакуйте архивы и сравните папки.";
    Reading = "Reading folders", "Чтение папок";
    Matching = "Matching paths", "Сопоставление путей";
    Checking = "Checking contents", "Проверка содержимого";
    Stopping = "Stopping", "Остановка";
    Cancelled = "Comparison cancelled", "Сравнение отменено";
    Completed = "Completed in", "Выполнено за";
    Files = "Files", "Файлов";
    Dirs = "Folders", "Папок";
    Differences = "Differences", "Различий";
    Unverified = "Size/date", "Размер/дата";
    Verified = "Bytes equal", "Байты совпали";
    Unknown = "Uncertain", "Не определено";
    Visible = "Rows", "Строк";
    Theme = "Theme", "Тема";
    Language = "Language", "Язык";
    System = "System", "Системная";
    Light = "Light", "Светлая";
    Dark = "Dark", "Тёмная";
    Case = "Name matching", "Сопоставление имён";
    Sensitive = "Case sensitive", "С учётом регистра";
    Insensitive = "Ignore case", "Без учёта регистра";
    Expand = "Expand all", "Развернуть всё";
    Collapse = "Collapse all", "Свернуть всё";
    Export = "Export report...", "Экспортировать отчёт...";
    Exit = "Exit", "Выход";
    About = "About FerrumCompare", "О FerrumCompare";
    OpenFiles = "Compare files...", "Сравнить файлы...";
    ChooseFile = "Choose a text file", "Выберите текстовый файл";
    AllFiles = "All files", "Все файлы";
    PreviousChange = "Previous", "Предыдущее";
    NextChange = "Next", "Следующее";
    Syntax = "Syntax", "Синтаксис";
    FindText = "Find text (case sensitive)", "Найти текст (с учётом регистра)";
    FindNext = "Find next", "Найти далее";
    IndexingText = "Indexing text", "Чтение текста";
    AligningText = "Aligning lines", "Сопоставление строк";
    LoadingText = "Reading visible lines", "Чтение видимых строк";
    TextTitle = "File comparison", "Сравнение файлов";
    CoarseText = "Alignment limit reached; some regions are shown as a whole", "Достигнут лимит выравнивания; часть участков показана целиком";
    CoarseInline = "Some inline highlights are simplified (work limit)", "Подсветка части строк упрощена (лимит обработки)";
    FormatText = "Encoding or line endings differ", "Различаются кодировка или переводы строк";
    EndingsText = "Line endings differ", "Отличаются окончания строк";
    TextOptions = "Comparison", "Сравнение";
    FoldUnchanged = "Fold unchanged regions", "Свернуть одинаковые участки";
    FoldedLines = "unchanged lines (click to expand)", "одинаковых строк (нажмите, чтобы раскрыть)";
    FoldLimit = "Folding disabled: navigation index limit", "Сворачивание отключено: достигнут лимит индекса";
    GoLine = "Go to line... (Ctrl+G)", "Перейти к строке... (Ctrl+G)";
    LineNumber = "Line number", "Номер строки";
    GoLineHelp = "The line number belongs to the active pane. Click a pane to select it.", "Номер строки в активной панели. Нажмите на нужную панель перед переходом.";
    InvalidLine = "Enter an existing line number.", "Введите существующий номер строки.";
    CopyText = "Copy selection (Ctrl+C)", "Копировать выделение (Ctrl+C)";
    EncodingLeft = "Left encoding", "Кодировка слева";
    EncodingRight = "Right encoding", "Кодировка справа";
    IgnoreCrlf = "Ignore CRLF/LF differences", "Игнорировать различия CRLF/LF";
    IgnoreSpaces = "Ignore spaces", "Игнорировать пробелы";
    IgnoreTabs = "Ignore tabs", "Игнорировать табуляцию";
    IgnoreBlankLines = "Ignore blank lines", "Игнорировать пустые строки";
    IgnoreTextCase = "Ignore case", "Игнорировать регистр";
    IgnoreLineEndings = "Ignore CR/LF/CRLF styles", "Игнорировать окончания CR/LF/CRLF";
    ExactTextRules = "Exact text comparison", "Точное сравнение текста";
    IgnoredRules = "Ignored", "Игнорируются";
    SpacesRule = "spaces", "пробелы";
    TabsRule = "tabs", "табуляция";
    BlankLinesRule = "blank lines", "пустые строки";
    CaseRule = "case", "регистр";
    EndingsRule = "CR/LF/CRLF styles", "окончания CR/LF/CRLF";
    IgnoringCrlf = "CRLF/LF differences ignored", "Различия CRLF/LF игнорируются";
    SettingsTextMatch = "Matches with comparison settings", "Совпадают с учётом настроек";
    ExactTextMatch = "Text matches", "Текст совпадает";
    Lines = "lines", "строк";
    Seconds = "s", "с";
    NotFound = "Text not found", "Текст не найден";
    ReadOnlyText = "Read-only. F7: next change. Shift+F7: previous. Ctrl+F: find.", "Только чтение. F7: следующее различие. Shift+F7: предыдущее. Ctrl+F: поиск.";
    Choose = "Choose a folder", "Выберите папку";
    Missing = "Choose two folders or two files first.", "Сначала выберите две папки или два файла.";
    Save = "Save report", "Сохранить отчёт";
    Saved = "Report saved", "Отчёт сохранён";
    ExportBusy = "Exporting report", "Экспорт отчёта";
    AboutText = concat!("FerrumCompare ", env!("CARGO_PKG_VERSION"), "\nRead-only folder and text comparison.\nContent checking covers unnamed file data. Reparse points are compared without traversal. ACLs and alternate streams are not compared.\nF5: compare folders. Escape: stop. Double-click a file: compare text.\nText window: F7 / Shift+F7: next / previous change. Ctrl+F: find.\n\nBased on FerrumTree. MIT OR Apache-2.0."),
        concat!("FerrumCompare ", env!("CARGO_PKG_VERSION"), "\nСравнение папок и текста без изменения исходных файлов.\nПроверяется основной поток данных файлов. Точки повторного анализа сравниваются без перехода по ним. Права доступа и альтернативные потоки не сравниваются.\nF5: сравнить папки. Escape: остановить. Двойной щелчок по файлу: сравнить текст.\nВ окне текста: F7 / Shift+F7: следующее / предыдущее различие. Ctrl+F: поиск.\n\nНа основе FerrumTree. MIT OR Apache-2.0.");
}

pub fn state(s: State) -> &'static str {
    let (en, russian) = match s {
        State::Directory => ("Folder", "Папка"),
        State::LeftOnly => ("Only on left", "Только слева"),
        State::RightOnly => ("Only on right", "Только справа"),
        State::Type => ("Object types differ", "Типы различаются"),
        State::Size | State::Different => ("Contents differ", "Содержимое различается"),
        State::Time => ("Dates differ", "Даты различаются"),
        State::Name => ("Name case differs", "Регистр имени различается"),
        State::Metadata => ("Size and date match", "Размер и дата совпадают"),
        State::Equal => ("Contents match", "Содержимое совпадает"),
        State::Link => ("Reparse point not checked", "Ссылка не проверена"),
        State::Error => ("Could not verify", "Не удалось проверить"),
        State::Changed => ("Changed during comparison", "Изменён во время сравнения"),
        State::Cloud => ("Cloud data not read", "Облачный файл не прочитан"),
        State::Children => ("Contains differences", "Есть различия"),
    };
    if ru() { russian } else { en }
}

pub fn state_detail(s: State) -> &'static str {
    if s == State::Size {
        if ru() {
            "Содержимое различается: размеры файлов не совпадают."
        } else {
            "Contents differ: file sizes do not match."
        }
    } else {
        state(s)
    }
}

pub fn error(e: &std::io::Error) -> String {
    match e.to_string().as_str() {
        "archive contents are not supported" => return t(S::DropArchive).into(),
        "source kinds differ" => return t(S::DropKinds).into(),
        "drop count exceeded" => return t(S::DropCount).into(),
        "selection exceeds the clipboard memory limit" if ru() => {
            return "Выделенный текст превышает лимит буфера обмена (64 МБ). Выберите меньший участок.".into();
        }
        "file masks exceed the 4096-byte limit" if ru() => return "Маски превышают допустимую длину (4096 байт).".into(),
        "use up to 64 name masks separated by semicolons; paths are not masks" if ru() => {
            return "Укажите до 64 масок имён через точку с запятой, без путей к папкам.".into();
        }
        _ => {}
    }
    if !ru() || e.raw_os_error().is_some() {
        return e.to_string();
    }
    if let Some(code) = e.to_string().strip_prefix("OLE drop registration failed: ") {
        return format!("Не удалось включить перетаскивание: {code}");
    }
    if let Some(code) = e.to_string().strip_prefix("file dialog failed: ") {
        return format!("Не удалось открыть диалог выбора файла или папки: {code}");
    }
    match e.to_string().as_str() {
        "archive contents are not supported" => t(S::DropArchive).into(),
        "only files and folders can be compared" => "Для сравнения выберите обычные файлы или папки.".into(),
        "file changed during text comparison" => "Файл изменился во время сравнения. Закройте окно и сравните файлы повторно.".into(),
        "file is not valid UTF-8 or BOM-marked UTF-16 text" | "file is not valid in the selected text encoding" => "Текст не соответствует выбранной кодировке. Выберите кодировку в меню «Сравнение». Двоичные файлы можно сравнить побайтово в режиме сравнения папок.".into(),
        "selected encoding conflicts with BOM" => "Выбранная кодировка не соответствует BOM файла. Выберите автоматическое определение или кодировку, указанную в BOM.".into(),
        "text comparison requires local regular files" => "Для просмотра выберите обычные локальные файлы. Ссылки и облачные файлы не открываются.".into(),
        "text index memory limit exceeded" => "Индекс строк превысил лимит памяти. Сравнение остановлено без усечения файлов. В CLI лимит можно изменить параметром --text-memory.".into(),
        "text source became shorter" => "Размер исходного файла изменился во время чтения.".into(),
        "text row limit exceeded" => "Превышено допустимое число строк.".into(),
        "report destination is inside a compared folder" => "Сохраните отчёт за пределами сравниваемых папок.".into(),
        "directory index limit exceeded" => "Превышен допустимый размер индекса.".into(),
        "directory worker failed" => "Не удалось завершить чтение каталога.".into(),
        "comparison worker failed" => "Не удалось завершить сравнение.".into(),
        _ => match e.kind() {
            std::io::ErrorKind::Interrupted => t(S::Cancelled).into(),
            std::io::ErrorKind::OutOfMemory => "Недостаточно памяти.".into(),
            _ => format!("Не удалось выполнить операцию: {e}"),
        },
    }
}

pub fn number(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i != 0 && (s.len() - i).is_multiple_of(3) {
            out.push(if ru() { '\u{a0}' } else { ',' });
        }
        out.push(c);
    }
    out
}

pub fn size(n: u64) -> String {
    let mut f = n as f64;
    let mut unit = 0;
    while f >= 1024.0 && unit < 4 {
        f /= 1024.0;
        unit += 1;
    }
    let units = if ru() { ["Б", "КБ", "МБ", "ГБ", "ТБ"] } else { ["B", "KB", "MB", "GB", "TB"] };
    if unit == 0 { format!("{} {}", number(n), units[unit]) } else { format!("{f:.1} {}", units[unit]) }
}
