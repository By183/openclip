//! Runtime localization of the user interface.
//!
//! Every user-facing string lives in the [`strings!`] table below with one
//! translation per [`Lang`]. The active language is a process-global atomic so
//! any thread can format a message; it is set once from the saved [`Settings`]
//! at start-up and whenever the user picks another language.
//!
//! [`Settings`]: crate::settings::Settings
//!
//! Call sites use the [`t!`](crate::t) macro, which resolves the key through
//! [`key`] and is therefore checked at compile time:
//!
//! ```ignore
//! ui.label(t!(NAV_HOME));                       // &'static str
//! ui.label(t!(READY_TO_RECORD, source_label));  // String, fills {0}
//! ```
//!
//! Placeholders are `{0}`, `{1}`, … so translations may reorder them.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// A language the interface can be shown in. English is the fallback for any
/// string a translation is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Lang {
    #[default]
    #[serde(rename = "en")]
    En,
    #[serde(rename = "ro")]
    Ro,
    #[serde(rename = "ru")]
    Ru,
    #[serde(rename = "uk")]
    Uk,
}

impl Lang {
    pub const ALL: [Lang; 4] = [Lang::En, Lang::Ro, Lang::Ru, Lang::Uk];

    /// ISO 639-1 code, also what is written to `settings.json`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ro => "ro",
            Lang::Ru => "ru",
            Lang::Uk => "uk",
        }
    }

    /// Name of the language in that language, for the picker.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ro => "Română",
            Lang::Ru => "Русский",
            Lang::Uk => "Українська",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        let code = code.trim().to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.code() == code || code.starts_with(&format!("{}-", l.code())))
    }

    fn from_u8(v: u8) -> Lang {
        Lang::ALL.get(v as usize).copied().unwrap_or(Lang::En)
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The language the interface is currently drawn in.
pub fn lang() -> Lang {
    Lang::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// Switches the interface language; takes effect on the next repaint.
pub fn set_lang(lang: Lang) {
    let idx = Lang::ALL.iter().position(|l| *l == lang).unwrap_or(0) as u8;
    CURRENT.store(idx, Ordering::Relaxed);
}

/// Translates `key` into the active language, falling back to English.
pub fn t(key: &str) -> &'static str {
    lookup(lang(), key).or_else(|| lookup(Lang::En, key)).unwrap_or_else(|| {
        debug_assert!(false, "missing translation key: {key}");
        ""
    })
}

/// Like [`t`] but substitutes `{0}`, `{1}`, … with `args`.
pub fn tf(key: &str, args: &[&str]) -> String {
    let mut out = t(key).to_string();
    for (i, arg) in args.iter().enumerate() {
        out = out.replace(&format!("{{{i}}}"), arg);
    }
    out
}

/// Looks up a translated string. `t!(KEY)` returns `&'static str`;
/// `t!(KEY, a, b)` returns a `String` with `{0}`/`{1}` filled in.
#[macro_export]
macro_rules! t {
    ($name:ident) => {
        $crate::i18n::t($crate::i18n::key::$name)
    };
    ($name:ident, $($arg:expr),+ $(,)?) => {
        $crate::i18n::tf($crate::i18n::key::$name, &[$(&::std::string::ToString::to_string(&$arg)),+])
    };
}

/// Declares the translation table and the compile-time-checked key constants.
macro_rules! strings {
    ($($name:ident => $en:expr, $ro:expr, $ru:expr, $uk:expr;)*) => {
        /// String keys, so `t!(SOME_KEY)` fails to compile on a typo.
        pub mod key {
            $(pub const $name: &str = stringify!($name);)*
        }

        fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
            match key {
                $(stringify!($name) => Some(match lang {
                    Lang::En => $en,
                    Lang::Ro => $ro,
                    Lang::Ru => $ru,
                    Lang::Uk => $uk,
                }),)*
                _ => None,
            }
        }

        /// Every key in the table, for the completeness test below.
        #[cfg(test)]
        const ALL_KEYS: &[&str] = &[$(stringify!($name)),*];
    };
}

