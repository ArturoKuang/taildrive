fn main() {
    for key in [
        "TAILDRIVE_CHANNEL",
        "TAILDRIVE_BUILD_NUMBER",
        "TAILDRIVE_COMMIT_SHA",
        "TAILDRIVE_BUILT_AT",
        "TAILDRIVE_UPDATE_MANIFEST_URL",
        "TAILDRIVE_UPDATE_PUBLIC_KEY",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
}
