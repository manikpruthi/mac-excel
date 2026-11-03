#[cfg(not(target_arch = "wasm32"))]
use super::Arc;
use super::FontDefinitions;
use super::font_setup::HanOrder;
#[cfg(not(target_arch = "wasm32"))]
use super::font_setup::{in_dirs, try_load};

/// Writing systems whose fallback faces are loaded only once something shows them. Each face is
/// a whole font file kept in memory (macOS's Korean face alone is 55 MB), so a Latin-only session
/// loads none of them; the symbol faces, which the interface itself uses, are always loaded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scripts(u8);

impl Scripts {
    pub const NONE: Scripts = Scripts(0);
    /// Chinese characters and Japanese kana.
    pub const HAN: Scripts = Scripts(1);
    pub const HANGUL: Scripts = Scripts(2);
    pub const THAI: Scripts = Scripts(4);
    pub const LAO: Scripts = Scripts(8);
    pub const HEBREW: Scripts = Scripts(16);
    pub const ALL: Scripts = Scripts(31);

    pub const fn union(self, other: Scripts) -> Scripts {
        Scripts(self.0 | other.0)
    }
    pub const fn contains(self, other: Scripts) -> bool {
        self.0 & other.0 == other.0
    }
    /// The script of one character, when it needs a fallback face.
    pub fn of_char(c: char) -> Scripts {
        match u32::from(c) {
            0x0590..=0x05FF | 0xFB1D..=0xFB4F => Scripts::HEBREW,
            0x0E00..=0x0E7F => Scripts::THAI,
            0x0E80..=0x0EFF => Scripts::LAO,
            0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF => Scripts::HANGUL,
            0x2E80..=0x2FFF | 0x3000..=0x312F | 0x3190..=0x9FFF | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF => {
                Scripts::HAN
            }
            _ => Scripts::NONE,
        }
    }
    /// Every script in `text` that needs a fallback face (ASCII text needs none).
    pub fn of_text(text: &str) -> Scripts {
        if text.is_ascii() {
            return Scripts::NONE;
        }
        let mut s = Scripts::NONE;
        for c in text.chars() {
            s = s.union(Scripts::of_char(c));
            if s == Scripts::ALL {
                break;
            }
        }
        s
    }
    /// The script the interface language is written in.
    pub fn of_language(language: crate::i18n::Language) -> Scripts {
        use crate::i18n::Language;
        match language {
            Language::Zh | Language::Ja => Scripts::HAN,
            Language::Ko => Scripts::HANGUL,
            _ => Scripts::NONE,
        }
    }
    /// The script a fallback key serves; the symbol faces serve none and are always loaded.
    #[cfg(not(target_arch = "wasm32"))]
    fn of_key(key: &str) -> Scripts {
        match key {
            "sys-cjk-han-ja" | "sys-cjk-han-zh" => Scripts::HAN,
            "sys-cjk-hangul" => Scripts::HANGUL,
            "sys-thai" => Scripts::THAI,
            "sys-lao" => Scripts::LAO,
            "sys-hebrew" => Scripts::HEBREW,
            _ => Scripts::NONE,
        }
    }
}

/// The scripts shown so far in this run. It only grows: a face loaded once stays in the shared
/// font-file cache anyway.
static NEEDED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// Set when [`NEEDED`] grew, until [`take_scripts_grown`] reports it.
static GROWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Records that `scripts` are on screen (or about to be). Returns whether that added one, in which
/// case the font set has to be rebuilt to carry its face.
pub fn need_scripts(scripts: Scripts) -> bool {
    use std::sync::atomic::Ordering;
    let before = NEEDED.fetch_or(scripts.0, Ordering::Relaxed);
    let grew = before | scripts.0 != before;
    if grew {
        GROWN.store(true, Ordering::Relaxed);
    }
    grew
}

/// The scripts recorded with [`need_scripts`].
pub fn needed_scripts() -> Scripts {
    Scripts(NEEDED.load(std::sync::atomic::Ordering::Relaxed))
}

