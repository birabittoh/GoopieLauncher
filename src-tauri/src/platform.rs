//! Platform information and OS-level operations (open URL, open folder, pick folder).

/// Returns "Windows", "macOS", or "Linux".
pub fn get_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos"   => "macOS",
        _         => "Linux",
    }
}

/// Returns the machine architecture (e.g. "x86_64", "aarch64").
pub fn get_arch() -> &'static str {
    std::env::consts::ARCH
}

/// Open a URL in the default browser.
pub fn open_url(url: &str) {
    let _ = open::that(url);
}

/// Open a directory in the system file manager, creating it if necessary.
pub fn open_folder(path: &str) {
    let _ = std::fs::create_dir_all(path);
    let _ = open::that(path);
}

/// Show a native folder-picker dialog and return the selected path, or `None` if cancelled.
pub fn pick_folder(title: &str) -> Option<String> {
    rfd::FileDialog::new()
        .set_title(title)
        .pick_folder()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Show a native file-picker dialog filtered to `.iso` files.
pub fn pick_iso_file() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("Select ISO file")
        .add_filter("ISO files", &["iso"])
        .pick_file()
        .map(|p| p.to_string_lossy().into_owned())
}
