export type Lang = "ar" | "en";

const ar = {
  appName: "محوّل أوروك",
  appNameEn: "URUK CONVERT",
  tagline: "اسحب ملفًا إلى قرص الشمس وحوّله إلى الصيغة التي تحتاجها — كل شيء على جهازك، دون رفع أي ملف.",
  dropHere: "أفلت الملفات هنا",
  dropHint: "ملف واحد، ملفات متعددة، أو مجلد كامل — أو اختر من الجهاز",
  browse: "اختيار ملفات",
  browseFolder: "اختيار مجلد",
  queue: "قائمة التحويل",
  empty: "لا ملفات بعد. اسحب أي ملف للبدء.",
  convert: "تحويل",
  convertAll: "تحويل الكل",
  cancel: "إلغاء",
  remove: "إزالة",
  reveal: "إظهار في المجلد",
  open: "فتح",
  extract: "فكّ الضغط",
  compress: "ضغط الصورة",
  settings: "الإعدادات",
  language: "اللغة",
  outputFolder: "مجلد الإخراج",
  sameAsSource: "نفس مجلد المصدر",
  customFolder: "مجلد مخصص",
  choose: "اختيار…",
  fileSuffix: "لاحقة اسم الملف",
  suffixConverted: "‎.converted (مثل: photo.converted.jpg)",
  suffixNone: "بدون لاحقة (في المجلد المخصص فقط)",
  imageQuality: "جودة الصور (JPG/WebP)",
  wheelBehavior: "سلوك العجلة",
  wheelBehaviorNote: "تعمل العجلة في كل النظام: اسحب ملفًا في أي مكان واضغط Shift أثناء السحب لتظهر العجلة عند المؤشر، ثم أفلت فوق الصيغة المطلوبة. هذه النافذة للسجل والإعدادات والإفلات الاحتياطي فقط.",
  toolsTitle: "الأدوات المدمجة",
  toolsNote: "تعمل كل التحويلات محليًا بأدوات مدمجة: FFmpeg للفيديو والصوت، ImageMagick للصور، 7-Zip للأرشيف.",
  localOnly: "ملفاتك لا تغادر جهازك أبدًا — لا يوجد خادم ولا رفع.",
  statusQueued: "بالانتظار",
  statusRunning: "جارٍ التحويل…",
  statusDone: "تم",
  statusError: "خطأ",
  statusCancelled: "أُلغي",
  compressTitle: "ضغط الصورة",
  originalSize: "الحجم الأصلي",
  mode: "الوضع",
  percentMode: "نسبة مئوية",
  targetMode: "حجم مستهدف",
  reduceBy: "تقليل بمقدار",
  targetKb: "الحجم الأقصى (KB)",
  outputAtMost: "سيكون الناتج بحد أقصى",
  startCompress: "ابدأ الضغط",
  close: "إغلاق",
  esc: "إغلاق",
  toQueue: "إلى القائمة بدلًا من ذلك",
  familyImage: "صورة",
  familyAudio: "صوت",
  familyVideo: "فيديو",
  familyArchive: "أرشيف",
  familyPdf: "PDF",
  familyDocument: "مستند",
  familySlides: "عرض تقديمي",
  familySpreadsheet: "جدول بيانات",
  familySubtitle: "ترجمة",
  familyUnknown: "ملف",
  errGeneric: "حدث خطأ أثناء التحويل.",
  errToolMissing: "الأداة المطلوبة غير متوفرة في هذه النسخة.",
  errHeic: "تصدير HEIC غير متاح في الأدوات المدمجة على هذا النظام بعد (يمكن فتح ملفات HEIC وتحويلها لصيغ أخرى).",
  errNoAudio: "هذا الفيديو لا يحتوي مسارًا صوتيًا لتصديره كـ MP3.",
  errPdfSheet: "تحويل PDF إلى جدول بيانات غير متاح: ملف PDF يحتوي نصًا مرسومًا لا خلايا حقيقية، ولا توجد أداة مدمجة تستطيع إعادة بناء الخلايا بصدق.",
  errCancelled: "أُلغي التحويل.",
  agentTitle: "الوكيل في الخلفية",
  agentNote: "يعيش محوّل أوروك في شريط النظام/القائمة ويعمل دون نافذة. إغلاق هذه النافذة يخفيها فقط — أعد فتحها من الأيقونة في أي وقت.",
  summonKey: "مفتاح استدعاء العجلة أثناء السحب",
  toolsKey: "مفتاح عجلة الأدوات (مع مفتاح العجلة)",
  actionsKey: "مفتاح عجلة الإجراءات",
  keyShift: "Shift",
  keyControl: "Control",
  keyAlt: "Alt",
  autostart: "التشغيل تلقائيًا عند بدء النظام",
  sensing: "استشعار السحب على مستوى النظام",
  sessionTier: "طبقة الجلسة",
  onboardingTitle: "حوّل من أي مكان — دون فتح التطبيق",
  onboardingBody: "اسحب أي ملف في أي مكان من نظامك (من مدير الملفات أو سطح المكتب)، وأثناء السحب اضغط Shift — تظهر عجلة الصيغ عند المؤشر. أفلت الملف فوق الصيغة المطلوبة فيتحول فورًا، وتظهر النتيجة في نافذة صغيرة عائمة. Esc أثناء السحب يلغي العجلة.",
  onboardingPermMac: "في macOS: يحتاج «محوّل أوروك» إذن «إمكانية الوصول» (Accessibility) لاستشعار السحب — بدونه تبقى الإفلاتات داخل النافذة وقائمة Finder متاحة.",
  onboardingPermWayland: "في Wayland: لا يسمح المؤلّف (compositor) لأي تطبيق بمراقبة سحب التطبيقات الأخرى. حيث يُدعم ذلك تظهر كرة ثابتة للإفلات، وإلا استخدم هذه النافذة.",
  onboardingGotIt: "فهمت — ابدأ",
  historyTitle: "سجل التحويل",
  historyEmpty: "لا تحويلات بعد. جرّب: اسحب ملفًا في أي مكان واضغط Shift أثناء السحب.",
  historyOverlay: "العجلة",
  historyWindow: "النافذة",
  aboutLine: "محوّل أوروك — إعادة بناء نظيفة ومفتوحة المصدر، أول تطبيقات ثلاثية أوروك الثانية.",
};

