fn main() {
    #[cfg(windows)]
    {
        // Version fields (ProductVersion/FileVersion — what Explorer's file
        // Properties > Details tab shows) default to Cargo.toml's package
        // version; no need to set them by hand here.
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "NP2PTP");
        // Windows Firewall's "allow access" prompt (and Task Manager) show this
        // field verbatim as the app name — keep it the bare product name.
        res.set("FileDescription", "NP2PTP");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows version resource: {e}");
        }
    }
}
