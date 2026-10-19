# Uruk Convert v1.0.0 — محوّل أوروك

**An offline file converter that lives in your tray, not in a window.**
Drag any file, anywhere on your desktop. Tap **Shift** mid-drag and a
golden sun-disc wheel appears at your cursor — drop on a format and the
file is converted on the spot. **Shift+Alt** opens the tools wheel
(Crop, Trim, PDF Split/Merge/Rotate/Compress, true Redact, Metadata
strip…), **Control** opens file actions (Copy path, Move to…, Reveal…).
Everything runs locally; nothing is uploaded, ever.

Arabic + English UI (Arabic first, full RTL). Free and open source
(Apache-2.0).

## Downloads

| Platform | File |
|---|---|
| Linux (Debian/Ubuntu) | `Uruk.Convert_1.0.0_amd64.deb` |
| Linux (Fedora/RHEL) | `Uruk.Convert-1.0.0-1.x86_64.rpm` |
| Linux (portable) | `Uruk.Convert_1.0.0_amd64.AppImage` |
| Windows | `Uruk.Convert_1.0.0_x64-setup.exe` (NSIS) or `Uruk.Convert_1.0.0_x64_en-US.msi` |
| macOS (Apple Silicon) | `Uruk.Convert_1.0.0_aarch64.dmg` |
| macOS (Intel) | `Uruk.Convert_1.0.0_x64.dmg` |

## Install

- **Linux (.deb):** `sudo apt install ./Uruk*.deb` — dependencies
  (WebKitGTK, poppler-utils, ImageMagick, Ghostscript) resolve
  automatically. For documents/spreadsheets also install LibreOffice
  (see below).
- **Linux (AppImage):** `chmod +x` and run. Image/PDF tools use your
  system's ImageMagick, poppler-utils and Ghostscript if present.
- **Windows:** run the setup `.exe`. The app is **unsigned**, so
  SmartScreen will warn: click **More info → Run anyway**.
- **macOS:** open the `.dmg`, drag the app to Applications. The app is
  **unsigned** (no Apple certificate), so on first launch
  **right-click the app → Open → Open**. For the drag wheel to work,
  grant **Accessibility** permission when asked (System Settings →
  Privacy & Security → Accessibility) — without it the app still works
  via its window, and says so honestly.

## Please know (honest limits)

- **LibreOffice is not bundled.** Documents, slides, spreadsheets and
  PDF export are powered by a **system-installed LibreOffice** (free,
  ~284 MB — bundling it would triple this download). If it's missing,
  those conversions are unavailable and the app tells you plainly;
  Settings shows whether it was detected. Get it from libreoffice.org
  or your package manager.
- **macOS / Windows drag gesture: build-proven, runtime proof pending.**
  The installers are built and smoke-tested in CI, but CI machines
  cannot perform a real mid-drag gesture (macOS Accessibility, real
  desktops), so the global wheel on those platforms is marked
  *implemented, pending real-machine proof*. On **Linux/X11 the full
  gesture is proven end-to-end**. On Wayland, compositors forbid
  observing other apps' drags by design — a standing orb / fallback
  window ships instead.
- **HEIC export:** the engine probes at runtime for a real encoder and
  uses the first it finds — Apple's `sips` on macOS, an ImageMagick/
  FFmpeg HEIC writer where the platform provides one (the Linux
  release build passes a real HEIC-encode test in CI). Where no
  encoder exists, HEIC *export* is refused with a plain explanation
  rather than faked (HEIC *input* converts fine everywhere).
- **PDF → XLSX is refused by design** (a PDF stores drawn text, not
  cells). PDF → DOCX/PPTX/EPUB are text-faithful rebuilds, labelled as
  such. Pages and RAR creation are not offered — no open toolchain
  writes them.
- 237 conversions ship, each covered by automated fixture tests
  (engine suite runs in CI on this exact tag). Full matrix and every
  asterisk: see PARITY.md in the repository.

---

## بالعربية

**محوّل ملفات يعمل بلا إنترنت ويعيش في شريط النظام — لا في نافذة.**
اسحب أي ملف في أي مكان على سطح المكتب، واضغط **Shift** أثناء السحب
لتظهر عجلة الشمس الذهبية عند المؤشر — أفلت فوق الصيغة المطلوبة فيتحوّل
الملف فورًا. **Shift+Alt** لعجلة الأدوات (قص، تقليم، تقسيم/دمج/تدوير/
ضغط PDF، تنقيح حقيقي يزيل النص فعليًا، حذف البيانات الوصفية…)،
و**Control** لإجراءات الملف (نسخ المسار، نقل إلى…، إظهار في المجلد…).
كل شيء يعمل على جهازك؛ لا يُرفع أي ملف أبدًا.

الواجهة عربية وإنجليزية (العربية أولًا، RTL كامل). مجاني ومفتوح المصدر.

**التثبيت:** على لينكس ثبّت ملف `.deb` بـ
`sudo apt install ./Uruk*.deb`. على ويندوز شغّل ملف الإعداد — التطبيق
**غير موقّع**، فاختر «مزيد من المعلومات ← تشغيل على أي حال» عند تحذير
SmartScreen. على ماك افتح `.dmg` واسحب التطبيق إلى Applications، وعند
أول تشغيل: **زر أيمن على التطبيق ← فتح** (التطبيق غير موقّع)، وامنحه
إذن **إمكانية الوصول** لتعمل عجلة السحب.

**حدود صريحة:** برنامج **LibreOffice غير مدمج** — المستندات والعروض
والجداول وتصدير PDF تعتمد على تثبيته في نظامك (مجاني، ~284MB)، وبدونه
يخبرك التطبيق بوضوح أن تلك التحويلات غير متاحة. عجلة السحب على
ماك/ويندوز **مبنية ومُختبَرة بناءً في CI وبانتظار إثبات على جهاز
حقيقي**؛ على لينكس/X11 مُبرهَنة بالكامل من الطرف إلى الطرف. تصدير
HEIC يعمل حيث يجد المحرك مرمّزًا حقيقيًا (على ماك عبر أداة آبل
`sips`، وعلى لينكس مُبرهَن باختبار فعلي في CI)؛ وحيث لا يوجد مرمّز
يُرفض التصدير بشرح صريح ولا يُزيَّف، بينما فتح ملفات HEIC وتحويلها
يعمل في كل مكان. تحويل PDF إلى XLSX مرفوض تصميمًا — الـ PDF يحفظ
نصًا مرسومًا لا خلايا.
