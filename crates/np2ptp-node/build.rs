fn main() {
    #[cfg(windows)]
    {
        // Version fields (ProductVersion/FileVersion — what Explorer's file
        // Properties > Details tab shows) default to Cargo.toml's package
        // version; no need to set them by hand here.
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "NP2PTP");
        res.set("FileDescription", "NP2PTP node CLI (pack / get / serve / fetch)");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows version resource: {e}");
        }
    }
}