/// Whether [`need_scripts`] added a script since the last call: the font set needs a rebuild.
pub fn take_scripts_grown() -> bool {
    GROWN.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Loads the system non-Latin fallback faces (one file per script family) for `scripts`, plus the
/// symbol faces, and returns their keys so every family can carry them as fallback. egui's bundled fonts have no coverage beyond Latin, so
/// without this a Thai, Hebrew, Japanese or Chinese workbook shows tofu boxes even though the text
/// was read correctly. With a face installed those scripts get real glyphs instead of tofu; nothing
/// is bundled, and a machine without the face keeps the old behaviour.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn load_fallback(fonts: &mut FontDefinitions, han: HanOrder, scripts: Scripts) -> Vec<String> {
    let mut keys = Vec::new();
    // Files already loaded as a fallback face. A file serves at most one key: the region-merged
    // Noto Sans CJK .ttc found for Han also carries Hangul (and Tahoma carries both Thai and
    // Hebrew), so loading it again for a later script would only duplicate tens of MB in memory.
    let mut loaded: Vec<String> = Vec::new();
    for (key, cands) in fallback_faces(han) {
        if !scripts.contains(Scripts::of_key(key)) {
            continue;
        }
        for (path, index) in &cands {
            if loaded.contains(path) {
                break; // a face loaded for an earlier script already covers this one
            }
            if let Some(fd) = try_load(&[(path.as_str(), *index)]) {
                fonts.font_data.insert(key.to_string(), Arc::new(fd));
                keys.push(key.to_string());
                loaded.push(path.clone());
                break;
            }
        }
    }
    keys
}

#[cfg(target_arch = "wasm32")]
pub(super) fn load_fallback(_fonts: &mut FontDefinitions, _han: HanOrder, _scripts: Scripts) -> Vec<String> {
    Vec::new()
}

/// Every fallback face we look for, in chain order: `(key, candidate files)`. One file per script
/// family is enough — epaint picks the first face with the glyph, and a script's glyphs live in one
/// file. Order only matters between faces that both cover a glyph (Japanese vs Chinese kanji), so
/// the interface language reorders the two Han entries and nothing else.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn fallback_faces(han: HanOrder) -> Vec<(&'static str, Vec<(String, u32)>)> {
    let ja = ("sys-cjk-han-ja", cjk_han_japanese());
    let zh = ("sys-cjk-han-zh", cjk_han_simplified());
    let (first, second) = match han {
        HanOrder::Japanese => (ja, zh),
        HanOrder::Chinese => (zh, ja),
    };
    vec![
        first,
        second,
        ("sys-cjk-hangul", cjk_hangul()),
        ("sys-thai", thai()),
        ("sys-lao", lao()),
        ("sys-hebrew", hebrew()),
        ("sys-symbols", symbols()),
        ("sys-symbols-2", symbols_more()),
    ]
}

/// Linux directories that hold CJK fonts across distributions: Debian/Ubuntu (`opentype/noto`,
/// `truetype/wqy`), Fedora (`google-noto-cjk`), Arch (`noto-cjk`, `wenquanyi`), ChromeOS's Linux
/// container (`chromeos/notocjk`, #1762), plus local and
/// flat layouts. Flatpak exposes the same host layouts through read-only font mounts.
#[cfg(not(target_arch = "wasm32"))]
const LINUX_CJK_DIRS: [&str; 22] = [
    // Flatpak mounts host fonts here without a broad filesystem permission. Keep the same
    // distribution layouts as below; /usr/share/fonts inside the sandbox belongs to the runtime.
    "/run/host/fonts/opentype/noto",
    "/run/host/fonts/google-noto-cjk",
    "/run/host/fonts/noto-cjk",
    "/run/host/fonts/truetype/noto",
    "/run/host/fonts/noto",
    "/run/host/fonts/truetype",
    "/run/host/fonts/TTF",
    "/run/host/fonts",
    "/run/host/local-fonts",
    "/run/host/local-fonts/noto-cjk",
    "/run/host/user-fonts",
    "/usr/share/fonts/opentype/noto",
    "/usr/share/fonts/google-noto-cjk",
    "/usr/share/fonts/noto-cjk",
    "/usr/share/fonts/truetype/noto",
    "/usr/share/fonts/noto",
    "/usr/share/fonts/truetype",
    "/usr/share/fonts/TTF",
    "/usr/share/fonts",
    "/usr/local/share/fonts",
    "/usr/local/share/fonts/noto-cjk",
    "/usr/share/fonts/chromeos/notocjk",
];

/// The Windows fonts folder (`%WINDIR%\Fonts`, or the default install location).
#[cfg(not(target_arch = "wasm32"))]
fn windows_fonts() -> String {
    std::env::var("WINDIR").map(|w| format!("{w}\\Fonts")).unwrap_or_else(|_| "C:\\Windows\\Fonts".into())
}

/// The Noto Sans CJK faces for one locale (`jp`, `sc`, `kr`): the per-locale OTF, then the
/// region-merged TTC at `face`, in every Linux font directory.
#[cfg(not(target_arch = "wasm32"))]
fn noto_cjk(locale: &str, face: u32) -> Vec<(String, u32)> {
    let mut v = Vec::new();
    for d in LINUX_CJK_DIRS {
        v.push((format!("{d}/NotoSansCJK{locale}-Regular.otf"), 0));
        v.push((format!("{d}/NotoSansCJK-Regular.ttc"), face));
    }
    v
}