const en: typeof ar = {
  appName: "Uruk Convert",
  appNameEn: "URUK CONVERT",
  tagline: "Drop a file onto the sun disc and turn it into the format you need — everything on your machine, nothing uploaded.",
  dropHere: "Drop files here",
  dropHint: "One file, many files, or a whole folder — or pick from your device",
  browse: "Choose files",
  browseFolder: "Choose folder",
  queue: "Conversion queue",
  empty: "No files yet. Drop any file to begin.",
  convert: "Convert",
  convertAll: "Convert all",
  cancel: "Cancel",
  remove: "Remove",
  reveal: "Show in folder",
  open: "Open",
  extract: "Extract",
  compress: "Compress image",
  settings: "Settings",
  language: "Language",
  outputFolder: "Output folder",
  sameAsSource: "Same as source",
  customFolder: "Custom folder",
  choose: "Choose…",
  fileSuffix: "File name suffix",
  suffixConverted: ".converted (e.g. photo.converted.jpg)",
  suffixNone: "No suffix (custom folder only)",
  imageQuality: "Image quality (JPG/WebP)",
  wheelBehavior: "Wheel behavior",
  wheelBehaviorNote: "The wheel works system-wide: drag a file anywhere and press Shift mid-drag — the wheel appears at the cursor; drop on a format to convert. This window is for history, settings and fallback drops only.",
  toolsTitle: "Bundled tools",
  toolsNote: "All conversions run locally with bundled tools: FFmpeg for video & audio, ImageMagick for images, 7-Zip for archives.",
  localOnly: "Your files never leave your machine — there is no server and no upload.",
  statusQueued: "Queued",
  statusRunning: "Converting…",
  statusDone: "Done",
  statusError: "Error",
  statusCancelled: "Cancelled",
  compressTitle: "Compress image",
  originalSize: "Original size",
  mode: "Mode",
  percentMode: "Percentage",
  targetMode: "Target size",
  reduceBy: "Reduce by",
  targetKb: "Maximum size (KB)",
  outputAtMost: "Output will be at most",
  startCompress: "Start compressing",
  close: "Close",
  esc: "Close",
  toQueue: "Send to queue instead",
  familyImage: "Image",
  familyAudio: "Audio",
  familyVideo: "Video",
  familyArchive: "Archive",
  familyPdf: "PDF",
  familyDocument: "Document",
  familySlides: "Slides",
  familySpreadsheet: "Spreadsheet",
  familySubtitle: "Subtitles",
  familyUnknown: "File",
  errGeneric: "Something went wrong during conversion.",
  errToolMissing: "The required tool is not available in this build.",
  errHeic: "HEIC export is not available in the bundled tools on this system yet (HEIC files can still be opened and converted to other formats).",
  errNoAudio: "This video has no audio track to export as MP3.",
  errPdfSheet: "PDF to spreadsheet is not available: a PDF holds drawn text, not real cells, and no bundled tool can honestly rebuild cells from it.",
  errCancelled: "Conversion cancelled.",
  agentTitle: "Background agent",
  agentNote: "Uruk Convert lives in the system tray / menu bar and works with no window open. Closing this window only hides it — reopen it from the tray icon any time.",
  summonKey: "Wheel summon key during a drag",
  toolsKey: "Tools wheel key (held with the wheel key)",
  actionsKey: "Actions wheel key",
  keyShift: "Shift",
  keyControl: "Control",
  keyAlt: "Alt",
  autostart: "Launch automatically at login",
  sensing: "System-wide drag sensing",
  sessionTier: "Session tier",
  onboardingTitle: "Convert from anywhere — no app window needed",
  onboardingBody: "Drag any file anywhere in your OS (file manager, desktop…), and while dragging press Shift — the format wheel appears at the cursor. Drop the file on a format and it converts instantly, with the result in a small floating toast. Esc during the drag cancels the wheel.",
  onboardingPermMac: "On macOS, Uruk Convert needs the Accessibility permission to sense drags — without it, in-window drops and the Finder menu remain available.",
  onboardingPermWayland: "On Wayland, the compositor does not let any app watch other apps' drags. Where supported, a standing orb accepts drops; otherwise use this window.",
  onboardingGotIt: "Got it — start",
  historyTitle: "Conversion history",
  historyEmpty: "No conversions yet. Try it: drag a file anywhere and press Shift while dragging.",
  historyOverlay: "Wheel",
  historyWindow: "Window",
  aboutLine: "Uruk Convert — a clean-room, open-source rebuild; first app of the URUK Trilogy Two.",
};

