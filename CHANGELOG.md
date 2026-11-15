# Changelog

All notable changes to OpenClip in this fork compared to the original project ([catalingrigoriev285/openclip](https://github.com/catalingrigoriev285/openclip)) are documented in this file.

---

## [0.3.0] — 2026-11-15

### English

#### Added
- **Crash-Resistant Fragmented MP4 (`ISO BMFF fMP4`)**:
  - Migrated MP4 multiplexer to progressive fragmented ISO BMFF format (`ftyp` + initial `moov` with `mvex`/`trex`, followed by sequential `moof` + `mdat` fragments with progressive disk `flush()`).
  - New fragments start on keyframes (IDRs) with duration threshold $\ge 1.5$ s and safety flush at $\ge 3.0$ s.
  - Complete crash resilience: unfinalized recordings (due to power cuts, game crashes, or unexpected process termination) remain intact and immediately playable in standard media players (VLC, Windows Media Player) without data corruption.
  - Generates optional `mfra` index on clean finalization for instant seeking.
- **Hardware GPU Overlay Compositing (Direct3D 11 HLSL)**:
  - Cursor halo highlight, animated click ripple rings, cursor pointer, and watermark overlays are now composited entirely in GPU VRAM via dedicated Direct3D 11 HLSL vertex and pixel shaders (`GpuOverlayCompositor`).
  - Analytical SDF antialiasing for cursor highlight and smooth ripple rings without bitmap artifacts.
  - Eliminates CPU readback fallback when overlays are enabled: recording at 1440p120 with mouse effects and watermark operates on the 100% hardware zero-readback GPU path with CPU usage <2%.
  - Live preview in settings now renders GPU overlays in real-time.
- **Dynamic Custom Text Watermark**:
  - Replaced static watermark image with dynamic RGBA rasterization powered by the bundled high-legibility **Inter** font with full Latin and Cyrillic character support.
  - Renders a sleek translucent pill container that automatically scales to text length. Preserves classic branded badge when default `"openclip"` is used.
  - New settings field in Video $\rightarrow$ Watermark with instant live preview on every keystroke, 48-character limit, and one-click reset to default.
- **Audio Hot-Plug & Automatic Stream Recovery**:
  - Integrated Windows Core Audio device notifications (`IMMNotificationClient`) to instantly detect device arrival, removal, state changes, and default audio endpoint switches.
  - Seamless CPAL stream recreation for loopback playback and microphone input with ~300 ms debounce for Windows USB driver initialization and 1 s retry interval.
  - Recording timeline origin remains strictly anchored during disconnects; mixer automatically pads absent device periods with smooth silence maintaining perfect audio-video synchronization.
  - Dynamic `KaiserSincResampler` re-initialization and channel mapping via `Mixer::update_source` when reconnected device changes sample rate (e.g. 48 kHz $\leftrightarrow$ 44.1 kHz) or channel count.

---

### Русский

#### Добавлено
- **Устойчивый к сбоям фрагментированный MP4 (`ISO BMFF fMP4`)**:
  - Перевод MP4-мультиплексора на прогрессивный фрагментированный формат ISO BMFF (`ftyp` + инициализационный `moov` с блоками `mvex`/`trex`, затем периодические фрагменты `moof` + `mdat` с непрерывным сбросом на диск `flush()`).
  - Нарезка фрагментов по ключевым кадрам (IDR) с порогом $\ge 1.5$ с и аварийным сбросом $\ge 3.0$ с.
  - Полная защита от потери записей: при внезапном отключении питания, краше игры или принудительном завершении процесса файл не повреждается и открывается стандартными плеерами (VLC, Проигрыватель Windows).
  - Запись индекса `mfra` при штатном завершении для мгновенной перемотки.
- **Аппаратный GPU-композитинг оверлеев (Direct3D 11 HLSL)**:
  - Подсветка курсора, анимированные кольца кликов мыши, стрелка указателя и водяной знак теперь накладываются прямо в видеопамяти через пиксельные и вершинные HLSL-шейдеры Direct3D 11 (`GpuOverlayCompositor`).
  - Аналитическое SDF-сглаживание круга подсветки и колец кликов без ступенек и артефактов растеризации.
  - Устранено падение в CPU-память при включении эффектов: запись 1440p120 с подсветкой, кликами и плашкой идет по аппаратному конвейеру с нагрузкой на CPU <2%.
  - Встроенный предпросмотр в настройках отображает оверлеи мыши и водяного знака в реальном времени.
- **Кастомный текстовый водяной знак**:
  - Динамическая растеризация плашки с использованием встроенного шрифта **Inter** с полной поддержкой кириллицы и латиницы.
  - Автоматическая подгонка ширины аккуратной полупрозрачной плашки со скруглёнными углами под длину текста; сохранение оригинального фирменного стиля при значении `"openclip"`.
  - Поле ввода в разделе «Видео → Водяной знак» с мгновенным предпросмотром на каждый символ, лимитом в 48 символов и кнопкой быстрого сброса.
- **Горячая смена и автовосстановление звуковых устройств**:
  - Интеграция наблюдателя Windows Core Audio (`IMMNotificationClient`) для отслеживания подключения, отключения и смены системных устройств вывода и ввода по умолчанию.
  - Бесшовное пересоздание потоков CPAL (loopback и микрофон) с задержкой debounce ~300 мс (необходимой для инициализации USB-драйвера Windows) и повторными попытками раз в 1 с.
  - Таймлайн записи не сдвигается: на время отсутствия устройства микшер выдает плавную тишину, сохраняя идеальную синхронизацию звука и видео.
  - Автоматическая переинициализация sinc-ресемплера Кайзера (`KaiserSincResampler`) и раскладки каналов при смене частоты (например, 48 кГц $\leftrightarrow$ 44.1 кГц) без сброса таймлайна.

---

## [0.2.1] — 2026-11-15

### English

#### Fixed
- **Global Hotkeys Responsiveness**:
  - Fixed: global hotkeys did not respond while the main window was covered or minimized.
  - Implemented `eframe::App::logic` event processing and proactive Win32 message queue wake-up (`RedrawWindow` + `PostMessage`) bypassing Windows DWM occlusion and minimization throttling.
  - Desktop freeze-frame capture is now taken synchronously at the exact moment of pressing `Ctrl+Alt+Q`.

---

### Русский

#### Исправлено
- **Отзывчивость глобальных горячих клавиш**:
  - Исправлено: глобальные горячие клавиши не срабатывали, пока главное окно было перекрыто другим окном или свёрнуто.
  - Реализована фоновая обработка в `eframe::App::logic` и принудительное пробуждение Win32 (`RedrawWindow` + `PostMessage`) для обхода троттлинга DWM Windows при перекрытии или минимизации окна.
  - Замороженный снимок экрана теперь фиксируется синхронно непосредственно в момент нажатия `Ctrl+Alt+Q`.

---

## [0.2.0] — 2026-11-15

### English

#### Added
- **Interactive Screenshot Editor (`Ctrl+Alt+Q`)**:
  - Desktop freeze-frame overlay with multi-monitor DPI support.
  - Interactive vector annotations: rectangle, arrow, straight line, freehand pen, highlighter/marker, text labels, and pixelate blur.
  - Interactive move and resize handles for selections and vector shapes.
  - Multi-level Undo (`Ctrl+Z`), direct save to PNG, and instant copy to clipboard (`Ctrl+C` / Enter).
  - Robust window lifecycle: subclassed message handling (`WM_NCDESTROY` cleanup) and graceful focus loss deactivation.
- **Global Recording Hotkey (`Ctrl+Alt+R`) with Sound Feedback**:
  - Low-level keyboard hook enabling hotkey triggering from full-screen games and background apps.
  - Audio feedback beeps on recording start and stop that do not bleed into the captured audio tracks.
  - User-configurable feedback modes in settings: *Off*, *Hotkey only*, or *Always*.
- **Zero-Readback GPU Color Conversion**:
  - Direct3D 11 Video Processor (`ID3D11VideoProcessor`) performing hardware `BGRA` $\rightarrow$ `NV12` color conversion entirely inside GPU VRAM.
  - Zero-copy texture streaming into Windows Media Foundation hardware encoders (verified on AMD AMF).
  - Color conversion pipeline selector in Video Settings: *Auto* (prefers GPU, falls back to CPU), *GPU* (enforces hardware pipeline), or *CPU* (SIMD YUV).
- **Adaptive Frame Clock ("Auto" FPS)**:
  - Dynamically aligns recording cadence to display refresh rate (vsync grid).
  - Suppresses redundant duplicate frame encoding on static desktop screens, drastically reducing file size and encoder load.
- **Advanced Audio Controls**:
  - Independent audio device selection for loopback playback and microphone input.
  - Software gain amplification up to 200%.
  - Separate audio tracks recording mode for independent post-processing.
- **Hardware GPU Decoding in Media Player**:
  - Built-in video player accelerates preview playback via Direct3D 11 NV12-to-RGBA hardware rendering.
- **Localization**:
  - Added Ukrainian (`uk`) language support.
  - Completed and verified English, Romanian, and Russian localizations.
- **Diagnostics & Test Suite**:
  - Structured runtime diagnostics logged to `openclip.log`.
  - Added 178 integration tests covering container muxers (MP4 and OpenDML AVI roundtrips), audio synchronization, and hotkey sequencing.

#### Improved
- **High-Fidelity Audio Resampling**:
  - Upgraded from simple linear resampling to a 16-tap Kaiser-windowed sinc resampler (`KaiserSincResampler`, $\beta=6.0$), eliminating aliasing distortion across 44.1 kHz and 48 kHz rates.
- **Performance & Quality Presets**:
  - Expanded constant bitrate (CBR) settings up to 100,000 kbps (100 Mbps).
  - Added high framerate presets up to 144 FPS.

#### Removed
- **Network Code & Auto-Updater**:
  - Completely removed GitHub update checks, automatic background downloaders, and online sidecar repair routines.
  - The application performs **0 network calls** and operates strictly offline.
  - Removed unused dependencies (`ureq`, `self-replace`).

---

### Русский

#### Добавлено
- **Интерактивный редактор скриншотов (`Ctrl+Alt+Q`)**:
  - Мгновенная «заморозка» экрана с корректным масштабированием на нескольких мониторах с разным DPI.
  - Набор векторных инструментов рисования: прямоугольник, стрелка, прямая линия, перо, маркер/выделитель, текстовые блоки и размытие (пикселизация).
  - Перемещение и изменение размера выделенной области и векторных элементов.
  - Многоуровневая отмена действий (`Ctrl+Z`), прямое сохранение в PNG и мгновенное копирование в буфер обмена (`Ctrl+C` / Enter).
  - Безопасное управление жизненным циклом окна: подклассирование с очисткой дескрипторов на `WM_NCDESTROY` и корректное закрытие при потере фокуса.
- **Глобальная горячая клавиша записи (`Ctrl+Alt+R`) со звуковыми сигналами**:
  - Низкоуровневый перехватчик клавиатуры для срабатывания из полноэкранных игр и любых окон.
  - Звуковые сигналы при старте и остановке записи, изолированные от итоговой дорожки.
  - Режимы обратной связи в настройках: *Отключено*, *Только по хоткею* или *Всегда*.
- **GPU-конвертация цвета без копирования в память**:
  - Использование Direct3D 11 Video Processor (`ID3D11VideoProcessor`) для аппаратного преобразования `BGRA` $\rightarrow$ `NV12` целиком в видеопамяти.
  - Подача текстур без копирования в системную память в аппаратные кодировщики Windows Media Foundation (протестировано на AMD AMF).
  - Выбор режима конвертации в настройках видео: *Авто* (предпочитает GPU с откатом на CPU), *GPU* (только видеокарта) или *CPU* (SIMD YUV).
- **Режим «Авто» FPS и адаптивная сетка кадров**:
  - Подстройка шага кадров под частоту обновления монитора (сетка vsync).
  - Подавление кодирования одинаковых кадров на статичном рабочем столе для экономии места на диске и ресурсов процессора.
- **Расширенные настройки звука**:
  - Выбор конкретных устройств воспроизведения (loopback) и микрофона.
  - Программное усиление громкости до 200 %.
  - Режим сохранения системного звука и микрофона в отдельные дорожки.
- **Аппаратное GPU-декодирование в плеере**:
  - Ускоренное воспроизведение роликов во встроенном плеере через Direct3D 11 NV12-в-RGBA конвертацию.
- **Локализация**:
  - Добавлен украинский язык интерфейса (`uk`).
  - Дополнены и выверены переводы на русский, английский и румынский языки.
- **Диагностика и автотесты**:
  - Запись диагностических сообщений в файл `openclip.log`.
  - Добавлено 178 интеграционных тестов для проверки муксеров MP4 и AVI, аудиосинхронизации и последовательности горячих клавиш.

#### Улучшено
- **Качественный sinc-ресемплер звука**:
  - Переход на 16-отводный оконный sinc-фильтр Кайзера (`KaiserSincResampler`, $\beta=6.0$), устраняющий искажения при передискретизации между 44.1 кГц и 48 кГц.
- **Битрейты и частота кадров**:
  - Максимальный битрейт увеличен до 100 000 кбит/с (100 Мбит/с).
  - Добавлены пресеты частоты кадров вплоть до 144 FPS.

#### Удалено
- **Сетевой код и модуль автообновления**:
  - Вырезаны проверка версий на GitHub, фоновое скачивание и сетевое восстановление библиотек.
  - Программа не делает ни одного сетевого вызова и работает полностью офлайн.
  - Удалены неиспользуемые зависимости (`ureq`, `self-replace`).
