fn main() {
    let Ok(raw) = std::env::var("SENDSPIN_BUILD_VERSION") else {
        return;
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }

    let normalized = trimmed.strip_prefix('v').unwrap_or(trimmed);
    println!("cargo:rustc-env=SENDSPIN_BUILD_VERSION={normalized}");
}