export const dict: Record<Lang, typeof ar> = { ar, en };

export function familyLabel(lang: Lang, family: string): string {
  const t = dict[lang];
  switch (family) {
    case "image": return t.familyImage;
    case "audio": return t.familyAudio;
    case "video": return t.familyVideo;
    case "archive": return t.familyArchive;
    case "pdf": return t.familyPdf;
    case "document": return t.familyDocument;
    case "slides": return t.familySlides;
    case "spreadsheet": return t.familySpreadsheet;
    case "subtitle": return t.familySubtitle;
    default: return t.familyUnknown;
  }
}

export function errorLabel(lang: Lang, code: string, fallback: string): string {
  const t = dict[lang];
  switch (code) {
    case "tool_missing": return t.errToolMissing;
    case "heic_encode_unavailable": return t.errHeic;
    case "no_audio_stream": return t.errNoAudio;
    case "pdf_to_spreadsheet_unavailable": return t.errPdfSheet;
    case "cancelled": return t.errCancelled;
    default: return fallback || t.errGeneric;
  }
}

export function formatBytes(n: number, lang: Lang): string {
  if (!Number.isFinite(n) || n < 0) return "—";
  const units = ["B", "KB", "MB", "GB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  const num = v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2);
  return lang === "ar" ? `${num} ${units[i]}` : `${num} ${units[i]}`;
}