/// Japanese Han + Kana faces (Japanese kanji glyph shapes).
///
/// The region-merged `NotoSansCJK-Regular.ttc` holds one face per locale (JP 0, KR 1, SC 2, TC 3,
/// HK 4) with the same glyph coverage, so the index only picks glyph shapes. Whichever Han list
/// comes first loads it; [`load_fallback`] does not load the same file again for the other.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn cjk_han_japanese() -> Vec<(String, u32)> {
    let mac = "/System/Library/Fonts";
    let win = windows_fonts();
    let mut v: Vec<(String, u32)> = Vec::new();
    v.push((format!("{win}\\YuGothR.ttc"), 0)); // Yu Gothic
    v.push((format!("{win}\\msgothic.ttc"), 0)); // MS Gothic
    v.push((format!("{mac}/ヒラギノ角ゴシック W3.ttc"), 0)); // Hiragino Sans
    v.extend(noto_cjk("jp", 0));
    v
}

/// Simplified Chinese Han faces. Loaded apart from the Japanese ones so a glyph one family lacks
/// (simplified-only hanzi in a Japanese font) still falls through to a real glyph.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn cjk_han_simplified() -> Vec<(String, u32)> {
    let mac = "/System/Library/Fonts";
    let win = windows_fonts();
    let mut v: Vec<(String, u32)> = vec![
        (format!("{win}\\msyh.ttc"), 0),            // Microsoft YaHei
        (format!("{win}\\simsun.ttc"), 0),          // SimSun
        (format!("{win}\\simhei.ttf"), 0),          // SimHei
        (format!("{mac}/Hiragino Sans GB.ttc"), 0), // ships on every recent macOS
        (format!("{mac}/PingFang.ttc"), 0),         // older macOS only; now a downloadable asset
    ];
    v.extend(noto_cjk("sc", 2));
    for d in LINUX_CJK_DIRS {
        v.push((format!("{d}/wqy/wqy-microhei.ttc"), 0));
        v.push((format!("{d}/wenquanyi/wqy-microhei.ttc"), 0));
        v.push((format!("{d}/wqy-microhei.ttc"), 0));
    }
    v
}

/// Hangul faces (Korean). The Han faces above that come from the region-merged Noto Sans CJK file
/// already carry Hangul; [`load_fallback`] then skips this list rather than load the file twice.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn cjk_hangul() -> Vec<(String, u32)> {
    let mac = "/System/Library/Fonts";
    let win = windows_fonts();
    let mut v: Vec<(String, u32)> = Vec::new();
    v.push((format!("{win}\\malgun.ttf"), 0)); // Malgun Gothic
    v.push((format!("{mac}/AppleSDGothicNeo.ttc"), 0));
    v.extend(noto_cjk("kr", 1));
    v
}

/// Thai faces. Windows ships Leelawadee UI and Tahoma/Microsoft Sans Serif (both carry Thai);
/// Linux font directories searched by the Thai and Hebrew faces, in priority order.
#[cfg(not(target_arch = "wasm32"))]
fn linux_noto_dirs() -> [&'static str; 5] {
    [
        "/usr/share/fonts/truetype/noto",
        "/usr/share/fonts/opentype/noto",
        "/usr/share/fonts/truetype",
        "/usr/share/fonts",
        "/usr/share/fonts/chromeos/noto", // ChromeOS's Linux container (#1762)
    ]
}

/// Faces for one script: Windows files under `WINDIR\Fonts` first, then macOS paths, then the
/// Noto/Linux files in every [`linux_noto_dirs`] directory, then the Fedora variable font.
#[cfg(not(target_arch = "wasm32"))]
fn script_fonts(win: &[&str], mac: &[&str], lin_files: &[&str], variable: &str) -> Vec<(String, u32)> {
    let w = windows_fonts();
    let mut v: Vec<(String, u32)> = win.iter().map(|f| (format!("{w}\\{f}"), 0)).collect();
    v.extend(mac.iter().map(|p| (p.to_string(), 0)));
    v.extend(in_dirs(&linux_noto_dirs(), lin_files).into_iter().map(|p| (p, 0)));
    v.push((variable.into(), 0));
    v
}

/// macOS has Thonburi; Linux usually has Noto Sans Thai or Garuda.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn thai() -> Vec<(String, u32)> {
    script_fonts(
        // Leelawadee UI, Tahoma, Microsoft Sans Serif
        &["LeelawUI.ttf", "tahoma.ttf", "micross.ttf"],
        &["/System/Library/Fonts/Supplemental/Thonburi.ttc", "/System/Library/Fonts/SukhumvitSet.ttc"],
        &["NotoSansThai-Regular.ttf", "tlwg/Garuda.ttf", "garuda/Garuda.ttf"],
        // Fedora ships the variable Noto Sans Thai; its default instance is Regular.
        "/usr/share/fonts/google-noto-vf/NotoSansThai[wght].ttf",
    )
}