strings! {
    // ----- generic ---------------------------------------------------------
    OK => "OK", "OK", "OK", "OK";
    CANCEL => "Cancel", "Anulează", "Отмена", "Скасувати";
    CLOSE => "Close", "Închide", "Закрыть", "Закрити";
    DELETE => "Delete", "Șterge", "Удалить", "Видалити";
    OPEN => "Open", "Deschide", "Открыть", "Відкрити";
    SETTINGS => "Settings", "Setări", "Настройки", "Налаштування";
    NONE_PAREN => "(none)", "(niciunul)", "(нет)", "(немає)";
    ON => "on", "pornit", "вкл", "увімк";
    OFF => "off", "oprit", "выкл", "вимк";
    AUTO => "Auto", "Automat", "Авто", "Авто";
    YES_GPU => "yes (GPU)", "da (GPU)", "да (GPU)", "так (GPU)";
    NO_CPU => "no (CPU)", "nu (CPU)", "нет (CPU)", "ні (CPU)";
    MONO => "Mono", "Mono", "Моно", "Моно";
    STEREO => "Stereo", "Stereo", "Стерео", "Стерео";
    MONO_LOWER => "mono", "mono", "моно", "моно";
    STEREO_LOWER => "stereo", "stereo", "стерео", "стерео";
    UNIT_FPS => "fps", "fps", "кадр/с", "кадр/с";
    UNIT_KBPS => "kbps", "kbps", "кбит/с", "кбіт/с";
    UNIT_HZ => "Hz", "Hz", "Гц", "Гц";
    UNIT_SEC => "s", "s", "с", "с";
    UNIT_MS => "ms", "ms", "мс", "мс";

    // ----- languages -------------------------------------------------------
    LANGUAGE => "Language", "Limbă", "Язык", "Мова";
    LANGUAGE_HINT => "Applies immediately; saved with your settings.",
        "Se aplică imediat; se salvează împreună cu setările.",
        "Применяется сразу и сохраняется вместе с настройками.",
        "Застосовується одразу та зберігається разом із налаштуваннями.";

    // ----- toolbar ---------------------------------------------------------
    MODE_REGION => "Region", "Regiune", "Область", "Область";
    MODE_MONITOR => "Monitor", "Monitor", "Монитор", "Монітор";
    MODE_WINDOW => "Window", "Fereastră", "Окно", "Вікно";
    MODE_TIP => "{0} recording mode", "Mod de înregistrare: {0}", "Режим записи: {0}", "Режим запису: {0}";
    SYSTEM_AUDIO => "System audio", "Sunet de sistem", "Системный звук", "Системний звук";
    MICROPHONE => "Microphone", "Microfon", "Микрофон", "Мікрофон";
    SHOW_CURSOR => "Show cursor", "Arată cursorul", "Показывать курсор", "Показувати курсор";
    CURSOR => "Cursor", "Cursor", "Курсор", "Курсор";
    TIP_SNAPSHOT => "Take a snapshot (PNG)", "Fă o captură de ecran (PNG)", "Сделать снимок экрана (PNG)",
        "Зробити знімок екрана (PNG)";
    TIP_MINIBAR => "Mini bar: collapse to a small floating recorder bar (close the bar to come back)",
        "Bară mică: restrânge fereastra la o bară plutitoare (închide bara pentru a reveni)",
        "Мини-панель: свернуть в маленькую плавающую панель записи (закройте панель, чтобы вернуться)",
        "Міні-панель: згорнути у невелику плаваючу панель запису (закрийте панель, щоб повернутися)";
    TIP_REC_START => "Start recording", "Începe înregistrarea", "Начать запись", "Почати запис";
    TIP_REC_STOP => "Stop recording", "Oprește înregistrarea", "Остановить запись", "Зупинити запис";
    TIP_REC_SAVING => "Saving recording…", "Se salvează înregistrarea…", "Сохранение записи…", "Збереження запису…";
    TIP_REC_CANCEL => "Cancel the countdown", "Anulează numărătoarea inversă", "Отменить обратный отсчёт",
        "Скасувати зворотний відлік";
    TIP_PAUSE => "Pause recording", "Suspendă înregistrarea", "Приостановить запись", "Призупинити запис";
    TIP_RESUME => "Resume recording", "Reia înregistrarea", "Продолжить запись", "Продовжити запис";

    // ----- countdown -------------------------------------------------------
    COUNTDOWN_TITLE => "Recording starts in", "Înregistrarea începe în", "Запись начнётся через", "Запис почнеться через";
    COUNTDOWN_CANCEL => "Cancel (Esc)", "Anulează (Esc)", "Отмена (Esc)", "Скасувати (Esc)";
    COUNTDOWN_STATUS => "Recording starts in {0}…", "Înregistrarea începe în {0}…", "Запись начнётся через {0}…",
        "Запис почнеться через {0}…";
    COUNTDOWN_ESC => "(Esc cancels)", "(Esc anulează)", "(Esc — отмена)", "(Esc — скасувати)";

    // ----- status strip ----------------------------------------------------
    STATUS_PAUSED => "PAUSED  {0}", "SUSPENDAT  {0}", "ПАУЗА  {0}", "ПАУЗА  {0}";
    STATUS_REC => "REC  {0}", "REC  {0}", "REC  {0}", "REC  {0}";
    STATUS_COUNTERS => "{0}×{1}   {2} fps   {3} dropped   {4}",
        "{0}×{1}   {2} fps   {3} pierdute   {4}",
        "{0}×{1}   {2} кадр/с   потеряно: {3}   {4}",
        "{0}×{1}   {2} кадр/с   втрачено: {3}   {4}";
    STATUS_ENCODER_BEHIND => "the encoder cannot keep up: {0} ms per frame, budget {1} ms",
        "codificatorul nu face față: {0} ms per cadru, buget {1} ms",
        "кодировщик не справляется: {0} мс на кадр, бюджет {1} мс",
        "кодувальник не встигає: {0} мс на кадр, бюджет {1} мс";

    // ----- recording counters (hover breakdown) ------------------------------
    COUNTER_ENCODER => "Encoder", "Codificator", "Кодировщик", "Кодувальник";
    COUNTER_SCREEN_FPS => "Screen updates", "Actualizări ale ecranului", "Обновления экрана", "Оновлення екрана";
    COUNTER_FILE_FPS => "Frames in file", "Cadre în fișier", "Кадров в файле", "Кадрів у файлі";
    COUNTER_CAPTURED => "Captured", "Capturate", "Захвачено", "Захоплено";
    COUNTER_ENCODED => "Encoded", "Codificate", "Закодировано", "Закодовано";
    COUNTER_REPEATED => "Repeated (screen unchanged)", "Repetate (ecran neschimbat)",
        "Повторы (экран не менялся)", "Повтори (екран не змінювався)";
    COUNTER_SUPERSEDED => "Superseded (newer frame arrived)", "Înlocuite (a sosit un cadru mai nou)",
        "Заменены (пришёл более новый кадр)", "Замінено (надійшов новіший кадр)";
    COUNTER_DROPPED => "Dropped (queue full)", "Pierdute (coadă plină)", "Потеряно (очередь заполнена)",
        "Втрачено (черга заповнена)";
    COUNTER_SLOTS_SKIPPED => "Slots skipped (encoder behind)", "Sloturi omise (codificator în urmă)",
        "Пропущено слотов (кодировщик отстаёт)", "Пропущено слотів (кодувальник відстає)";
    COUNTER_SKIPPED => "Not written", "Nescrise", "Не записано", "Не записано";
    COUNTER_ENCODE_MS => "Encode time", "Timp de codificare", "Время кодирования", "Час кодування";
    COUNTER_MUX_MS => "Write time", "Timp de scriere", "Время записи", "Час запису";
    COUNTER_SLOT_MS => "Total per frame", "Total per cadru", "Всего на кадр", "Загалом на кадр";
    COUNTER_OF_BUDGET => "{0} ms of {1} ms", "{0} ms din {1} ms", "{0} мс из {1} мс", "{0} мс із {1} мс";

    STATUS_PICKING => "Drag a rectangle on the screen to select the recording region (Esc to cancel)",
        "Trage un dreptunghi pe ecran pentru a alege regiunea înregistrată (Esc pentru anulare)",
        "Выделите прямоугольник на экране, чтобы выбрать область записи (Esc — отмена)",
        "Виділіть прямокутник на екрані, щоб обрати область запису (Esc — скасувати)";
    STATUS_READY => "Ready to record: {0}", "Gata de înregistrare: {0}", "Готово к записи: {0}", "Готово до запису: {0}";
    STATUS_NO_SOURCE => "Please select a recording source", "Alege o sursă de înregistrare", "Выберите источник записи",
        "Оберіть джерело запису";
    STATUS_CHANGE => "Change…", "Schimbă…", "Изменить…", "Змінити…";
    STATUS_CHANGE_TIP => "Choose the monitor, window or region", "Alege monitorul, fereastra sau regiunea",
        "Выберите монитор, окно или область", "Оберіть монітор, вікно або область";

    // ----- navigation ------------------------------------------------------
    NAV_HOME => "Home", "Acasă", "Главная", "Головна";
    NAV_GENERAL => "General", "General", "Общие", "Загальні";
    NAV_VIDEO => "Video", "Video", "Видео", "Відео";
    NAV_IMAGE => "Image", "Imagine", "Изображение", "Зображення";
    NAV_ABOUT => "About", "Despre", "О программе", "Про програму";

    // ----- Home tabs / library ---------------------------------------------
    TAB_VIDEOS => "Videos", "Videoclipuri", "Видео", "Відео";
    TAB_IMAGES => "Images", "Imagini", "Изображения", "Зображення";
    TAB_AUDIOS => "Audios", "Fișiere audio", "Аудио", "Аудіо";
    TAB_PREVIEW => "Preview", "Previzualizare", "Предпросмотр", "Перегляд";
    NO_VIDEOS_YET => "No videos yet", "Niciun videoclip încă", "Пока нет видео", "Ще немає відео";
    NO_IMAGES_YET => "No images yet", "Nicio imagine încă", "Пока нет изображений", "Ще немає зображень";
    NO_AUDIOS_YET => "No audio files yet", "Niciun fișier audio încă", "Пока нет аудиофайлов", "Ще немає аудіофайлів";
    OUTPUT_FOLDER_TIP => "Output folder (change it under General)", "Folderul de ieșire (îl poți schimba în General)",
        "Папка сохранения (изменяется в разделе «Общие»)", "Папка збереження (змінюється у розділі «Загальні»)";
    REFRESH_LIST => "Refresh list", "Reîmprospătează lista", "Обновить список", "Оновити список";
    OPEN_FOLDER => "Open folder", "Deschide folderul", "Открыть папку", "Відкрити папку";
    PLAY => "Play", "Redă", "Воспроизвести", "Відтворити";
    FOLDER => "Folder", "Folder", "Папка", "Папка";

    // ----- source row ------------------------------------------------------
    NO_MONITORS => "No monitors", "Niciun monitor", "Мониторы не найдены", "Монітори не знайдені";
    NO_WINDOWS => "No windows", "Nicio fereastră", "Окна не найдены", "Вікна не знайдені";
    DISPLAY_N => "Display {0}", "Ecran {0}", "Экран {0}", "Екран {0}";
    ERR_GAME_STILL => "a game cannot be captured as a still", "un joc nu poate fi capturat ca imagine statică",
        "игру нельзя захватить как статичный кадр", "гру неможливо захопити як статичний кадр";
    NO_MONITOR_SELECTED => "No monitor", "Niciun monitor", "Монитор не выбран", "Монітор не обрано";
    NO_WINDOW_SELECTED => "No window selected", "Nicio fereastră selectată", "Окно не выбрано", "Вікно не обрано";
    NO_REGION_SELECTED => "No region selected", "Nicio regiune selectată", "Область не выбрана", "Область не обрано";
    REGION_LABEL => "Region {0}×{1} at ({2}, {3}) on {4}", "Regiune {0}×{1} la ({2}, {3}) pe {4}",
        "Область {0}×{1} в ({2}, {3}) на {4}", "Область {0}×{1} у ({2}, {3}) на {4}";
    SELECT_REGION => "Select region…", "Alege regiunea…", "Выбрать область…", "Обрати область…";
    REFRESH_SOURCES_TIP => "Refresh monitors, windows and audio devices",
        "Reîmprospătează monitoarele, ferestrele și dispozitivele audio",
        "Обновить список мониторов, окон и аудиоустройств",
        "Оновити список моніторів, вікон і аудіопристроїв";

    // ----- preview ---------------------------------------------------------
    PREVIEW_UNAVAILABLE => "Preview unavailable: {0}", "Previzualizare indisponibilă: {0}",
        "Предпросмотр недоступен: {0}", "Перегляд недоступний: {0}";
    PREVIEW_PICK_SOURCE => "Select a monitor, window or region to preview",
        "Alege un monitor, o fereastră sau o regiune pentru previzualizare",
        "Выберите монитор, окно или область для предпросмотра",
        "Оберіть монітор, вікно або область для перегляду";
    PREVIEW_STARTING => "Starting preview…", "Se pornește previzualizarea…", "Запуск предпросмотра…",
        "Запуск перегляду…";

    // ----- General page ----------------------------------------------------
    SECTION_OUTPUT => "Output", "Ieșire", "Сохранение", "Збереження";
    ROW_SAVE_TO => "Save to", "Salvează în", "Сохранять в", "Зберігати в";
    CHOOSE_FOLDER => "Choose folder…", "Alege folderul…", "Выбрать папку…", "Обрати папку…";
    ROW_FILE_PREFIX => "File name prefix", "Prefixul numelui", "Префикс имени файла", "Префікс імені файлу";
    SECTION_RECORDING => "Recording", "Înregistrare", "Запись", "Запис";
    ROW_COUNTDOWN => "Countdown", "Numărătoare inversă", "Обратный отсчёт", "Зворотний відлік";
    COUNTDOWN_CHECKBOX => "Count down before recording starts", "Numără invers înainte de începerea înregistrării",
        "Обратный отсчёт перед началом записи", "Зворотний відлік перед початком запису";
    COUNTDOWN_NOTE => "shown in the window and the mini bar, never in the video",
        "afișată în fereastră și în bara mică, niciodată în videoclip",
        "показывается в окне и на мини-панели, но не попадает в видео",
        "показується у вікні та на міні-панелі, але не потрапляє у відео";
    ROW_COLOR_CONVERSION => "Color conversion", "Conversie culoare", "Конвертация цвета", "Конвертація кольору";
    COLOR_CONVERSION_NOTE => "Auto uses GPU zero-readback and falls back to CPU only if GPU initialization fails",
        "Automat folosește GPU fără citire în RAM și trece la CPU doar dacă inițializarea GPU eșuează",
        "Авто использует GPU без копирования и переходит на CPU только при сбое инициализации GPU",
        "Авто використовує GPU без копіювання і переходить на CPU лише при збої ініціалізації GPU";
    SECTION_HOTKEY => "Shortcut", "Comandă rapidă", "Горячая клавиша", "Гаряча клавіша";
    ROW_HOTKEY => "Start / Stop recording", "Pornire / Oprire înregistrare", "Старт / Остановка записи", "Старт / Зупинка запису";
    HOTKEY_RECORDING => "Press keys… (Esc cancels)", "Apăsați tastele… (Esc anulează)", "Нажмите комбинацию… (Esc отмена)", "Натисніть комбінацію… (Esc скасувати)";
    HOTKEY_RESET => "Reset", "Resetează", "Сбросить", "Скинути";
    HOTKEY_SOUND => "Sound feedback", "Semnal sonor", "Звуковой сигнал", "Звуковий сигнал";
    HOTKEY_SOUND_NOTE => "Play a short tone when recording starts or stops",
        "Redă un ton scurt la pornirea sau oprirea înregistrării",
        "Воспроизводить короткий сигнал при старте и остановке",
        "Відтворювати короткий сигнал при старті та зупинці";
    HOTKEY_STATUS_CONFLICT => "Shortcut is in use by another app ({0})",
        "Comanda rapidă este utilizată de altă aplicație ({0})",
        "Комбинация занята другим приложением ({0})",
        "Комбінація зайнята іншою програмою ({0})";
    HOTKEY_STATUS_FALLBACK => "Active via keyboard hook (game/app conflict fallback)",
        "Activ prin hook de tastatură (mod rezervă)",
        "Активен через перехват клавиатуры (запасной режим)",
        "Активний через перехоплення клавіатури (запасний режим)";
    SECTION_SOURCES => "Sources", "Surse", "Источники", "Джерела";
    ROW_DEVICES => "Devices", "Dispozitive", "Устройства", "Пристрої";
    REFRESH_DEVICES => "Refresh monitors, windows and devices", "Reîmprospătează monitoarele, ferestrele și dispozitivele",
        "Обновить мониторы, окна и устройства", "Оновити монітори, вікна та пристрої";
    ROW_SETTINGS_FILE => "Settings file", "Fișier de setări", "Файл настроек", "Файл налаштувань";

    // ----- Video page ------------------------------------------------------
    TAB_RECORD => "Record", "Înregistrare", "Запись", "Запис";
    TAB_MOUSE => "Mouse", "Mouse", "Мышь", "Миша";
    TAB_WATERMARK => "Watermark", "Filigran", "Водяной знак", "Водяний знак";
    CHK_SHOW_CURSOR => "Show mouse cursor", "Arată cursorul mouse-ului", "Показывать курсор мыши",
        "Показувати курсор миші";
    CHK_CLICK_EFFECTS => "Add mouse click effects", "Adaugă efecte la clic", "Добавлять эффекты нажатий мыши",
        "Додавати ефекти натискань миші";
    CHK_CLICK_EFFECT => "Add mouse click effect", "Adaugă efect la clic", "Добавлять эффект нажатия мыши",
        "Додавати ефект натискання миші";
    CHK_HIGHLIGHT => "Add mouse highlight effect", "Adaugă efect de evidențiere a mouse-ului",
        "Добавлять подсветку курсора", "Додавати підсвічування курсора";
    CHK_SYSTEM_AUDIO => "Record what you hear (speakers / headphones)", "Înregistrează ce auzi (difuzoare / căști)",
        "Записывать то, что слышно (динамики / наушники)", "Записувати те, що чутно (динаміки / навушники)";
    CHK_MICROPHONE => "Record microphone", "Înregistrează microfonul", "Записывать микрофон", "Записувати мікрофон";
    ROW_DEVICE => "Device", "Dispozitiv", "Устройство", "Пристрій";
    ROW_PLAYBACK_DEVICE => "Output device", "Dispozitiv de ieșire", "Устройство вывода", "Пристрій виводу";
    DEFAULT_DEVICE => "Default (system)", "Implicit (sistem)", "По умолчанию (система)", "За замовчуванням (система)";
    NO_INPUT_DEVICES => "No input devices", "Niciun dispozitiv de intrare", "Нет устройств ввода", "Немає пристроїв вводу";
    NO_OUTPUT_DEVICES => "No output devices", "Niciun dispozitiv de ieșire", "Нет устройств вывода", "Немає пристроїв виводу";
    ROW_VOLUME => "Volume", "Volum", "Громкость", "Гучність";
    ROW_AUDIO_TRACKS => "Audio tracks", "Piste audio", "Аудиодорожки", "Аудіодоріжки";
    TRACKS_MIXED => "Single mix (System + Mic)", "Mix unic (Sistem + Mic)", "1 общая (Система + Мик.)",
        "1 спільна (Система + Мік.)";
    TRACKS_SEPARATE => "Separate (1: System, 2: Mic)", "Separate (1: Sistem, 2: Mic)", "Раздельные (1: Система, 2: Мик.)",
        "Окремі (1: Система, 2: Мік.)";
    TRACKS_MIXED_AND_SEPARATE => "Mix + separate (1: Mix, 2: Sys, 3: Mic)",
        "Mix + separate (1: Mix, 2: Sis, 3: Mic)",
        "Микс + раздельные (1: Микс, 2: Сист, 3: Мик)",
        "Мікс + окремі (1: Мікс, 2: Сист, 3: Мік)";
    TRACKS_NOTE => "Separate tracks save independent audio streams in MP4/AVI for editing; Track 1 keeps the full mix for playback when enabled.",
        "Pistele separate salvează fluxuri audio independente în MP4/AVI pentru editare; prima pistă păstrează mixul complet pentru redare.",
        "Раздельные дорожки сохраняют независимые аудиопотоки в MP4/AVI для монтажа; при включении микса первая дорожка готова к воспроизведению.",
        "Окремі доріжки зберігають незалежні аудіопотоки в MP4/AVI для монтажу; за увімкненого міксу перша доріжка готова до відтворення.";
    SECTION_FORMAT => "Format – {0}", "Format – {0}", "Формат – {0}", "Формат – {0}";
    BOX_VIDEO => "Video", "Video", "Видео", "Відео";
    BOX_AUDIO => "Audio", "Audio", "Аудио", "Аудіо";
    FORMAT_SETTINGS_TIP => "Container, codecs, size and quality", "Container, codecuri, dimensiune și calitate",
        "Контейнер, кодеки, размер и качество", "Контейнер, кодеки, розмір і якість";
    SCANNING_ENCODERS => "scanning encoders…", "se scanează codificatoarele…", "поиск кодировщиков…",
        "пошук кодувальників…";
    SRC_SYSTEM_AND_MIC => "system audio + microphone", "sunet de sistem + microfon", "системный звук + микрофон",
        "системний звук + мікрофон";
    SRC_SYSTEM => "system audio", "sunet de sistem", "системный звук", "системний звук";
    SRC_MIC => "microphone", "microfon", "микрофон", "мікрофон";
    SRC_NO_AUDIO => "no audio", "fără sunet", "без звука", "без звуку";
    SRC_WITH_TRACKS => "{0} ({1} tracks)", "{0} ({1} piste)", "{0} (дорожек: {1})", "{0} (доріжок: {1})";

    // ----- Mouse tab -------------------------------------------------------
    SECTION_MOUSE_FX => "Mouse effects", "Efecte de mouse", "Эффекты мыши", "Ефекти миші";
    ROW_SIZE_INDENT => "      Size", "      Dimensiune", "      Размер", "      Розмір";
    ROW_LEFT_CLICK_COLOR => "      Left click color", "      Culoare clic stânga", "      Цвет левого клика",
        "      Колір лівого кліку";
    ROW_RIGHT_CLICK_COLOR => "      Right click color", "      Culoare clic dreapta", "      Цвет правого клика",
        "      Колір правого кліку";
    ROW_HIGHLIGHT_COLOR => "      Highlight color", "      Culoare evidențiere", "      Цвет подсветки",
        "      Колір підсвічування";
    ROW_OPACITY => "      Opacity", "      Opacitate", "      Прозрачность", "      Прозорість";
    APP_DRAWN => "(app-drawn)", "(desenat de aplicație)", "(рисует приложение)", "(малює програма)";
    FX_PREVIEW_HINT => "Click inside to test the click effect.", "Dă clic înăuntru pentru a testa efectul.",
        "Щёлкните внутри, чтобы проверить эффект нажатия.", "Клацніть усередині, щоб перевірити ефект натискання.";

    // ----- Watermark tab ---------------------------------------------------
    SECTION_WATERMARK => "Watermark", "Filigran", "Водяной знак", "Водяний знак";
    CHK_WATERMARK => "Show watermark", "Afișează filigranul", "Показывать водяной знак", "Показувати водяний знак";
    ROW_POSITION_INDENT => "      Position", "      Poziție", "      Положение", "      Положення";
    POS_TOP_LEFT => "Top left", "Stânga sus", "Сверху слева", "Зверху ліворуч";
    POS_TOP_RIGHT => "Top right", "Dreapta sus", "Сверху справа", "Зверху праворуч";
    POS_BOTTOM_LEFT => "Bottom left", "Stânga jos", "Снизу слева", "Знизу ліворуч";
    POS_BOTTOM_RIGHT => "Bottom right", "Dreapta jos", "Снизу справа", "Знизу праворуч";
    ROW_WATERMARK => "Watermark", "Filigran", "Водяной знак", "Водяний знак";
    WATERMARK_PREVIEW_HINT => "Shown larger than it will appear in the recording.",
        "Afișat mai mare decât va apărea în înregistrare.",
        "Показан крупнее, чем будет в записи.",
        "Показаний більшим, ніж буде у записі.";
    WATERMARK_FOOTNOTE => "Added to recordings and snapshots. openclip is free and open source — you can turn it off.",
        "Adăugat la înregistrări și capturi. openclip este gratuit și open source — îl poți dezactiva.",
        "Добавляется к записям и снимкам. openclip бесплатен и с открытым кодом — его можно отключить.",
        "Додається до записів і знімків. openclip безкоштовний і з відкритим кодом — його можна вимкнути.";

    // ----- Image page ------------------------------------------------------
    SECTION_SNAPSHOT => "Snapshot", "Captură de ecran", "Снимок экрана", "Знімок екрана";
    BOX_IMAGE => "Image", "Imagine", "Изображение", "Зображення";
    SNAPSHOT_DETAIL => "Full size, saved next to your recordings", "Dimensiune completă, salvată lângă înregistrări",
        "Полный размер, сохраняется рядом с записями", "Повний розмір, зберігається поруч із записами";
    ROW_CAPTURE => "Capture", "Captură", "Съёмка", "Зйомка";
    TAKE_SNAPSHOT_NOW => "Take snapshot now", "Fă o captură acum", "Сделать снимок сейчас", "Зробити знімок зараз";
    ROW_SOURCE => "Source", "Sursă", "Источник", "Джерело";
    ROW_REGION_SCREENSHOT => "Interactive region capture", "Captură interactivă de regiune",
        "Выделение области при снимке экрана", "Виділення області при знімку екрана";
    ROW_REGION_SCREENSHOT_HINT => "Freeze the screen and select an area with the mouse",
        "Îngheață ecranul și selectează o zonă cu mouse-ul",
        "Зафиксировать экран и выделить нужную область мышью",
        "Зафіксувати екран і виділити потрібну область мишею";
    ROW_ANNOTATE_SCREENSHOT => "Open editor after selection", "Deschide editorul după selectare",
        "Открывать редактор после выделения", "Відкривати редактор після виділення";
    ROW_ANNOTATE_SCREENSHOT_HINT => "Show drawing and annotation tools immediately after selecting region",
        "Afișează instrumentele de desenat și adnotare imediat după selectarea regiunii",
        "Показывать инструменты рисования и аннотаций сразу после выделения",
        "Показувати інструменти малювання та анотацій одразу після виділення";
    ROW_SCREENSHOT_HOTKEY => "Screenshot hotkey", "Tastă rapidă pentru captură",
        "Горячая клавиша снимка экрана", "Гаряча клавіша знімка екрана";

    // ----- Snipping & Annotation tool --------------------------------------
    SNIP_HINT => "Drag to select area · Esc to cancel", "Trage pentru a selecta zona · Esc pentru anulare",
        "Зажмите ЛКМ и выделите область · Esc — отмена", "Затисніть ЛКМ та виділіть область · Esc — скасування";
    SNIP_TOOL_MOVE => "Move / Select", "Mutare / Selectare", "Перемещение / Выбор", "Переміщення / Вибір";
    SNIP_TOOL_RECT => "Rectangle", "Dreptunghi", "Прямоугольник", "Прямокутник";
    SNIP_TOOL_ARROW => "Arrow", "Săgeată", "Стрелка", "Стрілка";
    SNIP_TOOL_LINE => "Line", "Linie", "Линия", "Лінія";
    SNIP_TOOL_PEN => "Pen", "Stilou", "Карандаш", "Олівець";
    SNIP_TOOL_MARKER => "Highlighter", "Evidențiator", "Маркер", "Маркер";
    SNIP_TOOL_TEXT => "Text", "Text", "Текст", "Текст";
    SNIP_TOOL_PIXELATE => "Blur / Pixelate", "Estompare / Pixelare", "Размытие / Пикселизация", "Розмиття / Пікселізація";
    SNIP_TOOL_UNDO => "Undo (Ctrl+Z)", "Anulează (Ctrl+Z)", "Отменить (Ctrl+Z)", "Скасувати (Ctrl+Z)";
    SNIP_TOOL_DELETE => "Delete selected (Del)", "Șterge selecția (Del)", "Удалить выбранное (Del)", "Видалити вибране (Del)";
    SNIP_TOOL_SAVE => "Save & Copy (Enter)", "Salvează și copiază (Enter)", "Сохранить и скопировать (Enter)", "Зберегти та скопіювати (Enter)";
    SNIP_TOOL_COPY => "Copy (Ctrl+C)", "Copiază (Ctrl+C)", "Скопировать (Ctrl+C)", "Скопіювати (Ctrl+C)";
    SNIP_TOOL_CANCEL => "Cancel (Esc)", "Anulează (Esc)", "Отмена (Esc)", "Скасувати (Esc)";
    SNIP_TOOL_TEXT_SMALLER => "Decrease font size", "Micșorează fontul", "Уменьшить шрифт", "Зменшити шрифт";
    SNIP_TOOL_TEXT_LARGER => "Increase font size", "Mărește fontul", "Увеличить шрифт", "Збільшити шрифт";
    MSG_SNAPSHOT_COPIED => "Screenshot copied to clipboard", "Captura a fost copiată în clipboard",
        "Снимок скопирован в буфер обмена", "Знімок скопійовано в буфер обміну";

    // ----- About page ------------------------------------------------------
    ABOUT_VERSION => "Version {0}", "Versiunea {0}", "Версия {0}", "Версія {0}";
    ABOUT_TAGLINE => "A self-contained screen recorder: no ffmpeg, no codec packs.",
        "Un recorder de ecran de sine stătător: fără ffmpeg, fără pachete de codecuri.",
        "Самодостаточная программа записи экрана: без ffmpeg и наборов кодеков.",
        "Самодостатня програма запису екрана: без ffmpeg та сторонніх кодек-паків.";
    ABOUT_FORK_NOTICE => "Based on OpenClip (catalingrigoriev285/openclip), Apache 2.0 License. Modified fork (v0.2.0).",
        "Bazat pe OpenClip (catalingrigoriev285/openclip), licența Apache 2.0. Versiune modificată (v0.2.0).",
        "Основано на OpenClip (catalingrigoriev285/openclip), лицензия Apache 2.0. Доработанная версия (v0.2.0).",
        "Базується на OpenClip (catalingrigoriev285/openclip), ліцензія Apache 2.0. Доопрацьована версія (v0.2.0).";
    ABOUT_VIDEO => "Video: H.264 via bundled OpenH264, plus hardware H.264 / HEVC (NVENC, AMF, Quick Sync) through Windows Media Foundation",
        "Video: H.264 prin OpenH264 inclus, plus H.264 / HEVC hardware (NVENC, AMF, Quick Sync) prin Windows Media Foundation",
        "Видео: H.264 через встроенный OpenH264, а также аппаратные H.264 / HEVC (NVENC, AMF, Quick Sync) через Windows Media Foundation",
        "Відео: H.264 через вбудований OpenH264, а також апаратні H.264 / HEVC (NVENC, AMF, Quick Sync) через Windows Media Foundation";
    ABOUT_AUDIO => "Audio: MP3 via bundled LAME, AAC (Windows Media Foundation) or PCM · Containers: in-house MP4 and AVI muxers",
        "Audio: MP3 prin LAME inclus, AAC (Windows Media Foundation) sau PCM · Containere: muxere MP4 și AVI proprii",
        "Аудио: MP3 через встроенный LAME, AAC (Windows Media Foundation) или PCM · Контейнеры: собственные мультиплексоры MP4 и AVI",
        "Аудіо: MP3 через вбудований LAME, AAC (Windows Media Foundation) або PCM · Контейнери: власні мультиплексори MP4 і AVI";
    ABOUT_LICENSE => "Licensed under Apache-2.0. OpenH264 is BSD-2-Clause (source build, no Cisco patent coverage); LAME is LGPL.",
        "Licențiat Apache-2.0. OpenH264 este BSD-2-Clause (compilat din surse, fără acoperirea brevetelor Cisco); LAME este LGPL.",
        "Лицензия Apache-2.0. OpenH264 — BSD-2-Clause (сборка из исходников, без патентного покрытия Cisco); LAME — LGPL.",
        "Ліцензія Apache-2.0. OpenH264 — BSD-2-Clause (збірка з вихідного коду, без патентного покриття Cisco); LAME — LGPL.";

    // ----- Updates (General page, About page, status strip, dialog) --------
    SECTION_UPDATES => "Updates", "Actualizări", "Обновления", "Оновлення";
    ROW_UPDATES => "Check", "Verificare", "Проверка", "Перевірка";
    UPDATES_CHECKBOX => "Check for updates when openclip starts", "Verifică actualizările la pornirea openclip",
        "Проверять обновления при запуске openclip", "Перевіряти оновлення під час запуску openclip";
    UPDATES_NOTE => "update checks and downloads from GitHub are disabled in this build",
        "verificarea și descărcarea actualizărilor de pe GitHub sunt dezactivate în această versiune",
        "проверка и загрузка обновлений с GitHub отключены в этой сборке",
        "перевірку та завантаження оновлень із GitHub вимкнено в цій збірці";
    UPDATE_CHECK_BUTTON => "Check for updates", "Caută actualizări", "Проверить обновления", "Перевірити оновлення";
    UPDATE_DISABLED => "Disabled", "Dezactivată", "Отключено", "Вимкнено";
    UPDATE_CHECKING => "checking for updates…", "se caută actualizări…", "проверка обновлений…", "перевірка оновлень…";
    UPDATE_UP_TO_DATE => "You have the latest version.", "Ai cea mai nouă versiune.", "У вас последняя версия.",
        "У вас найновіша версія.";
    UPDATE_AVAILABLE => "Version {0} is available.", "Versiunea {0} este disponibilă.", "Доступна версия {0}.",
        "Доступна версія {0}.";
    UPDATE_INSTALLED_SHORT => "Version {0} is installed — restart to use it.",
        "Versiunea {0} este instalată — repornește pentru a o folosi.",
        "Версия {0} установлена — перезапустите, чтобы использовать её.",
        "Версія {0} встановлена — перезапустіть, щоб користуватися нею.";
    UPDATE_CHECK_FAILED => "Could not check for updates: {0}", "Nu s-au putut verifica actualizările: {0}",
        "Не удалось проверить обновления: {0}", "Не вдалося перевірити оновлення: {0}";
    UPD_CHECK_DISCONNECTED => "the update check did not finish", "verificarea actualizărilor nu s-a încheiat",
        "проверка обновлений не завершилась", "перевірка оновлень не завершилася";
    UPD_DOWNLOAD_DISCONNECTED => "the download did not finish", "descărcarea nu s-a încheiat",
        "загрузка не завершилась", "завантаження не завершилося";
    UPDATE_CHIP => "Update to {0}", "Actualizează la {0}", "Обновить до {0}", "Оновити до {0}";
    UPDATE_CHIP_TIP => "A newer version of openclip is available", "Este disponibilă o versiune mai nouă de openclip",
        "Доступна новая версия openclip", "Доступна нова версія openclip";
    UPDATE_DETAILS => "Details…", "Detalii…", "Подробнее…", "Детальніше…";
    UPDATE_TITLE => "Update available", "Actualizare disponibilă", "Доступно обновление", "Доступне оновлення";
    UPDATE_VERSIONS => "openclip {0} — you have {1}", "openclip {0} — ai {1}", "openclip {0} — у вас {1}",
        "openclip {0} — у вас {1}";
    UPDATE_NOTES => "Release notes", "Note de lansare", "Примечания к выпуску", "Примітки до випуску";
    UPDATE_NO_NOTES => "(no release notes)", "(fără note de lansare)", "(без примечаний к выпуску)",
        "(без приміток до випуску)";
    UPDATE_DOWNLOAD_INSTALL => "Download and install", "Descarcă și instalează", "Скачать и установить",
        "Завантажити та встановити";
    UPDATE_OPEN_PAGE => "Open release page", "Deschide pagina versiunii", "Открыть страницу выпуска",
        "Відкрити сторінку випуску";
    UPDATE_LATER => "Later", "Mai târziu", "Позже", "Пізніше";
    UPDATE_DOWNLOADING => "Downloading… {0} / {1}", "Se descarcă… {0} / {1}", "Загрузка… {0} / {1}",
        "Завантаження… {0} / {1}";
    UPDATE_INSTALLING => "Verifying and installing…", "Se verifică și se instalează…", "Проверка и установка…",
        "Перевірка та встановлення…";
    UPDATE_INSTALLED => "Version {0} is installed. Restart openclip to use it.",
        "Versiunea {0} este instalată. Repornește openclip pentru a o folosi.",
        "Версия {0} установлена. Перезапустите openclip, чтобы использовать её.",
        "Версія {0} встановлена. Перезапустіть openclip, щоб використовувати її.";
    UPDATE_RESTART_NOW => "Restart now", "Repornește acum", "Перезапустить сейчас", "Перезапустити зараз";
    UPDATE_FAILED => "Update failed: {0}", "Actualizarea a eșuat: {0}", "Ошибка обновления: {0}",
        "Помилка оновлення: {0}";
    UPDATE_RETRY => "Retry", "Reîncearcă", "Повторить", "Повторити";
    UPDATE_MANUAL_ONLY => "The program folder is not writable — download the new version from the release page.",
        "Folderul programului nu poate fi scris — descarcă noua versiune de pe pagina versiunii.",
        "Папка программы недоступна для записи — скачайте новую версию со страницы выпуска.",
        "Папка програми недоступна для запису — завантажте нову версію зі сторінки випуску.";
    UPDATE_NO_ASSET => "No download for this platform — see the release page.",
        "Nu există descărcare pentru această platformă — vezi pagina versiunii.",
        "Для этой платформы нет сборки — см. страницу выпуска.",
        "Для цієї платформи немає збірки — див. сторінку випуску.";
    UPDATE_LOCKED_RECORDING => "Finish the recording before installing the update.",
        "Termină înregistrarea înainte de a instala actualizarea.",
        "Завершите запись перед установкой обновления.",
        "Завершіть запис перед встановленням оновлення.";
    UPDATE_RESTART_FAILED => "Could not start the new version: {0}", "Nu s-a putut porni noua versiune: {0}",
        "Не удалось запустить новую версию: {0}", "Не вдалося запустити нову версію: {0}";
    MSG_UPDATE_AVAILABLE => "openclip {0} is available — see the status bar or About.",
        "openclip {0} este disponibil — vezi bara de stare sau Despre.",
        "Доступен openclip {0} — см. строку состояния или «О программе».",
        "Доступний openclip {0} — див. рядок стану або «Про програму».";

    // ----- footer / messages ------------------------------------------------
    FOOTER_IDLE => "openclip – screen recorder", "openclip – recorder de ecran", "openclip — запись экрана",
        "openclip — запис екрана";
    WHAT_RECORDING => "Recording", "Înregistrarea", "Запись", "Запис";
    WHAT_SNAPSHOT => "Snapshot", "Captura", "Снимок", "Знімок";
    MSG_SAVED => "{0} saved: {1} ({2})", "{0} a fost salvată: {1} ({2})", "{0} сохранена: {1} ({2})",
        "{0} збережено: {1} ({2})";
    MSG_SELECT_SOURCE_FIRST => "Select something to record first.", "Alege mai întâi ce vrei să înregistrezi.",
        "Сначала выберите, что записывать.", "Спочатку оберіть, що записувати.";
    MSG_SELECT_CAPTURE_FIRST => "Select something to capture first.", "Alege mai întâi ce vrei să capturezi.",
        "Сначала выберите, что снимать.", "Спочатку оберіть, що знімати.";
    MSG_CANCELLED => "Recording cancelled.", "Înregistrare anulată.", "Запись отменена.", "Запис скасовано.";
    MSG_START_FAILED => "Could not start recording: {0}", "Înregistrarea nu a putut porni: {0}",
        "Не удалось начать запись: {0}", "Не вдалося розпочати запис: {0}";
    MSG_RECORDING_FAILED => "Recording failed: {0}", "Înregistrarea a eșuat: {0}", "Ошибка записи: {0}",
        "Помилка запису: {0}";
    MSG_SNAPSHOT_FAILED => "Snapshot failed: {0}", "Captura a eșuat: {0}", "Не удалось сделать снимок: {0}",
        "Не вдалося зробити знімок: {0}";
    MSG_HOOK_REPAIRED => "Game-capture component restored — restart openclip",
        "Componenta pentru captura jocurilor a fost restaurată — repornește openclip",
        "Компонент захвата игр восстановлен — перезапустите openclip",
        "Компонент захоплення ігор відновлено — перезапустіть openclip";
    MSG_PICKER_FAILED => "Region picker: {0}", "Selectorul de regiune: {0}", "Выбор области: {0}",
        "Вибір області: {0}";
    MSG_DELETED => "Deleted {0}", "Șters: {0}", "Удалено: {0}", "Видалено: {0}";
    MSG_DELETE_FAILED => "Could not delete: {0}", "Nu s-a putut șterge: {0}", "Не удалось удалить: {0}",
        "Не вдалося видалити: {0}";

    // ----- delete dialog ---------------------------------------------------
    DELETE_TITLE => "Delete file?", "Ștergi fișierul?", "Удалить файл?", "Видалити файл?";
    DELETE_BODY => "This permanently deletes the file from disk.", "Fișierul va fi șters definitiv de pe disc.",
        "Файл будет безвозвратно удалён с диска.", "Файл буде безповоротно видалено з диска.";

    // ----- format dialog ---------------------------------------------------
    FMT_TITLE => "Format settings", "Setări de format", "Настройки формата", "Налаштування формату";
    FMT_GROUP_FILE_TYPE => "File Type", "Tip de fișier", "Тип файла", "Тип файлу";
    FMT_LOCKED => "Settings cannot be changed while recording.", "Setările nu pot fi schimbate în timpul înregistrării.",
        "Настройки нельзя менять во время записи.", "Налаштування не можна змінювати під час запису.";
    FMT_HELP => "[ Help ]", "[ Ajutor ]", "[ Справка ]", "[ Довідка ]";
    FMT_ROW_SIZE => "Size", "Dimensiune", "Размер", "Розмір";
    FMT_FULL_SIZE => "Full Size", "Dimensiune completă", "Полный размер", "Повний розмір";
    FMT_HALF_SIZE => "Half Size", "Jumătate de dimensiune", "Половина размера", "Половина розміру";
    FMT_CUSTOM_PERCENT => "Custom (W% × H%)", "Personalizat (L% × Î%)", "Свой (Ш% × В%)", "Власний (Ш% × В%)";
    FMT_CUSTOM_SIZE_LABEL => "Custom {0}% × {1}%", "Personalizat {0}% × {1}%", "Свой {0}% × {1}%",
        "Власний {0}% × {1}%";
    FMT_ROW_FPS => "FPS", "FPS", "Кадры/с", "Кадрів/с";
    FMT_CUSTOM_FPS => "Custom ({0})", "Personalizat ({0})", "Свой ({0})", "Власний ({0})";
    FMT_CUSTOM => "Custom…", "Personalizat…", "Свой…", "Власний…";
    FMT_FPS_RANGE => "Frame rate (1–144 Hz)", "Rată de cadre (1–144 Hz)", "Частота кадров (1–144 Гц)",
        "Частота кадрів (1–144 Гц)";
    FMT_ROW_CODEC => "Codec", "Codec", "Кодек", "Кодек";
    FMT_AUTO_TIP => "Uses a GPU encoder when one is available, otherwise OpenH264",
        "Folosește un codificator GPU dacă există, altfel OpenH264",
        "Использует кодировщик GPU, если он доступен, иначе OpenH264",
        "Використовує кодувальник GPU, якщо він доступний, інакше OpenH264";
    FMT_ENCODER_MISSING => "This encoder was not found on this system", "Acest codificator nu a fost găsit pe acest sistem",
        "Этот кодировщик не найден в системе", "Цей кодувальник не знайдено в системі";
    FMT_NO_MF_ENCODERS => "No Media Foundation encoders found", "Niciun codificator Media Foundation găsit",
        "Кодировщики Media Foundation не найдены", "Кодувальники Media Foundation не знайдено";
    FMT_MF_WINDOWS_ONLY => "Hardware encoders are only available on Windows",
        "Codificatoarele hardware sunt disponibile doar pe Windows",
        "Аппаратные кодировщики доступны только в Windows",
        "Апаратні кодувальники доступні лише у Windows";
    FMT_ENCODER_DETAILS => "Encoder details", "Detalii codificator", "Сведения о кодировщике",
        "Відомості про кодувальник";
    FMT_ROW_QUALITY => "Quality", "Calitate", "Качество", "Якість";
    FMT_QUALITY_BEST => "100 (best)", "100 (cea mai bună)", "100 (лучшее)", "100 (найкраща)";
    FMT_QUALITY_SMALLEST => "10 (smallest file)", "10 (fișier minim)", "10 (наименьший файл)", "10 (найменший файл)";
    FMT_BITRATE_TIP => "Bitrate presets, custom rate and keyframe settings",
        "Presetări bitrate, rată personalizată și cadre-cheie",
        "Пресеты битрейта, ручной битрейт и ключевые кадры",
        "Пресети бітрейту, власний бітрейт і ключові кадри";
    FMT_BITRATE_PRESETS => "Presets", "Presetări", "Пресеты", "Пресети";
    FMT_BUILTIN_PRESETS => "Built-in presets", "Presetări integrate", "Встроенные пресеты", "Вбудовані пресети";
    FMT_CUSTOM_PRESETS => "Custom presets", "Presetări personalizate", "Свои пресеты", "Власні пресети";
    FMT_PRESET_NAME_HINT => "Preset name…", "Nume presetare…", "Название пресета…", "Назва пресету…";
    FMT_PRESET_SAVE => "Save", "Salvează", "Сохранить", "Зберегти";
    FMT_PRESET_DELETE_TIP => "Remove custom preset", "Șterge presetarea", "Удалить свой пресет", "Видалити власний пресет";
    FMT_PRESET_RESET => "Clear custom", "Șterge personalizările", "Очистить свои", "Очистити власні";
    FMT_MB_PER_MIN => "≈ {0} MB/min", "≈ {0} MB/min", "≈ {0} МБ/мин", "≈ {0} МБ/хв";
    PRESET_720P_STREAM => "720p Stream", "Streaming 720p", "Стрим 720p", "Стрім 720p";
    PRESET_1080P_STANDARD => "1080p60 Standard", "1080p60 Standard", "1080p60 Стандарт", "1080p60 Стандарт";
    PRESET_1080P_HIGH => "1080p60 High", "1080p60 Înalt", "1080p60 Высокое", "1080p60 Висока";
    PRESET_1440P_HIGH => "1440p60 Crisp", "1440p60 Clar", "1440p60 Чёткое", "1440p60 Чітка";
    PRESET_144HZ_GAMING => "144 Hz Gaming", "Jocuri 144 Hz", "Игры 144 Гц", "Ігри 144 Гц";
    PRESET_4K_MASTER => "4K Master", "4K Master", "4K Мастер", "4K Майстер";
    FMT_ROW_PROFILE => "Profile", "Profil", "Профиль", "Профіль";
    FMT_PROFILE_HELP => "Auto lets the encoder choose. Baseline plays everywhere but compresses worst; High gives the best quality per bitrate on modern players.",
        "Automat lasă codificatorul să aleagă. Baseline se redă oriunde, dar comprimă cel mai slab; High oferă cea mai bună calitate per bitrate pe playerele moderne.",
        "«Авто» оставляет выбор кодировщику. Baseline воспроизводится везде, но сжимает хуже всех; High даёт лучшее качество при том же битрейте на современных плеерах.",
        "«Авто» залишає вибір кодувальнику. Baseline відтворюється всюди, але стискає найгірше; High дає найкращу якість при тому ж бітрейті на сучасних плеєрах.";
    FMT_ROW_BITRATE => "Bitrate", "Bitrate", "Битрейт", "Бітрейт";
    FMT_AAC_NEEDS_WINDOWS => "AAC needs Windows (Media Foundation)", "AAC necesită Windows (Media Foundation)",
        "AAC требует Windows (Media Foundation)", "AAC потребує Windows (Media Foundation)";
    FMT_PCM_AVI_ONLY => "PCM is only available in AVI", "PCM este disponibil doar în AVI", "PCM доступен только в AVI",
        "PCM доступний лише в AVI";
    FMT_ROW_CHANNELS => "Channels", "Canale", "Каналы", "Канали";
    FMT_ROW_FREQUENCY => "Frequency", "Frecvență", "Частота", "Частота";
    FMT_QUALITY_MODE => "Quality-based", "În funcție de calitate",
        "По качеству (VBR)", "За якістю (VBR)";
    FMT_CBR_MODE => "Constant bitrate", "Bitrate constant", "Постоянный битрейт", "Постійний бітрейт";
    FMT_BITRATE_ESTIMATE => "≈ {0} kbps at {1}×{2}, {3} fps", "≈ {0} kbps la {1}×{2}, {3} fps",
        "≈ {0} кбит/с при {1}×{2}, {3} кадр/с", "≈ {0} кбіт/с при {1}×{2}, {3} кадр/с";
    FMT_ROW_KEYFRAME => "Keyframe every", "Cadru-cheie la fiecare", "Ключевой кадр каждые", "Ключовий кадр кожні";
    FMT_RATE_CONTROL_NOTE => "Quality mode lets the encoder vary the bitrate with the content (hardware encoders use their own quality rate control). Constant bitrate gives predictable file sizes.",
        "Modul calitate lasă codificatorul să varieze bitrate-ul în funcție de conținut (codificatoarele hardware folosesc propriul control al calității). Bitrate-ul constant oferă dimensiuni previzibile ale fișierelor.",
        "Режим качества позволяет кодировщику менять битрейт в зависимости от содержимого (аппаратные кодировщики используют собственное управление качеством). Постоянный битрейт даёт предсказуемый размер файла.",
        "Режим якості дозволяє кодувальнику змінювати бітрейт залежно від вмісту (апаратні кодувальники використовують власне керування якістю). Постійний бітрейт забезпечує передбачуваний розмір файлу.";
    FMT_ROW_ENCODER => "Encoder", "Codificator", "Кодировщик", "Кодувальник";
    FMT_ROW_VENDOR => "Vendor", "Producător", "Производитель", "Виробник";
    FMT_ROW_HARDWARE => "Hardware", "Hardware", "Аппаратный", "Апаратний";
    FMT_ROW_TRANSFORM => "Transform", "Transformare", "Преобразование", "Перетворення";
    FMT_ROW_DETAILS => "Details", "Detalii", "Подробности", "Подробиці";
    FMT_OPENH264_DETAILS => "Bundled OpenH264 (Cisco), software encoder on the CPU. Works everywhere; use a hardware encoder for high resolutions and frame rates.",
        "OpenH264 inclus (Cisco), codificator software pe procesor. Funcționează oriunde; folosește un codificator hardware pentru rezoluții și rate de cadre mari.",
        "Встроенный OpenH264 (Cisco), программный кодировщик на процессоре. Работает везде; для высоких разрешений и частоты кадров используйте аппаратный кодировщик.",
        "Вбудований OpenH264 (Cisco), програмний кодувальник на процесорі. Працює всюди; для високих роздільностей і частоти кадрів використовуйте апаратний кодувальник.";
    FMT_NOT_FOUND => "Not found on this system.", "Nu a fost găsit pe acest sistem.", "Не найден в этой системе.",
        "Не знайдено в цій системі.";
    FMT_MF_COUNT => "{0} Media Foundation encoder(s) found.", "{0} codificator(oare) Media Foundation găsite.",
        "Найдено кодировщиков Media Foundation: {0}.", "Знайдено кодувальників Media Foundation: {0}.";
    FMT_RESCAN => "Rescan encoders", "Rescanează codificatoarele", "Повторный поиск кодировщиков",
        "Пересканувати кодувальники";

    // ----- format summaries (settings.rs) ----------------------------------
    CODEC_AUTO_GENERIC => "Auto (hardware H.264 if available)", "Automat (H.264 hardware dacă există)",
        "Авто (аппаратный H.264, если доступен)", "Авто (апаратний H.264, якщо доступний)";
    CODEC_OPENH264 => "H264 (OpenH264, CPU)", "H264 (OpenH264, CPU)", "H264 (OpenH264, CPU)", "H264 (OpenH264, CPU)";
    CODEC_MF_GENERIC => "{0} (Media Foundation encoder)", "{0} (codificator Media Foundation)",
        "{0} (кодировщик Media Foundation)", "{0} (кодувальник Media Foundation)";
    CODEC_AUTO_RESOLVED => "Auto → {0}", "Automat → {0}", "Авто → {0}", "Авто → {0}";
    QUALITY_LABEL => "quality {0}", "calitate {0}", "качество {0}", "якість {0}";
    CBR_LABEL => "{0} kbps CBR", "{0} kbps CBR", "{0} кбит/с CBR", "{0} кбіт/с CBR";
    VIDEO_SUMMARY => "{0}, {1} fps, {2}{3}, {4} profile", "{0}, {1} fps, {2}{3}, profil {4}",
        "{0}, {1} кадр/с, {2}{3}, профиль {4}", "{0}, {1} кадр/с, {2}{3}, профіль {4}";
    VIDEO_SUMMARY_BITRATE => " (≈ {0} kbps)", " (≈ {0} kbps)", " (≈ {0} кбит/с)", " (≈ {0} кбіт/с)";
    AUDIO_SUMMARY_PCM => "{0}, {1}, 16-bit – {2}", "{0}, {1}, 16 biți – {2}", "{0}, {1}, 16 бит — {2}",
        "{0}, {1}, 16 біт — {2}";
    AUDIO_SUMMARY => "{0}, {1}, {2}kbps – {3}", "{0}, {1}, {2}kbps – {3}", "{0}, {1}, {2} кбит/с — {3}",
        "{0}, {1}, {2} кбіт/с — {3}";

    // ----- normalize() notes ------------------------------------------------
    NOTE_PCM_AVI_ONLY => "PCM audio is only available in AVI; using {0}.",
        "Sunetul PCM este disponibil doar în AVI; se folosește {0}.",
        "Звук PCM доступен только в AVI; используется {0}.",
        "Звук PCM доступний лише в AVI; використовується {0}.";
    NOTE_AAC_NEEDS_MF => "AAC needs Windows Media Foundation; using MP3.",
        "AAC necesită Windows Media Foundation; se folosește MP3.",
        "AAC требует Windows Media Foundation; используется MP3.",
        "AAC потребує Windows Media Foundation; використовується MP3.";
    NOTE_HEVC_MP4_ONLY => "HEVC is only written to MP4; using {0}.", "HEVC se scrie doar în MP4; se folosește {0}.",
        "HEVC записывается только в MP4; используется {0}.", "HEVC записується лише у MP4; використовується {0}.";
    NOTE_ENCODER_MISSING => "The selected {0} encoder is not available on this system; using OpenH264.",
        "Codificatorul {0} selectat nu este disponibil pe acest sistem; se folosește OpenH264.",
        "Выбранный кодировщик {0} недоступен в этой системе; используется OpenH264.",
        "Обраний кодувальник {0} недоступний у цій системі; використовується OpenH264.";

    // ----- mini bar --------------------------------------------------------
    BAR_RECORDING_AREA => "Recording Area", "Zona înregistrată", "Область записи", "Область запису";
    BAR_INPUTS => "Recorded inputs", "Intrări înregistrate", "Записываемые входы", "Записувані входи";
    BAR_DIMENSIONS => "Dimensions", "Dimensiuni", "Размеры", "Розміри";
    BAR_PICK => "Pick…", "Alege…", "Выбрать…", "Обрати…";
    BAR_SELECT => "Select…", "Alege…", "Выбрать…", "Обрати…";
    BAR_DRAG_NEW_REGION => "Drag a new region", "Trage o regiune nouă", "Выделите новую область", "Виділіть нову область";
    BAR_REFRESH_TIP => "Refresh monitors and windows", "Reîmprospătează monitoarele și ferestrele",
        "Обновить мониторы и окна", "Оновити монітори та вікна";
    BAR_FORMAT_TIP => "Format settings – {0} / {1}", "Setări de format – {0} / {1}", "Настройки формата — {0} / {1}",
        "Налаштування формату — {0} / {1}";
    BAR_STARTING_IN => "Starting in {0}", "Începe în {0}", "Начало через {0}", "Початок через {0}";
    BAR_ESC_CANCELS => "Esc cancels", "Esc anulează", "Esc — отмена", "Esc — скасувати";
    BAR_READY => "Ready", "Gata", "Готово", "Готово";
    BAR_PICK_SOURCE => "Pick a source", "Alege o sursă", "Выберите источник", "Оберіть джерело";

    // ----- runtime status notes ---------------------------------------------
    NOTE_CAPTURE_BORDER_UNSUPPORTED => "this Windows version cannot hide the capture border",
        "această versiune de Windows nu poate ascunde chenarul de captură",
        "эта версия Windows не может скрыть рамку захвата",
        "ця версія Windows не може приховати рамку захоплення";
    NOTE_CAPTURE_CURSOR_UNSUPPORTED => "this Windows version cannot hide the cursor in the capture",
        "această versiune de Windows nu poate ascunde cursorul în captură",
        "эта версия Windows не может скрыть курсор в записи",
        "ця версія Windows не може приховати курсор у записі";
    MODE_GAME => "Game", "Joc", "Игра", "Гра";
    GAME_WAITING => "Waiting for a game…", "Se așteaptă un joc…", "Ожидание игры…", "Очікування гри…";
    GAME_NOT_ARMED => "Game mode is off", "Modul joc este oprit", "Игровой режим выключен", "Ігровий режим вимкнено";
    GAME_WINDOWS_ONLY => "Game recording is Windows-only",
        "Înregistrarea jocurilor este doar pentru Windows",
        "Запись игр доступна только в Windows",
        "Запис ігор доступний лише у Windows";
    GAME_HOOKED_LABEL => "{0} — {1}, {2} fps", "{0} — {1}, {2} fps", "{0} — {1}, {2} кадр/с",
        "{0} — {1}, {2} кадр/с";
    TAB_GAME => "Game", "Joc", "Игра", "Гра";
    SECTION_FPS_COUNTER => "In-game frame rate", "Cadre pe secundă în joc", "Частота кадров в игре",
        "Частота кадрів у грі";
    CHK_FPS_COUNTER => "Show the counter in the game",
        "Afișează contorul în joc",
        "Показывать счётчик в игре",
        "Показувати лічильник у грі";
    CHK_FPS_IN_RECORDING => "      Also record it", "      Include-l și în înregistrare", "      Записывать его тоже",
        "      Записувати його також";
    FPS_COUNTER_FOOTNOTE => "Green while armed, red while recording — the only way to tell what openclip is doing without leaving a fullscreen game. Off by default in the recording itself.",
        "Verde cât timp e pregătit, roșu în timpul înregistrării — singurul mod de a ști ce face openclip fără să ieși dintr-un joc pe tot ecranul. Implicit nu apare în înregistrare.",
        "Зелёный — готов, красный — идёт запись: единственный способ понять, что делает openclip, не выходя из полноэкранной игры. В саму запись по умолчанию не попадает.",
        "Зелений — готовий, червоний — триває запис: єдиний спосіб бачити стан openclip, не виходячи з повноекранної гри. Типово в сам запис не потрапляє.";
    SECTION_GAME_CAPTURE => "Game capture", "Captura jocurilor", "Захват игр", "Захоплення ігор";
    GAME_STATUS => "Status", "Stare", "Состояние", "Стан";
    GAME_MODE_ROW => "Game mode", "Mod joc", "Игровой режим", "Ігровий режим";
    GAME_HOOK_VERSION => "Hook", "Componentă", "Компонент", "Компонент";
    HOOK_MISSING => "The game-capture component is missing.",
        "Componenta pentru captura jocurilor lipsește.",
        "Компонент захвата игр отсутствует.",
        "Компонент захоплення ігор відсутній.";
    HOOK_REPAIRING => "Restoring the game-capture component… {0}",
        "Se restaurează componenta pentru captura jocurilor… {0}",
        "Восстановление компонента захвата игр… {0}",
        "Відновлення компонента захоплення ігор… {0}";
    HOOK_REPAIRED_RESTART => "Restored — restart openclip to use Game mode.",
        "Restaurată — repornește openclip pentru modul Joc.",
        "Восстановлен — перезапустите openclip для игрового режима.",
        "Відновлено — перезапустіть openclip для ігрового режиму.";
    HOOK_REPAIR_FAILED => "Could not restore it: {0}",
        "Nu s-a putut restaura: {0}",
        "Не удалось восстановить: {0}",
        "Не вдалося відновити: {0}";
    HOOK_REPAIR_BUTTON => "Repair", "Repară", "Восстановить", "Відновити";
    GAME_HOOK_LOG => "Hook log", "Jurnalul componentei", "Журнал компонента", "Журнал компонента";
    GAME_IGNORE_ROW => "This application", "Această aplicație", "Это приложение", "Цей застосунок";
    GAME_IGNORE => "Never hook", "Nu te atașa niciodată", "Никогда не подключаться", "Ніколи не підключатися";
    GAME_ANTICHEAT_FOOTNOTE => "openclip loads a small component into the game to take frames from it. It refuses any game running an anti-cheat, and there is no way to override that — competitive online games should be recorded with Monitor mode instead.",
        "openclip încarcă o componentă mică în joc pentru a prelua cadre. Refuză orice joc cu anti-cheat, fără excepții — jocurile online competitive ar trebui înregistrate cu modul Monitor.",
        "openclip загружает в игру небольшой компонент, чтобы забирать кадры. Он отказывается работать с играми, где есть анти-чит, и обойти это нельзя — соревновательные онлайн-игры записывайте в режиме «Монитор».",
        "openclip завантажує у гру невеликий компонент, щоб отримувати кадри. Він відмовляється працювати з іграми, де є античіт, і обійти це неможливо — змагальні онлайн-ігри записуйте в режимі «Монітор».";
    MSG_GAME_HOOKED_CANNOT_UPDATE => "stop Game mode before updating; the game-capture component is in use",
        "oprește modul joc înainte de actualizare; componenta de captură este în uz",
        "остановите игровой режим перед обновлением: компонент захвата используется",
        "зупиніть ігровий режим перед оновленням: компонент захоплення використовується";
    GAME_CONSENT_TITLE => "About Game mode", "Despre modul joc", "О игровом режиме", "Про ігровий режим";
    GAME_CONSENT_BODY => "To record a game at the source, openclip loads a small component (openclip_hook64.dll) into it. That component takes frames from the game's own picture and draws the frame-rate counter you see in the corner. It is loaded with a documented Windows API, is named plainly, and does nothing to hide itself.",
        "Pentru a înregistra un joc de la sursă, openclip încarcă în el o componentă mică (openclip_hook64.dll). Aceasta preia cadre din imaginea jocului și desenează contorul de cadre din colț. Este încărcată printr-un API Windows documentat, are un nume clar și nu face nimic pentru a se ascunde.",
        "Чтобы записывать игру из источника, openclip загружает в неё небольшой компонент (openclip_hook64.dll). Он забирает кадры из картинки самой игры и рисует счётчик кадров в углу. Компонент загружается документированным API Windows, назван прямо и никак себя не скрывает.",
        "Щоб записувати гру з джерела, openclip завантажує в неї невеликий компонент (openclip_hook64.dll). Він бере кадри з зображення самої гри та малює лічильник кадрів у кутку. Компонент завантажується документованим API Windows, має прозору назву й ніяк себе не приховує.";
    GAME_CONSENT_ANTICHEAT => "Games running an anti-cheat are refused outright and cannot be overridden. Even so, loading anything into an online game carries a risk to your account — record those with Monitor mode.",
        "Jocurile cu anti-cheat sunt refuzate complet, fără excepții. Chiar și așa, încărcarea a ceva într-un joc online implică un risc pentru contul tău — înregistrează-le cu modul Monitor.",
        "Игры с анти-читом отклоняются полностью, без исключений. И всё же загрузка чего-либо в онлайн-игру создаёт риск для вашей учётной записи — записывайте их в режиме «Монитор».",
        "Ігри з античітом відхиляються повністю, без винятків. Проте завантаження будь-чого в онлайн-гру несе ризик для вашого облікового запису — записуйте їх у режимі «Монітор».";
    GAME_CONSENT_ACCEPT => "I understand", "Am înțeles", "Понятно", "Зрозуміло";
    GAME_STATE_READY => "Ready", "Pregătit", "Готов", "Готовий";
    GAME_STATE_RECORDING => "Recording", "Se înregistrează", "Идёт запись", "Триває запис";
    FPS_PREVIEW_HINT => "Shown larger than it will appear in the game.",
        "Afișat mai mare decât va apărea în joc.",
        "Показан крупнее, чем будет в игре.",
        "Показаний більшим, ніж буде у грі.";
    MSG_GAME_NO_LOG => "the hook has not written a log yet",
        "componenta nu a scris încă un jurnal",
        "компонент ещё не записал журнал",
        "компонент ще не записав журнал";
    GAME_ARM => "Arm", "Activează", "Включить", "Увімкнути";
    GAME_DISARM => "Stop", "Oprește", "Остановить", "Зупинити";
    MSG_GAME_DLL_MISSING => "the game-capture component is missing; reinstall openclip",
        "componenta de captură a jocurilor lipsește; reinstalează openclip",
        "компонент захвата игр отсутствует; переустановите openclip",
        "компонент захоплення ігор відсутній; перевстановіть openclip";
    MSG_GAME_REFUSED_ANTICHEAT => "{0} is protected by {1}; openclip will not hook it",
        "{0} este protejat de {1}; openclip nu se va atașa",
        "{0} защищён системой {1}; openclip не будет подключаться",
        "{0} захищено системою {1}; openclip не підключатиметься";
    MSG_GAME_REFUSED_X86 => "{0} is 32-bit; only 64-bit games can be recorded this way",
        "{0} este pe 32 de biți; doar jocurile pe 64 de biți pot fi înregistrate astfel",
        "{0} 32-битная; так можно записывать только 64-битные игры",
        "{0} 32-бітна; так можна записувати лише 64-бітні ігри";
    MSG_GAME_REFUSED_D3D9 => "{0} uses Direct3D 9, which is not supported",
        "{0} folosește Direct3D 9, care nu este acceptat",
        "{0} использует Direct3D 9, который не поддерживается",
        "{0} використовує Direct3D 9, який не підтримується";
    MSG_GAME_REFUSED_PROTECTED => "{0} cannot be inspected; it may be running as administrator",
        "{0} nu poate fi inspectat; poate rulează ca administrator",
        "{0} нельзя проверить; возможно, запущен от имени администратора",
        "{0} неможливо перевірити; можливо, запущено від імені адміністратора";
    NOTE_GAME_SLOW_TRANSPORT => "this game's frames go through system memory, which costs frame rate",
        "cadrele acestui joc trec prin memoria de sistem, ceea ce costă cadre pe secundă",
        "кадры этой игры идут через системную память, что снижает частоту кадров",
        "кадри цієї гри проходять через системну пам'ять, що знижує частоту кадрів";
    NOTE_GAME_DIFFERENT_ADAPTER => "the game runs on another GPU; capture may be slower",
        "jocul rulează pe alt GPU; captura poate fi mai lentă",
        "игра работает на другом GPU; захват может быть медленнее",
        "гра працює на іншому GPU; захоплення може бути повільнішим";
    NOTE_SOURCE_RESIZED => "the source changed size; fitted into {0}×{1}",
        "sursa și-a schimbat dimensiunea; încadrată în {0}×{1}",
        "источник изменил размер; вписан в {0}×{1}",
        "джерело змінило розмір; вписано в {0}×{1}";
    NOTE_SYSTEM_AUDIO_UNAVAILABLE => "system audio unavailable: {0}", "sunetul de sistem nu este disponibil: {0}",
        "системный звук недоступен: {0}", "системний звук недоступний: {0}";
    NOTE_MIC_UNAVAILABLE => "microphone unavailable: {0}", "microfonul nu este disponibil: {0}",
        "микрофон недоступен: {0}", "мікрофон недоступний: {0}";
    NOTE_AAC_UNAVAILABLE => "AAC unavailable ({0}); recorded MP3", "AAC indisponibil ({0}); s-a înregistrat MP3",
        "AAC недоступен ({0}); записан MP3", "AAC недоступний ({0}); записано MP3";
    NOTE_AAC_NEEDS_WINDOWS => "AAC needs Windows; recorded MP3", "AAC necesită Windows; s-a înregistrat MP3",
        "AAC требует Windows; записан MP3", "AAC потребує Windows; записано MP3";
    NOTE_ENCODER_NOT_FOUND => "the selected {0} encoder was not found", "codificatorul {0} selectat nu a fost găsit",
        "выбранный кодировщик {0} не найден", "обраний кодувальник {0} не знайдено";
    NOTE_ENCODER_FAILED => "{0} failed ({1})", "{0} a eșuat ({1})", "сбой {0} ({1})", "збій {0} ({1})";
    NOTE_USING_ENCODER => "{0}; using {1}", "{0}; se folosește {1}", "{0}; используется {1}", "{0}; використовується {1}";
    NOTE_FELL_BACK_OPENH264 => "{0}; recorded H.264 with OpenH264", "{0}; s-a înregistrat H.264 cu OpenH264",
        "{0}; записан H.264 через OpenH264", "{0}; записано H.264 через OpenH264";
    NOTE_HW_NEEDS_WINDOWS => "{0} hardware encoding needs Windows", "codificarea hardware {0} necesită Windows",
        "аппаратное кодирование {0} требует Windows", "апаратне кодування {0} потребує Windows";
    NOTE_NO_HARDWARE_ENCODER => "no hardware H.264 encoder was found; recording with the CPU",
        "nu s-a găsit niciun codificator H.264 hardware; se înregistrează cu procesorul",
        "аппаратный кодировщик H.264 не найден; запись выполняется процессором",
        "апаратний кодувальник H.264 не знайдено; запис виконується процесором";
    NOTE_SOFTWARE_TOO_SLOW => "the CPU encoder cannot keep up at {0}×{1} @ {2} fps; try Half size or a lower frame rate",
        "codificatorul pe procesor nu face față la {0}×{1} @ {2} fps; încearcă Jumătate sau o rată mai mică",
        "процессорный кодировщик не справляется при {0}×{1} @ {2} к/с; попробуйте половинный размер или меньшую частоту",
        "процесорний кодувальник не встигає при {0}×{1} @ {2} к/с; спробуйте половинний розмір або меншу частоту";

    // ----- media viewer ----------------------------------------------------
    VIEWER_BACK => "Library", "Bibliotecă", "Библиотека", "Бібліотека";
    VIEWER_CLOSE_TIP => "Close (Esc)", "Închide (Esc)", "Закрыть (Esc)", "Закрити (Esc)";
    VIEWER_OPEN_EXTERNAL => "Open in system player", "Deschide în playerul sistemului",
        "Открыть в системном плеере", "Відкрити у системному плеєрі";
    VIEWER_REVEAL_TIP => "Show in folder", "Arată în folder", "Показать в папке", "Показати в папці";
    VIEWER_PLAY_TIP => "Play (Space)", "Redă (Space)", "Воспроизвести (пробел)", "Відтворити (пробіл)";
    VIEWER_PAUSE_TIP => "Pause (Space)", "Pauză (Space)", "Пауза (пробел)", "Пауза (пробіл)";
    VIEWER_STEP_BACK => "Previous frame", "Cadrul anterior", "Предыдущий кадр", "Попередній кадр";
    VIEWER_STEP_FORWARD => "Next frame", "Cadrul următor", "Следующий кадр", "Наступний кадр";
    VIEWER_SNAPSHOT_TIP => "Save this frame as PNG", "Salvează acest cadru ca PNG",
        "Сохранить этот кадр в PNG", "Зберегти цей кадр як PNG";
    VIEWER_SNAPSHOT_LOST => "the decoder stopped before the frame was saved",
        "decodorul s-a oprit înainte de salvarea cadrului",
        "декодер остановился до сохранения кадра",
        "декодер зупинився до збереження кадра";
    VIEWER_VOLUME => "Volume", "Volum", "Громкость", "Гучність";
    VIEWER_MUTE => "Mute", "Fără sunet", "Выключить звук", "Вимкнути звук";
    VIEWER_UNMUTE => "Unmute", "Cu sunet", "Включить звук", "Увімкнути звук";
    VIEWER_FIT => "Fit to window", "Încadrează în fereastră", "Вписать в окно", "Вписати у вікно";
    VIEWER_ACTUAL_SIZE => "Actual size", "Dimensiune reală", "Реальный размер", "Реальний розмір";
    VIEWER_OPENING => "Opening…", "Se deschide…", "Открытие…", "Відкриття…";
    VIEWER_ENDED => "End of file", "Sfârșitul fișierului", "Конец файла", "Кінець файлу";
    VIEWER_NO_PLAYBACK => "In-app playback is only available on Windows.",
        "Redarea în aplicație este disponibilă doar pe Windows.",
        "Воспроизведение внутри приложения доступно только в Windows.",
        "Відтворення всередині програми доступне лише у Windows.";
    VIEWER_FAILED => "This file cannot be played here: {0}",
        "Acest fișier nu poate fi redat aici: {0}", "Этот файл нельзя воспроизвести здесь: {0}",
        "Цей файл неможливо відтворити тут: {0}";
    VIEWER_NO_AUDIO_DEVICE => "No audio output device — playing without sound.",
        "Niciun dispozitiv de ieșire audio — se redă fără sunet.",
        "Нет устройства вывода звука — воспроизведение без звука.",
        "Немає пристрою виводу звуку — відтворення без звуку.";
    VIEWER_UNKNOWN_LENGTH => "unknown length", "durată necunoscută", "длительность неизвестна",
        "тривалість невідома";
    VIEWER_NO_IMAGE => "This picture could not be opened: {0}",
        "Această imagine nu a putut fi deschisă: {0}", "Не удалось открыть это изображение: {0}",
        "Не вдалося відкрити це зображення: {0}";

    // ----- region picker ---------------------------------------------------
    PICKER_TITLE => "Select region — drag to select, Esc to cancel",
        "Alege regiunea — trage pentru a selecta, Esc pentru anulare",
        "Выбор области — выделите мышью, Esc для отмены",
        "Вибір області — виділіть мишею, Esc для скасування";
    PICKER_HINT => "Drag to select a region · Esc to cancel", "Trage pentru a selecta o regiune · Esc pentru anulare",
        "Выделите область мышью · Esc для отмены", "Виділіть область мишею · Esc для скасування";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_resolves_in_every_language() {
        for key in ALL_KEYS {
            for lang in Lang::ALL {
                assert!(lookup(lang, key).is_some(), "{key} missing for {}", lang.code());
                assert!(!lookup(lang, key).unwrap().is_empty(), "{key} empty for {}", lang.code());
            }
        }
    }

    #[test]
    fn translations_keep_their_placeholders() {
        for key in ALL_KEYS {
            let en = lookup(Lang::En, key).unwrap();
            for i in 0..6 {
                let ph = format!("{{{i}}}");
                let expected = en.contains(&ph);
                for lang in [Lang::Ro, Lang::Ru, Lang::Uk] {
                    assert_eq!(
                        lookup(lang, key).unwrap().contains(&ph),
                        expected,
                        "{key}: {ph} mismatch in {}",
                        lang.code()
                    );
                }
            }
        }
    }

    #[test]
    fn language_codes_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
        assert_eq!(Lang::from_code("ro-MD"), Some(Lang::Ro));
        assert_eq!(Lang::from_code("ru-RU"), Some(Lang::Ru));
        assert_eq!(Lang::from_code("uk-UA"), Some(Lang::Uk));
        assert_eq!(Lang::from_code("de"), None);
    }

    #[test]
    fn tf_substitutes_positionally() {
        set_lang(Lang::En);
        assert_eq!(tf(key::MSG_DELETED, &["a.mp4"]), "Deleted a.mp4");
        set_lang(Lang::Ro);
        assert_eq!(tf(key::MSG_DELETED, &["a.mp4"]), "Șters: a.mp4");
        set_lang(Lang::Ru);
        assert_eq!(tf(key::MSG_DELETED, &["a.mp4"]), "Удалено: a.mp4");
        set_lang(Lang::Uk);
        assert_eq!(tf(key::MSG_DELETED, &["a.mp4"]), "Видалено: a.mp4");
        set_lang(Lang::En);
    }
}