/// Lao faces. Windows ships Leelawadee UI (and DokChampa on older versions); macOS has Lao Sangam
/// MN and Lao MN; Linux usually has Noto Sans Lao or Phetsarath OT.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn lao() -> Vec<(String, u32)> {
    let mac_sup = "/System/Library/Fonts/Supplemental";
    let win = windows_fonts();
    let lin = ["/usr/share/fonts/truetype/noto", "/usr/share/fonts/opentype/noto", "/usr/share/fonts/truetype", "/usr/share/fonts"];
    let mut v: Vec<(String, u32)> = vec![
        (format!("{win}\\LeelawUI.ttf"), 0), // Leelawadee UI
        (format!("{win}\\dokchamp.ttf"), 0), // DokChampa
        (format!("{mac_sup}/Lao Sangam MN.ttf"), 0),
        (format!("{mac_sup}/Lao MN.ttc"), 0),
    ];
    for d in lin {
        v.push((format!("{d}/NotoSansLao-Regular.ttf"), 0));
        v.push((format!("{d}/lao/Phetsarath_OT.ttf"), 0));
        v.push((format!("{d}/Phetsarath_OT.ttf"), 0));
    }
    v
}

/// Hebrew faces. Windows has David and FrankRuehl (and Tahoma/Courier New carry Hebrew); macOS has
/// Arial Hebrew and David; Linux usually has Noto Sans Hebrew or DejaVu Sans.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn hebrew() -> Vec<(String, u32)> {
    script_fonts(
        // David, FrankRuehl, Tahoma
        &["david.ttf", "frank.ttf", "tahoma.ttf"],
        &[
            "/System/Library/Fonts/ArialHB.ttc",
            "/System/Library/Fonts/SFHebrew.ttf",
            "/System/Library/Fonts/Supplemental/Arial Hebrew.ttf",
            "/System/Library/Fonts/Supplemental/David.ttf",
            "/System/Library/Fonts/LucidaGrande.ttc",
            "/System/Library/Fonts/Lucida Grande.ttc",
        ],
        &["NotoSansHebrew-Regular.ttf", "dejavu/DejaVuSans.ttf", "freefont/FreeSans.ttf"],
        "/usr/share/fonts/google-noto-vf/NotoSansHebrew[wght].ttf",
    )
}

/// Symbol faces behind the UI font. Interface fonts such as Noto Sans and Cantarell (and egui's
/// built-in Ubuntu) lack arrows, check marks, geometric shapes and keyboard symbols (▾ ✓ ✕ ▲ ↳ ⇧),
/// which then render as empty boxes (#197, #54). DejaVu Sans covers most of them; the paths cover
/// Debian/Ubuntu, Fedora and Arch layouts. [`load_fallback`] skips the file if Hebrew already
/// loaded it.
#[cfg(not(target_arch = "wasm32"))]
fn symbols() -> Vec<(String, u32)> {
    let win = std::env::var("WINDIR").map(|w| format!("{w}\\Fonts")).unwrap_or_else(|_| "C:\\Windows\\Fonts".into());
    let mut v: Vec<(String, u32)> = vec![(format!("{win}\\seguisym.ttf"), 0), ("/System/Library/Fonts/Apple Symbols.ttf".into(), 0)];
    for d in [
        "/usr/share/fonts/truetype/dejavu",
        "/usr/share/fonts/dejavu-sans-fonts",
        "/usr/share/fonts/TTF",
        "/usr/share/fonts/dejavu",
        "/usr/share/fonts/truetype/ttf-dejavu",
        "/usr/local/share/fonts/dejavu",
        "/usr/local/share/fonts",
    ] {
        v.push((format!("{d}/DejaVuSans.ttf"), 0));
    }
    v
}

/// A second symbol face for what the first lacks (⌘ and other technical symbols): Noto Sans
/// Symbols 2 where it is installed, or Segoe UI Symbol behind Segoe UI.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn symbols_more() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    for d in [
        "/usr/share/fonts/noto",
        "/usr/share/fonts/truetype/noto",
        "/usr/share/fonts/opentype/noto",
        "/usr/share/fonts/google-noto-sans-symbols-2-fonts",
        "/usr/share/fonts/google-noto",
        "/usr/local/share/fonts/noto",
        "/usr/share/fonts/chromeos/noto",
    ] {
        v.push((format!("{d}/NotoSansSymbols2-Regular.ttf"), 0));
    }
    v
}

/// Whether any symbol fallback face is installed (tests skip their glyph checks without one).
pub fn symbol_face_installed() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        symbols().iter().chain(symbols_more().iter()).any(|(p, _)| std::path::Path::new(p).exists())
    }
    #[cfg(target_arch = "wasm32")]
    {
        false
    }
}
